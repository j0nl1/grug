use {
    crate::{authenticate_tx, process_msg, process_query, AppError, AppResult},
    cw_db::{Batch, CacheStore, Flush, SharedStore},
    cw_std::{
        from_json, to_json, Account, Addr, Binary, BlockInfo, Config, GenesisState, Hash, Item,
        Map, QueryRequest, Storage, Tx,
    },
    std::sync::{Arc, RwLock},
    tracing::{debug, info},
};

pub const CHAIN_ID:             Item<String>        = Item::new("chain_id");
pub const CONFIG:               Item<Config>        = Item::new("config");
pub const LAST_FINALIZED_BLOCK: Item<BlockInfo>     = Item::new("last_finalized_block");
pub const CODES:                Map<&Hash, Binary>  = Map::new("c");
pub const ACCOUNTS:             Map<&Addr, Account> = Map::new("a");
pub const CONTRACT_NAMESPACE:   &[u8]               = b"w";

struct PendingData {
    batch: Batch,
    block: BlockInfo,
}

#[derive(Clone)]
pub struct App<S> {
    store:   SharedStore<S>,
    pending: Arc<RwLock<Option<PendingData>>>,
}

impl<S> App<S> {
    pub fn new(store: S) -> Self {
        Self {
            store:   SharedStore::new(store),
            pending: Arc::new(RwLock::new(None)),
        }
    }

    // TODO: cleanup these speghatti code

    fn take_pending(&self) -> AppResult<(Batch, BlockInfo)> {
        // TODO: handle poison error
        self.pending
            .write()
            .unwrap()
            .take()
            .map(|data| (data.batch, data.block))
            .ok_or(AppError::PendingDataNotSet)
    }

    fn put_pending(&self, batch: Batch, block: BlockInfo) -> AppResult<()> {
        // TODO: handle poison error
        if self
            .pending
            .write()
            .unwrap()
            .replace(PendingData {
                batch,
                block,
            })
            .is_none()
        {
            Ok(())
        } else {
            Err(AppError::PendingDataExists)
        }
    }
}

impl<S> App<S>
where
    S: Storage + 'static,
{
    pub fn do_init_chain(
        &self,
        chain_id: String,
        block: BlockInfo,
        app_state_bytes: &[u8],
    ) -> AppResult<Hash> {
        let mut store = self.store.share();

        // deserialize the genesis state
        let genesis_state: GenesisState = from_json(app_state_bytes)?;

        // save the config and genesis block. some genesis messages may need it
        CHAIN_ID.save(&mut store, &chain_id)?;
        CONFIG.save(&mut store, &genesis_state.config)?;
        LAST_FINALIZED_BLOCK.save(&mut store, &block)?;

        // not sure which address to use as genesis message sender. currently we
        // just use an all-zero address.
        // probably should make the sender Option in the contexts. None if it's
        // in genesis.
        let sender = Addr::mock(0);

        // loop through genesis messages and execute each one.
        // it's expected that genesis messages should all successfully execute.
        // if anyone fails, it's fatal error and we abort the genesis.
        // the developer should examine the error, fix it, and retry.
        for (idx, msg) in genesis_state.msgs.into_iter().enumerate() {
            debug!(idx, "processing genesis message");
            process_msg(self.store.share(), &block, &sender, msg)?;
        }

        info!(chain_id, "completed genesis");

        // return an empty apphash as placeholder, since we haven't implemented
        // state merklization yet
        Ok(Hash::zero())
    }

    // TODO: return events, txResults, appHash
    pub fn do_finalize_block(
        &self,
        block:   BlockInfo,
        raw_txs: Vec<impl AsRef<[u8]>>,
    ) -> AppResult<()> {
        let cached = SharedStore::new(CacheStore::new(self.store.share(), None));

        for (idx, raw_tx) in raw_txs.into_iter().enumerate() {
            // TODO: add txhash to the debug print
            debug!(idx, "processing tx");
            run_tx(cached.share(), &block, from_json(raw_tx)?)?;
        }

        let (_, batch) = cached.disassemble()?.disassemble();

        self.put_pending(batch, block.clone())?;

        info!(height = block.height, timestamp = block.timestamp, "finalized block");

        Ok(())
    }

    // returns (last_block_height, last_block_app_hash)
    pub fn do_info(&self) -> AppResult<(i64, Hash)> {
        let block = LAST_FINALIZED_BLOCK.load(&self.store)?;
        // return an all-zero hash as a placeholder, since we haven't implemented
        // state merklization yet
        Ok((block.height as i64, Hash::zero()))
    }

    pub fn do_query(&self, raw_query: &[u8]) -> AppResult<Binary> {
        // note: when doing query, we use the state from the last finalized block,
        // do not include uncommitted changes from the current block.
        let block = LAST_FINALIZED_BLOCK.load(&self.store)?;

        let req: QueryRequest = from_json(raw_query)?;
        let res = process_query(self.store.share(), &block, req)?;

        to_json(&res).map_err(Into::into)
    }
}

impl<S> App<S>
where
    S: Storage + Flush + 'static,
{
    // TODO: we need to think about what to do if the flush fails here...
    pub fn do_commit(&self) -> AppResult<()> {
        let mut store = self.store.share();
        let (batch, block) = self.take_pending()?;

        // apply the DB ops effected by txs in this block
        store.flush(batch)?;

        // update the last finalized block info
        LAST_FINALIZED_BLOCK.save(&mut store, &block)?;

        info!(height = block.height, "committed state deltas");

        Ok(())
    }
}

fn run_tx<S>(store: S, block: &BlockInfo, tx: Tx) -> AppResult<()>
where
    S: Storage + Flush + 'static,
{
    // create cached store for this tx
    let cached = SharedStore::new(CacheStore::new(store, None));

    // first, authenticate tx by calling the sender account's before_tx method
    if authenticate_tx(cached.share(), block, &tx).is_err() {
        // if authentication fails, abort, discard uncommitted changes
        return Ok(());
    }

    // update the account state. as long as authentication succeeds, regardless
    // of whether the message are successful, we update account state. if auth
    // fails, we don't update account state.
    cached.write_access().commit()?;

    // now that the tx is authenticated, we loop through the messages and
    // execute them one by one
    for (idx, msg) in tx.msgs.into_iter().enumerate() {
        debug!(idx, "processing msg");
        if process_msg(cached.share(), block, &tx.sender, msg).is_err() {
            // if any one of the msgs fails, the entire tx fails.
            // abort, discard uncommitted changes (the changes from the before_tx
            // call earlier are persisted)
            return Ok(());
        }
    }

    // all messages succeeded. commit the state changes
    cached.write_access().commit()?;

    Ok(())
}
