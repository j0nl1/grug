use {
    crate::Signer,
    anyhow::ensure,
    grug_app::{App, AppError, AppResult, Vm},
    grug_crypto::sha2_256,
    grug_db_memory::MemDb,
    grug_types::{
        from_json_value, to_json_value, Addr, Binary, BlockInfo, BlockOutcome, Coins,
        ConfigUpdates, Duration, GenericResult, GenesisState, Hash256, InfoResponse, Json, Message,
        NumberConst, Op, Outcome, QueryRequest, StdError, Tx, TxOutcome, Uint256, Uint64,
    },
    grug_vm_rust::RustVm,
    serde::{de::DeserializeOwned, ser::Serialize},
    std::collections::{BTreeMap, HashMap},
};

pub struct TestSuite<VM = RustVm>
where
    VM: Vm,
{
    app: App<MemDb, VM>,
    /// The chain ID can be queries from the `app`, but we internally track it in
    /// the test suite, so we don't need to query it every time we need it.
    chain_id: String,
    /// Interally track the last finalized block.
    block: BlockInfo,
    /// Each time we make a new block, we set the new block's time as the
    /// previous block's time plus this value.
    block_time: Duration,
    /// Internally track each account's sequence number.
    sequences: HashMap<Addr, u32>,
}

impl<VM> TestSuite<VM>
where
    VM: Vm + Clone,
    AppError: From<VM::Error>,
{
    /// Create a new test suite with a given VM.
    ///
    /// It's not recommended to call this directly. Use [`TestBuilder`](crate::TestBuilder)
    /// instead.
    pub fn new_with_vm(
        vm: VM,
        chain_id: String,
        block_time: Duration,
        genesis_block: BlockInfo,
        genesis_state: GenesisState,
    ) -> anyhow::Result<Self> {
        // Use `u64::MAX` as query gas limit so that there's practically no limit.
        let app = App::new(MemDb::new(), vm, u64::MAX);

        app.do_init_chain(chain_id.clone(), genesis_block, genesis_state)?;

        Ok(Self {
            app,
            chain_id,
            block: genesis_block,
            block_time,
            sequences: HashMap::new(),
        })
    }

    /// Make a new block with the given transactions.
    pub fn make_block(&mut self, txs: Vec<Tx>) -> anyhow::Result<BlockOutcome> {
        let num_txs = txs.len();

        // Advance block height and time
        self.block.height += Uint64::ONE;
        self.block.timestamp = self.block.timestamp + self.block_time;

        // Call ABCI `FinalizeBlock` method
        let block_outcome = self.app.do_finalize_block(self.block, txs)?;

        // Sanity check: the number of tx results returned by the app should
        // equal the number of txs.
        ensure!(
            num_txs == block_outcome.tx_outcomes.len(),
            "sent {} txs but received {} tx results; something is wrong",
            num_txs,
            block_outcome.tx_outcomes.len()
        );

        // Call ABCI `Commit` method
        self.app.do_commit()?;

        Ok(block_outcome)
    }

    /// Make a new block without any transaction.
    pub fn make_empty_block(&mut self) -> anyhow::Result<()> {
        self.make_block(vec![])?;
        Ok(())
    }

    /// Execute a single message under the given gas limit.
    pub fn send_message_with_gas(
        &mut self,
        signer: &dyn Signer,
        gas_limit: u64,
        msg: Message,
    ) -> anyhow::Result<TxOutcome> {
        self.send_messages_with_gas(signer, gas_limit, vec![msg])
    }

    /// Execute one or more messages under the given gas limit.
    pub fn send_messages_with_gas(
        &mut self,
        signer: &dyn Signer,
        gas_limit: u64,
        msgs: Vec<Message>,
    ) -> anyhow::Result<TxOutcome> {
        ensure!(!msgs.is_empty(), "please send more than zero messages");

        // Compose and sign a single message
        let sequence = self.sequences.entry(signer.address()).or_insert(0);
        let tx = signer.sign_transaction(msgs.clone(), gas_limit, &self.chain_id, *sequence)?;
        *sequence += 1;

        // Make a new block with only this transaction
        let mut block_outcome = self.make_block(vec![tx])?;

        // Sanity check: we sent one transaction, so there should be exactly one
        // transaction outcome in the block outcome.
        ensure!(
            block_outcome.tx_outcomes.len() == 1,
            "expecting exactly one transaction outcome, got {}; something is wrong!",
            block_outcome.tx_outcomes.len()
        );

        Ok(block_outcome.tx_outcomes.pop().unwrap())
    }

    /// Update the chain's config under the given gas limit.
    pub fn configure_with_gas(
        &mut self,
        signer: &dyn Signer,
        gas_limit: u64,
        updates: ConfigUpdates,
        app_updates: BTreeMap<String, Op<Json>>,
    ) -> anyhow::Result<()> {
        self.send_message_with_gas(signer, gas_limit, Message::configure(updates, app_updates))?
            .result
            .should_succeed();

        Ok(())
    }

    /// Make a transfer of tokens under the given gas limit.
    pub fn transfer_with_gas<C>(
        &mut self,
        signer: &dyn Signer,
        gas_limit: u64,
        to: Addr,
        coins: C,
    ) -> anyhow::Result<()>
    where
        C: TryInto<Coins>,
        StdError: From<C::Error>,
    {
        self.send_message_with_gas(signer, gas_limit, Message::transfer(to, coins)?)?
            .result
            .should_succeed();

        Ok(())
    }

    /// Upload a code under the given gas limit. Return the code's hash.
    pub fn upload_with_gas<B>(
        &mut self,
        signer: &dyn Signer,
        gas_limit: u64,
        code: B,
    ) -> anyhow::Result<Hash256>
    where
        B: Into<Binary>,
    {
        let code = code.into();
        let code_hash = Hash256::from_array(sha2_256(&code));

        self.send_message_with_gas(signer, gas_limit, Message::upload(code))?
            .result
            .should_succeed();

        Ok(code_hash)
    }

    /// Instantiate a contract under the given gas limit. Return the contract's
    /// address.
    pub fn instantiate_with_gas<M, S, C>(
        &mut self,
        signer: &dyn Signer,
        gas_limit: u64,
        code_hash: Hash256,
        salt: S,
        msg: &M,
        funds: C,
    ) -> anyhow::Result<Addr>
    where
        M: Serialize,
        S: Into<Binary>,
        C: TryInto<Coins>,
        StdError: From<C::Error>,
    {
        let salt = salt.into();
        let address = Addr::compute(signer.address(), code_hash, &salt);

        self.send_message_with_gas(
            signer,
            gas_limit,
            Message::instantiate(code_hash, msg, salt, funds, None)?,
        )?
        .result
        .should_succeed();

        Ok(address)
    }

    /// Upload a code and instantiate a contract with it in one go under the
    /// given gas limit. Return the code hash as well as the contract's address.
    pub fn upload_and_instantiate_with_gas<M, B, S, C>(
        &mut self,
        signer: &dyn Signer,
        gas_limit: u64,
        code: B,
        salt: S,
        msg: &M,
        funds: C,
    ) -> anyhow::Result<(Hash256, Addr)>
    where
        M: Serialize,
        B: Into<Binary>,
        S: Into<Binary>,
        C: TryInto<Coins>,
        StdError: From<C::Error>,
    {
        let code = code.into();
        let code_hash = Hash256::from_array(sha2_256(&code));
        let salt = salt.into();
        let address = Addr::compute(signer.address(), code_hash, &salt);

        self.send_messages_with_gas(signer, gas_limit, vec![
            Message::upload(code),
            Message::instantiate(code_hash, msg, salt, funds, None)?,
        ])?
        .result
        .should_succeed();

        Ok((code_hash, address))
    }

    /// Execute a contrat under the given gas limit.
    pub fn execute_with_gas<M, C>(
        &mut self,
        signer: &dyn Signer,
        gas_limit: u64,
        contract: Addr,
        msg: &M,
        funds: C,
    ) -> anyhow::Result<()>
    where
        M: Serialize,
        C: TryInto<Coins>,
        StdError: From<C::Error>,
    {
        self.send_message_with_gas(signer, gas_limit, Message::execute(contract, msg, funds)?)?
            .result
            .should_succeed();

        Ok(())
    }

    /// Migrate a contract to a new code hash, under the given gas limit.
    pub fn migrate_with_gas<M>(
        &mut self,
        signer: &dyn Signer,
        gas_limit: u64,
        contract: Addr,
        new_code_hash: Hash256,
        msg: &M,
    ) -> anyhow::Result<()>
    where
        M: Serialize,
    {
        self.send_message_with_gas(
            signer,
            gas_limit,
            Message::migrate(contract, new_code_hash, msg)?,
        )?
        .result
        .should_succeed();

        Ok(())
    }

    pub fn check_tx(&self, tx: Tx) -> anyhow::Result<Outcome> {
        Ok(self.app.do_check_tx(tx)?)
    }

    pub fn query_info(&self) -> GenericResult<InfoResponse> {
        self.app
            .do_query_app(QueryRequest::Info {}, 0, false)
            .map(|val| val.as_info())
            .into()
    }

    pub fn query_balance(&self, account: &dyn Signer, denom: &str) -> GenericResult<Uint256> {
        self.app
            .do_query_app(
                QueryRequest::Balance {
                    address: account.address(),
                    denom: denom.to_string(),
                },
                0, // zero means to use the latest height
                false,
            )
            .map(|res| res.as_balance().amount)
            .into()
    }

    pub fn query_balances(&self, account: &dyn Signer) -> GenericResult<Coins> {
        self.app
            .do_query_app(
                QueryRequest::Balances {
                    address: account.address(),
                    start_after: None,
                    limit: Some(u32::MAX),
                },
                0, // zero means to use the latest height
                false,
            )
            .map(|res| res.as_balances())
            .into()
    }

    pub fn query_wasm_smart<M, R>(&self, contract: Addr, msg: &M) -> GenericResult<R>
    where
        M: Serialize,
        R: DeserializeOwned,
    {
        (|| -> AppResult<_> {
            let msg_raw = to_json_value(msg)?;
            let res_raw = self
                .app
                .do_query_app(
                    QueryRequest::WasmSmart {
                        contract,
                        msg: msg_raw,
                    },
                    0, // zero means to use the latest height
                    false,
                )?
                .as_wasm_smart();
            Ok(from_json_value(res_raw)?)
        })()
        .into()
    }
}

// Rust VM doesn't support gas, so we introduce these convenience methods that
// don't take a `gas_limit` parameter.
impl TestSuite<RustVm> {
    /// Create a new test suite.
    ///
    /// It's not recommended to call this directly. Use [`TestBuilder`](crate::TestBuilder)
    /// instead.
    pub fn new(
        chain_id: String,
        block_time: Duration,
        genesis_block: BlockInfo,
        genesis_state: GenesisState,
    ) -> anyhow::Result<Self> {
        Self::new_with_vm(
            RustVm::new(),
            chain_id,
            block_time,
            genesis_block,
            genesis_state,
        )
    }

    /// Execute a single message.
    pub fn send_message(&mut self, signer: &dyn Signer, msg: Message) -> anyhow::Result<TxOutcome> {
        self.send_message_with_gas(signer, u64::MAX, msg)
    }

    /// Execute one or more messages.
    pub fn send_messages(
        &mut self,
        signer: &dyn Signer,
        msgs: Vec<Message>,
    ) -> anyhow::Result<TxOutcome> {
        self.send_messages_with_gas(signer, u64::MAX, msgs)
    }

    /// Update the chain's config.
    pub fn configure(
        &mut self,
        signer: &dyn Signer,
        updates: ConfigUpdates,
        app_updates: BTreeMap<String, Op<Json>>,
    ) -> anyhow::Result<()> {
        self.configure_with_gas(signer, u64::MAX, updates, app_updates)
    }

    /// Make a transfer of tokens.
    pub fn transfer<C>(&mut self, signer: &dyn Signer, to: Addr, coins: C) -> anyhow::Result<()>
    where
        C: TryInto<Coins>,
        StdError: From<C::Error>,
    {
        self.transfer_with_gas(signer, u64::MAX, to, coins)
    }

    /// Upload a code. Return the code's hash.
    pub fn upload<B>(&mut self, signer: &dyn Signer, code: B) -> anyhow::Result<Hash256>
    where
        B: Into<Binary>,
    {
        self.upload_with_gas(signer, u64::MAX, code)
    }

    /// Instantiate a contract. Return the contract's address.
    pub fn instantiate<M, S, C>(
        &mut self,
        signer: &dyn Signer,
        code_hash: Hash256,
        salt: S,
        msg: &M,
        funds: C,
    ) -> anyhow::Result<Addr>
    where
        M: Serialize,
        S: Into<Binary>,
        C: TryInto<Coins>,
        StdError: From<C::Error>,
    {
        self.instantiate_with_gas(signer, u64::MAX, code_hash, salt, msg, funds)
    }

    /// Upload a code and instantiate a contract with it in one go. Return the
    /// code hash as well as the contract's address.
    pub fn upload_and_instantiate<M, B, S, C>(
        &mut self,
        signer: &dyn Signer,
        code: B,
        salt: S,
        msg: &M,
        funds: C,
    ) -> anyhow::Result<(Hash256, Addr)>
    where
        M: Serialize,
        B: Into<Binary>,
        S: Into<Binary>,
        C: TryInto<Coins>,
        StdError: From<C::Error>,
    {
        self.upload_and_instantiate_with_gas(signer, u64::MAX, code, salt, msg, funds)
    }

    /// Execute a contrat.
    pub fn execute<M, C>(
        &mut self,
        signer: &dyn Signer,
        contract: Addr,
        msg: &M,
        funds: C,
    ) -> anyhow::Result<()>
    where
        M: Serialize,
        C: TryInto<Coins>,
        StdError: From<C::Error>,
    {
        self.execute_with_gas(signer, u64::MAX, contract, msg, funds)
    }

    /// Migrate a contract to a new code hash.
    pub fn migrate<M>(
        &mut self,
        signer: &dyn Signer,
        contract: Addr,
        new_code_hash: Hash256,
        msg: &M,
    ) -> anyhow::Result<()>
    where
        M: Serialize,
    {
        self.migrate_with_gas(signer, u64::MAX, contract, new_code_hash, msg)
    }
}
