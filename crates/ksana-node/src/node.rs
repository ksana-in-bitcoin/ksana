//! Node assembly: chain, entry pool, BTC sync and settlement broadcast, block production loop, network layer (M4) and
//! RPC server.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use jsonrpsee::server::ServerHandle;
use ksana_btc::client::BtcClient;
use ksana_consensus::PowAlgorithm;
use ksana_mempool::{Mempool, Policy};
use ksana_primitives::Params;
use tokio::sync::broadcast;

use crate::chain::{Chain, ChainOptions, Pruning};
use crate::genesis::GenesisFile;
use crate::net::{NetHandler, Network};
use crate::ops::Relay;
use crate::producer::{Announce, FeePolicy, Funding, Producer, TemplateSync};
use crate::rescue::Rescuer;
use crate::rpc::NodeRpc;
use crate::NodeError;

/// Network layer config (D-040).
#[derive(Clone)]
pub struct NetConfig {
    /// Node identity key (`p2p.key` in the data directory).
    pub keypair: ksana_p2p::Keypair,
    /// Listen addresses.
    pub listen: Vec<ksana_p2p::Multiaddr>,
    /// Static bootstrap nodes.
    pub peers: Vec<ksana_p2p::Multiaddr>,
    /// Timing and scoring parameters (local policy; tests may shorten them).
    pub tuning: ksana_p2p::Tuning,
}

/// Node config.
#[derive(Clone)]
pub struct NodeConfig {
    /// Data directory.
    pub data_dir: PathBuf,
    /// Genesis config.
    pub genesis: GenesisFile,
    /// Protocol params.
    pub params: Params,
    /// RPC listen address (port 0 means assigned by the system).
    pub rpc_addr: SocketAddr,
    /// Cross-origin access to the RPC (browser pages, D-055).
    pub rpc_cors: ksana_rpc::Cors,
    /// Producer public key; `None` means no block production.
    pub producer: Option<[u8; 32]>,
    /// Payee account of republish payloads (D-095): `None` takes the producer account; when neither is given,
    /// republishing earns no carrier reward.
    pub carrier_payee: Option<[u8; 32]>,
    /// Funding wallet of the settlement transactions; when producing blocks without it, blocks only contain
    /// forced-inclusion and unlock entries.
    pub funding: Option<Funding>,
    /// Entry pool policy.
    pub policy: Policy,
    /// bitcoind (or the mock chain used by tests).
    pub btc: Arc<dyn BtcClient>,
    /// Interval of syncing with bitcoind and handling settlement broadcasts.
    pub btc_poll: Duration,
    /// Minimum interval between two blocks (local policy, 0 means no limit).
    pub min_block_interval: Duration,
    /// Network layer; `None` means no connection to other nodes (single node).
    pub net: Option<NetConfig>,
    /// Mining algorithm: by default (`None`) chosen by the network params; tests may substitute another algorithm
    /// (D-062).
    pub pow_algorithm: Option<Arc<dyn PowAlgorithm>>,
    /// Pruning (D-097): on by default ([`Pruning::default`]).
    pub pruning: Pruning,
}

/// A running node.
pub struct Node {
    chain: Arc<Chain>,
    rpc_addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    producer_thread: Option<JoinHandle<()>>,
    btc_thread: Option<JoinHandle<()>>,
    network: Option<Arc<Network>>,
    rpc_handle: ServerHandle,
    runtime: tokio::runtime::Runtime,
}

impl Node {
    /// Open the data directory (initializing the genesis state when needed), start the RPC server, and start the block
    /// production loop when required.
    pub fn start(config: NodeConfig) -> Result<Node, NodeError> {
        let chain = Arc::new(Chain::open_with(
            &config.data_dir,
            config.params.clone(),
            &config.genesis,
            Arc::clone(&config.btc),
            ChainOptions {
                algorithm: config.pow_algorithm.clone(),
                pruning: config.pruning,
            },
        )?);
        // Sync to bitcoind's tip first so the block production loop has an anchor available; on failure the background
        // thread keeps retrying
        loop {
            match chain.sync_btc() {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => {
                    eprintln!("[ksana-node] syncing with bitcoind failed (will retry later): {e}");
                    break;
                }
            }
        }
        if let Err(e) = chain.resume() {
            eprintln!("[ksana-node] crash recovery incomplete (will retry later): {e}");
        }
        let mempool = Arc::new(Mutex::new(Mempool::new(
            config.params.clone(),
            config.policy.clone(),
        )));
        let (events, _) = broadcast::channel(256);
        let shutdown = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let sync = config.producer.map(|_| Arc::new(TemplateSync::default()));

        let network = match config.net.clone() {
            Some(net) => {
                let handler = Arc::new(NetHandler {
                    chain: Arc::clone(&chain),
                    mempool: Arc::clone(&mempool),
                    events: events.clone(),
                });
                let p2p = ksana_p2p::P2p::start_with(
                    ksana_p2p::P2pConfig {
                        keypair: net.keypair,
                        listen: net.listen,
                        peers: net.peers,
                        genesis: chain.genesis().hash(),
                        max_op_bytes: config.params.max_op_bytes,
                    },
                    net.tuning,
                    handler,
                )
                .map_err(|e| NodeError::Config(format!("starting the network layer: {e}")))?;
                let network = Arc::new(Network::new(p2p));
                eprintln!(
                    "[ksana-node] network layer started: peer id {}, listening on {:?}",
                    network.local_peer_id(),
                    network.listen_addrs()
                );
                Some(network)
            }
            None => None,
        };
        let relay: Option<Relay> = network.as_ref().map(|n| {
            let n = Arc::clone(n);
            Arc::new(move |op: &ksana_primitives::VmOp| n.publish_op(op)) as Relay
        });
        let announce: Option<Announce> = network.as_ref().map(|n| {
            let n = Arc::clone(n);
            Arc::new(move |block: &ksana_primitives::Block| n.publish_block(block)) as Announce
        });

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("ksana-rpc")
            .build()?;
        let backend = Arc::new(NodeRpc {
            chain: Arc::clone(&chain),
            mempool: Arc::clone(&mempool),
            sync: sync.clone(),
            paused: Arc::clone(&paused),
            events: events.clone(),
            relay,
            fees: config
                .funding
                .as_ref()
                .map_or(FeePolicy::new(0), |f| f.fees),
            calls: AtomicUsize::new(0),
        });
        let (rpc_addr, rpc_handle) = runtime
            .block_on(ksana_rpc::start_server(
                config.rpc_addr,
                backend,
                &config.rpc_cors,
            ))
            .map_err(NodeError::Config)?;

        let rescuer = config.funding.clone().map(|funding| {
            Rescuer::new(
                Arc::clone(&chain),
                funding,
                config.producer,
                config.carrier_payee,
            )
        });
        let btc_thread = {
            let chain = Arc::clone(&chain);
            let mempool = Arc::clone(&mempool);
            let events = events.clone();
            let shutdown = Arc::clone(&shutdown);
            let poll = config.btc_poll;
            std::thread::Builder::new()
                .name("ksana-btc".into())
                .spawn(move || {
                    btc_loop(&chain, &mempool, &events, rescuer.as_ref(), &shutdown, poll)
                })?
        };
        let producer_thread = match (config.producer, sync) {
            (Some(producer), Some(sync)) => {
                let producer = Producer {
                    chain: Arc::clone(&chain),
                    mempool,
                    producer,
                    funding: config.funding.clone(),
                    pack: true,
                    sync,
                    events,
                    shutdown: Arc::clone(&shutdown),
                    paused: Arc::clone(&paused),
                    min_interval: config.min_block_interval,
                    announce,
                    cache: Default::default(),
                };
                Some(
                    std::thread::Builder::new()
                        .name("ksana-producer".into())
                        .spawn(move || producer.run())?,
                )
            }
            _ => None,
        };
        Ok(Node {
            chain,
            rpc_addr,
            shutdown,
            paused,
            producer_thread,
            btc_thread: Some(btc_thread),
            network,
            rpc_handle,
            runtime,
        })
    }

    /// The network layer (`None` when not enabled).
    pub fn network(&self) -> Option<&Arc<Network>> {
        self.network.as_ref()
    }

    /// The address the RPC actually listens on.
    pub fn rpc_addr(&self) -> SocketAddr {
        self.rpc_addr
    }

    /// The chain.
    pub fn chain(&self) -> &Arc<Chain> {
        &self.chain
    }

    /// Pause or resume block production (for tests: submit a batch of ops first, then let them enter the same block).
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    /// Block until SIGINT or SIGTERM is received.
    pub fn wait_for_signal(&self) -> Result<(), NodeError> {
        self.runtime.block_on(async {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            tokio::select! {
                result = tokio::signal::ctrl_c() => result,
                _ = terminate.recv() => Ok(()),
            }
        })?;
        Ok(())
    }

    /// Stop the block production loop and the RPC server, close the database.
    pub fn stop(mut self) -> Result<(), NodeError> {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.producer_thread.take() {
            thread
                .join()
                .map_err(|_| NodeError::Internal("producer thread exited abnormally".into()))?;
        }
        if let Some(thread) = self.btc_thread.take() {
            thread
                .join()
                .map_err(|_| NodeError::Internal("BTC thread exited abnormally".into()))?;
        }
        if let Some(network) = self.network.take() {
            network.stop();
        }
        // stop returns an error when the server has already stopped; ignored
        let _ = self.rpc_handle.stop();
        self.runtime.block_on(self.rpc_handle.clone().stopped());
        Ok(())
    }
}

/// The BTC thread: syncs with bitcoind (when there are new blocks, keeps processing until caught up; the finality
/// decision may re-select the main chain, D-082), then handles the broadcast of settlement transactions (D-037),
/// rescue settlement and header republishing (D-045, D-047, when a funding wallet is configured). Main-chain changes
/// caused by sync and crash recovery maintain the entry pool here (D-041).
fn btc_loop(
    chain: &Chain,
    mempool: &Mutex<Mempool>,
    events: &broadcast::Sender<ksana_rpc::BlockSummary>,
    rescuer: Option<&Rescuer>,
    shutdown: &AtomicBool,
    poll: Duration,
) {
    while !shutdown.load(Ordering::Relaxed) {
        loop {
            match chain.sync_btc() {
                Ok(true) if !shutdown.load(Ordering::Relaxed) => continue,
                Ok(_) => break,
                Err(e) => {
                    eprintln!("[ksana-node] syncing with bitcoind failed: {e}");
                    break;
                }
            }
        }
        if let Err(e) = chain.resume() {
            eprintln!("[ksana-node] crash recovery incomplete: {e}");
        }
        if let Err(e) = crate::ops::apply_chain_changes(chain, mempool, events) {
            eprintln!(
                "[ksana-node] failed to maintain the entry pool after a main-chain change: {e}"
            );
        }
        if let Err(e) = chain.process_settlements() {
            eprintln!("[ksana-node] failed to process settlement transactions: {e}");
        }
        if let Some(rescuer) = rescuer {
            if let Err(e) = rescuer.run_once() {
                eprintln!("[ksana-node] rescue settlement / header republishing failed: {e}");
            }
        }
        std::thread::sleep(poll);
    }
}
