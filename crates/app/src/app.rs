//! The lifecycle of `S`:
//!
//! App takes a generic S which implements the Storage trait. For production,
//! this should be a wrapper around a RocksDB instance. For testing, this should
//! be cw_std::MockStorage.
//!
//! When finalize_block is called, this is wrapped in CacheStore<S>.
//! We then loop through the txs in the block.
//!
//! For each tx, it is wrapped again: CacheStore<CacheStore<S>>.
//! We then loop through the messages in the tx.
//!
//! When processing the msg, if calling a Wasm instance is involved, it is
//! wrapped again: PrefixStore<CacheStore<CacheStore<S>>>.
//!
//! Whether a message is successful or not, we unwrap the PrefixStore and return
//! the underlying: CacheStore<CacheStore<S>>.
//!
//! If all messages are successful, we commit the pending ops. If any message
//! fails, we discard the pending ops. Either way, the cache is unwrapped:
//! CacheStore<S>.
//!
//! Back to the scope of finalize_block. After all txs have been processed, we
//! disassamble the cache store, and keep the S and pending batch in memory.
//!
//! When commit is called, we flush the pending ops into S.

use {
    crate::{Batch, CacheStore, Flush, PrefixStore},
    anyhow::{anyhow, ensure},
    cw_std::{
        hash, Account, AccountResponse, Addr, Binary, BlockInfo, Bound, Coin, GenesisState, Hash,
        InfoResponse, Item, Map, Message, Order, Query, QueryResponse, Storage, Tx,
        WasmRawResponse, WasmSmartResponse,
    },
    cw_vm::{db_next, db_read, db_remove, db_scan, db_write, debug, Host, InstanceBuilder},
    tracing::{debug, error, info},
    wasmi::{Instance, Store},
};

// storage types
const CHAIN_ID:             Item<String>        = Item::new("cid");
const LAST_FINALIZED_BLOCK: Item<BlockInfo>     = Item::new("lfb");
const CODES:                Map<&Hash, Binary>  = Map::new("c");
const ACCOUNTS:             Map<&Addr, Account> = Map::new("a");
const CONTRACT_NAMESPACE:   &[u8]               = b"w";

// pagination parameters
const DEFAULT_PAGE_LIMIT: u32 = 30;

pub struct App<S> {
    store:         Option<S>,
    pending:       Option<Batch>,
    current_block: Option<BlockInfo>,
}

impl<S> App<S> {
    pub fn new(store: S) -> Self {
        Self {
            store:         Some(store),
            pending:       None,
            current_block: None,
        }
    }

    fn take_store(&mut self) -> anyhow::Result<S> {
        self.store.take().ok_or(anyhow!("[App]: store not found"))
    }

    fn take_pending(&mut self) -> anyhow::Result<Batch> {
        self.pending.take().ok_or(anyhow!("[App]: pending batch not found"))
    }

    fn take_current_block(&mut self) -> anyhow::Result<BlockInfo> {
        self.current_block.take().ok_or(anyhow!("[App]: current block info not found"))
    }

    fn put_store(&mut self, store: S) -> anyhow::Result<()> {
        ensure!(self.store.is_none(), "[App]: store already exists");
        self.store = Some(store);
        Ok(())
    }

    fn put_pending(&mut self, pending: Batch) -> anyhow::Result<()> {
        ensure!(self.pending.is_none(), "[App]: pending batch already exists");
        self.pending = Some(pending);
        Ok(())
    }

    fn put_current_block(&mut self, current_block: BlockInfo) -> anyhow::Result<()> {
        ensure!(self.current_block.is_none(), "[App]: current block info already exists");
        self.current_block = Some(current_block);
        Ok(())
    }
}

impl<S> App<S>
where
    S: Storage + 'static,
{
    pub fn init_chain(&mut self, genesis_state: GenesisState) -> anyhow::Result<()> {
        info!(
            chain_id = genesis_state.chain_id,
            gen_msgs = genesis_state.msgs.len(),
            "initializing chain",
        );

        let mut store = self.take_store()?;

        CHAIN_ID.save(&mut store, &genesis_state.chain_id)?;

        debug_assert!(genesis_state.msgs.is_empty(), "UNIMPLEMENTED: genesis msg is not supported yet");

        self.put_store(store)
    }

    pub fn finalize_block(&mut self, block: BlockInfo, txs: Vec<Tx>) -> anyhow::Result<()> {
        info!(
            height    = block.height,
            timestamp = block.timestamp,
            num_txs   = txs.len(),
            "finalizing block",
        );

        let store = self.take_store()?;

        // TODO: check block height and time is valid
        // height must be that of the last finalized block + 1
        // time must be greater than that of the last finalized block

        let mut cached = CacheStore::new(store, self.pending.take());

        for (idx, tx) in txs.into_iter().enumerate() {
            // TODO: add txhash to the debug print?
            debug!(idx, "processing tx");
            cached = run_tx(cached, tx)?;
        }

        let (store, pending) = cached.disassemble();

        self.put_store(store)?;
        self.put_pending(pending)?;
        self.put_current_block(block)
    }

    pub fn query(&mut self, req: Query) -> anyhow::Result<QueryResponse> {
        debug!(req = ?serde_json_wasm::to_string(&req)?, "processing query");

        let store = self.take_store()?;

        // perform the query
        let (res, store) = query(store, req);

        // put the store back
        self.put_store(store)?;

        res
    }
}

impl<S> App<S>
where
    S: Storage + Flush + 'static,
{
    pub fn commit(&mut self) -> anyhow::Result<()> {
        info!("committing state changes");

        let mut store = self.take_store()?;
        let pending = self.take_pending()?;
        let current_block = self.take_current_block()?;

        // apply the DB ops effected by txs in this block
        store.flush(pending)?;

        // update the last finalized block info
        LAST_FINALIZED_BLOCK.save(&mut store, &current_block)?;

        // put the store back
        self.put_store(store)
    }
}

fn run_tx<S>(store: S, tx: Tx) -> anyhow::Result<S>
where
    S: Storage + Flush + 'static,
{
    // TODO: authenticate txs

    // create cached store for this tx
    // if execution fails, state changes won't be committed
    let mut result;
    let mut cached = CacheStore::new(store, None);

    for (idx, msg) in tx.msgs.into_iter().enumerate() {
        debug!(idx, "processing msg");

        (result, cached) = run_msg(cached, msg);

        // if any one of the msgs fails, the entire tx fails.
        // discard uncommitted changes and return the underlying store
        if result.is_err() {
            let (store, _) = cached.disassemble();
            return Ok(store);
        }
    }

    // all messages succeeded. commit the state changes
    cached.flush()
}

// take an owned mutable Storage value and execute a message on it. return
// the Storage value and a result indicating whether the message was successful.
//
// we don't need to create a cached store for the msg, because tx execution is
// atomic - a single msg fails, the entire tx fails, and the cache created in
// run_tx will be discarded.
fn run_msg<S>(mut store: S, msg: Message) -> (anyhow::Result<()>, S)
where
    S: Storage + 'static,
{
    match msg {
        Message::StoreCode {
            wasm_byte_code,
        } => {
            match store_code(&mut store, &wasm_byte_code) {
                Ok(hash) => info!(hash = hash.to_string(), "stored code"),
                Err(err) => error!(?err, "failed to store code"),
            };
            (Ok(()), store)
        },
        Message::Instantiate {
            code_hash,
            msg,
            salt,
            funds,
            admin,
        } => {
            let (result, store) = instantiate(store, code_hash, msg, salt, funds, admin);
            match result {
                Ok(report) => info!(
                    address   = report.address.to_string(),
                    code_hash = report.code_hash.to_string(),
                    admin     = ?report.admin,
                    "instantiated contract",
                ),
                Err(err) => error!(?err, "failed to instantiate contract"),
            }
            (Ok(()), store)
        },
        Message::Execute {
            contract,
            msg,
            funds,
        } => {
            let (result, store) = execute(store, &contract, msg, funds);
            match result {
                Ok(_) => info!(contract = contract.to_string(), "executed contract"),
                Err(err) => error!(?err, "failed to execute contract"),
            }
            (Ok(()), store)
        },
    }
}

fn store_code<S: Storage>(store: &mut S, wasm_byte_code: &Binary) -> anyhow::Result<Hash> {
    // TODO: static check, ensure wasm code has necessary imports/exports
    let hash = hash(wasm_byte_code);

    let exists = CODES.has(store, &hash);
    ensure!(!exists, "Do not upload the same code twice");

    CODES.save(store, &hash, wasm_byte_code)?;

    Ok(hash)
}

struct InstantiateReport {
    pub address:   Addr,
    pub code_hash: Hash,
    pub admin:     Option<Addr>,
}

fn instantiate<S: Storage + 'static>(
    store:     S,
    code_hash: Hash,
    msg:       Binary,
    salt:      Binary,
    funds:     Vec<Coin>,
    admin:     Option<Addr>,
) -> (anyhow::Result<InstantiateReport>, S) {
    debug_assert!(funds.is_empty(), "UNIMPLEMENTED: sending funds is not supported yet");

    // load wasm code
    let wasm_byte_code = match CODES.load(&store, &code_hash) {
        Ok(wasm_byte_code) => wasm_byte_code,
        Err(err) => return (Err(err), store),
    };

    // compute contract address
    let address = Addr::compute(&code_hash, &salt);

    // create wasm host
    let (instance, mut wasm_store) = must_build_wasm_instance(store, &address, wasm_byte_code);
    let mut host = Host::new(&instance, &mut wasm_store);

    // call instantiate
    let resp = match host.call_instantiate(msg) {
        Ok(resp) => resp,
        Err(err) => {
            let store = wasm_store.into_data().disassemble();
            return (Err(err), store);
        },
    };

    debug_assert!(resp.msgs.is_empty(), "UNIMPLEMENTED: submessage is not supported yet");

    // save account info
    let mut store = wasm_store.into_data().disassemble();
    let account = Account {
        code_hash,
        admin,
    };
    if let Err(err) = ACCOUNTS.save(&mut store, &address, &account) {
        return (Err(err), store);
    }

    let report = InstantiateReport {
        address,
        code_hash: account.code_hash,
        admin:     account.admin,
    };

    (Ok(report), store)
}

fn execute<S: Storage + 'static>(
    store:     S,
    contract:  &Addr,
    msg:       Binary,
    funds:     Vec<Coin>,
) -> (anyhow::Result<()>, S) {
    debug_assert!(funds.is_empty(), "UNIMPLEMENTED: sending funds is not supported yet");

    // load contract info
    let account = match ACCOUNTS.load(&store, contract) {
        Ok(account) => account,
        Err(err) => return (Err(err), store),
    };

    // load wasm code
    let wasm_byte_code = match CODES.load(&store, &account.code_hash) {
        Ok(wasm_byte_code) => wasm_byte_code,
        Err(err) => return (Err(err), store),
    };

    // create wasm host
    let (instance, mut wasm_store) = must_build_wasm_instance(store, contract, wasm_byte_code);
    let mut host = Host::new(&instance, &mut wasm_store);

    // call execute
    let resp = match host.call_execute(msg) {
        Ok(resp) => resp,
        Err(err) => {
            let store = wasm_store.into_data().disassemble();
            return (Err(err), store);
        },
    };

    debug_assert!(resp.msgs.is_empty(), "UNIMPLEMENTED: submessage is not supported yet");

    (Ok(()), wasm_store.into_data().disassemble())
}

fn query<S: Storage + 'static>(store: S, req: Query) -> (anyhow::Result<QueryResponse>, S) {
    match req {
        Query::Info {} => (query_info(&store).map(QueryResponse::Info), store),
        Query::Account {
            address,
        } => (query_account(&store, address).map(QueryResponse::Account), store),
        Query::Accounts {
            start_after,
            limit,
        } => (query_accounts(&store, start_after, limit).map(QueryResponse::Accounts), store),
        Query::WasmRaw {
            contract,
            key,
        } => (query_wasm_raw(&store, contract, key).map(QueryResponse::WasmRaw), store),
        Query::WasmSmart {
            contract,
            msg
        } => {
            let (resp, store) = query_wasm_smart(store, contract, msg);
            (resp.map(QueryResponse::WasmSmart), store)
        },
    }
}

fn query_info(store: &dyn Storage) -> anyhow::Result<InfoResponse> {
    Ok(InfoResponse {
        chain_id: CHAIN_ID.load(store)?,
        last_finalized_block: LAST_FINALIZED_BLOCK.load(store)?,
    })
}

fn query_account(store: &dyn Storage, address: Addr) -> anyhow::Result<AccountResponse> {
    let account = ACCOUNTS.load(store, &address)?;
    Ok(AccountResponse {
        address,
        code_hash: account.code_hash,
        admin:     account.admin,
    })
}

fn query_accounts(
    store:       &dyn Storage,
    start_after: Option<Addr>,
    limit:       Option<u32>,
) -> anyhow::Result<Vec<AccountResponse>> {
    let start = start_after.as_ref().map(Bound::exclusive);
    let limit = limit.unwrap_or(DEFAULT_PAGE_LIMIT);

    ACCOUNTS
        .range(store, start, None, Order::Ascending)
        .take(limit as usize)
        .map(|item| {
            let (address, account) = item?;
            Ok(AccountResponse {
                address,
                code_hash: account.code_hash,
                admin:     account.admin,
            })
        })
        .collect()
}

fn query_wasm_raw(
    _store:    &dyn Storage,
    _contract: Addr,
    _key:      Binary,
) -> anyhow::Result<WasmRawResponse> {
    todo!()
}

fn query_wasm_smart<S: Storage + 'static>(
    store:    S,
    contract: Addr,
    msg:      Binary,
) -> (anyhow::Result<WasmSmartResponse>, S) {
    // load contract info
    let account = match ACCOUNTS.load(&store, &contract) {
        Ok(account) => account,
        Err(err) => return (Err(err), store),
    };

    // load wasm code
    let wasm_byte_code = match CODES.load(&store, &account.code_hash) {
        Ok(wasm_byte_code) => wasm_byte_code,
        Err(err) => return (Err(err), store),
    };

    // create wasm host
    let (instance, mut wasm_store) = must_build_wasm_instance(store, &contract, wasm_byte_code);
    let mut host = Host::new(&instance, &mut wasm_store);

    // call query
    let data = match host.call_query(msg) {
        Ok(data) => data,
        Err(err) => {
            let store = wasm_store.into_data().disassemble();
            return (Err(err), store);
        },
    };

    let query_res = WasmSmartResponse {
        contract,
        data,
    };

    (Ok(query_res), wasm_store.into_data().disassemble())
}

fn must_build_wasm_instance<S: Storage + 'static>(
    store: S,
    addr:  &Addr,
    wasm:  impl AsRef<[u8]>,
) -> (Instance, Store<PrefixStore<S>>) {
    build_wasm_instance(store, addr, wasm)
        .unwrap_or_else(|err| panic!("Fatal error! Failed to build wasm instance: {err}"))
}

fn build_wasm_instance<S: Storage + 'static>(
    store: S,
    addr:  &Addr,
    wasm:  impl AsRef<[u8]>,
) -> anyhow::Result<(Instance, Store<PrefixStore<S>>)> {
    InstanceBuilder::default()
        .with_wasm_bytes(wasm)?
        .with_storage(PrefixStore::new(store, &[CONTRACT_NAMESPACE, addr.as_ref()]))
        .with_host_function("db_read", db_read)?
        .with_host_function("db_write", db_write)?
        .with_host_function("db_remove", db_remove)?
        .with_host_function("db_scan", db_scan)?
        .with_host_function("db_next", db_next)?
        .with_host_function("debug", debug)?
        .finalize()
}
