//! `ksana-indexer`: the standalone indexer binary (SPEC §10.3, D-048).
//!
//! ```sh
//! ksana-indexer --data-dir ./indexer --genesis devnet/genesis.json --rpc-addr 127.0.0.1:8546 \
//!     --btc-rpc-url http://127.0.0.1:18443 --btc-rpc-user ksana --btc-rpc-pass ksana
//! ```
//!
//! Connects only to bitcoind (requires `-txindex=1`); the genesis config must be the same one the fast-lane nodes use.
//! One sync round every `--poll-ms` milliseconds: scans new BTC blocks, executes the fast-lane blocks entering the
//! settled prefix and verifies the `state_root`. After a successful start it prints one line
//! `ksana-indexer rpc=<address>` on stdout; logs go to stderr. Exits normally on SIGINT / SIGTERM.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use ksana_btc::client::RpcClient;
use ksana_btc::prune::Pruning;
use ksana_execution::GenesisFile;
use ksana_indexer::{Indexer, IndexerConfig, IndexerError};
use ksana_primitives::Network;

/// On Linux every allocation of the indexer, RocksDB's included, goes to jemalloc, as in the node (D-103).
#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Interval at which the stop flag is checked while waiting for the next round.
const STOP_CHECK: Duration = Duration::from_millis(50);

/// Ksana standalone indexer: connects only to bitcoind, rebuilds the fast-lane state and verifies the state_root block
/// by block.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Data directory.
    #[arg(long)]
    data_dir: PathBuf,
    /// Genesis config file (JSON, the same as the fast-lane nodes').
    #[arg(long)]
    genesis: PathBuf,
    /// Fast-lane network: `devnet`, `signet` (D-055) or `mainnet` (D-096); must be the same as the nodes'.
    #[arg(long, default_value = "devnet")]
    chain: String,
    /// bitcoind RPC address (requires `-txindex=1`).
    #[arg(long, default_value = "http://127.0.0.1:18443")]
    btc_rpc_url: String,
    /// bitcoind RPC user name.
    #[arg(long, default_value = "ksana")]
    btc_rpc_user: String,
    /// bitcoind RPC password (or the environment variable KSANA_BTC_RPC_PASS, which keeps it off the command line).
    #[arg(
        long,
        env = "KSANA_BTC_RPC_PASS",
        hide_env_values = true,
        default_value = "ksana"
    )]
    btc_rpc_pass: String,
    /// JSON-RPC listening address (HTTP and WebSocket).
    #[arg(long, default_value = "127.0.0.1:8546")]
    rpc_addr: SocketAddr,
    /// Interval between two sync rounds (milliseconds).
    #[arg(long, default_value_t = 500)]
    poll_ms: u64,
    /// Disable pruning (D-097): keep the whole block DAG, the executed main chain's summaries and the BTC snapshot in
    /// memory (memory grows with the chain's history). Pruning is on by default: memory holds only what the main
    /// chains the indexer can still follow need, the executed main chain below that is answered from the index on
    /// disk, and each pruning writes a checkpoint the next start continues from.
    #[arg(long)]
    no_pruning: bool,
}

fn run(cli: Cli) -> Result<(), IndexerError> {
    let btc = RpcClient::new(&cli.btc_rpc_url, &cli.btc_rpc_user, &cli.btc_rpc_pass)
        .map_err(|e| IndexerError::Config(format!("connecting to bitcoind: {e}")))?;
    let indexer = Arc::new(Indexer::open(IndexerConfig {
        data_dir: cli.data_dir,
        genesis: GenesisFile::load(&cli.genesis)?,
        params: Network::from_name(&cli.chain)
            .ok_or_else(|| {
                IndexerError::Config(format!(
                    "unknown network {} (devnet, signet or mainnet)",
                    cli.chain
                ))
            })?
            .params()
            .clone(),
        btc: Arc::new(btc),
        pow_algorithm: None,
        pruning: if cli.no_pruning {
            Pruning::disabled()
        } else {
            Pruning::default()
        },
    })?);
    eprintln!(
        "[ksana-indexer] upgrade table (D-076): {}",
        indexer.params().upgrade_schedule()
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("ksana-indexer-rpc")
        .build()
        .map_err(|e| IndexerError::Internal(format!("creating the tokio runtime: {e}")))?;
    let (rpc_addr, rpc_handle) = runtime
        .block_on(ksana_indexer::rpc::start_server(
            cli.rpc_addr,
            Arc::clone(&indexer),
        ))
        .map_err(IndexerError::Config)?;

    // SIGINT / SIGTERM: set the stop flag; a sync round in progress returns after the current block
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut terminate = runtime
        .block_on(async {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        })
        .map_err(|e| IndexerError::Internal(format!("registering SIGTERM: {e}")))?;
    {
        let shutdown = Arc::clone(&shutdown);
        let indexer = Arc::clone(&indexer);
        runtime.spawn(async move {
            tokio::select! {
                result = tokio::signal::ctrl_c() => {
                    if let Err(e) = result {
                        eprintln!("[ksana-indexer] waiting for SIGINT failed: {e}");
                    }
                }
                _ = terminate.recv() => {}
            }
            shutdown.store(true, Ordering::Relaxed);
            indexer.request_stop();
        });
    }

    let status = indexer.status();
    eprintln!(
        "[ksana-indexer] started: verified height {}, state root {}",
        status.height,
        hex::encode(status.state_root)
    );
    println!("ksana-indexer rpc={rpc_addr}");

    let slices = (cli.poll_ms / 50).max(1);
    let mut last_error: Option<String> = None;
    let mut reported_halt = false;
    while !shutdown.load(Ordering::Relaxed) {
        match indexer.sync_once() {
            Ok(progress) => {
                last_error = None;
                if progress.changed {
                    eprintln!(
                        "[ksana-indexer] verified up to height {} (block tree main-chain height {}, BTC height {})",
                        progress.height,
                        progress.main_height,
                        progress
                            .btc_height
                            .map_or_else(|| "-".to_string(), |h| h.to_string())
                    );
                }
                if !reported_halt {
                    if let Some(reason) = indexer.status().halted {
                        eprintln!("[ksana-indexer] processing halted: {reason}");
                        reported_halt = true;
                    }
                }
            }
            Err(e) => {
                let message = e.to_string();
                if last_error.as_deref() != Some(message.as_str()) {
                    eprintln!("[ksana-indexer] sync failed (will retry later): {message}");
                    last_error = Some(message);
                }
            }
        }
        for _ in 0..slices {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(STOP_CHECK);
        }
    }
    eprintln!("[ksana-indexer] stopping");
    // stop returns an error when the server has already stopped; ignored
    let _ = rpc_handle.stop();
    runtime.block_on(rpc_handle.stopped());
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[ksana-indexer] error: {e}");
            ExitCode::FAILURE
        }
    }
}
