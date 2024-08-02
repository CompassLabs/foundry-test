//! In-memory blockchain backend.

use self::state::trie_storage;
use crate::{
    config::PruneStateHistoryConfig,
    eth::{
        backend::{
            cheats::CheatsManager,
            db::{Db, MaybeFullDatabase, SerializableState},
            executor::{ExecutedTransactions, TransactionExecutor},
            fork::ClientFork,
            genesis::GenesisConfig,
            mem::{
                state::{storage_root, trie_accounts},
                storage::MinedTransactionReceipt,
            },
            notifications::{NewBlockNotification, NewBlockNotifications},
            time::{utc_from_secs, TimeManager},
            validate::TransactionValidator,
        },
        error::{BlockchainError, ErrDetail, InvalidTransactionError},
        fees::{FeeDetails, FeeManager, MIN_SUGGESTED_PRIORITY_FEE},
        macros::node_info,
        pool::transactions::PoolTransaction,
        util::get_precompiles_for,
    },
    inject_precompiles,
    mem::{
        inspector::Inspector,
        storage::{BlockchainStorage, InMemoryBlockStates, MinedBlockOutcome},
    },
    revm::{db::DatabaseRef, primitives::AccountInfo},
    NodeConfig, PrecompileFactory,
};
use alloy_consensus::{Account, Header, Receipt, ReceiptWithBloom};
use alloy_eips::eip4844::MAX_BLOBS_PER_BLOCK;
use alloy_primitives::{keccak256, Address, Bytes, TxHash, TxKind, B256, U256, U64};
use alloy_rpc_types::{
    anvil::Forking,
    request::TransactionRequest,
    serde_helpers::JsonStorageKey,
    state::StateOverride,
    trace::{
        filter::TraceFilter,
        geth::{
            GethDebugBuiltInTracerType, GethDebugTracerType, GethDebugTracingCallOptions,
            GethDebugTracingOptions, GethTrace, NoopFrame,
        },
        parity::LocalizedTransactionTrace,
    },
    AccessList, Block as AlloyBlock, BlockId, BlockNumberOrTag as BlockNumber,
    EIP1186AccountProofResponse as AccountProof, EIP1186StorageProof as StorageProof, Filter,
    FilteredParams, Header as AlloyHeader, Index, Log, Transaction, TransactionReceipt,
};
use alloy_serde::WithOtherFields;
use alloy_trie::{proof::ProofRetainer, HashBuilder, Nibbles};
use anvil_core::eth::{
    block::{Block, BlockInfo},
    transaction::{
        DepositReceipt, MaybeImpersonatedTransaction, PendingTransaction, ReceiptResponse,
        TransactionInfo, TypedReceipt, TypedTransaction,
    },
    utils::meets_eip155,
};
use anvil_rpc::error::RpcError;

use alloy_chains::NamedChain;
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use foundry_evm::{
    backend::{DatabaseError, DatabaseResult, RevertSnapshotAction},
    constants::DEFAULT_CREATE2_DEPLOYER_RUNTIME_CODE,
    decode::RevertDecoder,
    inspectors::AccessListInspector,
    revm::{
        db::CacheDB,
        interpreter::InstructionResult,
        primitives::{
            BlockEnv, CfgEnvWithHandlerCfg, EnvWithHandlerCfg, ExecutionResult, Output, SpecId,
            TxEnv, KECCAK_EMPTY,
        },
    },
    traces::TracingInspectorConfig,
    utils::new_evm_with_inspector_ref,
    InspectorExt,
};
use futures::channel::mpsc::{unbounded, UnboundedSender};
use parking_lot::{Mutex, RwLock};
use revm::{
    db::WrapDatabaseRef,
    primitives::{
        calc_blob_gasprice, BlobExcessGasAndPrice, HashMap, OptimismFields, ResultAndState,
    },
};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    sync::Arc,
    time::Duration,
};
use storage::{Blockchain, MinedTransaction, DEFAULT_HISTORY_LIMIT};
use tokio::sync::RwLock as AsyncRwLock;

pub mod cache;
pub mod fork_db;
pub mod in_memory_db;
pub mod inspector;
pub mod state;
pub mod storage;

// Gas per transaction not creating a contract.
pub const MIN_TRANSACTION_GAS: u128 = 21000;
// Gas per transaction creating a contract.
pub const MIN_CREATE_GAS: u128 = 53000;

pub type State = foundry_evm::utils::StateChangeset;

/// A block request, which includes the Pool Transactions if it's Pending
#[derive(Debug)]
pub enum BlockRequest {
    Pending(Vec<Arc<PoolTransaction>>),
    Number(u64),
}

impl BlockRequest {
    pub fn block_number(&self) -> BlockNumber {
        match *self {
            Self::Pending(_) => BlockNumber::Pending,
            Self::Number(n) => BlockNumber::Number(n),
        }
    }
}

/// Gives access to the [revm::Database]
#[derive(Clone)]
pub struct Backend {
    /// Access to [`revm::Database`] abstraction.
    ///
    /// This will be used in combination with [`revm::Evm`] and is responsible for feeding data to
    /// the evm during its execution.
    ///
    /// At time of writing, there are two different types of `Db`:
    ///   - [`MemDb`](crate::mem::in_memory_db::MemDb): everything is stored in memory
    ///   - [`ForkDb`](crate::mem::fork_db::ForkedDatabase): forks off a remote client, missing
    ///     data is retrieved via RPC-calls
    ///
    /// In order to commit changes to the [`revm::Database`], the [`revm::Evm`] requires mutable
    /// access, which requires a write-lock from this `db`. In forking mode, the time during
    /// which the write-lock is active depends on whether the `ForkDb` can provide all requested
    /// data from memory or whether it has to retrieve it via RPC calls first. This means that it
    /// potentially blocks for some time, even taking into account the rate limits of RPC
    /// endpoints. Therefor the `Db` is guarded by a `tokio::sync::RwLock` here so calls that
    /// need to read from it, while it's currently written to, don't block. E.g. a new block is
    /// currently mined and a new [`Self::set_storage_at()`] request is being executed.
    db: Arc<AsyncRwLock<Box<dyn Db>>>,
    /// stores all block related data in memory
    blockchain: Blockchain,
    /// Historic states of previous blocks
    states: Arc<RwLock<InMemoryBlockStates>>,
    /// env data of the chain
    env: Arc<RwLock<EnvWithHandlerCfg>>,
    /// this is set if this is currently forked off another client
    fork: Arc<RwLock<Option<ClientFork>>>,
    /// provides time related info, like timestamp
    time: TimeManager,
    /// Contains state of custom overrides
    cheats: CheatsManager,
    /// contains fee data
    fees: FeeManager,
    /// initialised genesis
    genesis: GenesisConfig,
    /// listeners for new blocks that get notified when a new block was imported
    new_block_listeners: Arc<Mutex<Vec<UnboundedSender<NewBlockNotification>>>>,
    /// keeps track of active snapshots at a specific block
    active_snapshots: Arc<Mutex<HashMap<U256, (u64, B256)>>>,
    enable_steps_tracing: bool,
    print_logs: bool,
    /// How to keep history state
    prune_state_history_config: PruneStateHistoryConfig,
    /// max number of blocks with transactions in memory
    transaction_block_keeper: Option<usize>,
    node_config: Arc<AsyncRwLock<NodeConfig>>,
    /// Slots in an epoch
    slots_in_an_epoch: u64,
    /// Precompiles to inject to the EVM.
    precompile_factory: Option<Arc<dyn PrecompileFactory>>,
}

impl Backend {
    /// Initialises the balance of the given accounts
    #[allow(clippy::too_many_arguments)]
    pub async fn with_genesis(
        db: Arc<AsyncRwLock<Box<dyn Db>>>,
        env: Arc<RwLock<EnvWithHandlerCfg>>,
        genesis: GenesisConfig,
        fees: FeeManager,
        fork: Arc<RwLock<Option<ClientFork>>>,
        enable_steps_tracing: bool,
        print_logs: bool,
        prune_state_history_config: PruneStateHistoryConfig,
        max_persisted_states: Option<usize>,
        transaction_block_keeper: Option<usize>,
        automine_block_time: Option<Duration>,
        node_config: Arc<AsyncRwLock<NodeConfig>>,
    ) -> Self {
        // if this is a fork then adjust the blockchain storage
        let blockchain = if let Some(fork) = fork.read().as_ref() {
            trace!(target: "backend", "using forked blockchain at {}", fork.block_number());
            Blockchain::forked(fork.block_number(), fork.block_hash(), fork.total_difficulty())
        } else {
            Blockchain::new(
                &env.read(),
                fees.is_eip1559().then(|| fees.base_fee()),
                genesis.timestamp,
            )
        };

        let start_timestamp = if let Some(fork) = fork.read().as_ref() {
            fork.timestamp()
        } else {
            genesis.timestamp
        };

        let states = if prune_state_history_config.is_config_enabled() {
            // if prune state history is enabled, configure the state cache only for memory
            prune_state_history_config
                .max_memory_history
                .map(|limit| InMemoryBlockStates::new(limit, 0))
                .unwrap_or_default()
                .memory_only()
        } else if max_persisted_states.is_some() {
            max_persisted_states
                .map(|limit| InMemoryBlockStates::new(DEFAULT_HISTORY_LIMIT, limit))
                .unwrap_or_default()
        } else {
            Default::default()
        };

        let (slots_in_an_epoch, precompile_factory) = {
            let cfg = node_config.read().await;
            (cfg.slots_in_an_epoch, cfg.precompile_factory.clone())
        };

        let backend = Self {
            db,
            blockchain,
            states: Arc::new(RwLock::new(states)),
            env,
            fork,
            time: TimeManager::new(start_timestamp),
            cheats: Default::default(),
            new_block_listeners: Default::default(),
            fees,
            genesis,
            active_snapshots: Arc::new(Mutex::new(Default::default())),
            enable_steps_tracing,
            print_logs,
            prune_state_history_config,
            transaction_block_keeper,
            node_config,
            slots_in_an_epoch,
            precompile_factory,
        };

        if let Some(interval_block_time) = automine_block_time {
            backend.update_interval_mine_block_time(interval_block_time);
        }

        // Note: this can only fail in forking mode, in which case we can't recover
        backend.apply_genesis().await.expect("Failed to create genesis");
        backend
    }

    /// Writes the CREATE2 deployer code directly to the database at the address provided.
    pub async fn set_create2_deployer(&self, address: Address) -> DatabaseResult<()> {
        self.set_code(address, Bytes::from_static(DEFAULT_CREATE2_DEPLOYER_RUNTIME_CODE)).await?;

        Ok(())
    }

    /// Updates memory limits that should be more strict when auto-mine is enabled
    pub(crate) fn update_interval_mine_block_time(&self, block_time: Duration) {
        self.states.write().update_interval_mine_block_time(block_time)
    }

    /// Applies the configured genesis settings
    ///
    /// This will fund, create the genesis accounts
    async fn apply_genesis(&self) -> Result<(), DatabaseError> {
        trace!(target: "backend", "setting genesis balances");

        if self.fork.read().is_some() {
            // fetch all account first
            let mut genesis_accounts_futures = Vec::with_capacity(self.genesis.accounts.len());
            for address in self.genesis.accounts.iter().copied() {
                let db = Arc::clone(&self.db);

                // The forking Database backend can handle concurrent requests, we can fetch all dev
                // accounts concurrently by spawning the job to a new task
                genesis_accounts_futures.push(tokio::task::spawn(async move {
                    let db = db.read().await;
                    let info = db.basic_ref(address)?.unwrap_or_default();
                    Ok::<_, DatabaseError>((address, info))
                }));
            }

            let genesis_accounts = futures::future::join_all(genesis_accounts_futures).await;

            let mut db = self.db.write().await;

            for res in genesis_accounts {
                let (address, mut info) = res.unwrap()?;
                info.balance = self.genesis.balance;
                db.insert_account(address, info.clone());
            }
        } else {
            let mut db = self.db.write().await;
            for (account, info) in self.genesis.account_infos() {
                db.insert_account(account, info);
            }

            // insert the new genesis hash to the database so it's available for the next block in
            // the evm
            db.insert_block_hash(U256::from(self.best_number()), self.best_hash());
        }

        let db = self.db.write().await;
        // apply the genesis.json alloc
        self.genesis.apply_genesis_json_alloc(db)?;
        Ok(())
    }

    /// Sets the account to impersonate
    ///
    /// Returns `true` if the account is already impersonated
    pub fn impersonate(&self, addr: Address) -> bool {
        if self.cheats.impersonated_accounts().contains(&addr) {
            return true
        }
        // Ensure EIP-3607 is disabled
        let mut env = self.env.write();
        env.cfg.disable_eip3607 = true;
        self.cheats.impersonate(addr)
    }

    /// Removes the account that from the impersonated set
    ///
    /// If the impersonated `addr` is a contract then we also reset the code here
    pub fn stop_impersonating(&self, addr: Address) {
        self.cheats.stop_impersonating(&addr);
    }

    /// If set to true will make every account impersonated
    pub fn auto_impersonate_account(&self, enabled: bool) {
        self.cheats.set_auto_impersonate_account(enabled);
    }

    /// Returns the configured fork, if any
    pub fn get_fork(&self) -> Option<ClientFork> {
        self.fork.read().clone()
    }

    /// Returns the database
    pub fn get_db(&self) -> &Arc<AsyncRwLock<Box<dyn Db>>> {
        &self.db
    }

    /// Returns the `AccountInfo` from the database
    pub async fn get_account(&self, address: Address) -> DatabaseResult<AccountInfo> {
        Ok(self.db.read().await.basic_ref(address)?.unwrap_or_default())
    }

    /// Whether we're forked off some remote client
    pub fn is_fork(&self) -> bool {
        self.fork.read().is_some()
    }

    pub fn precompiles(&self) -> Vec<Address> {
        get_precompiles_for(self.env.read().handler_cfg.spec_id)
    }

    /// Resets the fork to a fresh state
    pub async fn reset_fork(&self, forking: Forking) -> Result<(), BlockchainError> {
        if !self.is_fork() {
            if let Some(eth_rpc_url) = forking.clone().json_rpc_url {
                let mut env = self.env.read().clone();

                let (db, config) = {
                    let mut node_config = self.node_config.write().await;

                    // we want to force the correct base fee for the next block during
                    // `setup_fork_db_config`
                    node_config.base_fee.take();

                    node_config.setup_fork_db_config(eth_rpc_url, &mut env, &self.fees).await
                };

                *self.db.write().await = Box::new(db);

                let fork = ClientFork::new(config, Arc::clone(&self.db));

                *self.env.write() = env;
                *self.fork.write() = Some(fork);
            } else {
                return Err(RpcError::invalid_params(
                    "Forking not enabled and RPC URL not provided to start forking",
                )
                .into());
            }
        }

        if let Some(fork) = self.get_fork() {
            let block_number =
                forking.block_number.map(BlockNumber::from).unwrap_or(BlockNumber::Latest);
            // reset the fork entirely and reapply the genesis config
            fork.reset(forking.json_rpc_url.clone(), block_number).await?;
            let fork_block_number = fork.block_number();
            let fork_block = fork
                .block_by_number(fork_block_number)
                .await?
                .ok_or(BlockchainError::BlockNotFound)?;
            // update all settings related to the forked block
            {
                let mut env = self.env.write();
                env.cfg.chain_id = fork.chain_id();

                env.block = BlockEnv {
                    number: U256::from(fork_block_number),
                    timestamp: U256::from(fork_block.header.timestamp),
                    gas_limit: U256::from(fork_block.header.gas_limit),
                    difficulty: fork_block.header.difficulty,
                    prevrandao: Some(fork_block.header.mix_hash.unwrap_or_default()),
                    // Keep previous `coinbase` and `basefee` value
                    coinbase: env.block.coinbase,
                    basefee: env.block.basefee,
                    ..env.block.clone()
                };

                self.time.reset(env.block.timestamp.to::<u64>());

                // this is the base fee of the current block, but we need the base fee of
                // the next block
                let next_block_base_fee = self.fees.get_next_block_base_fee_per_gas(
                    fork_block.header.gas_used,
                    fork_block.header.gas_limit,
                    fork_block.header.base_fee_per_gas.unwrap_or_default(),
                );

                self.fees.set_base_fee(next_block_base_fee);

                // also reset the total difficulty
                self.blockchain.storage.write().total_difficulty = fork.total_difficulty();
            }

            // reset storage
            *self.blockchain.storage.write() = BlockchainStorage::forked(
                fork.block_number(),
                fork.block_hash(),
                fork.total_difficulty(),
            );
            self.states.write().clear();
            self.db.write().await.clear();

            self.apply_genesis().await?;

            Ok(())
        } else {
            Err(RpcError::invalid_params("Forking not enabled").into())
        }
    }

    /// Returns the `TimeManager` responsible for timestamps
    pub fn time(&self) -> &TimeManager {
        &self.time
    }

    /// Returns the `CheatsManager` responsible for executing cheatcodes
    pub fn cheats(&self) -> &CheatsManager {
        &self.cheats
    }

    /// Returns the `FeeManager` that manages fee/pricings
    pub fn fees(&self) -> &FeeManager {
        &self.fees
    }

    /// The env data of the blockchain
    pub fn env(&self) -> &Arc<RwLock<EnvWithHandlerCfg>> {
        &self.env
    }

    /// Returns the current best hash of the chain
    pub fn best_hash(&self) -> B256 {
        self.blockchain.storage.read().best_hash
    }

    /// Returns the current best number of the chain
    pub fn best_number(&self) -> u64 {
        self.blockchain.storage.read().best_number.try_into().unwrap_or(u64::MAX)
    }

    /// Sets the block number
    pub fn set_block_number(&self, number: U256) {
        let mut env = self.env.write();
        env.block.number = number;
    }

    /// Returns the client coinbase address.
    pub fn coinbase(&self) -> Address {
        self.env.read().block.coinbase
    }

    /// Returns the client coinbase address.
    pub fn chain_id(&self) -> U256 {
        U256::from(self.env.read().cfg.chain_id)
    }

    pub fn set_chain_id(&self, chain_id: u64) {
        self.env.write().cfg.chain_id = chain_id;
    }

    /// Returns balance of the given account.
    pub async fn current_balance(&self, address: Address) -> DatabaseResult<U256> {
        Ok(self.get_account(address).await?.balance)
    }

    /// Returns balance of the given account.
    pub async fn current_nonce(&self, address: Address) -> DatabaseResult<u64> {
        Ok(self.get_account(address).await?.nonce)
    }

    /// Sets the coinbase address
    pub fn set_coinbase(&self, address: Address) {
        self.env.write().block.coinbase = address;
    }

    /// Sets the nonce of the given address
    pub async fn set_nonce(&self, address: Address, nonce: U256) -> DatabaseResult<()> {
        self.db.write().await.set_nonce(address, nonce.try_into().unwrap_or(u64::MAX))
    }

    /// Sets the balance of the given address
    pub async fn set_balance(&self, address: Address, balance: U256) -> DatabaseResult<()> {
        self.db.write().await.set_balance(address, balance)
    }

    /// Sets the code of the given address
    pub async fn set_code(&self, address: Address, code: Bytes) -> DatabaseResult<()> {
        self.db.write().await.set_code(address, code.0.into())
    }

    /// Sets the value for the given slot of the given address
    pub async fn set_storage_at(
        &self,
        address: Address,
        slot: U256,
        val: B256,
    ) -> DatabaseResult<()> {
        self.db.write().await.set_storage_at(address, slot, U256::from_be_bytes(val.0))
    }

    /// Returns the configured specid
    pub fn spec_id(&self) -> SpecId {
        self.env.read().handler_cfg.spec_id
    }

    /// Returns true for post London
    pub fn is_eip1559(&self) -> bool {
        (self.spec_id() as u8) >= (SpecId::LONDON as u8)
    }

    /// Returns true for post Merge
    pub fn is_eip3675(&self) -> bool {
        (self.spec_id() as u8) >= (SpecId::MERGE as u8)
    }

    /// Returns true for post Berlin
    pub fn is_eip2930(&self) -> bool {
        (self.spec_id() as u8) >= (SpecId::BERLIN as u8)
    }

    /// Returns true for post Cancun
    pub fn is_eip4844(&self) -> bool {
        (self.spec_id() as u8) >= (SpecId::CANCUN as u8)
    }

    /// Returns true for post Prague
    pub fn is_eip7702(&self) -> bool {
        (self.spec_id() as u8) >= (SpecId::PRAGUE as u8)
    }

    /// Returns true if op-stack deposits are active
    pub fn is_optimism(&self) -> bool {
        self.env.read().handler_cfg.is_optimism
    }

    /// Returns an error if EIP1559 is not active (pre Berlin)
    pub fn ensure_eip1559_active(&self) -> Result<(), BlockchainError> {
        if self.is_eip1559() {
            return Ok(());
        }
        Err(BlockchainError::EIP1559TransactionUnsupportedAtHardfork)
    }

    /// Returns an error if EIP1559 is not active (pre muirGlacier)
    pub fn ensure_eip2930_active(&self) -> Result<(), BlockchainError> {
        if self.is_eip2930() {
            return Ok(());
        }
        Err(BlockchainError::EIP2930TransactionUnsupportedAtHardfork)
    }

    pub fn ensure_eip4844_active(&self) -> Result<(), BlockchainError> {
        if self.is_eip4844() {
            return Ok(());
        }
        Err(BlockchainError::EIP4844TransactionUnsupportedAtHardfork)
    }

    pub fn ensure_eip7702_active(&self) -> Result<(), BlockchainError> {
        if self.is_eip7702() {
            return Ok(());
        }
        Err(BlockchainError::EIP7702TransactionUnsupportedAtHardfork)
    }

    /// Returns an error if op-stack deposits are not active
    pub fn ensure_op_deposits_active(&self) -> Result<(), BlockchainError> {
        if self.is_optimism() {
            return Ok(())
        }
        Err(BlockchainError::DepositTransactionUnsupported)
    }

    /// Returns the block gas limit
    pub fn gas_limit(&self) -> u128 {
        self.env.read().block.gas_limit.to()
    }

    /// Sets the block gas limit
    pub fn set_gas_limit(&self, gas_limit: u128) {
        self.env.write().block.gas_limit = U256::from(gas_limit);
    }

    /// Returns the current base fee
    pub fn base_fee(&self) -> u128 {
        self.fees.base_fee()
    }

    pub fn excess_blob_gas_and_price(&self) -> Option<BlobExcessGasAndPrice> {
        self.fees.excess_blob_gas_and_price()
    }

    /// Sets the current basefee
    pub fn set_base_fee(&self, basefee: u128) {
        self.fees.set_base_fee(basefee)
    }

    /// Sets the gas price
    pub fn set_gas_price(&self, price: u128) {
        self.fees.set_gas_price(price)
    }

    pub fn elasticity(&self) -> f64 {
        self.fees.elasticity()
    }

    /// Returns the total difficulty of the chain until this block
    ///
    /// Note: this will always be `0` in memory mode
    /// In forking mode this will always be the total difficulty of the forked block
    pub fn total_difficulty(&self) -> U256 {
        self.blockchain.storage.read().total_difficulty
    }

    /// Creates a new `evm_snapshot` at the current height
    ///
    /// Returns the id of the snapshot created
    pub async fn create_snapshot(&self) -> U256 {
        let num = self.best_number();
        let hash = self.best_hash();
        let id = self.db.write().await.snapshot();
        trace!(target: "backend", "creating snapshot {} at {}", id, num);
        self.active_snapshots.lock().insert(id, (num, hash));
        id
    }

    /// Reverts the state to the snapshot identified by the given `id`.
    pub async fn revert_snapshot(&self, id: U256) -> Result<bool, BlockchainError> {
        let block = { self.active_snapshots.lock().remove(&id) };
        if let Some((num, hash)) = block {
            let best_block_hash = {
                // revert the storage that's newer than the snapshot
                let current_height = self.best_number();
                let mut storage = self.blockchain.storage.write();

                for n in ((num + 1)..=current_height).rev() {
                    trace!(target: "backend", "reverting block {}", n);
                    let n = U64::from(n);
                    if let Some(hash) = storage.hashes.remove(&n) {
                        if let Some(block) = storage.blocks.remove(&hash) {
                            for tx in block.transactions {
                                let _ = storage.transactions.remove(&tx.hash());
                            }
                        }
                    }
                }

                storage.best_number = U64::from(num);
                storage.best_hash = hash;
                hash
            };
            let block =
                self.block_by_hash(best_block_hash).await?.ok_or(BlockchainError::BlockNotFound)?;

            let reset_time = block.header.timestamp;
            self.time.reset(reset_time);

            let mut env = self.env.write();
            env.block = BlockEnv {
                number: U256::from(num),
                timestamp: U256::from(block.header.timestamp),
                difficulty: block.header.difficulty,
                // ensures prevrandao is set
                prevrandao: Some(block.header.mix_hash.unwrap_or_default()),
                gas_limit: U256::from(block.header.gas_limit),
                // Keep previous `coinbase` and `basefee` value
                coinbase: env.block.coinbase,
                basefee: env.block.basefee,
                ..Default::default()
            };
        }
        Ok(self.db.write().await.revert(id, RevertSnapshotAction::RevertRemove))
    }

    pub fn list_snapshots(&self) -> BTreeMap<U256, (u64, B256)> {
        self.active_snapshots.lock().clone().into_iter().collect()
    }

    /// Get the current state.
    pub async fn serialized_state(&self) -> Result<SerializableState, BlockchainError> {
        let at = self.env.read().block.clone();
        let best_number = self.blockchain.storage.read().best_number;
        let blocks = self.blockchain.storage.read().serialized_blocks();
        let transactions = self.blockchain.storage.read().serialized_transactions();
        let state = self.db.read().await.dump_state(at, best_number, blocks, transactions)?;
        state.ok_or_else(|| {
            RpcError::invalid_params("Dumping state not supported with the current configuration")
                .into()
        })
    }

    /// Write all chain data to serialized bytes buffer
    pub async fn dump_state(&self) -> Result<Bytes, BlockchainError> {
        let state = self.serialized_state().await?;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(&serde_json::to_vec(&state).unwrap_or_default())
            .map_err(|_| BlockchainError::DataUnavailable)?;
        Ok(encoder.finish().unwrap_or_default().into())
    }

    /// Apply [SerializableState] data to the backend storage.
    pub async fn load_state(&self, state: SerializableState) -> Result<bool, BlockchainError> {
        // reset the block env
        if let Some(block) = state.block.clone() {
            self.env.write().block = block.clone();

            // Set the current best block number.
            // Defaults to block number for compatibility with existing state files.
            self.blockchain.storage.write().best_number =
                state.best_block_number.unwrap_or(block.number.to::<U64>());
        }

        if !self.db.write().await.load_state(state.clone())? {
            return Err(RpcError::invalid_params(
                "Loading state not supported with the current configuration",
            )
            .into());
        }

        self.blockchain.storage.write().load_blocks(state.blocks.clone());
        self.blockchain.storage.write().load_transactions(state.transactions.clone());

        Ok(true)
    }

    /// Deserialize and add all chain data to the backend storage
    pub async fn load_state_bytes(&self, buf: Bytes) -> Result<bool, BlockchainError> {
        let orig_buf = &buf.0[..];
        let mut decoder = GzDecoder::new(orig_buf);
        let mut decoded_data = Vec::new();

        let state: SerializableState = serde_json::from_slice(if decoder.header().is_some() {
            decoder
                .read_to_end(decoded_data.as_mut())
                .map_err(|_| BlockchainError::FailedToDecodeStateDump)?;
            &decoded_data
        } else {
            &buf.0
        })
        .map_err(|_| BlockchainError::FailedToDecodeStateDump)?;

        self.load_state(state).await
    }

    /// Returns the environment for the next block
    fn next_env(&self) -> EnvWithHandlerCfg {
        let mut env = self.env.read().clone();
        // increase block number for this block
        env.block.number = env.block.number.saturating_add(U256::from(1));
        env.block.basefee = U256::from(self.base_fee());
        env.block.timestamp = U256::from(self.time.current_call_timestamp());
        env
    }

    /// Creates an EVM instance with optionally injected precompiles.
    fn new_evm_with_inspector_ref<DB, I>(
        &self,
        db: DB,
        env: EnvWithHandlerCfg,
        inspector: I,
    ) -> revm::Evm<'_, I, WrapDatabaseRef<DB>>
    where
        DB: revm::DatabaseRef,
        I: InspectorExt<WrapDatabaseRef<DB>>,
    {
        let mut evm = new_evm_with_inspector_ref(db, env, inspector);
        if let Some(factory) = &self.precompile_factory {
            inject_precompiles(&mut evm, factory.precompiles());
        }
        evm
    }

    /// executes the transactions without writing to the underlying database
    pub async fn inspect_tx(
        &self,
        tx: Arc<PoolTransaction>,
    ) -> Result<
        (InstructionResult, Option<Output>, u64, State, Vec<revm::primitives::Log>),
        BlockchainError,
    > {
        let mut env = self.next_env();
        env.tx = tx.pending_transaction.to_revm_tx_env();

        if env.handler_cfg.is_optimism {
            env.tx.optimism.enveloped_tx =
                Some(alloy_rlp::encode(&tx.pending_transaction.transaction.transaction).into());
        }

        let db = self.db.read().await;
        let mut inspector = Inspector::default();
        let mut evm = self.new_evm_with_inspector_ref(&**db, env, &mut inspector);
        let ResultAndState { result, state } = evm.transact()?;
        let (exit_reason, gas_used, out, logs) = match result {
            ExecutionResult::Success { reason, gas_used, logs, output, .. } => {
                (reason.into(), gas_used, Some(output), Some(logs))
            }
            ExecutionResult::Revert { gas_used, output } => {
                (InstructionResult::Revert, gas_used, Some(Output::Call(output)), None)
            }
            ExecutionResult::Halt { reason, gas_used } => (reason.into(), gas_used, None, None),
        };

        drop(evm);
        inspector.print_logs();

        Ok((exit_reason, out, gas_used, state, logs.unwrap_or_default()))
    }

    /// Creates the pending block
    ///
    /// This will execute all transaction in the order they come but will not mine the block
    pub async fn pending_block(&self, pool_transactions: Vec<Arc<PoolTransaction>>) -> BlockInfo {
        self.with_pending_block(pool_transactions, |_, block| block).await
    }

    /// Creates the pending block
    ///
    /// This will execute all transaction in the order they come but will not mine the block
    pub async fn with_pending_block<F, T>(
        &self,
        pool_transactions: Vec<Arc<PoolTransaction>>,
        f: F,
    ) -> T
    where
        F: FnOnce(Box<dyn MaybeFullDatabase + '_>, BlockInfo) -> T,
    {
        let db = self.db.read().await;
        let env = self.next_env();

        let mut cache_db = CacheDB::new(&*db);

        let storage = self.blockchain.storage.read();

        let cfg_env = CfgEnvWithHandlerCfg::new(env.cfg.clone(), env.handler_cfg);
        let executor = TransactionExecutor {
            db: &mut cache_db,
            validator: self,
            pending: pool_transactions.into_iter(),
            block_env: env.block.clone(),
            cfg_env,
            parent_hash: storage.best_hash,
            gas_used: 0,
            blob_gas_used: 0,
            enable_steps_tracing: self.enable_steps_tracing,
            print_logs: self.print_logs,
            precompile_factory: self.precompile_factory.clone(),
        };

        // create a new pending block
        let executed = executor.execute();
        f(Box::new(cache_db), executed.block)
    }

    /// Mines a new block and stores it.
    ///
    /// this will execute all transaction in the order they come in and return all the markers they
    /// provide.
    pub async fn mine_block(
        &self,
        pool_transactions: Vec<Arc<PoolTransaction>>,
    ) -> MinedBlockOutcome {
        self.do_mine_block(pool_transactions).await
    }

    async fn do_mine_block(
        &self,
        pool_transactions: Vec<Arc<PoolTransaction>>,
    ) -> MinedBlockOutcome {
        trace!(target: "backend", "creating new block with {} transactions", pool_transactions.len());

        let (outcome, header, block_hash) = {
            let current_base_fee = self.base_fee();
            let current_excess_blob_gas_and_price = self.excess_blob_gas_and_price();

            let mut env = self.env.read().clone();

            if env.block.basefee.is_zero() {
                // this is an edge case because the evm fails if `tx.effective_gas_price < base_fee`
                // 0 is only possible if it's manually set
                env.cfg.disable_base_fee = true;
            }

            // increase block number for this block
            env.block.number = env.block.number.saturating_add(U256::from(1));
            env.block.basefee = U256::from(current_base_fee);
            env.block.blob_excess_gas_and_price = current_excess_blob_gas_and_price;

            // pick a random value for prevrandao
            env.block.prevrandao = Some(B256::random());

            let best_hash = self.blockchain.storage.read().best_hash;

            if self.prune_state_history_config.is_state_history_supported() {
                let db = self.db.read().await.current_state();
                // store current state before executing all transactions
                self.states.write().insert(best_hash, db);
            }

            let (executed_tx, block_hash) = {
                let mut db = self.db.write().await;

                // finally set the next block timestamp, this is done just before execution, because
                // there can be concurrent requests that can delay acquiring the db lock and we want
                // to ensure the timestamp is as close as possible to the actual execution.
                env.block.timestamp = U256::from(self.time.next_timestamp());

                let executor = TransactionExecutor {
                    db: &mut *db,
                    validator: self,
                    pending: pool_transactions.into_iter(),
                    block_env: env.block.clone(),
                    cfg_env: CfgEnvWithHandlerCfg::new(env.cfg.clone(), env.handler_cfg),
                    parent_hash: best_hash,
                    gas_used: 0,
                    blob_gas_used: 0,
                    enable_steps_tracing: self.enable_steps_tracing,
                    print_logs: self.print_logs,
                    precompile_factory: self.precompile_factory.clone(),
                };
                let executed_tx = executor.execute();

                // we also need to update the new blockhash in the db itself
                let block_hash = executed_tx.block.block.header.hash_slow();
                db.insert_block_hash(U256::from(executed_tx.block.block.header.number), block_hash);

                (executed_tx, block_hash)
            };

            // create the new block with the current timestamp
            let ExecutedTransactions { block, included, invalid } = executed_tx;
            let BlockInfo { block, transactions, receipts } = block;

            let mut storage = self.blockchain.storage.write();
            let header = block.header.clone();
            let block_number = storage.best_number.saturating_add(U64::from(1));

            trace!(
                target: "backend",
                "Mined block {} with {} tx {:?}",
                block_number,
                transactions.len(),
                transactions.iter().map(|tx| tx.transaction_hash).collect::<Vec<_>>()
            );

            // update block metadata
            storage.best_number = block_number;
            storage.best_hash = block_hash;
            // Difficulty is removed and not used after Paris (aka TheMerge). Value is replaced with
            // prevrandao. https://github.com/bluealloy/revm/blob/1839b3fce8eaeebb85025576f2519b80615aca1e/crates/interpreter/src/instructions/host_env.rs#L27
            if !self.is_eip3675() {
                storage.total_difficulty =
                    storage.total_difficulty.saturating_add(header.difficulty);
            }

            storage.blocks.insert(block_hash, block);
            storage.hashes.insert(block_number, block_hash);

            node_info!("");
            // insert all transactions
            for (info, receipt) in transactions.into_iter().zip(receipts) {
                // log some tx info
                node_info!("    Transaction: {:?}", info.transaction_hash);
                if let Some(contract) = &info.contract_address {
                    node_info!("    Contract created: {contract:?}");
                }
                node_info!("    Gas used: {}", receipt.cumulative_gas_used());
                if !info.exit.is_ok() {
                    let r = RevertDecoder::new().decode(
                        info.out.as_ref().map(|b| &b[..]).unwrap_or_default(),
                        Some(info.exit),
                    );
                    node_info!("    Error: reverted with: {r}");
                }
                node_info!("");

                let mined_tx = MinedTransaction {
                    info,
                    receipt,
                    block_hash,
                    block_number: block_number.to::<u64>(),
                };
                storage.transactions.insert(mined_tx.info.transaction_hash, mined_tx);
            }

            // remove old transactions that exceed the transaction block keeper
            if let Some(transaction_block_keeper) = self.transaction_block_keeper {
                if storage.blocks.len() > transaction_block_keeper {
                    let to_clear = block_number
                        .to::<u64>()
                        .saturating_sub(transaction_block_keeper.try_into().unwrap());
                    storage.remove_block_transactions_by_number(to_clear)
                }
            }

            // we intentionally set the difficulty to `0` for newer blocks
            env.block.difficulty = U256::from(0);

            // update env with new values
            *self.env.write() = env;

            let timestamp = utc_from_secs(header.timestamp);

            node_info!("    Block Number: {}", block_number);
            node_info!("    Block Hash: {:?}", block_hash);
            node_info!("    Block Time: {:?}\n", timestamp.to_rfc2822());

            let outcome = MinedBlockOutcome { block_number, included, invalid };

            (outcome, header, block_hash)
        };
        let next_block_base_fee = self.fees.get_next_block_base_fee_per_gas(
            header.gas_used,
            header.gas_limit,
            header.base_fee_per_gas.unwrap_or_default(),
        );
        let next_block_excess_blob_gas = self.fees.get_next_block_blob_excess_gas(
            header.excess_blob_gas.unwrap_or_default(),
            header.blob_gas_used.unwrap_or_default(),
        );

        // update next base fee
        self.fees.set_base_fee(next_block_base_fee);
        self.fees
            .set_blob_excess_gas_and_price(BlobExcessGasAndPrice::new(next_block_excess_blob_gas));

        // notify all listeners
        self.notify_on_new_block(header, block_hash);

        outcome
    }

    /// Executes the [TransactionRequest] without writing to the DB
    ///
    /// # Errors
    ///
    /// Returns an error if the `block_number` is greater than the current height
    pub async fn call(
        &self,
        request: WithOtherFields<TransactionRequest>,
        fee_details: FeeDetails,
        block_request: Option<BlockRequest>,
        overrides: Option<StateOverride>,
    ) -> Result<(InstructionResult, Option<Output>, u128, State), BlockchainError> {
        self.with_database_at(block_request, |state, block| {
            let block_number = block.number.to::<u64>();
            let (exit, out, gas, state) = match overrides {
                None => self.call_with_state(state, request, fee_details, block),
                Some(overrides) => {
                    let state = state::apply_state_override(overrides.into_iter().collect(), state)?;
                    self.call_with_state(state, request, fee_details, block)
                },
            }?;
            trace!(target: "backend", "call return {:?} out: {:?} gas {} on block {}", exit, out, gas, block_number);
            Ok((exit, out, gas, state))
        }).await?
    }

    fn build_call_env(
        &self,
        request: WithOtherFields<TransactionRequest>,
        fee_details: FeeDetails,
        block_env: BlockEnv,
    ) -> EnvWithHandlerCfg {
        let WithOtherFields::<TransactionRequest> {
            inner:
                TransactionRequest {
                    from,
                    to,
                    gas,
                    value,
                    input,
                    nonce,
                    access_list,
                    blob_versioned_hashes,
                    sidecar: _,
                    chain_id: _,
                    transaction_type: _,
                    .. // Rest of the gas fees related fields are taken from `fee_details`
                },
            ..
        } = request;

        let FeeDetails {
            gas_price,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            max_fee_per_blob_gas,
        } = fee_details;

        let gas_limit = gas.unwrap_or(block_env.gas_limit.to());
        let mut env = self.env.read().clone();
        env.block = block_env;
        // we want to disable this in eth_call, since this is common practice used by other node
        // impls and providers <https://github.com/foundry-rs/foundry/issues/4388>
        env.cfg.disable_block_gas_limit = true;

        // The basefee should be ignored for calls against state for
        // - eth_call
        // - eth_estimateGas
        // - eth_createAccessList
        // - tracing
        env.cfg.disable_base_fee = true;

        let gas_price = gas_price.or(max_fee_per_gas).unwrap_or_else(|| {
            self.fees().raw_gas_price().saturating_add(MIN_SUGGESTED_PRIORITY_FEE)
        });
        let caller = from.unwrap_or_default();
        let to = to.as_ref().and_then(TxKind::to);
        env.tx = TxEnv {
            caller,
            gas_limit: gas_limit as u64,
            gas_price: U256::from(gas_price),
            gas_priority_fee: max_priority_fee_per_gas.map(U256::from),
            max_fee_per_blob_gas: max_fee_per_blob_gas.map(U256::from),
            transact_to: match to {
                Some(addr) => TxKind::Call(*addr),
                None => TxKind::Create,
            },
            value: value.unwrap_or_default(),
            data: input.into_input().unwrap_or_default(),
            chain_id: None,
            nonce,
            access_list: access_list.unwrap_or_default().into(),
            blob_hashes: blob_versioned_hashes.unwrap_or_default(),
            optimism: OptimismFields { enveloped_tx: Some(Bytes::new()), ..Default::default() },
            authorization_list: None,
        };

        if env.block.basefee.is_zero() {
            // this is an edge case because the evm fails if `tx.effective_gas_price < base_fee`
            // 0 is only possible if it's manually set
            env.cfg.disable_base_fee = true;
        }

        env
    }

    pub fn call_with_state<D>(
        &self,
        state: D,
        request: WithOtherFields<TransactionRequest>,
        fee_details: FeeDetails,
        block_env: BlockEnv,
    ) -> Result<(InstructionResult, Option<Output>, u128, State), BlockchainError>
    where
        D: DatabaseRef<Error = DatabaseError>,
    {
        let mut inspector = Inspector::default();

        let env = self.build_call_env(request, fee_details, block_env);
        let mut evm = self.new_evm_with_inspector_ref(state, env, &mut inspector);
        let ResultAndState { result, state } = evm.transact()?;
        let (exit_reason, gas_used, out) = match result {
            ExecutionResult::Success { reason, gas_used, output, .. } => {
                (reason.into(), gas_used, Some(output))
            }
            ExecutionResult::Revert { gas_used, output } => {
                (InstructionResult::Revert, gas_used, Some(Output::Call(output)))
            }
            ExecutionResult::Halt { reason, gas_used } => (reason.into(), gas_used, None),
        };
        drop(evm);
        inspector.print_logs();
        Ok((exit_reason, out, gas_used as u128, state))
    }

    pub async fn call_with_tracing(
        &self,
        request: WithOtherFields<TransactionRequest>,
        fee_details: FeeDetails,
        block_request: Option<BlockRequest>,
        opts: GethDebugTracingCallOptions,
    ) -> Result<GethTrace, BlockchainError> {
        let GethDebugTracingCallOptions { tracing_options, block_overrides: _, state_overrides: _ } =
            opts;
        let GethDebugTracingOptions { config, tracer, tracer_config, .. } = tracing_options;

        self.with_database_at(block_request, |state, block| {
            let block_number = block.number;

            if let Some(tracer) = tracer {
                return match tracer {
                    GethDebugTracerType::BuiltInTracer(tracer) => match tracer {
                        GethDebugBuiltInTracerType::CallTracer => {
                            let call_config = tracer_config
                                .into_call_config()
                                .map_err(|e| (RpcError::invalid_params(e.to_string())))?;

                            let mut inspector = Inspector::default().with_config(
                                TracingInspectorConfig::from_geth_call_config(&call_config),
                            );

                            let env = self.build_call_env(request, fee_details, block);
                            let mut evm =
                                self.new_evm_with_inspector_ref(state, env, &mut inspector);
                            let ResultAndState { result, state: _ } = evm.transact()?;

                            drop(evm);
                            let tracing_inspector = inspector.tracer.expect("tracer disappeared");

                            Ok(tracing_inspector
                                .into_geth_builder()
                                .geth_call_traces(call_config, result.gas_used())
                                .into())
                        }
                        GethDebugBuiltInTracerType::NoopTracer => Ok(NoopFrame::default().into()),
                        GethDebugBuiltInTracerType::FourByteTracer |
                        GethDebugBuiltInTracerType::PreStateTracer |
                        GethDebugBuiltInTracerType::MuxTracer => {
                            Err(RpcError::invalid_params("unsupported tracer type").into())
                        }
                    },

                    GethDebugTracerType::JsTracer(_code) => {
                        Err(RpcError::invalid_params("unsupported tracer type").into())
                    }
                }
            }

            // defaults to StructLog tracer used since no tracer is specified
            let mut inspector =
                Inspector::default().with_config(TracingInspectorConfig::from_geth_config(&config));

            let env = self.build_call_env(request, fee_details, block);
            let mut evm = self.new_evm_with_inspector_ref(state, env, &mut inspector);
            let ResultAndState { result, state: _ } = evm.transact()?;

            let (exit_reason, gas_used, out) = match result {
                ExecutionResult::Success { reason, gas_used, output, .. } => {
                    (reason.into(), gas_used, Some(output))
                }
                ExecutionResult::Revert { gas_used, output } => {
                    (InstructionResult::Revert, gas_used, Some(Output::Call(output)))
                }
                ExecutionResult::Halt { reason, gas_used } => (reason.into(), gas_used, None),
            };

            drop(evm);
            let tracing_inspector = inspector.tracer.expect("tracer disappeared");
            let return_value = out.as_ref().map(|o| o.data().clone()).unwrap_or_default();

            trace!(target: "backend", ?exit_reason, ?out, %gas_used, %block_number, "trace call");

            let res = tracing_inspector
                .into_geth_builder()
                .geth_traces(gas_used, return_value, config)
                .into();

            Ok(res)
        })
        .await?
    }

    pub fn build_access_list_with_state<D>(
        &self,
        state: D,
        request: WithOtherFields<TransactionRequest>,
        fee_details: FeeDetails,
        block_env: BlockEnv,
    ) -> Result<(InstructionResult, Option<Output>, u64, AccessList), BlockchainError>
    where
        D: DatabaseRef<Error = DatabaseError>,
    {
        let from = request.from.unwrap_or_default();
        let to = if let Some(TxKind::Call(to)) = request.to {
            to
        } else {
            let nonce = state.basic_ref(from)?.unwrap_or_default().nonce;
            from.create(nonce)
        };

        let mut inspector = AccessListInspector::new(
            request.access_list.clone().unwrap_or_default(),
            from,
            to,
            self.precompiles(),
        );

        let env = self.build_call_env(request, fee_details, block_env);
        let mut evm = self.new_evm_with_inspector_ref(state, env, &mut inspector);
        let ResultAndState { result, state: _ } = evm.transact()?;
        let (exit_reason, gas_used, out) = match result {
            ExecutionResult::Success { reason, gas_used, output, .. } => {
                (reason.into(), gas_used, Some(output))
            }
            ExecutionResult::Revert { gas_used, output } => {
                (InstructionResult::Revert, gas_used, Some(Output::Call(output)))
            }
            ExecutionResult::Halt { reason, gas_used } => (reason.into(), gas_used, None),
        };
        drop(evm);
        let access_list = inspector.access_list();
        Ok((exit_reason, out, gas_used, access_list))
    }

    /// returns all receipts for the given transactions
    fn get_receipts(&self, tx_hashes: impl IntoIterator<Item = TxHash>) -> Vec<TypedReceipt> {
        let storage = self.blockchain.storage.read();
        let mut receipts = vec![];

        for hash in tx_hashes {
            if let Some(tx) = storage.transactions.get(&hash) {
                receipts.push(tx.receipt.clone());
            }
        }

        receipts
    }

    /// Returns the logs of the block that match the filter
    async fn logs_for_block(
        &self,
        filter: Filter,
        hash: B256,
    ) -> Result<Vec<Log>, BlockchainError> {
        if let Some(block) = self.blockchain.get_block_by_hash(&hash) {
            return Ok(self.mined_logs_for_block(filter, block));
        }

        if let Some(fork) = self.get_fork() {
            return Ok(fork.logs(&filter).await?);
        }

        Ok(Vec::new())
    }

    /// Returns all `Log`s mined by the node that were emitted in the `block` and match the `Filter`
    fn mined_logs_for_block(&self, filter: Filter, block: Block) -> Vec<Log> {
        let params = FilteredParams::new(Some(filter.clone()));
        let mut all_logs = Vec::new();
        let block_hash = block.header.hash_slow();
        let mut block_log_index = 0u32;

        let storage = self.blockchain.storage.read();

        for tx in block.transactions {
            let Some(tx) = storage.transactions.get(&tx.hash()) else {
                continue;
            };
            let logs = tx.receipt.logs();
            let transaction_hash = tx.info.transaction_hash;

            for log in logs {
                let mut is_match: bool = true;
                if !filter.address.is_empty() && filter.has_topics() {
                    if !params.filter_address(&log.address) || !params.filter_topics(log.topics()) {
                        is_match = false;
                    }
                } else if !filter.address.is_empty() {
                    if !params.filter_address(&log.address) {
                        is_match = false;
                    }
                } else if filter.has_topics() && !params.filter_topics(log.topics()) {
                    is_match = false;
                }

                if is_match {
                    let log = Log {
                        inner: log.clone(),
                        block_hash: Some(block_hash),
                        block_number: Some(block.header.number),
                        block_timestamp: Some(block.header.timestamp),
                        transaction_hash: Some(transaction_hash),
                        transaction_index: Some(tx.info.transaction_index),
                        log_index: Some(block_log_index as u64),
                        removed: false,
                    };
                    all_logs.push(log);
                }
                block_log_index += 1;
            }
        }

        all_logs
    }

    /// Returns the logs that match the filter in the given range of blocks
    async fn logs_for_range(
        &self,
        filter: &Filter,
        mut from: u64,
        to: u64,
    ) -> Result<Vec<Log>, BlockchainError> {
        let mut all_logs = Vec::new();

        // get the range that predates the fork if any
        if let Some(fork) = self.get_fork() {
            let mut to_on_fork = to;

            if !fork.predates_fork(to) {
                // adjust the ranges
                to_on_fork = fork.block_number();
            }

            if fork.predates_fork_inclusive(from) {
                // this data is only available on the forked client
                let filter = filter.clone().from_block(from).to_block(to_on_fork);
                all_logs = fork.logs(&filter).await?;

                // update the range
                from = fork.block_number() + 1;
            }
        }

        for number in from..=to {
            if let Some(block) = self.get_block(number) {
                all_logs.extend(self.mined_logs_for_block(filter.clone(), block));
            }
        }

        Ok(all_logs)
    }

    /// Returns the logs according to the filter
    pub async fn logs(&self, filter: Filter) -> Result<Vec<Log>, BlockchainError> {
        trace!(target: "backend", "get logs [{:?}]", filter);
        if let Some(hash) = filter.get_block_hash() {
            self.logs_for_block(filter, hash).await
        } else {
            let best = self.best_number();
            let to_block =
                self.convert_block_number(filter.block_option.get_to_block().copied()).min(best);
            let from_block =
                self.convert_block_number(filter.block_option.get_from_block().copied());
            if from_block > best {
                // requested log range does not exist yet
                return Ok(vec![]);
            }

            self.logs_for_range(&filter, from_block, to_block).await
        }
    }

    pub async fn block_by_hash(&self, hash: B256) -> Result<Option<AlloyBlock>, BlockchainError> {
        trace!(target: "backend", "get block by hash {:?}", hash);
        if let tx @ Some(_) = self.mined_block_by_hash(hash) {
            return Ok(tx);
        }

        if let Some(fork) = self.get_fork() {
            return Ok(fork.block_by_hash(hash).await?);
        }

        Ok(None)
    }

    pub async fn block_by_hash_full(
        &self,
        hash: B256,
    ) -> Result<Option<AlloyBlock>, BlockchainError> {
        trace!(target: "backend", "get block by hash {:?}", hash);
        if let tx @ Some(_) = self.get_full_block(hash) {
            return Ok(tx);
        }

        if let Some(fork) = self.get_fork() {
            return Ok(fork.block_by_hash_full(hash).await?)
        }

        Ok(None)
    }

    fn mined_block_by_hash(&self, hash: B256) -> Option<AlloyBlock> {
        let block = self.blockchain.get_block_by_hash(&hash)?;
        Some(self.convert_block(block))
    }

    pub(crate) async fn mined_transactions_by_block_number(
        &self,
        number: BlockNumber,
    ) -> Option<Vec<WithOtherFields<Transaction>>> {
        if let Some(block) = self.get_block(number) {
            return self.mined_transactions_in_block(&block);
        }
        None
    }

    /// Returns all transactions given a block
    pub(crate) fn mined_transactions_in_block(
        &self,
        block: &Block,
    ) -> Option<Vec<WithOtherFields<Transaction>>> {
        let mut transactions = Vec::with_capacity(block.transactions.len());
        let base_fee = block.header.base_fee_per_gas;
        let storage = self.blockchain.storage.read();
        for hash in block.transactions.iter().map(|tx| tx.hash()) {
            let info = storage.transactions.get(&hash)?.info.clone();
            let tx = block.transactions.get(info.transaction_index as usize)?.clone();

            let tx = transaction_build(Some(hash), tx, Some(block), Some(info), base_fee);
            transactions.push(tx);
        }
        Some(transactions)
    }

    pub async fn block_by_number(
        &self,
        number: BlockNumber,
    ) -> Result<Option<AlloyBlock>, BlockchainError> {
        trace!(target: "backend", "get block by number {:?}", number);
        if let tx @ Some(_) = self.mined_block_by_number(number) {
            return Ok(tx);
        }

        if let Some(fork) = self.get_fork() {
            let number = self.convert_block_number(Some(number));
            if fork.predates_fork_inclusive(number) {
                return Ok(fork.block_by_number(number).await?)
            }
        }

        Ok(None)
    }

    pub async fn block_by_number_full(
        &self,
        number: BlockNumber,
    ) -> Result<Option<AlloyBlock>, BlockchainError> {
        trace!(target: "backend", "get block by number {:?}", number);
        if let tx @ Some(_) = self.get_full_block(number) {
            return Ok(tx);
        }

        if let Some(fork) = self.get_fork() {
            let number = self.convert_block_number(Some(number));
            if fork.predates_fork_inclusive(number) {
                return Ok(fork.block_by_number_full(number).await?)
            }
        }

        Ok(None)
    }

    pub fn get_block(&self, id: impl Into<BlockId>) -> Option<Block> {
        let hash = match id.into() {
            BlockId::Hash(hash) => hash.block_hash,
            BlockId::Number(number) => {
                let storage = self.blockchain.storage.read();
                let slots_in_an_epoch = U64::from(self.slots_in_an_epoch);
                match number {
                    BlockNumber::Latest => storage.best_hash,
                    BlockNumber::Earliest => storage.genesis_hash,
                    BlockNumber::Pending => return None,
                    BlockNumber::Number(num) => *storage.hashes.get(&U64::from(num))?,
                    BlockNumber::Safe => {
                        if storage.best_number > (slots_in_an_epoch) {
                            *storage.hashes.get(&(storage.best_number - (slots_in_an_epoch)))?
                        } else {
                            storage.genesis_hash // treat the genesis block as safe "by definition"
                        }
                    }
                    BlockNumber::Finalized => {
                        if storage.best_number > (slots_in_an_epoch * U64::from(2)) {
                            *storage
                                .hashes
                                .get(&(storage.best_number - (slots_in_an_epoch * U64::from(2))))?
                        } else {
                            storage.genesis_hash
                        }
                    }
                }
            }
        };
        self.get_block_by_hash(hash)
    }

    pub fn get_block_by_hash(&self, hash: B256) -> Option<Block> {
        self.blockchain.get_block_by_hash(&hash)
    }

    pub fn mined_block_by_number(&self, number: BlockNumber) -> Option<AlloyBlock> {
        let block = self.get_block(number)?;
        let mut block = self.convert_block(block);
        block.transactions.convert_to_hashes();
        Some(block)
    }

    pub fn get_full_block(&self, id: impl Into<BlockId>) -> Option<AlloyBlock> {
        let block = self.get_block(id)?;
        let transactions = self.mined_transactions_in_block(&block)?;
        let block = self.convert_block(block);
        Some(block.into_full_block(transactions.into_iter().map(|t| t.inner).collect()))
    }

    /// Takes a block as it's stored internally and returns the eth api conform block format.
    pub fn convert_block(&self, block: Block) -> AlloyBlock {
        let size = U256::from(alloy_rlp::encode(&block).len() as u32);

        let Block { header, transactions, .. } = block;

        let hash = header.hash_slow();
        let Header {
            parent_hash,
            ommers_hash,
            beneficiary,
            state_root,
            transactions_root,
            receipts_root,
            logs_bloom,
            difficulty,
            number,
            gas_limit,
            gas_used,
            timestamp,
            requests_root,
            extra_data,
            mix_hash,
            nonce,
            base_fee_per_gas,
            withdrawals_root: _,
            blob_gas_used,
            excess_blob_gas,
            parent_beacon_block_root,
        } = header;

        let mut block = AlloyBlock {
            header: AlloyHeader {
                hash: Some(hash),
                parent_hash,
                uncles_hash: ommers_hash,
                miner: beneficiary,
                state_root,
                transactions_root,
                receipts_root,
                number: Some(number),
                gas_used,
                gas_limit,
                extra_data: extra_data.0.into(),
                logs_bloom,
                timestamp,
                total_difficulty: Some(self.total_difficulty()),
                difficulty,
                mix_hash: Some(mix_hash),
                nonce: Some(nonce),
                base_fee_per_gas,
                withdrawals_root: None,
                blob_gas_used,
                excess_blob_gas,
                parent_beacon_block_root,
                requests_root,
            },
            size: Some(size),
            transactions: alloy_rpc_types::BlockTransactions::Hashes(
                transactions.into_iter().map(|tx| tx.hash()).collect(),
            ),
            uncles: vec![],
            withdrawals: None,
            other: Default::default(),
        };

        // If Arbitrum, apply chain specifics to converted block.
        if let Ok(
            NamedChain::Arbitrum |
            NamedChain::ArbitrumGoerli |
            NamedChain::ArbitrumNova |
            NamedChain::ArbitrumTestnet,
        ) = NamedChain::try_from(self.env.read().env.cfg.chain_id)
        {
            // Block number is the best number.
            block.header.number = Some(self.best_number());
            // Set `l1BlockNumber` field.
            block.other.insert("l1BlockNumber".to_string(), number.into());
        }

        block
    }

    /// Converts the `BlockNumber` into a numeric value
    ///
    /// # Errors
    ///
    /// returns an error if the requested number is larger than the current height
    pub async fn ensure_block_number<T: Into<BlockId>>(
        &self,
        block_id: Option<T>,
    ) -> Result<u64, BlockchainError> {
        let current = self.best_number();
        let requested =
            match block_id.map(Into::into).unwrap_or(BlockId::Number(BlockNumber::Latest)) {
                BlockId::Hash(hash) => self
                    .block_by_hash(hash.block_hash)
                    .await?
                    .ok_or(BlockchainError::BlockNotFound)?
                    .header
                    .number
                    .ok_or(BlockchainError::BlockNotFound)?,
                BlockId::Number(num) => match num {
                    BlockNumber::Latest | BlockNumber::Pending => self.best_number(),
                    BlockNumber::Earliest => U64::ZERO.to::<u64>(),
                    BlockNumber::Number(num) => num,
                    BlockNumber::Safe => current.saturating_sub(self.slots_in_an_epoch),
                    BlockNumber::Finalized => current.saturating_sub(self.slots_in_an_epoch * 2),
                },
            };

        if requested > current {
            Err(BlockchainError::BlockOutOfRange(current, requested))
        } else {
            Ok(requested)
        }
    }

    pub fn convert_block_number(&self, block: Option<BlockNumber>) -> u64 {
        let current = self.best_number();
        match block.unwrap_or(BlockNumber::Latest) {
            BlockNumber::Latest | BlockNumber::Pending => current,
            BlockNumber::Earliest => 0,
            BlockNumber::Number(num) => num,
            BlockNumber::Safe => current.saturating_sub(self.slots_in_an_epoch),
            BlockNumber::Finalized => current.saturating_sub(self.slots_in_an_epoch * 2),
        }
    }

    /// Helper function to execute a closure with the database at a specific block
    pub async fn with_database_at<F, T>(
        &self,
        block_request: Option<BlockRequest>,
        f: F,
    ) -> Result<T, BlockchainError>
    where
        F: FnOnce(Box<dyn MaybeFullDatabase + '_>, BlockEnv) -> T,
    {
        let block_number = match block_request {
            Some(BlockRequest::Pending(pool_transactions)) => {
                let result = self
                    .with_pending_block(pool_transactions, |state, block| {
                        let block = block.block;
                        let block = BlockEnv {
                            number: U256::from(block.header.number),
                            coinbase: block.header.beneficiary,
                            timestamp: U256::from(block.header.timestamp),
                            difficulty: block.header.difficulty,
                            prevrandao: Some(block.header.mix_hash),
                            basefee: U256::from(block.header.base_fee_per_gas.unwrap_or_default()),
                            gas_limit: U256::from(block.header.gas_limit),
                            ..Default::default()
                        };
                        f(state, block)
                    })
                    .await;
                return Ok(result);
            }
            Some(BlockRequest::Number(bn)) => Some(BlockNumber::Number(bn)),
            None => None,
        };
        let block_number: U256 = U256::from(self.convert_block_number(block_number));

        if block_number < self.env.read().block.number {
            if let Some((block_hash, block)) = self
                .block_by_number(BlockNumber::Number(block_number.to::<u64>()))
                .await?
                .and_then(|block| Some((block.header.hash?, block)))
            {
                if let Some(state) = self.states.write().get(&block_hash) {
                    let block = BlockEnv {
                        number: block_number,
                        coinbase: block.header.miner,
                        timestamp: U256::from(block.header.timestamp),
                        difficulty: block.header.difficulty,
                        prevrandao: block.header.mix_hash,
                        basefee: U256::from(block.header.base_fee_per_gas.unwrap_or_default()),
                        gas_limit: U256::from(block.header.gas_limit),
                        ..Default::default()
                    };
                    return Ok(f(Box::new(state), block));
                }
            }

            warn!(target: "backend", "Not historic state found for block={}", block_number);
            return Err(BlockchainError::BlockOutOfRange(
                self.env.read().block.number.to::<u64>(),
                block_number.to::<u64>(),
            ));
        }

        let db = self.db.read().await;
        let block = self.env.read().block.clone();
        Ok(f(Box::new(&*db), block))
    }

    pub async fn storage_at(
        &self,
        address: Address,
        index: U256,
        block_request: Option<BlockRequest>,
    ) -> Result<B256, BlockchainError> {
        self.with_database_at(block_request, |db, _| {
            trace!(target: "backend", "get storage for {:?} at {:?}", address, index);
            let val = db.storage_ref(address, index)?;
            Ok(val.into())
        })
        .await?
    }

    /// Returns the code of the address
    ///
    /// If the code is not present and fork mode is enabled then this will try to fetch it from the
    /// forked client
    pub async fn get_code(
        &self,
        address: Address,
        block_request: Option<BlockRequest>,
    ) -> Result<Bytes, BlockchainError> {
        self.with_database_at(block_request, |db, _| self.get_code_with_state(db, address)).await?
    }

    pub fn get_code_with_state<D>(
        &self,
        state: D,
        address: Address,
    ) -> Result<Bytes, BlockchainError>
    where
        D: DatabaseRef<Error = DatabaseError>,
    {
        trace!(target: "backend", "get code for {:?}", address);
        let account = state.basic_ref(address)?.unwrap_or_default();
        if account.code_hash == KECCAK_EMPTY {
            // if the code hash is `KECCAK_EMPTY`, we check no further
            return Ok(Default::default());
        }
        let code = if let Some(code) = account.code {
            code
        } else {
            state.code_by_hash_ref(account.code_hash)?
        };
        Ok(code.bytes()[..code.len()].to_vec().into())
    }

    /// Returns the balance of the address
    ///
    /// If the requested number predates the fork then this will fetch it from the endpoint
    pub async fn get_balance(
        &self,
        address: Address,
        block_request: Option<BlockRequest>,
    ) -> Result<U256, BlockchainError> {
        self.with_database_at(block_request, |db, _| self.get_balance_with_state(db, address))
            .await?
    }

    pub async fn get_account_at_block(
        &self,
        address: Address,
        block_request: Option<BlockRequest>,
    ) -> Result<Account, BlockchainError> {
        self.with_database_at(block_request, |block_db, _| {
            let db = block_db.maybe_as_full_db().ok_or(BlockchainError::DataUnavailable)?;
            let account = db.get(&address).cloned().unwrap_or_default();
            let storage_root = storage_root(&account.storage);
            let code_hash = account.info.code_hash;
            let balance = account.info.balance;
            let nonce = account.info.nonce;
            Ok(Account { balance, nonce, code_hash, storage_root })
        })
        .await?
    }

    pub fn get_balance_with_state<D>(
        &self,
        state: D,
        address: Address,
    ) -> Result<U256, BlockchainError>
    where
        D: DatabaseRef<Error = DatabaseError>,
    {
        trace!(target: "backend", "get balance for {:?}", address);
        Ok(state.basic_ref(address)?.unwrap_or_default().balance)
    }

    /// Returns the nonce of the address
    ///
    /// If the requested number predates the fork then this will fetch it from the endpoint
    pub async fn get_nonce(
        &self,
        address: Address,
        block_request: BlockRequest,
    ) -> Result<u64, BlockchainError> {
        if let BlockRequest::Pending(pool_transactions) = &block_request {
            if let Some(value) = get_pool_transactions_nonce(pool_transactions, address) {
                return Ok(value);
            }
        }
        let final_block_request = match block_request {
            BlockRequest::Pending(_) => BlockRequest::Number(self.best_number()),
            BlockRequest::Number(bn) => BlockRequest::Number(bn),
        };

        self.with_database_at(Some(final_block_request), |db, _| {
            trace!(target: "backend", "get nonce for {:?}", address);
            Ok(db.basic_ref(address)?.unwrap_or_default().nonce)
        })
        .await?
    }

    /// Returns the traces for the given transaction
    pub async fn trace_transaction(
        &self,
        hash: B256,
    ) -> Result<Vec<LocalizedTransactionTrace>, BlockchainError> {
        if let Some(traces) = self.mined_parity_trace_transaction(hash) {
            return Ok(traces);
        }

        if let Some(fork) = self.get_fork() {
            return Ok(fork.trace_transaction(hash).await?)
        }

        Ok(vec![])
    }

    /// Returns the traces for the given transaction
    pub(crate) fn mined_parity_trace_transaction(
        &self,
        hash: B256,
    ) -> Option<Vec<LocalizedTransactionTrace>> {
        self.blockchain.storage.read().transactions.get(&hash).map(|tx| tx.parity_traces())
    }

    /// Returns the traces for the given transaction
    pub(crate) fn mined_transaction(&self, hash: B256) -> Option<MinedTransaction> {
        self.blockchain.storage.read().transactions.get(&hash).cloned()
    }

    /// Returns the traces for the given block
    pub(crate) fn mined_parity_trace_block(
        &self,
        block: u64,
    ) -> Option<Vec<LocalizedTransactionTrace>> {
        let block = self.get_block(block)?;
        let mut traces = vec![];
        let storage = self.blockchain.storage.read();
        for tx in block.transactions {
            traces.extend(storage.transactions.get(&tx.hash())?.parity_traces());
        }
        Some(traces)
    }

    /// Returns the traces for the given transaction
    pub async fn debug_trace_transaction(
        &self,
        hash: B256,
        opts: GethDebugTracingOptions,
    ) -> Result<GethTrace, BlockchainError> {
        if let Some(trace) = self.mined_geth_trace_transaction(hash, opts.clone()) {
            return trace;
        }

        if let Some(fork) = self.get_fork() {
            return Ok(fork.debug_trace_transaction(hash, opts).await?)
        }

        Ok(GethTrace::Default(Default::default()))
    }

    fn mined_geth_trace_transaction(
        &self,
        hash: B256,
        opts: GethDebugTracingOptions,
    ) -> Option<Result<GethTrace, BlockchainError>> {
        self.blockchain.storage.read().transactions.get(&hash).map(|tx| tx.geth_trace(opts))
    }

    /// Returns the traces for the given block
    pub async fn trace_block(
        &self,
        block: BlockNumber,
    ) -> Result<Vec<LocalizedTransactionTrace>, BlockchainError> {
        let number = self.convert_block_number(Some(block));
        if let Some(traces) = self.mined_parity_trace_block(number) {
            return Ok(traces);
        }

        if let Some(fork) = self.get_fork() {
            if fork.predates_fork(number) {
                return Ok(fork.trace_block(number).await?)
            }
        }

        Ok(vec![])
    }

    pub async fn transaction_receipt(
        &self,
        hash: B256,
    ) -> Result<Option<ReceiptResponse>, BlockchainError> {
        if let Some(receipt) = self.mined_transaction_receipt(hash) {
            return Ok(Some(receipt.inner));
        }

        if let Some(fork) = self.get_fork() {
            let receipt = fork.transaction_receipt(hash).await?;
            let number = self.convert_block_number(
                receipt.clone().and_then(|r| r.block_number).map(BlockNumber::from),
            );

            if fork.predates_fork_inclusive(number) {
                return Ok(receipt);
            }
        }

        Ok(None)
    }

    // Returns the traces matching a given filter
    pub async fn trace_filter(
        &self,
        filter: TraceFilter,
    ) -> Result<Vec<LocalizedTransactionTrace>, BlockchainError> {
        let matcher = filter.matcher();
        let start = filter.from_block.unwrap_or(0);
        let end = filter.to_block.unwrap_or(self.best_number());

        let dist = end.saturating_sub(start);
        if dist == 0 {
            return Err(BlockchainError::RpcError(RpcError::invalid_params(
                "invalid block range, ensure that to block is greater than from block".to_string(),
            )));
        }
        if dist > 300 {
            return Err(BlockchainError::RpcError(RpcError::invalid_params(
                "block range too large, currently limited to 300".to_string(),
            )));
        }

        // Accumulate tasks for block range
        let mut trace_tasks = vec![];
        for num in start..=end {
            trace_tasks.push(self.trace_block(num.into()));
        }

        // Execute tasks and filter traces
        let traces = futures::future::try_join_all(trace_tasks).await?;
        let filtered_traces =
            traces.into_iter().flatten().filter(|trace| matcher.matches(&trace.trace));

        // Apply after and count
        let filtered_traces: Vec<_> = if let Some(after) = filter.after {
            filtered_traces.skip(after as usize).collect()
        } else {
            filtered_traces.collect()
        };

        let filtered_traces: Vec<_> = if let Some(count) = filter.count {
            filtered_traces.into_iter().take(count as usize).collect()
        } else {
            filtered_traces
        };

        Ok(filtered_traces)
    }

    /// Returns all receipts of the block
    pub fn mined_receipts(&self, hash: B256) -> Option<Vec<TypedReceipt>> {
        let block = self.mined_block_by_hash(hash)?;
        let mut receipts = Vec::new();
        let storage = self.blockchain.storage.read();
        for tx in block.transactions.hashes() {
            let receipt = storage.transactions.get(&tx)?.receipt.clone();
            receipts.push(receipt);
        }
        Some(receipts)
    }

    /// Returns all transaction receipts of the block
    pub fn mined_block_receipts(&self, id: impl Into<BlockId>) -> Option<Vec<ReceiptResponse>> {
        let mut receipts = Vec::new();
        let block = self.get_block(id)?;

        for transaction in block.transactions {
            let receipt = self.mined_transaction_receipt(transaction.hash())?;
            receipts.push(receipt.inner);
        }

        Some(receipts)
    }

    /// Returns the transaction receipt for the given hash
    pub(crate) fn mined_transaction_receipt(&self, hash: B256) -> Option<MinedTransactionReceipt> {
        let MinedTransaction { info, receipt: tx_receipt, block_hash, .. } =
            self.blockchain.get_transaction_by_hash(&hash)?;

        let index = info.transaction_index as usize;
        let block = self.blockchain.get_block_by_hash(&block_hash)?;
        let transaction = block.transactions[index].clone();

        // Cancun specific
        let excess_blob_gas = block.header.excess_blob_gas;
        let blob_gas_price = calc_blob_gasprice(excess_blob_gas.map_or(0, |g| g as u64));
        let blob_gas_used = transaction.blob_gas();

        let effective_gas_price = match transaction.transaction {
            TypedTransaction::Legacy(t) => t.tx().gas_price,
            TypedTransaction::EIP2930(t) => t.tx().gas_price,
            TypedTransaction::EIP1559(t) => block
                .header
                .base_fee_per_gas
                .unwrap_or_else(|| self.base_fee())
                .saturating_add(t.tx().max_priority_fee_per_gas),
            TypedTransaction::EIP4844(t) => block
                .header
                .base_fee_per_gas
                .unwrap_or_else(|| self.base_fee())
                .saturating_add(t.tx().tx().max_priority_fee_per_gas),
            TypedTransaction::EIP7702(t) => block
                .header
                .base_fee_per_gas
                .unwrap_or_else(|| self.base_fee())
                .saturating_add(t.tx().max_priority_fee_per_gas),
            TypedTransaction::Deposit(_) => 0_u128,
        };

        let receipts = self.get_receipts(block.transactions.iter().map(|tx| tx.hash()));
        let next_log_index = receipts[..index].iter().map(|r| r.logs().len()).sum::<usize>();

        let receipt = tx_receipt.as_receipt_with_bloom().receipt.clone();
        let receipt = Receipt {
            status: receipt.status,
            cumulative_gas_used: receipt.cumulative_gas_used,
            logs: receipt
                .logs
                .into_iter()
                .enumerate()
                .map(|(index, log)| alloy_rpc_types::Log {
                    inner: log,
                    block_hash: Some(block_hash),
                    block_number: Some(block.header.number),
                    block_timestamp: Some(block.header.timestamp),
                    transaction_hash: Some(info.transaction_hash),
                    transaction_index: Some(info.transaction_index),
                    log_index: Some((next_log_index + index) as u64),
                    removed: false,
                })
                .collect(),
        };
        let receipt_with_bloom =
            ReceiptWithBloom { receipt, logs_bloom: tx_receipt.as_receipt_with_bloom().logs_bloom };

        let inner = match tx_receipt {
            TypedReceipt::EIP1559(_) => TypedReceipt::EIP1559(receipt_with_bloom),
            TypedReceipt::Legacy(_) => TypedReceipt::Legacy(receipt_with_bloom),
            TypedReceipt::EIP2930(_) => TypedReceipt::EIP2930(receipt_with_bloom),
            TypedReceipt::EIP4844(_) => TypedReceipt::EIP4844(receipt_with_bloom),
            TypedReceipt::EIP7702(_) => TypedReceipt::EIP7702(receipt_with_bloom),
            TypedReceipt::Deposit(r) => TypedReceipt::Deposit(DepositReceipt {
                inner: receipt_with_bloom,
                deposit_nonce: r.deposit_nonce,
                deposit_receipt_version: r.deposit_receipt_version,
            }),
        };

        let inner = TransactionReceipt {
            inner,
            transaction_hash: info.transaction_hash,
            transaction_index: Some(info.transaction_index),
            block_number: Some(block.header.number),
            gas_used: info.gas_used,
            contract_address: info.contract_address,
            effective_gas_price,
            block_hash: Some(block_hash),
            from: info.from,
            to: info.to,
            state_root: Some(block.header.state_root),
            blob_gas_price: Some(blob_gas_price),
            blob_gas_used,
            authorization_list: None,
        };

        Some(MinedTransactionReceipt { inner, out: info.out.map(|o| o.0.into()) })
    }

    /// Returns the blocks receipts for the given number
    pub async fn block_receipts(
        &self,
        number: BlockNumber,
    ) -> Result<Option<Vec<ReceiptResponse>>, BlockchainError> {
        if let Some(receipts) = self.mined_block_receipts(number) {
            return Ok(Some(receipts));
        }

        if let Some(fork) = self.get_fork() {
            let number = self.convert_block_number(Some(number));

            if fork.predates_fork_inclusive(number) {
                let receipts = fork.block_receipts(number).await?;

                return Ok(receipts);
            }
        }

        Ok(None)
    }

    pub async fn transaction_by_block_number_and_index(
        &self,
        number: BlockNumber,
        index: Index,
    ) -> Result<Option<WithOtherFields<Transaction>>, BlockchainError> {
        if let Some(hash) = self.mined_block_by_number(number).and_then(|b| b.header.hash) {
            return Ok(self.mined_transaction_by_block_hash_and_index(hash, index));
        }

        if let Some(fork) = self.get_fork() {
            let number = self.convert_block_number(Some(number));
            if fork.predates_fork(number) {
                return Ok(fork.transaction_by_block_number_and_index(number, index.into()).await?)
            }
        }

        Ok(None)
    }

    pub async fn transaction_by_block_hash_and_index(
        &self,
        hash: B256,
        index: Index,
    ) -> Result<Option<WithOtherFields<Transaction>>, BlockchainError> {
        if let tx @ Some(_) = self.mined_transaction_by_block_hash_and_index(hash, index) {
            return Ok(tx);
        }

        if let Some(fork) = self.get_fork() {
            return Ok(fork.transaction_by_block_hash_and_index(hash, index.into()).await?)
        }

        Ok(None)
    }

    pub fn mined_transaction_by_block_hash_and_index(
        &self,
        block_hash: B256,
        index: Index,
    ) -> Option<WithOtherFields<Transaction>> {
        let (info, block, tx) = {
            let storage = self.blockchain.storage.read();
            let block = storage.blocks.get(&block_hash).cloned()?;
            let index: usize = index.into();
            let tx = block.transactions.get(index)?.clone();
            let info = storage.transactions.get(&tx.hash())?.info.clone();
            (info, block, tx)
        };

        Some(transaction_build(
            Some(info.transaction_hash),
            tx,
            Some(&block),
            Some(info),
            block.header.base_fee_per_gas,
        ))
    }

    pub async fn transaction_by_hash(
        &self,
        hash: B256,
    ) -> Result<Option<WithOtherFields<Transaction>>, BlockchainError> {
        trace!(target: "backend", "transaction_by_hash={:?}", hash);
        if let tx @ Some(_) = self.mined_transaction_by_hash(hash) {
            return Ok(tx);
        }

        if let Some(fork) = self.get_fork() {
            return fork.transaction_by_hash(hash).await.map_err(BlockchainError::AlloyForkProvider)
        }

        Ok(None)
    }

    pub fn mined_transaction_by_hash(&self, hash: B256) -> Option<WithOtherFields<Transaction>> {
        let (info, block) = {
            let storage = self.blockchain.storage.read();
            let MinedTransaction { info, block_hash, .. } =
                storage.transactions.get(&hash)?.clone();
            let block = storage.blocks.get(&block_hash).cloned()?;
            (info, block)
        };
        let tx = block.transactions.get(info.transaction_index as usize)?.clone();

        Some(transaction_build(
            Some(info.transaction_hash),
            tx,
            Some(&block),
            Some(info),
            block.header.base_fee_per_gas,
        ))
    }

    /// Prove an account's existence or nonexistence in the state trie.
    ///
    /// Returns a merkle proof of the account's trie node, `account_key` == keccak(address)
    pub async fn prove_account_at(
        &self,
        address: Address,
        keys: Vec<B256>,
        block_request: Option<BlockRequest>,
    ) -> Result<AccountProof, BlockchainError> {
        let block_number = block_request.as_ref().map(|r| r.block_number());

        self.with_database_at(block_request, |block_db, _| {
            trace!(target: "backend", "get proof for {:?} at {:?}", address, block_number);
            let db = block_db.maybe_as_full_db().ok_or(BlockchainError::DataUnavailable)?;
            let account = db.get(&address).cloned().unwrap_or_default();

            let mut builder = HashBuilder::default()
                .with_proof_retainer(ProofRetainer::new(vec![Nibbles::unpack(keccak256(address))]));

            for (key, account) in trie_accounts(db) {
                builder.add_leaf(key, &account);
            }

            let _ = builder.root();

            let proof = builder.take_proofs().values().cloned().collect::<Vec<_>>();
            let storage_proofs = prove_storage(&account.storage, &keys);

            let account_proof = AccountProof {
                address,
                balance: account.info.balance,
                nonce: account.info.nonce,
                code_hash: account.info.code_hash,
                storage_hash: storage_root(&account.storage),
                account_proof: proof,
                storage_proof: keys
                    .into_iter()
                    .zip(storage_proofs)
                    .map(|(key, proof)| {
                        let storage_key: U256 = key.into();
                        let value = account.storage.get(&storage_key).cloned().unwrap_or_default();
                        StorageProof { key: JsonStorageKey(key), value, proof }
                    })
                    .collect(),
            };

            Ok(account_proof)
        })
        .await?
    }

    /// Returns a new block event stream
    pub fn new_block_notifications(&self) -> NewBlockNotifications {
        let (tx, rx) = unbounded();
        self.new_block_listeners.lock().push(tx);
        trace!(target: "backed", "added new block listener");
        rx
    }

    /// Notifies all `new_block_listeners` about the new block
    fn notify_on_new_block(&self, header: Header, hash: B256) {
        // cleanup closed notification streams first, if the channel is closed we can remove the
        // sender half for the set
        self.new_block_listeners.lock().retain(|tx| !tx.is_closed());

        let notification = NewBlockNotification { hash, header: Arc::new(header) };

        self.new_block_listeners
            .lock()
            .retain(|tx| tx.unbounded_send(notification.clone()).is_ok());
    }
}

/// Get max nonce from transaction pool by address
fn get_pool_transactions_nonce(
    pool_transactions: &[Arc<PoolTransaction>],
    address: Address,
) -> Option<u64> {
    if let Some(highest_nonce) = pool_transactions
        .iter()
        .filter(|tx| *tx.pending_transaction.sender() == address)
        .map(|tx| tx.pending_transaction.nonce())
        .max()
    {
        let tx_count = highest_nonce.saturating_add(1);
        return Some(tx_count)
    }
    None
}

#[async_trait::async_trait]
impl TransactionValidator for Backend {
    async fn validate_pool_transaction(
        &self,
        tx: &PendingTransaction,
    ) -> Result<(), BlockchainError> {
        let address = *tx.sender();
        let account = self.get_account(address).await?;
        let env = self.next_env();
        Ok(self.validate_pool_transaction_for(tx, &account, &env)?)
    }

    fn validate_pool_transaction_for(
        &self,
        pending: &PendingTransaction,
        account: &AccountInfo,
        env: &EnvWithHandlerCfg,
    ) -> Result<(), InvalidTransactionError> {
        let tx = &pending.transaction;

        if let Some(tx_chain_id) = tx.chain_id() {
            let chain_id = self.chain_id();
            if chain_id.to::<u64>() != tx_chain_id {
                if let Some(legacy) = tx.as_legacy() {
                    // <https://github.com/ethereum/EIPs/blob/master/EIPS/eip-155.md>
                    if env.handler_cfg.spec_id >= SpecId::SPURIOUS_DRAGON &&
                        !meets_eip155(chain_id.to::<u64>(), legacy.signature().v())
                    {
                        warn!(target: "backend", ?chain_id, ?tx_chain_id, "incompatible EIP155-based V");
                        return Err(InvalidTransactionError::IncompatibleEIP155);
                    }
                } else {
                    warn!(target: "backend", ?chain_id, ?tx_chain_id, "invalid chain id");
                    return Err(InvalidTransactionError::InvalidChainId);
                }
            }
        }

        if tx.gas_limit() < MIN_TRANSACTION_GAS {
            warn!(target: "backend", "[{:?}] gas too low", tx.hash());
            return Err(InvalidTransactionError::GasTooLow);
        }

        // Check gas limit, iff block gas limit is set.
        if !env.cfg.disable_block_gas_limit && tx.gas_limit() > env.block.gas_limit.to() {
            warn!(target: "backend", "[{:?}] gas too high", tx.hash());
            return Err(InvalidTransactionError::GasTooHigh(ErrDetail {
                detail: String::from("tx.gas_limit > env.block.gas_limit"),
            }));
        }

        // check nonce
        let is_deposit_tx =
            matches!(&pending.transaction.transaction, TypedTransaction::Deposit(_));
        let nonce = tx.nonce();
        if nonce < account.nonce && !is_deposit_tx {
            warn!(target: "backend", "[{:?}] nonce too low", tx.hash());
            return Err(InvalidTransactionError::NonceTooLow);
        }

        if (env.handler_cfg.spec_id as u8) >= (SpecId::LONDON as u8) {
            if tx.gas_price() < env.block.basefee.to() && !is_deposit_tx {
                warn!(target: "backend", "max fee per gas={}, too low, block basefee={}",tx.gas_price(),  env.block.basefee);
                return Err(InvalidTransactionError::FeeCapTooLow);
            }

            if let (Some(max_priority_fee_per_gas), Some(max_fee_per_gas)) =
                (tx.essentials().max_priority_fee_per_gas, tx.essentials().max_fee_per_gas)
            {
                if max_priority_fee_per_gas > max_fee_per_gas {
                    warn!(target: "backend", "max priority fee per gas={}, too high, max fee per gas={}", max_priority_fee_per_gas, max_fee_per_gas);
                    return Err(InvalidTransactionError::TipAboveFeeCap);
                }
            }
        }

        // EIP-4844 Cancun hard fork validation steps
        if env.spec_id() >= SpecId::CANCUN && tx.transaction.is_eip4844() {
            // Light checks first: see if the blob fee cap is too low.
            if let Some(max_fee_per_blob_gas) = tx.essentials().max_fee_per_blob_gas {
                if let Some(blob_gas_and_price) = &env.block.blob_excess_gas_and_price {
                    if max_fee_per_blob_gas < blob_gas_and_price.blob_gasprice {
                        warn!(target: "backend", "max fee per blob gas={}, too low, block blob gas price={}", max_fee_per_blob_gas, blob_gas_and_price.blob_gasprice);
                        return Err(InvalidTransactionError::BlobFeeCapTooLow);
                    }
                }
            }

            // Heavy (blob validation) checks
            let tx = match &tx.transaction {
                TypedTransaction::EIP4844(tx) => tx.tx(),
                _ => unreachable!(),
            };

            let blob_count = tx.tx().blob_versioned_hashes.len();

            // Ensure there are blob hashes.
            if blob_count == 0 {
                return Err(InvalidTransactionError::NoBlobHashes)
            }

            // Ensure the tx does not exceed the max blobs per block.
            if blob_count > MAX_BLOBS_PER_BLOCK {
                return Err(InvalidTransactionError::TooManyBlobs(MAX_BLOBS_PER_BLOCK, blob_count))
            }

            // Check for any blob validation errors
            if let Err(err) = tx.validate(env.cfg.kzg_settings.get()) {
                return Err(InvalidTransactionError::BlobTransactionValidationError(err))
            }
        }

        let max_cost = tx.max_cost();
        let value = tx.value();
        // check sufficient funds: `gas * price + value`
        let req_funds = max_cost.checked_add(value.to()).ok_or_else(|| {
            warn!(target: "backend", "[{:?}] cost too high",
            tx.hash());
            InvalidTransactionError::InsufficientFunds
        })?;
        if account.balance < U256::from(req_funds) {
            warn!(target: "backend", "[{:?}] insufficient allowance={}, required={} account={:?}", tx.hash(), account.balance, req_funds, *pending.sender());
            return Err(InvalidTransactionError::InsufficientFunds);
        }
        Ok(())
    }

    fn validate_for(
        &self,
        tx: &PendingTransaction,
        account: &AccountInfo,
        env: &EnvWithHandlerCfg,
    ) -> Result<(), InvalidTransactionError> {
        self.validate_pool_transaction_for(tx, account, env)?;
        if tx.nonce() > account.nonce {
            return Err(InvalidTransactionError::NonceTooHigh);
        }
        Ok(())
    }
}

/// Creates a `Transaction` as it's expected for the `eth` RPC api from storage data
#[allow(clippy::too_many_arguments)]
pub fn transaction_build(
    tx_hash: Option<B256>,
    eth_transaction: MaybeImpersonatedTransaction,
    block: Option<&Block>,
    info: Option<TransactionInfo>,
    base_fee: Option<u128>,
) -> WithOtherFields<Transaction> {
    let mut transaction: Transaction = eth_transaction.clone().into();
    if info.is_some() && transaction.transaction_type == Some(0x7E) {
        transaction.nonce = info.as_ref().unwrap().nonce;
    }

    if eth_transaction.is_dynamic_fee() {
        if block.is_none() && info.is_none() {
            // transaction is not mined yet, gas price is considered just `max_fee_per_gas`
            transaction.gas_price = transaction.max_fee_per_gas;
        } else {
            // if transaction is already mined, gas price is considered base fee + priority fee: the
            // effective gas price.
            let base_fee = base_fee.unwrap_or(0u128);
            let max_priority_fee_per_gas = transaction.max_priority_fee_per_gas.unwrap_or(0);
            transaction.gas_price = Some(base_fee.saturating_add(max_priority_fee_per_gas));
        }
    } else {
        transaction.max_fee_per_gas = None;
        transaction.max_priority_fee_per_gas = None;
    }

    transaction.block_hash =
        block.as_ref().map(|block| B256::from(keccak256(alloy_rlp::encode(&block.header))));

    transaction.block_number = block.as_ref().map(|block| block.header.number);

    transaction.transaction_index = info.as_ref().map(|info| info.transaction_index);

    // need to check if the signature of the transaction is impersonated, if so then we
    // can't recover the sender, instead we use the sender from the executed transaction and set the
    // impersonated hash.
    if eth_transaction.is_impersonated() {
        transaction.from = info.as_ref().map(|info| info.from).unwrap_or_default();
        transaction.hash = eth_transaction.impersonated_hash(transaction.from);
    } else {
        transaction.from = eth_transaction.recover().expect("can recover signed tx");
    }

    // if a specific hash was provided we update the transaction's hash
    // This is important for impersonated transactions since they all use the `BYPASS_SIGNATURE`
    // which would result in different hashes
    // Note: for impersonated transactions this only concerns pending transactions because there's
    // no `info` yet.
    if let Some(tx_hash) = tx_hash {
        transaction.hash = tx_hash;
    }

    transaction.to = info.as_ref().map_or(eth_transaction.to(), |status| status.to);
    WithOtherFields::new(transaction)
}

/// Prove a storage key's existence or nonexistence in the account's storage
/// trie.
/// `storage_key` is the hash of the desired storage key, meaning
/// this will only work correctly under a secure trie.
/// `storage_key` == keccak(key)
pub fn prove_storage(storage: &HashMap<U256, U256>, keys: &[B256]) -> Vec<Vec<Bytes>> {
    let keys: Vec<_> = keys.iter().map(|key| Nibbles::unpack(keccak256(key))).collect();

    let mut builder = HashBuilder::default().with_proof_retainer(ProofRetainer::new(keys.clone()));

    for (key, value) in trie_storage(storage) {
        builder.add_leaf(key, &value);
    }

    let _ = builder.root();

    let mut proofs = Vec::new();
    let all_proof_nodes = builder.take_proofs();

    for proof_key in keys {
        // Iterate over all proof nodes and find the matching ones.
        // The filtered results are guaranteed to be in order.
        let matching_proof_nodes = all_proof_nodes
            .iter()
            .filter(|(path, _)| proof_key.starts_with(path))
            .map(|(_, node)| node.clone());
        proofs.push(matching_proof_nodes.collect());
    }

    proofs
}
