//! Ksana fast-lane P2P network (SPEC §11.1, DECISIONS D-040).
//!
//! - Transport: TCP + Noise + Yamux, plus identify and ping; the node key is ed25519 ([`load_or_create_key`]).
//! - gossipsub topics `/ft/1/blocks` (Borsh(`Block`), including the signed settlement transactions) and `/ft/1/ops`
//!   (Borsh([`OpMessage`]): path-B VmOps, at most the network's `MAX_OP_BYTES`, [`P2pConfig::max_op_bytes`]), with
//!   manual validation: every message is checked against the length limit before decoding, handed to the [`Handler`]
//!   for a verdict, and then reported to gossipsub as Accept / Ignore / Reject (only Accept messages are forwarded
//!   further). The message ID is the content hash, so the same block / op is processed only once no matter who
//!   published it. The messages awaiting a verdict are held within a byte budget per topic, and a source within a
//!   quarter of it (D-132).
//! - request-response protocol `/ksana/sync/2` (format in [`wire`], block DAG in D-063): `Status`, `GetBlocks`,
//!   `GetBlock`, `GetMempool`. Once a connection is established, the peer's `Status` is asked first: on a genesis hash
//!   mismatch the peer is disconnected and rejected; when the peer's main chain has more chain work, or the peer's DAG
//!   tips include blocks we lack, blocks are synced in batches with a locator and a cursor (ascending by
//!   `(blue_work, hash)`), then the missing parents of orphans are fetched by hash; after that the peer's entry pool is
//!   fetched once. From then on `Status` is asked again periodically (to make up for missed gossip), and immediately
//!   when a gossip block's parent is unknown.
//! - Scoring: every block / op judged Reject scores 1 point against the source peer; at the threshold the peer is
//!   disconnected and banned for a while, after which the score is cleared (local policy). A peer connected from a
//!   public address bans that address too: no inbound connection from it is taken while the ban lasts (D-132).
//! - Rate limiting: each source (the address a peer connects from, an IPv6 address by its /64; the peer itself for a
//!   loopback or private address) has one token bucket each for gossip blocks, gossip ops and inbound sync requests
//!   ([`PeerLimits`], D-059), shared by every identity connecting from it and kept across reconnections (D-132);
//!   messages over the quota are dropped before decoding and cost no score.
//! - Connections ([`ConnectionLimits`], D-132): inbound connections are capped in all, in the handshake, per address
//!   and per peer; inbound sync requests are served a few at a time, at most a few per source, and the responses being
//!   sent are held within a byte budget per source and in all.
//! - Static bootstrap peers: dialed at startup, redialed periodically after a disconnect (except rejected peers).
//!
//! The network layer runs in its own thread and tokio runtime; the public methods only send commands through channels
//! or read snapshots, and never block.
//! The [`Handler`] methods are called on that runtime's blocking thread pool and never block the event loop.
//!
//! Everything this crate handles is untrusted input: panics are forbidden as in the node. System time is used only for
//! timers and ban expiry and never enters any block or state.

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

mod gate;
mod limit;
mod session;
pub mod wire;
mod worker;

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use borsh::{BorshDeserialize, BorshSerialize};
use ksana_primitives::{codec, Block, VmOp};
use tokio::sync::mpsc;

pub use libp2p::{identity::Keypair, Multiaddr, PeerId};

pub use limit::HEAVY_REQUEST_COST;
use worker::{Command, Shared, TopicKind};

/// A `/ft/1/ops` message: a path-B VmOp (the path-A Contribution was removed along with path A, D-078).
///
/// The Borsh encoding is the encoding of `op`.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct OpMessage {
    /// The user-signed op.
    pub op: VmOp,
}

/// Sync status (D-040, D-063). `tip_work` is the chain work of the main-chain tip (its blue work plus its own work, the
/// work the fork choice compares, D-100), a 256-bit unsigned integer as 32 little-endian bytes.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Status {
    /// Genesis block hash.
    pub genesis: [u8; 32],
    /// Main-chain tip height.
    pub tip_height: u64,
    /// Main-chain tip block hash.
    pub tip_hash: [u8; 32],
    /// Chain work of the main-chain tip (256-bit unsigned integer, little-endian).
    pub tip_work: [u8; 32],
    /// The DAG tips that can still be merged (blocks without children), at most [`wire::MAX_STATUS_TIPS`]; the peer
    /// uses them to discover blocks we have and it lacks.
    pub tips: Vec<[u8; 32]>,
}

/// The node's verdict on a received block / op.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Valid and new: forward it.
    Accept,
    /// Do not forward, no score (duplicate, cannot be judged yet, etc.).
    Ignore,
    /// Invalid: do not forward, score against the source peer.
    ///
    /// Only for data that is invalid in itself (signature, PoW, encoding, consensus rule violation, etc.); cases that
    /// depend on our current state and that an honest node might also send (e.g. the nonce was already used by an op
    /// on our chain, a reorg beyond the window) should use Ignore, or honest nodes would get banned.
    Reject(String),
    /// Parent unknown: start a sync with the source peer (this message is treated as Ignore).
    MissingParent,
}

/// Implemented by the node; the p2p thread calls these methods on the blocking thread pool (they may read / write
/// storage and execute blocks).
///
/// Messages and syncs from different peers may call these methods concurrently; the implementation must do its own
/// locking.
/// After [`P2p::stop`] returns there are no further calls.
pub trait Handler: Send + Sync + 'static {
    /// This node's sync status.
    fn status(&self) -> Status;
    /// The locator of our main chain: block hashes starting from the tip, the first 10 consecutive, then at doubling
    /// intervals, the last one being the genesis block (height sequence in [`wire::locator_heights`]).
    fn locator(&self) -> Vec<[u8; 32]>;
    /// The peer's GetBlocks (D-063): take the first block F in the locator that is on our main chain (the genesis
    /// block if none is), and return the known blocks not in `past(F) ∪ {F}`, ascending by `(blue_work, hash)`
    /// (topological order); when `after` is given, only those whose sort key is greater, at most `max`. The
    /// implementation may omit old blocks that can no longer be merged. Returns empty when `after` is not a block we
    /// know. The response carries only the leading blocks whose encodings fit [`wire::MAX_SYNC_MESSAGE_BYTES`], so the
    /// implementation may stop there (D-132).
    fn blocks_after(&self, locator: &[[u8; 32]], after: Option<&[u8; 32]>, max: u32) -> Vec<Block>;
    /// The peer's GetBlock.
    fn block(&self, hash: &[u8; 32]) -> Option<Block>;
    /// The blocks in `hashes` we lack (not in the DAG and not a known invalid block; orphans count as lacking).
    fn unknown(&self, hashes: &[[u8; 32]]) -> Vec<[u8; 32]>;
    /// The missing parents of orphans (fetched from the peer by hash).
    fn missing_parents(&self) -> Vec<[u8; 32]>;
    /// The peer's GetMempool: at most `max` pending ops (the response carries the leading ones that fit
    /// [`wire::MAX_MEMPOOL_RESPONSE_BYTES`], D-132).
    fn mempool(&self, max: u32) -> Vec<OpMessage>;
    /// A block was received: `gossip` true means from /ft/1/blocks, otherwise from a sync response.
    fn on_block(&self, from: PeerId, block: Block, gossip: bool) -> Verdict;
    /// An op was received (/ft/1/ops or a GetMempool response).
    fn on_op(&self, from: PeerId, msg: OpMessage) -> Verdict;
}

/// Network layer configuration.
pub struct P2pConfig {
    /// The node identity key.
    pub keypair: Keypair,
    /// Listen addresses (port 0 means assigned by the system).
    pub listen: Vec<Multiaddr>,
    /// Static bootstrap peers (with `/p2p/<peer id>`): dialed at startup, redialed periodically after a disconnect
    /// (except blocked ones).
    ///
    /// May also omit `/p2p` (e.g. devnet's `/dns4/node2/tcp/9000`): dialed by address each time, remembering the peer
    /// connected through that address.
    pub peers: Vec<Multiaddr>,
    /// This network's genesis block hash: peers whose Status does not match are disconnected and rejected.
    pub genesis: [u8; 32],
    /// This network's `MAX_OP_BYTES` (`Params::max_op_bytes`): a gossip op longer than this cannot be valid, it is
    /// rejected without decoding and not broadcast (D-132).
    pub max_op_bytes: usize,
}

impl fmt::Debug for P2pConfig {
    // Do not print the key: only the peer id derived from the public key.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("P2pConfig")
            .field("peer_id", &self.keypair.public().to_peer_id())
            .field("listen", &self.listen)
            .field("peers", &self.peers)
            .field("genesis", &hex::encode(self.genesis))
            .field("max_op_bytes", &self.max_op_bytes)
            .finish()
    }
}

/// A token bucket's quota: `per_sec` tokens refilled per second, at most `burst` accumulated (D-059).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rate {
    /// Tokens refilled per second.
    pub per_sec: u32,
    /// Capacity (the allowed burst). When a message needs more tokens than the capacity, the capacity applies, i.e. a
    /// full bucket is required.
    pub burst: u32,
}

/// Quotas per source (D-059, local policy): the address a peer connects from (an IPv6 address by its /64 prefix), or
/// the peer itself for a loopback or private address; every identity connecting from one address shares them, and they
/// carry over a reconnection (D-132). Gossip messages over the quota are reported as Ignore before decoding (not
/// forwarded, no score), inbound requests over the quota are dropped outright (the peer sees a failure); sync responses
/// to our own requests do not count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerLimits {
    /// Gossip blocks (default 20 / s, capacity 100).
    pub blocks: Rate,
    /// Gossip ops (default 1000 / s, capacity 5000).
    pub ops: Rate,
    /// Inbound sync requests (default 50 / s, capacity 200): `Status` and `GetBlock` count 1, `GetBlocks` and
    /// `GetMempool` count [`HEAVY_REQUEST_COST`].
    pub requests: Rate,
}

impl Default for PeerLimits {
    fn default() -> Self {
        PeerLimits {
            blocks: Rate {
                per_sec: 20,
                burst: 100,
            },
            ops: Rate {
                per_sec: 1_000,
                burst: 5_000,
            },
            requests: Rate {
                per_sec: 50,
                burst: 200,
            },
        }
    }
}

/// Connection limits (D-132, local policy). The limits per address leave out loopback, private and link-local
/// addresses (the node's own machine, its local network, devnet's containers), whose connections are counted only in
/// the totals; outbound connections (bootstrap peers, explicit dials) are capped only per peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionLimits {
    /// Inbound connections established at the same time (default 100).
    pub max_inbound: u32,
    /// Inbound connections still in the handshake (default 32).
    pub max_pending_inbound: u32,
    /// Inbound connections from one address (an IPv6 address by its /64 prefix), established and in the handshake
    /// together (default 4).
    pub max_inbound_per_ip: u32,
    /// Connections with one peer (default 2: two nodes that dial each other at the same moment).
    pub max_per_peer: u32,
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        ConnectionLimits {
            max_inbound: 100,
            max_pending_inbound: 32,
            max_inbound_per_ip: 4,
            max_per_peer: 2,
        }
    }
}

/// Timing and scoring parameters (local policy, not consensus). The defaults are the D-040 values; tests may shorten
/// them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tuning {
    /// Interval of the periodic Status query to all connected peers (default 3 s).
    pub status_interval: Duration,
    /// Interval for redialing unconnected bootstrap peers (default 2 s).
    pub redial_interval: Duration,
    /// Disconnect and ban when the score reaches this value (default 3).
    pub ban_threshold: u32,
    /// Ban duration (default 60 s); the score is cleared when it expires.
    pub ban_duration: Duration,
    /// Timeout of sync requests (default 30 s).
    pub request_timeout: Duration,
    /// Quotas per source (D-059, D-132).
    pub peer_limits: PeerLimits,
    /// Connection limits (D-132).
    pub connections: ConnectionLimits,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            status_interval: Duration::from_secs(3),
            redial_interval: Duration::from_secs(2),
            ban_threshold: 3,
            ban_duration: Duration::from_secs(60),
            request_timeout: Duration::from_secs(30),
            peer_limits: PeerLimits::default(),
            connections: ConnectionLimits::default(),
        }
    }
}

/// Network layer errors.
#[derive(Debug, thiserror::Error)]
pub enum P2pError {
    /// The node key cannot be parsed or generated.
    #[error("node key error: {0}")]
    Key(String),
    /// File read / write failed.
    #[error("reading / writing {path} failed: {source}")]
    Io {
        /// The file path.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// Runtime, transport or protocol initialization failed.
    #[error("network layer initialization failed: {0}")]
    Init(String),
    /// Cannot listen on the configured address.
    #[error("listen failed: {0}")]
    Listen(String),
}

/// The running network layer (its own thread and tokio runtime).
///
/// Dropping it is the same as [`P2p::stop`]: stops the network thread and waits for it to exit.
pub struct P2p {
    local_peer_id: PeerId,
    max_op_bytes: usize,
    shared: Arc<Shared>,
    commands: mpsc::UnboundedSender<Command>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl P2p {
    /// Start with the default [`Tuning`].
    pub fn start(config: P2pConfig, handler: Arc<dyn Handler>) -> Result<P2p, P2pError> {
        Self::start_with(config, Tuning::default(), handler)
    }

    /// Start the network thread: set up the transport, listen on the configured addresses, return once every listen
    /// address is bound ([`P2p::listen_addrs`] is usable), then dial the bootstrap peers.
    pub fn start_with(
        config: P2pConfig,
        tuning: Tuning,
        handler: Arc<dyn Handler>,
    ) -> Result<P2p, P2pError> {
        let local_peer_id = config.keypair.public().to_peer_id();
        let max_op_bytes = config.max_op_bytes;
        let shared = Arc::new(Shared::default());
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = {
            let shared = Arc::clone(&shared);
            let commands = commands.clone();
            std::thread::Builder::new()
                .name(worker::THREAD_NAME.to_string())
                .spawn(move || {
                    worker::run_thread(
                        config, tuning, handler, shared, commands, command_rx, ready_tx,
                    )
                })
                .map_err(|e| P2pError::Init(format!("cannot create the network thread: {e}")))?
        };
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(P2p {
                local_peer_id,
                max_op_bytes,
                shared,
                commands,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(P2pError::Init(
                    "network thread exited unexpectedly".to_string(),
                ))
            }
        }
    }

    /// This node's peer id.
    pub fn local_peer_id(&self) -> PeerId {
        self.local_peer_id
    }

    /// The addresses actually listened on (after binding port 0), without the /p2p suffix. When listen addresses are
    /// configured there is at least one when start returns.
    pub fn listen_addrs(&self) -> Vec<Multiaddr> {
        lock(&self.shared.listen_addrs).clone()
    }

    /// Broadcast a block (/ft/1/blocks). A block whose encoding exceeds [`wire::MAX_GOSSIP_BYTES`] is not broadcast
    /// (peers can only obtain it through sync).
    /// When no peer has subscribed to the topic yet the message is dropped; peers sync later through the periodic
    /// Status query.
    pub fn publish_block(&self, block: &Block) {
        self.publish(
            TopicKind::Block,
            codec::encode(block),
            wire::MAX_GOSSIP_BYTES,
        );
    }

    /// Broadcast an op (/ft/1/ops). An op whose encoding exceeds [`P2pConfig::max_op_bytes`] is not broadcast.
    pub fn publish_op(&self, msg: &OpMessage) {
        self.publish(TopicKind::Op, codec::encode(msg), self.max_op_bytes);
    }

    fn publish(&self, kind: TopicKind, data: Result<Vec<u8>, codec::CodecError>, max: usize) {
        match data {
            Ok(data) if data.len() <= max => {
                self.send(Command::Publish { kind, data });
            }
            Ok(data) => eprintln!(
                "[ksana-p2p] {kind} encoding length {} exceeds the gossip limit {max}, not broadcast",
                data.len(),
            ),
            Err(e) => eprintln!("[ksana-p2p] {kind} encoding failed, not broadcast: {e}"),
        }
    }

    /// The currently connected peers.
    pub fn connected_peers(&self) -> Vec<PeerId> {
        lock(&self.shared.connected).iter().copied().collect()
    }

    /// The peers subscribed to the block topic: blocks published to them are delivered directly (tests use this to wait
    /// until gossip is ready).
    pub fn gossip_peers(&self) -> Vec<PeerId> {
        lock(&self.shared.gossip_peers).iter().copied().collect()
    }

    /// Isolation (tests and operations): disconnect and refuse connections with this peer, no more redials; restored
    /// after unblock (bootstrap peers get redialed).
    pub fn block_peer(&self, peer: PeerId) {
        self.send(Command::Block(peer));
    }

    /// Undo [`P2p::block_peer`], also clearing the peer's ban and score (including a rejection caused by a genesis
    /// mismatch).
    pub fn unblock_peer(&self, peer: PeerId) {
        self.send(Command::Unblock(peer));
    }

    /// Dial actively (once only, not added to the bootstrap list).
    pub fn dial(&self, addr: Multiaddr) {
        self.send(Command::Dial(addr));
    }

    /// Ask all connected peers for their Status right away and sync with those that have more work.
    pub fn request_sync(&self) {
        self.send(Command::RequestSync);
    }

    /// Stop and wait for the network thread to exit. On return all connections are closed, every [`Handler`] call in
    /// progress has returned, and there are no further calls.
    ///
    /// Do not call it from a [`Handler`] method, nor while holding a lock the Handler needs (it waits for calls in
    /// progress).
    pub fn stop(self) {
        drop(self);
    }

    fn send(&self, command: Command) {
        // Silently dropped when the network thread has exited (only possible after stop).
        let _ = self.commands.send(command);
    }
}

impl Drop for P2p {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.commands.send(Command::Stop);
            // When dropped inside the network thread's own runtime (e.g. the last reference held by the Handler), it
            // must not wait for itself.
            if !worker::on_network_thread() {
                let _ = thread.join();
            }
        }
    }
}

/// Read or generate (and save with 0600 permissions) the ed25519 node key (protobuf encoding).
///
/// A new key is first written to a temporary file in the same directory and then renamed, so a crash midway leaves no
/// incomplete key file.
pub fn load_or_create_key(path: &Path) -> Result<Keypair, P2pError> {
    match std::fs::read(path) {
        Ok(bytes) => Keypair::from_protobuf_encoding(&bytes)
            .map_err(|e| P2pError::Key(format!("cannot parse {}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let keypair = Keypair::generate_ed25519();
            let bytes = keypair
                .to_protobuf_encoding()
                .map_err(|e| P2pError::Key(format!("cannot encode the new key: {e}")))?;
            write_private_file(path, &bytes)?;
            Ok(keypair)
        }
        Err(source) => Err(P2pError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Write a file with 0600 permissions (temporary file first, then rename).
fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), P2pError> {
    let io_err = |path: &Path| {
        let path = path.to_path_buf();
        move |source| P2pError::Io { path, source }
    };
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).map_err(io_err(&tmp))?;
    // The temporary file may be left over from an earlier interruption (open does not change an existing file's
    // permissions): set them explicitly before writing.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(io_err(&tmp))?;
    }
    file.write_all(bytes).map_err(io_err(&tmp))?;
    file.sync_all().map_err(io_err(&tmp))?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(io_err(path))
}

/// Lock; when the lock is poisoned (the holder panicked) use the data anyway (it only holds snapshots and cannot be
/// in an inconsistent state).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
