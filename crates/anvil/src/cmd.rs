use crate::{
    config::{ForkChoice, DEFAULT_MNEMONIC},
    eth::{backend::db::SerializableState, pool::transactions::TransactionOrder, EthApi},
    AccountGenerator, Hardfork, NodeConfig, CHAIN_ID,
};
use alloy_consensus::Receipt;
use alloy_genesis::Genesis;
use alloy_primitives::{hex::ToHexExt, utils::Unit, Bytes, B256, U256};
use alloy_rpc_types::{Log, TransactionRequest};
use alloy_serde::WithOtherFields;
use alloy_signer_local::coins_bip39::{English, Mnemonic};
use anvil_core::eth::transaction::ReceiptResponse;
use anvil_server::ServerConfig;
use clap::{builder::TypedValueParser, Parser};
use hex::FromHex;
use core::fmt;
use foundry_config::{Chain, Config, FigmentProviders};
use futures::FutureExt;
use rand::{rngs::StdRng, SeedableRng};
use std::{
    future::Future,
    net::IpAddr,
    path::{Path, PathBuf},
    pin::Pin,
    str::FromStr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::time::{Instant, Interval};

#[derive(Clone, Debug, Parser)]
pub struct NodeArgs {
    /// Port number to listen on.
    #[arg(long, short, default_value = "8545", value_name = "NUM")]
    pub port: u16,

    /// Number of dev accounts to generate and configure.
    #[arg(long, short, default_value = "10", value_name = "NUM")]
    pub accounts: u64,

    /// The balance of every dev account in Ether.
    #[arg(long, default_value = "10000", value_name = "NUM")]
    pub balance: u64,

    /// The timestamp of the genesis block.
    #[arg(long, value_name = "NUM")]
    pub timestamp: Option<u64>,

    /// BIP39 mnemonic phrase used for generating accounts.
    /// Cannot be used if `mnemonic_random` or `mnemonic_seed` are used.
    #[arg(long, short, conflicts_with_all = &["mnemonic_seed", "mnemonic_random"])]
    pub mnemonic: Option<String>,

    /// Automatically generates a BIP39 mnemonic phrase, and derives accounts from it.
    /// Cannot be used with other `mnemonic` options.
    /// You can specify the number of words you want in the mnemonic.
    /// [default: 12]
    #[arg(long, conflicts_with_all = &["mnemonic", "mnemonic_seed"], default_missing_value = "12", num_args(0..=1))]
    pub mnemonic_random: Option<usize>,

    /// Generates a BIP39 mnemonic phrase from a given seed
    /// Cannot be used with other `mnemonic` options.
    ///
    /// CAREFUL: This is NOT SAFE and should only be used for testing.
    /// Never use the private keys generated in production.
    #[arg(long = "mnemonic-seed-unsafe", conflicts_with_all = &["mnemonic", "mnemonic_random"])]
    pub mnemonic_seed: Option<u64>,

    /// Sets the derivation path of the child key to be derived.
    ///
    /// [default: m/44'/60'/0'/0/]
    #[arg(long)]
    pub derivation_path: Option<String>,

    /// Don't print anything on startup and don't print logs
    #[arg(long)]
    pub silent: bool,

    /// The EVM hardfork to use.
    ///
    /// Choose the hardfork by name, e.g. `shanghai`, `paris`, `london`, etc...
    /// [default: latest]
    #[arg(long, value_parser = Hardfork::from_str)]
    pub hardfork: Option<Hardfork>,

    /// Block time in seconds for interval mining.
    #[arg(short, long, visible_alias = "blockTime", value_name = "SECONDS", value_parser = duration_from_secs_f64)]
    pub block_time: Option<Duration>,

    /// Slots in an epoch
    #[arg(long, value_name = "SLOTS_IN_AN_EPOCH", default_value_t = 32)]
    pub slots_in_an_epoch: u64,

    /// Writes output of `anvil` as json to user-specified file.
    #[arg(long, value_name = "OUT_FILE")]
    pub config_out: Option<String>,

    /// Disable auto and interval mining, and mine on demand instead.
    #[arg(long, visible_alias = "no-mine", conflicts_with = "block_time")]
    pub no_mining: bool,

    #[arg(long, visible_alias = "mixed-mining", requires = "block_time")]
    pub mixed_mining: bool,

    /// The hosts the server will listen on.
    #[arg(
        long,
        value_name = "IP_ADDR",
        env = "ANVIL_IP_ADDR",
        default_value = "127.0.0.1",
        help_heading = "Server options",
        value_delimiter = ','
    )]
    pub host: Vec<IpAddr>,

    /// How transactions are sorted in the mempool.
    #[arg(long, default_value = "fees")]
    pub order: TransactionOrder,

    /// Initialize the genesis block with the given `genesis.json` file.
    #[arg(long, value_name = "PATH", value_parser= read_genesis_file)]
    pub init: Option<Genesis>,

    /// This is an alias for both --load-state and --dump-state.
    ///
    /// It initializes the chain with the state and block environment stored at the file, if it
    /// exists, and dumps the chain's state on exit.
    #[arg(
        long,
        value_name = "PATH",
        value_parser = StateFile::parse,
        conflicts_with_all = &[
            "init",
            "dump_state",
            "load_state"
        ]
    )]
    pub state: Option<StateFile>,

    /// Interval in seconds at which the state and block environment is to be dumped to disk.
    ///
    /// See --state and --dump-state
    #[arg(short, long, value_name = "SECONDS")]
    pub state_interval: Option<u64>,

    /// Dump the state and block environment of chain on exit to the given file.
    ///
    /// If the value is a directory, the state will be written to `<VALUE>/state.json`.
    #[arg(long, value_name = "PATH", conflicts_with = "init")]
    pub dump_state: Option<PathBuf>,

    /// Initialize the chain from a previously saved state snapshot.
    #[arg(
        long,
        value_name = "PATH",
        value_parser = SerializableState::parse,
        conflicts_with = "init"
    )]
    pub load_state: Option<SerializableState>,

    #[arg(long, help = IPC_HELP, value_name = "PATH", visible_alias = "ipcpath")]
    pub ipc: Option<Option<String>>,

    /// Don't keep full chain history.
    /// If a number argument is specified, at most this number of states is kept in memory.
    #[arg(long)]
    pub prune_history: Option<Option<usize>>,

    /// Number of blocks with transactions to keep in memory.
    #[arg(long)]
    pub transaction_block_keeper: Option<usize>,

    #[command(flatten)]
    pub evm_opts: AnvilEvmArgs,

    #[command(flatten)]
    pub server_config: ServerConfig,
}

#[cfg(windows)]
const IPC_HELP: &str =
    "Launch an ipc server at the given path or default path = `\\.\\pipe\\anvil.ipc`";

/// The default IPC endpoint
#[cfg(not(windows))]
const IPC_HELP: &str = "Launch an ipc server at the given path or default path = `/tmp/anvil.ipc`";

/// Default interval for periodically dumping the state.
const DEFAULT_DUMP_INTERVAL: Duration = Duration::from_secs(60);

impl NodeArgs {
    pub fn into_node_config(self) -> NodeConfig {
        let genesis_balance = Unit::ETHER.wei().saturating_mul(U256::from(self.balance));
        let compute_units_per_second = if self.evm_opts.no_rate_limit {
            Some(u64::MAX)
        } else {
            self.evm_opts.compute_units_per_second
        };

        NodeConfig::default()
            .with_gas_limit(self.evm_opts.gas_limit)
            .disable_block_gas_limit(self.evm_opts.disable_block_gas_limit)
            .with_gas_price(self.evm_opts.gas_price)
            .with_hardfork(self.hardfork)
            .with_blocktime(self.block_time)
            .with_no_mining(self.no_mining)
            .with_mixed_mining(self.mixed_mining, self.block_time)
            .with_account_generator(self.account_generator())
            .with_genesis_balance(genesis_balance)
            .with_genesis_timestamp(self.timestamp)
            .with_port(self.port)
            .with_fork_choice(
                match (self.evm_opts.fork_block_number, self.evm_opts.fork_transaction_hash) {
                    (Some(block), None) => Some(ForkChoice::Block(block)),
                    (None, Some(hash)) => Some(ForkChoice::Transaction(hash)),
                    _ => {
                        self.evm_opts.fork_url.as_ref().and_then(|f| f.block).map(ForkChoice::Block)
                    }
                },
            )
            .with_fork_headers(self.evm_opts.fork_headers)
            .with_fork_chain_id(self.evm_opts.fork_chain_id.map(u64::from).map(U256::from))
            .fork_request_timeout(self.evm_opts.fork_request_timeout.map(Duration::from_millis))
            .fork_request_retries(self.evm_opts.fork_request_retries)
            .fork_retry_backoff(self.evm_opts.fork_retry_backoff.map(Duration::from_millis))
            .fork_compute_units_per_second(compute_units_per_second)
            .with_eth_rpc_url(self.evm_opts.fork_url.map(|fork| fork.url))
            .with_base_fee(self.evm_opts.block_base_fee_per_gas)
            .with_storage_caching(self.evm_opts.no_storage_caching)
            .with_server_config(self.server_config)
            .with_host(self.host)
            .set_silent(self.silent)
            .set_config_out(self.config_out)
            .with_chain_id(self.evm_opts.chain_id)
            .with_transaction_order(self.order)
            .with_genesis(self.init)
            .with_steps_tracing(self.evm_opts.steps_tracing)
            .with_print_logs(!self.evm_opts.disable_console_log)
            .with_auto_impersonate(self.evm_opts.auto_impersonate)
            .with_ipc(self.ipc)
            .with_code_size_limit(self.evm_opts.code_size_limit)
            .set_pruned_history(self.prune_history)
            .with_init_state(self.load_state.or_else(|| self.state.and_then(|s| s.state)))
            .with_transaction_block_keeper(self.transaction_block_keeper)
            .with_optimism(self.evm_opts.optimism)
            .with_disable_default_create2_deployer(self.evm_opts.disable_default_create2_deployer)
            .with_slots_in_an_epoch(self.slots_in_an_epoch)
            .with_memory_limit(self.evm_opts.memory_limit)
    }

    fn account_generator(&self) -> AccountGenerator {
        let mut gen = AccountGenerator::new(self.accounts as usize)
            .phrase(DEFAULT_MNEMONIC)
            .chain_id(self.evm_opts.chain_id.unwrap_or_else(|| CHAIN_ID.into()));
        if let Some(ref mnemonic) = self.mnemonic {
            gen = gen.phrase(mnemonic);
        } else if let Some(count) = self.mnemonic_random {
            let mut rng = rand::thread_rng();
            let mnemonic = match Mnemonic::<English>::new_with_count(&mut rng, count) {
                Ok(mnemonic) => mnemonic.to_phrase(),
                Err(_) => DEFAULT_MNEMONIC.to_string(),
            };
            gen = gen.phrase(mnemonic);
        } else if let Some(seed) = self.mnemonic_seed {
            let mut seed = StdRng::seed_from_u64(seed);
            let mnemonic = Mnemonic::<English>::new(&mut seed).to_phrase();
            gen = gen.phrase(mnemonic);
        }
        if let Some(ref derivation) = self.derivation_path {
            gen = gen.derivation_path(derivation);
        }
        gen
    }

    /// Returns the location where to dump the state to.
    fn dump_state_path(&self) -> Option<PathBuf> {
        self.dump_state.as_ref().or_else(|| self.state.as_ref().map(|s| &s.path)).cloned()
    }

    /// Starts the node
    ///
    /// See also [crate::spawn()]
    pub async fn run(self) -> eyre::Result<()> {
        let dump_state = self.dump_state_path();
        let dump_interval =
            self.state_interval.map(Duration::from_secs).unwrap_or(DEFAULT_DUMP_INTERVAL);

        let (api, mut handle) = crate::try_spawn(self.into_node_config()).await?;

        // sets the signal handler to gracefully shutdown.
        let mut fork = api.get_fork();
        let running = Arc::new(AtomicUsize::new(0));

        // handle for the currently running rt, this must be obtained before setting the crtlc
        // handler, See [Handle::current]
        let mut signal = handle.shutdown_signal_mut().take();

        let task_manager = handle.task_manager();
        let mut on_shutdown = task_manager.on_shutdown();

        let mut state_dumper = PeriodicStateDumper::new(api, dump_state, dump_interval);

        task_manager.spawn(async move {
            // wait for the SIGTERM signal on unix systems
            #[cfg(unix)]
            let mut sigterm = Box::pin(async {
                if let Ok(mut stream) =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                {
                    stream.recv().await;
                } else {
                    futures::future::pending::<()>().await;
                }
            });

            // on windows, this will never fires
            #[cfg(not(unix))]
            let mut sigterm = Box::pin(futures::future::pending::<()>());

            // await shutdown signal but also periodically flush state
            tokio::select! {
                 _ = &mut sigterm => {
                    trace!("received sigterm signal, shutting down");
                },
                _ = &mut on_shutdown =>{

                }
                _ = &mut state_dumper =>{}
            }

            // shutdown received
            state_dumper.dump().await;

            // cleaning up and shutting down
            // this will make sure that the fork RPC cache is flushed if caching is configured
            if let Some(fork) = fork.take() {
                trace!("flushing cache on shutdown");
                fork.database
                    .read()
                    .await
                    .maybe_flush_cache()
                    .expect("Could not flush cache on fork DB");
                // cleaning up and shutting down
                // this will make sure that the fork RPC cache is flushed if caching is configured
            }
            std::process::exit(0);
        });

        ctrlc::set_handler(move || {
            let prev = running.fetch_add(1, Ordering::SeqCst);
            if prev == 0 {
                trace!("received shutdown signal, shutting down");
                let _ = signal.take();
            }
        })
        .expect("Error setting Ctrl-C handler");

        Ok(handle.await??)
    }
}

/// Anvil's EVM related arguments.
#[derive(Clone, Debug, Parser)]
#[command(next_help_heading = "EVM options")]
pub struct AnvilEvmArgs {
    /// Fetch state over a remote endpoint instead of starting from an empty state.
    ///
    /// If you want to fetch state from a specific block number, add a block number like `http://localhost:8545@1400000` or use the `--fork-block-number` argument.
    #[arg(
        long,
        short,
        visible_alias = "rpc-url",
        value_name = "URL",
        help_heading = "Fork config"
    )]
    pub fork_url: Option<ForkUrl>,

    /// Headers to use for the rpc client, e.g. "User-Agent: test-agent"
    ///
    /// See --fork-url.
    #[arg(
        long = "fork-header",
        value_name = "HEADERS",
        help_heading = "Fork config",
        requires = "fork_url"
    )]
    pub fork_headers: Vec<String>,

    /// Timeout in ms for requests sent to remote JSON-RPC server in forking mode.
    ///
    /// Default value 45000
    #[arg(id = "timeout", long = "timeout", help_heading = "Fork config", requires = "fork_url")]
    pub fork_request_timeout: Option<u64>,

    /// Number of retry requests for spurious networks (timed out requests)
    ///
    /// Default value 5
    #[arg(id = "retries", long = "retries", help_heading = "Fork config", requires = "fork_url")]
    pub fork_request_retries: Option<u32>,

    /// Fetch state from a specific block number over a remote endpoint.
    ///
    /// See --fork-url.
    #[arg(long, requires = "fork_url", value_name = "BLOCK", help_heading = "Fork config")]
    pub fork_block_number: Option<u64>,

    /// Fetch state from a specific transaction hash over a remote endpoint.
    ///
    /// See --fork-url.
    #[arg(
        long,
        requires = "fork_url",
        value_name = "TRANSACTION",
        help_heading = "Fork config",
        conflicts_with = "fork_block_number"
    )]
    pub fork_transaction_hash: Option<B256>,

    /// Initial retry backoff on encountering errors.
    ///
    /// See --fork-url.
    #[arg(long, requires = "fork_url", value_name = "BACKOFF", help_heading = "Fork config")]
    pub fork_retry_backoff: Option<u64>,

    /// Specify chain id to skip fetching it from remote endpoint. This enables offline-start mode.
    ///
    /// You still must pass both `--fork-url` and `--fork-block-number`, and already have your
    /// required state cached on disk, anything missing locally would be fetched from the
    /// remote.
    #[arg(
        long,
        help_heading = "Fork config",
        value_name = "CHAIN",
        requires = "fork_block_number"
    )]
    pub fork_chain_id: Option<Chain>,

    /// Sets the number of assumed available compute units per second for this provider
    ///
    /// default value: 330
    ///
    /// See also --fork-url and <https://docs.alchemy.com/reference/compute-units#what-are-cups-compute-units-per-second>
    #[arg(
        long,
        requires = "fork_url",
        alias = "cups",
        value_name = "CUPS",
        help_heading = "Fork config"
    )]
    pub compute_units_per_second: Option<u64>,

    /// Disables rate limiting for this node's provider.
    ///
    /// default value: false
    ///
    /// See also --fork-url and <https://docs.alchemy.com/reference/compute-units#what-are-cups-compute-units-per-second>
    #[arg(
        long,
        requires = "fork_url",
        value_name = "NO_RATE_LIMITS",
        help_heading = "Fork config",
        visible_alias = "no-rpc-rate-limit"
    )]
    pub no_rate_limit: bool,

    /// Explicitly disables the use of RPC caching.
    ///
    /// All storage slots are read entirely from the endpoint.
    ///
    /// This flag overrides the project's configuration file.
    ///
    /// See --fork-url.
    #[arg(long, requires = "fork_url", help_heading = "Fork config")]
    pub no_storage_caching: bool,

    /// The block gas limit.
    #[arg(long, alias = "block-gas-limit", help_heading = "Environment config")]
    pub gas_limit: Option<u128>,

    /// Disable the `call.gas_limit <= block.gas_limit` constraint.
    #[arg(
        long,
        value_name = "DISABLE_GAS_LIMIT",
        help_heading = "Environment config",
        alias = "disable-gas-limit",
        conflicts_with = "gas_limit"
    )]
    pub disable_block_gas_limit: bool,

    /// EIP-170: Contract code size limit in bytes. Useful to increase this because of tests. By
    /// default, it is 0x6000 (~25kb).
    #[arg(long, value_name = "CODE_SIZE", help_heading = "Environment config")]
    pub code_size_limit: Option<usize>,

    /// The gas price.
    #[arg(long, help_heading = "Environment config")]
    pub gas_price: Option<u128>,

    /// The base fee in a block.
    #[arg(
        long,
        visible_alias = "base-fee",
        value_name = "FEE",
        help_heading = "Environment config"
    )]
    pub block_base_fee_per_gas: Option<u128>,

    /// The chain ID.
    #[arg(long, alias = "chain", help_heading = "Environment config")]
    pub chain_id: Option<Chain>,

    /// Enable steps tracing used for debug calls returning geth-style traces
    #[arg(long, visible_alias = "tracing")]
    pub steps_tracing: bool,

    /// Disable printing of `console.log` invocations to stdout.
    #[arg(long, visible_alias = "no-console-log")]
    pub disable_console_log: bool,

    /// Enable autoImpersonate on startup
    #[arg(long, visible_alias = "auto-impersonate")]
    pub auto_impersonate: bool,

    /// Run an Optimism chain
    #[arg(long, visible_alias = "optimism")]
    pub optimism: bool,

    /// Disable the default create2 deployer
    #[arg(long, visible_alias = "no-create2")]
    pub disable_default_create2_deployer: bool,

    /// The memory limit per EVM execution in bytes.
    #[arg(long)]
    pub memory_limit: Option<u64>,
}

/// Resolves an alias passed as fork-url to the matching url defined in the rpc_endpoints section
/// of the project configuration file.
/// Does nothing if the fork-url is not a configured alias.
impl AnvilEvmArgs {
    pub fn resolve_rpc_alias(&mut self) {
        if let Some(fork_url) = &self.fork_url {
            let config = Config::load_with_providers(FigmentProviders::Anvil);
            if let Some(Ok(url)) = config.get_rpc_url_with_alias(&fork_url.url) {
                self.fork_url = Some(ForkUrl { url: url.to_string(), block: fork_url.block });
            }
        }
    }
}

/// Helper type to periodically dump the state of the chain to disk
struct PeriodicStateDumper {
    in_progress_dump: Option<Pin<Box<dyn Future<Output = ()> + Send + Sync + 'static>>>,
    api: EthApi,
    dump_state: Option<PathBuf>,
    interval: Interval,
}

impl PeriodicStateDumper {
    fn new(api: EthApi, dump_state: Option<PathBuf>, interval: Duration) -> Self {
        let dump_state = dump_state.map(|mut dump_state| {
            if dump_state.is_dir() {
                dump_state = dump_state.join("state.json");
            }
            dump_state
        });

        // periodically flush the state
        let interval = tokio::time::interval_at(Instant::now() + interval, interval);
        Self { in_progress_dump: None, api, dump_state, interval }
    }

    async fn dump(&self) {
        if let Some(state) = self.dump_state.clone() {
            Self::dump_state(self.api.clone(), state).await
        }
    }

    /// Infallible state dump
    async fn dump_state(api: EthApi, dump_state: PathBuf) {
        trace!(path=?dump_state, "Dumping state on shutdown");
        match api.serialized_state().await {
            Ok(state) => {
                if let Err(err) = foundry_common::fs::write_json_file(&dump_state, &state) {
                    error!(?err, "Failed to dump state");
                } else {
                    trace!(path=?dump_state, "Dumped state on shutdown");
                }
            }
            Err(err) => {
                error!(?err, "Failed to extract state");
            }
        }
    }
}

// An endless future that periodically dumps the state to disk if configured.
impl Future for PeriodicStateDumper {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.dump_state.is_none() {
            return Poll::Pending
        }

        loop {
            if let Some(mut flush) = this.in_progress_dump.take() {
                match flush.poll_unpin(cx) {
                    Poll::Ready(_) => {
                        this.interval.reset();
                    }
                    Poll::Pending => {
                        this.in_progress_dump = Some(flush);
                        return Poll::Pending
                    }
                }
            }

            if this.interval.poll_tick(cx).is_ready() {
                let api = this.api.clone();
                let path = this.dump_state.clone().expect("exists; see above");
                this.in_progress_dump = Some(Box::pin(Self::dump_state(api, path)));
            } else {
                break
            }
        }

        Poll::Pending
    }
}

/// Represents the --state flag and where to load from, or dump the state to
#[derive(Clone, Debug)]
pub struct StateFile {
    pub path: PathBuf,
    pub state: Option<SerializableState>,
}

impl StateFile {
    /// This is used as the clap `value_parser` implementation to parse from file but only if it
    /// exists
    fn parse(path: &str) -> Result<Self, String> {
        Self::parse_path(path)
    }

    /// Parse from file but only if it exists
    pub fn parse_path(path: impl AsRef<Path>) -> Result<Self, String> {
        let mut path = path.as_ref().to_path_buf();
        if path.is_dir() {
            path = path.join("state.json");
        }
        let mut state = Self { path, state: None };
        if !state.path.exists() {
            return Ok(state)
        }

        state.state = Some(SerializableState::load(&state.path).map_err(|err| err.to_string())?);

        Ok(state)
    }
}

/// Represents the input URL for a fork with an optional trailing block number:
/// `http://localhost:8545@1000000`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkUrl {
    /// The endpoint url
    pub url: String,
    /// Optional trailing block
    pub block: Option<u64>,
}

impl fmt::Display for ForkUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.url.fmt(f)?;
        if let Some(block) = self.block {
            write!(f, "@{block}")?;
        }
        Ok(())
    }
}

impl FromStr for ForkUrl {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some((url, block)) = s.rsplit_once('@') {
            if block == "latest" {
                return Ok(Self { url: url.to_string(), block: None })
            }
            // this will prevent false positives for auths `user:password@example.com`
            if !block.is_empty() && !block.contains(':') && !block.contains('.') {
                let block: u64 = block
                    .parse()
                    .map_err(|_| format!("Failed to parse block number: `{block}`"))?;
                return Ok(Self { url: url.to_string(), block: Some(block) })
            }
        }
        Ok(Self { url: s.to_string(), block: None })
    }
}

/// Clap's value parser for genesis. Loads a genesis.json file.
fn read_genesis_file(path: &str) -> Result<Genesis, String> {
    foundry_common::fs::read_json_file(path.as_ref()).map_err(|err| err.to_string())
}

fn duration_from_secs_f64(s: &str) -> Result<Duration, String> {
    let s = s.parse::<f64>().map_err(|e| e.to_string())?;
    if s == 0.0 {
        return Err("Duration must be greater than 0".to_string());
    }
    Duration::try_from_secs_f64(s).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, net::Ipv4Addr};

    #[test]
    fn test_parse_fork_url() {
        let fork: ForkUrl = "http://localhost:8545@1000000".parse().unwrap();
        assert_eq!(
            fork,
            ForkUrl { url: "http://localhost:8545".to_string(), block: Some(1000000) }
        );

        let fork: ForkUrl = "http://localhost:8545".parse().unwrap();
        assert_eq!(fork, ForkUrl { url: "http://localhost:8545".to_string(), block: None });

        let fork: ForkUrl = "wss://user:password@example.com/".parse().unwrap();
        assert_eq!(
            fork,
            ForkUrl { url: "wss://user:password@example.com/".to_string(), block: None }
        );

        let fork: ForkUrl = "wss://user:password@example.com/@latest".parse().unwrap();
        assert_eq!(
            fork,
            ForkUrl { url: "wss://user:password@example.com/".to_string(), block: None }
        );

        let fork: ForkUrl = "wss://user:password@example.com/@100000".parse().unwrap();
        assert_eq!(
            fork,
            ForkUrl { url: "wss://user:password@example.com/".to_string(), block: Some(100000) }
        );
    }

    #[test]
    fn can_parse_hardfork() {
        let args: NodeArgs = NodeArgs::parse_from(["anvil", "--hardfork", "berlin"]);
        assert_eq!(args.hardfork, Some(Hardfork::Berlin));
    }

    #[test]
    fn can_parse_fork_headers() {
        let args: NodeArgs = NodeArgs::parse_from([
            "anvil",
            "--fork-url",
            "http,://localhost:8545",
            "--fork-header",
            "User-Agent: test-agent",
            "--fork-header",
            "Referrer: example.com",
        ]);
        assert_eq!(
            args.evm_opts.fork_headers,
            vec!["User-Agent: test-agent", "Referrer: example.com"]
        );
    }

    #[test]
    fn can_parse_prune_config() {
        let args: NodeArgs = NodeArgs::parse_from(["anvil", "--prune-history"]);
        assert!(args.prune_history.is_some());

        let args: NodeArgs = NodeArgs::parse_from(["anvil", "--prune-history", "100"]);
        assert_eq!(args.prune_history, Some(Some(100)));
    }

    #[test]
    fn can_parse_disable_block_gas_limit() {
        let args: NodeArgs = NodeArgs::parse_from(["anvil", "--disable-block-gas-limit"]);
        assert!(args.evm_opts.disable_block_gas_limit);

        let args =
            NodeArgs::try_parse_from(["anvil", "--disable-block-gas-limit", "--gas-limit", "100"]);
        assert!(args.is_err());
    }

    #[test]
    fn can_parse_host() {
        let args = NodeArgs::parse_from(["anvil"]);
        assert_eq!(args.host, vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]);

        let args = NodeArgs::parse_from([
            "anvil", "--host", "::1", "--host", "1.1.1.1", "--host", "2.2.2.2",
        ]);
        assert_eq!(
            args.host,
            ["::1", "1.1.1.1", "2.2.2.2"].map(|ip| ip.parse::<IpAddr>().unwrap()).to_vec()
        );

        let args = NodeArgs::parse_from(["anvil", "--host", "::1,1.1.1.1,2.2.2.2"]);
        assert_eq!(
            args.host,
            ["::1", "1.1.1.1", "2.2.2.2"].map(|ip| ip.parse::<IpAddr>().unwrap()).to_vec()
        );

        env::set_var("ANVIL_IP_ADDR", "1.1.1.1");
        let args = NodeArgs::parse_from(["anvil"]);
        assert_eq!(args.host, vec!["1.1.1.1".parse::<IpAddr>().unwrap()]);

        env::set_var("ANVIL_IP_ADDR", "::1,1.1.1.1,2.2.2.2");
        let args = NodeArgs::parse_from(["anvil"]);
        assert_eq!(
            args.host,
            ["::1", "1.1.1.1", "2.2.2.2"].map(|ip| ip.parse::<IpAddr>().unwrap()).to_vec()
        );
    }
}

// bhack: bindings test!
use pyo3::prelude::*;
use once_cell::sync::Lazy;
static RUNTIME : Lazy<Runtime> = Lazy::new(|| Runtime::new().unwrap());

impl NodeArgs {
    pub async fn pyrun(self) -> eyre::Result<EthApi> {
        let dump_state = self.dump_state_path();
        let dump_interval =
            self.state_interval.map(Duration::from_secs).unwrap_or(DEFAULT_DUMP_INTERVAL);

        let (api, mut handle) = crate::try_spawn(self.into_node_config()).await?;

        // sets the signal handler to gracefully shutdown.
        let mut fork = api.get_fork();
        let running = Arc::new(AtomicUsize::new(0));

        // handle for the currently running rt, this must be obtained before setting the crtlc
        // handler, See [Handle::current]
        let mut signal = handle.shutdown_signal_mut().take();

        let task_manager = handle.task_manager();
        let mut on_shutdown = task_manager.on_shutdown();

        let mut state_dumper = PeriodicStateDumper::new(api.clone(), dump_state, dump_interval);

        task_manager.spawn(async move {
            // wait for the SIGTERM signal on unix systems
            #[cfg(unix)]
            let mut sigterm = Box::pin(async {
                if let Ok(mut stream) =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                {
                    stream.recv().await;
                } else {
                    futures::future::pending::<()>().await;
                }
            });

            // on windows, this will never fires
            #[cfg(not(unix))]
            let mut sigterm = Box::pin(futures::future::pending::<()>());

            // await shutdown signal but also periodically flush state
            tokio::select! {
                 _ = &mut sigterm => {
                    trace!("received sigterm signal, shutting down");
                },
                _ = &mut on_shutdown =>{

                }
                _ = &mut state_dumper =>{}
            }

            // shutdown received
            state_dumper.dump().await;

            // cleaning up and shutting down
            // this will make sure that the fork RPC cache is flushed if caching is configured
            if let Some(fork) = fork.take() {
                trace!("flushing cache on shutdown");
                fork.database
                    .read()
                    .await
                    .maybe_flush_cache()
                    .expect("Could not flush cache on fork DB");
                // cleaning up and shutting down
                // this will make sure that the fork RPC cache is flushed if caching is configured
            }
            std::process::exit(0);
        });

        let _ = ctrlc::set_handler(move || {
            let prev = running.fetch_add(1, Ordering::SeqCst);
            if prev == 0 {
                trace!("received shutdown signal, shutting down");
                let _ = signal.take();
            }
        });

        RUNTIME.spawn(handle);

        Ok(api)
    }
}


#[pyclass]
struct NodeArgsWrapper {
    pub inner: usize 
}
impl NodeArgsWrapper {
    pub fn of_inner(inner: usize) -> Self {Self{inner}}
    pub fn to_node_args(self: NodeArgsWrapper) -> NodeArgs {
        let ptr = self.inner;
        let args : Box<NodeArgs>;
        unsafe {
            args = Box::from_raw(ptr as *mut NodeArgs);
        }
        *args
    }
}
struct EthApiWrapper {
    pub inner: usize
}
impl EthApiWrapper {
    pub fn of_inner(inner: usize) -> Self {Self{inner}}
    pub fn to_eth_api(self: EthApiWrapper) -> EthApi {
        let ptr = self.inner;
        let args : Box<EthApi>;
        unsafe {
            args = Box::from_raw(ptr as *mut EthApi);
        }
        let api = (*Box::leak(args)).clone();
        api
    }
}

use tokio::runtime::Runtime;
#[pyfunction]
fn run(ptr :usize) -> PyResult<usize> {
    let ptr = NodeArgsWrapper::of_inner(ptr);
    let args = ptr.to_node_args(); 
    let ethapi = RUNTIME.block_on(args.pyrun()).unwrap();
    let ethapi = Box::new(ethapi);
    let ethapi = Box::into_raw(ethapi);
    Ok(ethapi as usize)
}

#[pyfunction]
fn mine_block(ptr: usize) {
    let ptr = EthApiWrapper::of_inner(ptr);
    let ethapi = ptr.to_eth_api();
    RUNTIME.block_on(ethapi.mine_one())
}

#[pyclass(get_all, set_all, rename_all="camelCase")]
#[derive(Clone)]
pub struct PyLog {
    address: String,
    block_hash: Option<String>,
    block_number: Option<u64>,
    data: String,
    log_index: Option<u64>,
    topics:Vec<String>,
    transaction_hash:Option<String>,
    transaction_index:Option<u64>,
    removed: bool,
}
enum PyLogAttribute {
    Address(String),
    BlockHash(Option<String>),
    BlockNumber(Option<u64>),
    Data(String),
    LogIndex(Option<u64>),
    Topics(Vec<String>),
    TransactionHash(Option<String>),
    TransactionIndex(Option<u64>),
}
impl IntoPy<PyObject> for PyLogAttribute {
    fn into_py(self, py: Python<'_>) -> PyObject {
        match self {
            PyLogAttribute::Address(v) => v.into_py(py),
            PyLogAttribute::BlockHash(v) => v.into_py(py),
            PyLogAttribute::BlockNumber(v) => v.into_py(py),
            PyLogAttribute::Data(v) => v.into_py(py),
            PyLogAttribute::LogIndex(v) => v.into_py(py),
            PyLogAttribute::Topics(v) => v.into_py(py),
            PyLogAttribute::TransactionHash(v) => v.into_py(py),
            PyLogAttribute::TransactionIndex(v) => v.into_py(py),
        } 
    }
}
#[pymethods]
impl PyLog {
    fn __getitem__(&self, idx: &str) -> impl IntoPy<PyObject> {
        match idx {
            "address" => PyLogAttribute::Address(self.address.clone()),
            "blockHash" => PyLogAttribute::BlockHash(self.block_hash.clone()),
            "blockNumber" => PyLogAttribute::BlockNumber(self.block_number.clone()),
            "data" => PyLogAttribute::Data(self.data.clone()),
            "logIndex" => PyLogAttribute::LogIndex(self.log_index.clone()),
            "topics" => PyLogAttribute::Topics(self.topics.clone()),
            "transactionHash" => PyLogAttribute::TransactionHash(self.transaction_hash.clone()),
            "transactionIndex" => PyLogAttribute::TransactionIndex(self.transaction_index.clone()),
            _ => panic!("can't index that!")
        }
    }
}

#[pyclass(get_all, set_all, rename_all="camelCase")]
pub struct PyReceiptResponse {
    pub transaction_hash: String,
    pub transaction_index: Option<u64>,
    pub block_hash: Option<String>,
    pub block_number: Option<u64>,
    pub gas_used: u128,
    pub effective_gas_price: u128,
    pub blob_gas_used: Option<u128>,
    pub blob_gas_price: Option<u128>,
    pub from: String,
    pub to: Option<String>,
    pub contract_address: Option<String>,
    pub status: bool,
    pub cumulative_gas_used: u128,
    pub logs : Vec<PyLog>
}

#[pymethods]
impl PyReceiptResponse {
    fn __getitem__(&self, idx: &str) -> Vec<PyLog> {
        match idx {
            "logs" => self.logs.clone(), 
            _ => panic!("can't index that!")
        }
    }
}

use std::fmt::Write;
pub fn encode_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        write!(&mut s, "{:02x}", b).unwrap();
    }
    s
}
pub fn encode_hex32(bytes: &[u32]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        write!(&mut s, "{:02x}", b).unwrap();
    }
    s
}
pub fn encode_hex64(bytes: &[u64]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        write!(&mut s, "{:02x}", b).unwrap();
    }
    s
}

use alloy_consensus::Eip658Value::Eip658;
#[pyfunction]
fn get_transaction_receipt(ptr: usize, hash: [u8; 32]) -> PyReceiptResponse {
    let hash = B256::new(hash);
    let ptr = EthApiWrapper::of_inner(ptr);
    let ethapi = ptr.to_eth_api();
    let receipt : ReceiptResponse = RUNTIME.block_on(ethapi.transaction_receipt(hash)).unwrap().unwrap();
    let ReceiptResponse{
        transaction_hash,
        transaction_index,
        block_hash,
        block_number,
        gas_used,
        contract_address,
        effective_gas_price,
        from,
        to,
        blob_gas_price,
        blob_gas_used,
        state_root:_,
        authorization_list:_,
        inner,
    } = receipt;
    let Receipt {
        status,
        cumulative_gas_used,
        logs,
    } = inner.as_receipt_with_bloom().receipt.clone();
    let status = match status {Eip658(status) => status, _ => true};
    let logs = 
        logs.into_iter().map(
            |log : Log|  {
            let Log { inner:alloy_primitives::Log{ address, data }, block_hash, block_number, block_timestamp:_, transaction_hash, transaction_index, log_index, removed:_ } = log;
            let topics = data.topics();
            PyLog {
                address:encode_hex(address.as_slice()),
                block_hash:block_hash.map(|block_hash| encode_hex(block_hash.as_slice())),
                block_number:block_number,
                log_index: log_index,
                topics: topics.iter().map(|topic| encode_hex(topic.as_slice())).collect(), 
                transaction_hash: transaction_hash.map(|transaction_hash| encode_hex(transaction_hash.as_slice())),
                transaction_index: transaction_index,
                removed: false,
                data: hex::encode(data.data)
            }
        }
        ).collect();
    PyReceiptResponse {
        transaction_hash:encode_hex(transaction_hash.as_slice()),
        transaction_index,
        block_hash:block_hash.map(|block_hash| encode_hex(block_hash.as_slice())),
        block_number,
        gas_used,
        contract_address:contract_address.map(|contract_address| encode_hex(contract_address.as_slice())),
        effective_gas_price,
        from:encode_hex(from.as_slice()),
        to:to.map(|to| encode_hex(to.as_slice())),
        blob_gas_price,
        blob_gas_used,
        status,
        cumulative_gas_used,
        logs,
    }
}
#[derive(Clone)]
#[pyclass(get_all, set_all)]
pub enum PyTxKind {
    Create{},
    Call{v:String},
}
impl Into<alloy_primitives::TxKind> for PyTxKind {
    fn into(self) -> alloy_primitives::TxKind {
        match self {
            PyTxKind::Create{} => alloy_primitives::TxKind::Create,
            PyTxKind::Call{v} => alloy_primitives::TxKind::Call(alloy_primitives::Address::from_slice(&hex::decode(v).unwrap()))
        }
    }
}
#[derive(Clone)]
#[pyclass(dict, get_all, set_all, rename_all="camelCase")]
pub struct PyTransactionRequest {
    pub from: Option<String>,
    pub to: Option<PyTxKind>,
    pub gas_price: Option<u128>,
    pub max_fee_per_gas: Option<u128>,
    pub max_priority_fee_per_gas: Option<u128>,
    pub max_fee_per_blob_gas: Option<u128>,
    pub gas: Option<u128>,
    // pub value: Option<U256>,
    pub input: Option<String>,
    // pub nonce: Option<u64>,
    // pub chain_id: Option<ChainId>,
    // pub access_list: Option<AccessList>,
    pub transaction_type: Option<u8>,
    // pub blob_versioned_hashes: Option<Vec<B256>>,
    // pub sidecar: Option<BlobTransactionSidecar>,
}
impl Into<TransactionRequest> for &mut PyTransactionRequest {
    fn into(self) -> TransactionRequest {
        let v : PyTransactionRequest = self.clone();
        let PyTransactionRequest { from, to, gas_price, max_fee_per_gas, max_priority_fee_per_gas, max_fee_per_blob_gas, gas, input, transaction_type } = v;
        let from : Option<alloy_primitives::Address> = from.map(|from| hex::decode(from).ok()).flatten().map(|from| {
            let from: Vec<u8> = from;
            alloy_primitives::Address::from_slice(&from)
        });
        let to: Option<alloy_primitives::TxKind> = to.map(|to| to.into());
        let input: Option<Bytes> = 
            input.map(|input| {
                let input = hex::decode(input).unwrap();
                Bytes::copy_from_slice(&input)
            });
        let input: alloy_rpc_types::TransactionInput = 
            alloy_rpc_types::TransactionInput { input, data: None };
        TransactionRequest {from, to, gas_price, max_fee_per_gas, max_priority_fee_per_gas, max_fee_per_blob_gas, gas, value: None, input, transaction_type, blob_versioned_hashes: None, nonce: None, chain_id: None, access_list: None, sidecar: None }
    }    
}
#[pymethods]
impl PyTransactionRequest {
    #[staticmethod]
    fn create(from: Option<String>, to:Option<PyTxKind>, gas_price:Option<u128>, max_fee_per_gas:Option<u128>, max_priority_fee_per_gas:Option<u128>, max_fee_per_blob_gas:Option<u128>, gas:Option<u128>, input:Option<String>, transaction_type:Option<u8>) -> Self {
        Self { from, to, gas_price, max_fee_per_gas, max_priority_fee_per_gas, max_fee_per_blob_gas, gas, input, transaction_type }
    }
}

#[pyfunction]
fn call(ptr : usize, request : &mut PyTransactionRequest) -> Option<String> {
    let ptr = EthApiWrapper::of_inner(ptr);
    let ethapi = ptr.to_eth_api();
    let v = RUNTIME.block_on(ethapi.call(WithOtherFields::new(request.into()), None, None)).ok();
    v.map(|v| alloy_primitives::Bytes::encode_hex(&v))
}

#[pyfunction]
fn create(args : Vec<String>) -> PyResult<usize> {
    let args: NodeArgs = NodeArgs::parse_from(args);
    let args = Box::new(args);
    let args = Box::into_raw(args);
    Ok(args as usize)
}
/// A Python module implemented in Rust. The name of this function must match
/// the `lib.name` setting in the `Cargo.toml`, else Python will not be able to
/// import the module.
#[pymodule]
#[pyo3(name="anvil")]
fn anvil(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(run, m)?)?;
    m.add_function(wrap_pyfunction!(create, m)?)?;
    m.add_function(wrap_pyfunction!(mine_block, m)?)?;
    m.add_function(wrap_pyfunction!(get_transaction_receipt, m)?)?;
    m.add_function(wrap_pyfunction!(call, m)?)?;
    m.add_class::<PyTxKind>()?;
    m.add_class::<PyTransactionRequest>()?;

    Ok(())
}