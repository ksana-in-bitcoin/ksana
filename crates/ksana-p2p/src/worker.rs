//! The network thread: swarm event loop, command handling, gossip validation results, scoring and banning, bootstrap
//! peer redialing.
//!
//! The event loop only does bookkeeping and forwarding and never calls the [`Handler`]: all Handler calls happen in the
//! tasks of [`crate::session`] through the blocking thread pool, and the results come back to the event loop as
//! [`Command`]s.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ksana_primitives::hash::sha256;
use libp2p::core::transport::ListenerId;
use libp2p::futures::StreamExt;
use libp2p::gossipsub::{self, IdentTopic, MessageAcceptance, MessageId, PublishError, TopicHash};
use libp2p::request_response::{
    self, InboundRequestId, OutboundFailure, OutboundRequestId, ProtocolSupport, ResponseChannel,
};
use libp2p::swarm::dial_opts::{DialOpts, PeerCondition};
use libp2p::swarm::{ConnectionId, DialError, ListenError, NetworkBehaviour, SwarmEvent};
use libp2p::{
    allow_block_list, connection_limits, identify, identity, multiaddr, noise, ping, tcp, yamux,
    Multiaddr, PeerId, Swarm, SwarmBuilder,
};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, Interval, MissedTickBehavior};

use crate::gate::Gate;
use crate::limit::{request_cost, Budget, IpGroup, Limited, Limiter, Reserve, Serving, Source};
use crate::session::{self, GossipItem, TaskCtx};
use crate::wire::{self, Request, RequestFrame, Response, ScoringCodec};
use crate::{lock, ConnectionLimits, Handler, P2pConfig, P2pError, Tuning, Verdict};

/// The network thread's name (the runtime's worker and blocking threads use it as a prefix).
pub(crate) const THREAD_NAME: &str = "ksana-p2p";

/// How long a connection stays open when no protocol needs it: in a static network connections are kept forever.
const IDLE_CONNECTION_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
/// Upper bound on waiting for the listen addresses to bind.
const LISTEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Allowance of a gossipsub RPC frame beyond the message content (signature, public key, topic, protobuf fields).
const GOSSIP_FRAME_OVERHEAD: usize = 64 * 1024;
/// Allowance of one gossipsub message beyond its content (the publisher's peer id, sequence number, topic, signature,
/// public key and their protobuf fields: about 140 bytes with an ed25519 key, under 2 KiB with an RSA key of 4096
/// bits): the op topic's messages are refused by gossipsub beyond `MAX_OP_BYTES` and this (D-132).
const GOSSIP_MESSAGE_OVERHEAD: usize = 4 * 1024;
/// Number of heartbeats gossipsub caches messages for: a message awaiting validation is forwarded only if its result
/// is reported within this time (about 10 seconds).
const GOSSIP_HISTORY_LENGTH: usize = 10;
/// Maximum time a message waits in the gossipsub send queue (large blocks need longer on slow links).
const GOSSIP_QUEUE_DURATION: Duration = Duration::from_secs(10);
/// Limits on the bytes of gossip messages awaiting the Handler's verdict, per topic (D-132; one shared limit of 256 MiB
/// before): beyond them new messages are Ignored right away (sync fills in later), so a flood of ops cannot hold back
/// blocks. One source may hold at most a [`SOURCE_SHARE`] of a topic's limit.
const MAX_QUEUED_BLOCK_BYTES: usize = 256 * 1024 * 1024;
const MAX_QUEUED_OP_BYTES: usize = 32 * 1024 * 1024;
/// The share of a budget one source may hold: a quarter (D-132).
const SOURCE_SHARE: usize = 4;
/// Inbound sync requests served at the same time (D-132): the others wait their turn. Each may load up to a response's
/// worth of blocks.
const MAX_CONCURRENT_SERVES: usize = 4;
/// Inbound sync requests one source may have in flight, from the moment one is taken in until its response has been
/// sent (D-132): an honest peer asks one at a time per connection; the ones over it are dropped (the peer sees a
/// failure, no score).
const MAX_REQUESTS_PER_SOURCE: usize = 16;
/// Limit on the bytes of the sync responses being sent (D-132), a source holding at most a [`SOURCE_SHARE`]: a response
/// over it is dropped (the peer sees a failure and asks again later), so peers that read slowly cannot make this node
/// hold their responses without bound.
const MAX_RESPONSE_BYTES: usize = 256 * 1024 * 1024;
/// Streams of the sync protocol open at the same time on one connection, in both directions (D-132; libp2p's default
/// is 100): an honest peer has one request in flight, this node one of its own.
const MAX_SYNC_STREAMS: usize = 8;
/// When connections are being refused (connection limits, banned addresses), log at most once per this interval.
const REFUSAL_LOG_INTERVAL: Duration = Duration::from_secs(10);
/// The identify protocol version.
const IDENTIFY_PROTOCOL: &str = "/ksana/id/1";
/// Minimum timer period (`tokio::time::interval` does not accept 0).
const MIN_TICK: Duration = Duration::from_millis(10);
/// Maximum number of characters of a Handler-given reason kept in the log.
const MAX_REASON_CHARS: usize = 200;

/// Whether the current thread belongs to the network layer (the network thread or a thread of its runtime).
pub(crate) fn on_network_thread() -> bool {
    std::thread::current()
        .name()
        .is_some_and(|name| name.starts_with(THREAD_NAME))
}

/// Snapshots read by the public methods, maintained by the event loop.
#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) connected: Mutex<BTreeSet<PeerId>>,
    pub(crate) gossip_peers: Mutex<BTreeSet<PeerId>>,
    pub(crate) listen_addrs: Mutex<Vec<Multiaddr>>,
}

/// A gossip topic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TopicKind {
    Block,
    Op,
}

impl fmt::Display for TopicKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TopicKind::Block => "block",
            TopicKind::Op => "op",
        })
    }
}

/// Why a sync request failed.
#[derive(Debug)]
pub(crate) enum RequestError {
    /// The session has ended (the peer disconnected, reconnected, or the network layer stopped): the task should exit.
    Gone,
    /// This request failed (timeout, the peer closed the stream, etc.); the session continues.
    Failed(String),
}

/// Commands sent to the event loop: from the public methods and the internal tasks.
pub(crate) enum Command {
    Publish {
        kind: TopicKind,
        data: Vec<u8>,
    },
    Dial(Multiaddr),
    Block(PeerId),
    Unblock(PeerId),
    RequestSync,
    Stop,
    /// A sync request from a session task; when `session` is not the peer's current session, [`RequestError::Gone`]
    /// is replied right away.
    Request {
        peer: PeerId,
        session: u64,
        request: Request,
        reply: oneshot::Sender<Result<Response, RequestError>>,
    },
    /// The response to an inbound request and its encoded length, with the place it was served in.
    Respond {
        request_id: InboundRequestId,
        channel: ResponseChannel<Response>,
        response: Response,
        bytes: usize,
        permit: OwnedSemaphorePermit,
    },
    /// The verdict on a gossip message (`None`: the network layer is stopping or the Handler panicked; treated as
    /// Ignore).
    Verdict {
        kind: TopicKind,
        id: MessageId,
        from: PeerId,
        source: Source,
        verdict: Option<Verdict>,
        size: usize,
    },
    Penalize {
        peer: PeerId,
        reason: String,
    },
    GenesisMismatch {
        peer: PeerId,
        genesis: [u8; 32],
    },
}

#[derive(NetworkBehaviour)]
struct Behaviour {
    // Placed first: rejected peers and connections over the limits are refused before the other protocols allocate
    // resources.
    block_list: allow_block_list::Behaviour<allow_block_list::BlockedPeers>,
    limits: connection_limits::Behaviour,
    gate: Gate,
    identify: identify::Behaviour,
    ping: ping::Behaviour,
    gossipsub: gossipsub::Behaviour,
    sync: request_response::Behaviour<ScoringCodec>,
}

/// The session of a connected peer.
struct PeerEntry {
    session: u64,
    /// Wakes the session task to ask for Status again (`true`: re-sync even when the peer's tip is unchanged).
    /// Dropping it ends the session.
    wake: mpsc::UnboundedSender<bool>,
    /// Where its messages and requests are counted: the address of its first connection (D-132).
    source: Source,
}

/// A gossip topic's validation queue and the bytes its messages hold until their verdicts (D-132).
struct GossipQueue {
    sender: mpsc::UnboundedSender<GossipItem>,
    bytes: Budget,
}

/// A ban by score: until when, and the address banned with it (a peer connected from a public address, D-132).
struct Ban {
    until: Instant,
    group: Option<IpGroup>,
}

/// Refusals counted between two log lines.
#[derive(Default)]
struct Refusals {
    count: u64,
    logged: Option<Instant>,
}

impl Refusals {
    /// Count one; true when a log line is due (and start counting again).
    fn due(&mut self, now: Instant) -> bool {
        self.count = self.count.saturating_add(1);
        let due = self
            .logged
            .is_none_or(|at| now.saturating_duration_since(at) >= REFUSAL_LOG_INTERVAL);
        if due {
            self.logged = Some(now);
        }
        due
    }

    fn take(&mut self) -> u64 {
        std::mem::take(&mut self.count)
    }
}

/// A static bootstrap peer.
///
/// When the address contains `/p2p/<peer id>`, dial by peer id; when it does not (e.g. devnet's
/// `/dns4/node2/tcp/9000`), dial by address every time and remember the peer most recently connected through that
/// address: no redial while it is still connected or rejected. A peer that changed its key (data directory reset) can
/// reconnect as well.
struct Bootstrap {
    addr: Multiaddr,
    /// The /p2p part of the address.
    configured: Option<PeerId>,
    /// The peer most recently connected through this address (only for addresses without /p2p).
    learned: Option<PeerId>,
    /// The dial in progress for an address without /p2p.
    dialing: Option<ConnectionId>,
}

/// Network thread entry point: create the runtime and run the event loop; `ready` reports the startup result.
pub(crate) fn run_thread(
    config: P2pConfig,
    tuning: Tuning,
    handler: Arc<dyn Handler>,
    shared: Arc<Shared>,
    commands: mpsc::UnboundedSender<Command>,
    command_rx: mpsc::UnboundedReceiver<Command>,
    ready: std::sync::mpsc::Sender<Result<(), P2pError>>,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name(format!("{THREAD_NAME}-rt"))
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            let _ = ready.send(Err(P2pError::Init(format!(
                "cannot create the tokio runtime: {e}"
            ))));
            return;
        }
    };
    runtime.block_on(run(
        config, tuning, handler, shared, commands, command_rx, ready,
    ));
    // Drop the runtime: cancels all async tasks and waits for the Handler calls in progress on the blocking thread
    // pool to return (calls not yet started are cancelled).
    drop(runtime);
}

async fn run(
    config: P2pConfig,
    tuning: Tuning,
    handler: Arc<dyn Handler>,
    shared: Arc<Shared>,
    commands: mpsc::UnboundedSender<Command>,
    command_rx: mpsc::UnboundedReceiver<Command>,
    ready: std::sync::mpsc::Sender<Result<(), P2pError>>,
) {
    let mut worker = match Worker::new(config, tuning, handler, shared, commands) {
        Ok(worker) => worker,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    if let Err(e) = worker.bind().await {
        worker.shutdown();
        let _ = ready.send(Err(e));
        return;
    }
    if ready.send(Ok(())).is_err() {
        worker.shutdown();
        return;
    }
    worker.redial();
    worker.event_loop(command_rx).await;
    worker.shutdown();
}

struct Worker {
    swarm: Swarm<Behaviour>,
    ctx: TaskCtx,
    shared: Arc<Shared>,
    tuning: Tuning,
    local_peer_id: PeerId,
    listen: Vec<Multiaddr>,
    /// Number of addresses each listener has reported.
    listeners: Vec<(ListenerId, usize)>,
    listen_failure: Option<String>,
    blocks_topic: TopicHash,
    ops_topic: TopicHash,
    block_queue: GossipQueue,
    op_queue: GossipQueue,
    /// The network's `MAX_OP_BYTES` (D-132).
    max_op_bytes: usize,
    peers: BTreeMap<PeerId, PeerEntry>,
    next_session: u64,
    pending: BTreeMap<OutboundRequestId, oneshot::Sender<Result<Response, RequestError>>>,
    bootstrap: Vec<Bootstrap>,
    /// Permanently rejected (block_peer, genesis mismatch) until unblock_peer.
    blocked: BTreeSet<PeerId>,
    /// Banned by score.
    bans: BTreeMap<PeerId, Ban>,
    /// The addresses banned with a peer (D-132); the value is the expiry time.
    ip_bans: BTreeMap<IpGroup, Instant>,
    scores: BTreeMap<PeerId, u32>,
    gossip_peers: BTreeSet<PeerId>,
    /// Token buckets per source (D-059, D-132).
    limiter: Limiter,
    /// The inbound sync requests being served (D-132).
    serving: Serving<InboundRequestId>,
    /// Places for the Handler calls serving inbound requests ([`MAX_CONCURRENT_SERVES`]).
    serve_permits: Arc<Semaphore>,
    /// Inbound connections refused (connection limits, banned addresses) and sync requests dropped over the limits
    /// since the last log line.
    refused_connections: Refusals,
    dropped_requests: Refusals,
}

impl Worker {
    fn new(
        config: P2pConfig,
        tuning: Tuning,
        handler: Arc<dyn Handler>,
        shared: Arc<Shared>,
        commands: mpsc::UnboundedSender<Command>,
    ) -> Result<Worker, P2pError> {
        let P2pConfig {
            keypair,
            listen,
            peers,
            genesis,
            max_op_bytes,
        } = config;
        let local_peer_id = keypair.public().to_peer_id();
        let blocks_topic = IdentTopic::new(wire::BLOCKS_TOPIC);
        let ops_topic = IdentTopic::new(wire::OPS_TOPIC);
        let swarm = build_swarm(
            keypair,
            &Topics {
                blocks: &blocks_topic,
                ops: &ops_topic,
                max_op_bytes,
            },
            tuning.request_timeout,
            &tuning.connections,
        )?;

        let ctx = TaskCtx::new(
            handler,
            commands,
            Arc::new(AtomicBool::new(false)),
            genesis,
            max_op_bytes,
        );
        let (block_queue, block_rx) = mpsc::unbounded_channel();
        let (op_queue, op_rx) = mpsc::unbounded_channel();
        tokio::spawn(session::validate_gossip(
            ctx.clone(),
            TopicKind::Block,
            block_rx,
        ));
        tokio::spawn(session::validate_gossip(ctx.clone(), TopicKind::Op, op_rx));

        let bootstrap = peers
            .into_iter()
            .map(|addr| Bootstrap {
                configured: peer_id_of(&addr),
                addr,
                learned: None,
                dialing: None,
            })
            .collect();
        let limiter = Limiter::new(tuning.peer_limits.clone());
        Ok(Worker {
            swarm,
            ctx,
            shared,
            tuning,
            local_peer_id,
            listen,
            listeners: Vec::new(),
            listen_failure: None,
            blocks_topic: blocks_topic.hash(),
            ops_topic: ops_topic.hash(),
            block_queue: GossipQueue {
                sender: block_queue,
                bytes: Budget::new(
                    MAX_QUEUED_BLOCK_BYTES,
                    MAX_QUEUED_BLOCK_BYTES / SOURCE_SHARE,
                ),
            },
            op_queue: GossipQueue {
                sender: op_queue,
                bytes: Budget::new(MAX_QUEUED_OP_BYTES, MAX_QUEUED_OP_BYTES / SOURCE_SHARE),
            },
            max_op_bytes,
            peers: BTreeMap::new(),
            next_session: 0,
            pending: BTreeMap::new(),
            bootstrap,
            blocked: BTreeSet::new(),
            bans: BTreeMap::new(),
            ip_bans: BTreeMap::new(),
            scores: BTreeMap::new(),
            gossip_peers: BTreeSet::new(),
            limiter,
            serving: Serving::new(
                MAX_REQUESTS_PER_SOURCE,
                Budget::new(MAX_RESPONSE_BYTES, MAX_RESPONSE_BYTES / SOURCE_SHARE),
            ),
            serve_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_SERVES)),
            refused_connections: Refusals::default(),
            dropped_requests: Refusals::default(),
        })
    }

    /// Listen on the configured addresses and wait until every listener has reported at least one address.
    async fn bind(&mut self) -> Result<(), P2pError> {
        for addr in self.listen.clone() {
            let id = self
                .swarm
                .listen_on(addr.clone())
                .map_err(|e| P2pError::Listen(format!("{addr}: {e}")))?;
            self.listeners.push((id, 0));
        }
        let wait = async {
            loop {
                if let Some(failure) = self.listen_failure.take() {
                    return Err(P2pError::Listen(failure));
                }
                if self.listeners.iter().all(|(_, count)| *count > 0) {
                    return Ok(());
                }
                let event = self.swarm.select_next_some().await;
                self.on_swarm_event(event);
            }
        };
        tokio::time::timeout(LISTEN_TIMEOUT, wait)
            .await
            .unwrap_or_else(|_| {
                Err(P2pError::Listen(
                    "timed out waiting for the listen addresses".to_string(),
                ))
            })
    }

    async fn event_loop(&mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        // The first tick is delayed by one period: Status was already asked when the connections were established,
        // and the bootstrap peers were already dialed.
        let mut status_tick = ticker(self.tuning.status_interval);
        let mut maintenance_tick = ticker(self.tuning.redial_interval);
        loop {
            tokio::select! {
                event = self.swarm.select_next_some() => self.on_swarm_event(event),
                command = commands.recv() => match command {
                    Some(Command::Stop) | None => return,
                    Some(command) => self.on_command(command),
                },
                _ = status_tick.tick() => self.wake_sessions(false),
                _ = maintenance_tick.tick() => {
                    self.expire_bans();
                    self.limiter.prune(now());
                    self.redial();
                }
            }
        }
    }

    fn shutdown(&mut self) {
        // Sync / entry pool loops in progress stop before the next block / op; Handler calls not yet started are not
        // made.
        self.ctx.set_stopping();
        self.peers.clear();
        for (_, reply) in std::mem::take(&mut self.pending) {
            let _ = reply.send(Err(RequestError::Gone));
        }
    }

    fn on_swarm_event(&mut self, event: SwarmEvent<BehaviourEvent>) {
        match event {
            SwarmEvent::Behaviour(event) => self.on_behaviour_event(event),
            SwarmEvent::ConnectionEstablished {
                peer_id,
                connection_id,
                endpoint,
                num_established,
                ..
            } => {
                for entry in self.bootstrap.iter_mut() {
                    if entry.dialing == Some(connection_id) {
                        entry.dialing = None;
                        entry.learned = Some(peer_id);
                    }
                }
                if num_established.get() == 1 {
                    self.on_peer_connected(peer_id, endpoint.get_remote_address());
                }
            }
            SwarmEvent::IncomingConnectionError {
                send_back_addr,
                error: ListenError::Denied { cause },
                ..
            } => {
                // Connection limits or a banned address (D-132): logged at an interval, the refusals in between
                // counted
                if self.refused_connections.due(now()) {
                    eprintln!(
                        "[ksana-p2p] refusing inbound connections ({} since the last report; the latest from \
                         {send_back_addr}: {cause}, D-132)",
                        self.refused_connections.take()
                    );
                }
            }
            SwarmEvent::ConnectionClosed {
                peer_id,
                num_established,
                ..
            } => {
                if num_established == 0 {
                    self.on_peer_disconnected(peer_id);
                }
            }
            SwarmEvent::OutgoingConnectionError { connection_id, .. } => {
                // Dial failures (peer not started, connection refused, etc.) are not logged: bootstrap peers are
                // redialed every few seconds.
                for entry in self.bootstrap.iter_mut() {
                    if entry.dialing == Some(connection_id) {
                        entry.dialing = None;
                    }
                }
            }
            SwarmEvent::NewListenAddr {
                listener_id,
                address,
            } => {
                eprintln!(
                    "[ksana-p2p] listening on {address}/p2p/{}",
                    self.local_peer_id
                );
                lock(&self.shared.listen_addrs).push(address);
                if let Some((_, count)) =
                    self.listeners.iter_mut().find(|(id, _)| *id == listener_id)
                {
                    *count = count.saturating_add(1);
                }
            }
            SwarmEvent::ExpiredListenAddr { address, .. } => {
                lock(&self.shared.listen_addrs).retain(|a| *a != address);
            }
            SwarmEvent::ListenerClosed {
                listener_id,
                addresses,
                reason,
            } => {
                lock(&self.shared.listen_addrs).retain(|a| !addresses.contains(a));
                let reason = match reason {
                    Ok(()) => "closed".to_string(),
                    Err(e) => e.to_string(),
                };
                eprintln!("[ksana-p2p] listener closed: {reason}");
                self.listener_failed(listener_id, reason);
            }
            SwarmEvent::ListenerError { listener_id, error } => {
                eprintln!("[ksana-p2p] listener error: {error}");
                self.listener_failed(listener_id, error.to_string());
            }
            _ => {}
        }
    }

    /// A listener failed during startup before it had an address: startup fails.
    fn listener_failed(&mut self, listener_id: ListenerId, reason: String) {
        if self
            .listeners
            .iter()
            .any(|(id, count)| *id == listener_id && *count == 0)
        {
            self.listen_failure = Some(reason);
        }
    }

    fn on_peer_connected(&mut self, peer: PeerId, addr: &Multiaddr) {
        eprintln!("[ksana-p2p] connected to {peer} ({addr})");
        self.next_session = self.next_session.saturating_add(1);
        let session = self.next_session;
        let (wake, wake_rx) = mpsc::unbounded_channel();
        let source = Source::of(peer, addr);
        self.peers.insert(
            peer,
            PeerEntry {
                session,
                wake,
                source,
            },
        );
        lock(&self.shared.connected).insert(peer);
        tokio::spawn(session::run_session(
            self.ctx.clone(),
            peer,
            session,
            wake_rx,
        ));
    }

    fn on_peer_disconnected(&mut self, peer: PeerId) {
        eprintln!("[ksana-p2p] disconnected from {peer}");
        // Dropping the wake channel ends the session task; the request it is waiting on is ended by request-response
        // as a failure.
        // The source's token buckets stay: they are its address's, and a reconnection finds them as they were (D-132)
        self.peers.remove(&peer);
        lock(&self.shared.connected).remove(&peer);
        if self.gossip_peers.remove(&peer) {
            self.publish_gossip_peers();
        }
    }

    fn on_behaviour_event(&mut self, event: BehaviourEvent) {
        match event {
            BehaviourEvent::BlockList(never)
            | BehaviourEvent::Limits(never)
            | BehaviourEvent::Gate(never) => match never {},
            BehaviourEvent::Identify(_) => {}
            // A ping failure (timeout, etc.) does not disconnect automatically: when the peer is already unreachable
            // (e.g. the network was partitioned without a FIN arriving) close the connection ourselves, and bootstrap
            // peers get redialed afterwards. A peer not supporting ping does not count as a failure.
            BehaviourEvent::Ping(ping::Event {
                peer,
                connection,
                result: Err(failure),
            }) if !matches!(failure, ping::Failure::Unsupported) => {
                eprintln!("[ksana-p2p] ping to {peer} failed ({failure}), closing the connection");
                self.swarm.close_connection(connection);
            }
            BehaviourEvent::Ping(_) => {}
            BehaviourEvent::Gossipsub(event) => self.on_gossipsub_event(event),
            BehaviourEvent::Sync(event) => self.on_sync_event(event),
        }
    }

    fn on_gossipsub_event(&mut self, event: gossipsub::Event) {
        match event {
            gossipsub::Event::Message {
                propagation_source,
                message_id,
                message,
            } => self.on_gossip_message(propagation_source, message_id, message),
            gossipsub::Event::Subscribed { peer_id, topic } if topic == self.blocks_topic => {
                self.gossip_peers.insert(peer_id);
                self.publish_gossip_peers();
            }
            gossipsub::Event::Unsubscribed { peer_id, topic } if topic == self.blocks_topic => {
                self.gossip_peers.remove(&peer_id);
                self.publish_gossip_peers();
            }
            _ => {}
        }
    }

    fn publish_gossip_peers(&self) {
        *lock(&self.shared.gossip_peers) = self.gossip_peers.clone();
    }

    /// Hands the banned and blocked peers to the gossip validation tasks, which drop the messages of theirs still
    /// queued (M26). Called whenever `bans` or `blocked` changes.
    fn publish_rejected(&self) {
        let rejected = self
            .blocked
            .iter()
            .chain(self.bans.keys())
            .copied()
            .collect();
        self.ctx.set_rejected(rejected);
    }

    /// The source a connected peer's messages and requests are counted under (D-132): the address of its first
    /// connection; the peer itself when it is not connected (any more).
    fn source_of(&self, peer: &PeerId) -> Source {
        self.peers
            .get(peer)
            .map_or(Source::Peer(*peer), |entry| entry.source)
    }

    fn queue(&mut self, kind: TopicKind) -> &mut GossipQueue {
        match kind {
            TopicKind::Block => &mut self.block_queue,
            TopicKind::Op => &mut self.op_queue,
        }
    }

    /// A gossip message arrived: hand it to the topic's validation queue (handed to the Handler one by one in arrival
    /// order).
    fn on_gossip_message(&mut self, from: PeerId, id: MessageId, message: gossipsub::Message) {
        let kind = if message.topic == self.blocks_topic {
            TopicKind::Block
        } else if message.topic == self.ops_topic {
            TopicKind::Op
        } else {
            self.report(&id, &from, MessageAcceptance::Ignore);
            return;
        };
        let limited = match kind {
            TopicKind::Block => Limited::Block,
            TopicKind::Op => Limited::Op,
        };
        let source = self.source_of(&from);
        if !self.limiter.allow(source, limited, 1, now()) {
            // Over the source's quota (D-059): not decoded, not forwarded, no score
            self.report(&id, &from, MessageAcceptance::Ignore);
            return;
        }
        let size = message.data.len();
        if kind == TopicKind::Op && size > self.max_op_bytes {
            // No op this long can be valid (MAX_OP_BYTES, D-132): rejected without decoding
            self.report(&id, &from, MessageAcceptance::Reject);
            let max = self.max_op_bytes;
            self.penalize(
                from,
                &format!("gossip op of {size} bytes exceeds MAX_OP_BYTES ({max})"),
            );
            return;
        }
        let queue = self.queue(kind);
        if !queue.bytes.take(source, size) {
            // Too much validation backlog in the topic, or from this source (D-132): not forwarded, no score; sync
            // fills in later.
            self.report(&id, &from, MessageAcceptance::Ignore);
            return;
        }
        if let Err(mpsc::error::SendError(item)) = queue.sender.send(GossipItem {
            id,
            from,
            source,
            data: message.data,
        }) {
            queue.bytes.give(&source, size);
            self.report(&item.id, &item.from, MessageAcceptance::Ignore);
        }
    }

    fn on_verdict(
        &mut self,
        kind: TopicKind,
        id: MessageId,
        from: PeerId,
        source: Source,
        verdict: Option<Verdict>,
        size: usize,
    ) {
        self.queue(kind).bytes.give(&source, size);
        let acceptance = match &verdict {
            Some(Verdict::Accept) => MessageAcceptance::Accept,
            Some(Verdict::Reject(_)) => MessageAcceptance::Reject,
            Some(Verdict::Ignore) | Some(Verdict::MissingParent) | None => {
                MessageAcceptance::Ignore
            }
        };
        self.report(&id, &from, acceptance);
        match verdict {
            Some(Verdict::Reject(reason)) => {
                self.penalize(from, &format!("invalid gossip {kind}: {reason}"))
            }
            Some(Verdict::MissingParent) => self.wake(from, false),
            _ => {}
        }
    }

    fn report(&mut self, id: &MessageId, from: &PeerId, acceptance: MessageAcceptance) {
        // A false return means the message is no longer in the cache (validation was too slow); it is then not
        // forwarded and nothing needs doing.
        let _ = self
            .swarm
            .behaviour_mut()
            .gossipsub
            .report_message_validation_result(id, from, acceptance);
    }

    fn on_sync_event(&mut self, event: request_response::Event<RequestFrame, Response>) {
        match event {
            request_response::Event::Message { peer, message, .. } => match message {
                request_response::Message::Request {
                    request_id,
                    request: RequestFrame::Valid(request),
                    channel,
                } => {
                    // Over the source's quota (D-059), or the source already has its most requests in flight
                    // (D-132): drop the channel (the peer sees a failure), no score
                    let source = self.source_of(&peer);
                    if !self
                        .limiter
                        .allow(source, Limited::Request, request_cost(&request), now())
                    {
                        return;
                    }
                    if !self.serving.admit(request_id, source) {
                        self.dropped_request(
                            source,
                            &format!("{MAX_REQUESTS_PER_SOURCE} requests in flight"),
                        );
                        return;
                    }
                    self.serve(request_id, request, channel);
                }
                // Drop the channel: the peer sees a failure.
                request_response::Message::Request {
                    request: RequestFrame::Invalid(reason),
                    ..
                } => self.penalize(peer, &format!("invalid sync request: {reason}")),
                request_response::Message::Response {
                    request_id,
                    response,
                } => {
                    if let Some(reply) = self.pending.remove(&request_id) {
                        let _ = reply.send(Ok(response));
                    }
                }
            },
            request_response::Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            } => {
                let malformed = matches!(&error, OutboundFailure::Io(e) if e.kind() == io::ErrorKind::InvalidData);
                let message = error.to_string();
                if let Some(reply) = self.pending.remove(&request_id) {
                    let _ = reply.send(Err(RequestError::Failed(message.clone())));
                }
                if malformed {
                    self.penalize(peer, &format!("invalid sync response: {message}"));
                }
            }
            // Inbound request timeout, peer closed early, etc.: no score (undecodable requests are handled above). Either
            // way the request is no longer in flight (D-132).
            request_response::Event::InboundFailure { request_id, .. }
            | request_response::Event::ResponseSent { request_id, .. } => {
                self.serving.finish(&request_id)
            }
        }
    }

    /// An inbound request: call the Handler on the blocking thread pool, at most [`MAX_CONCURRENT_SERVES`] at a time
    /// (D-132), and send the result and its length back to the event loop for sending. The place is held until the
    /// event loop has counted the response's bytes, so that no more responses than that wait to be counted.
    fn serve(
        &self,
        request_id: InboundRequestId,
        request: Request,
        channel: ResponseChannel<Response>,
    ) {
        let ctx = self.ctx.clone();
        let permits = Arc::clone(&self.serve_permits);
        tokio::spawn(async move {
            let Ok(permit) = permits.acquire_owned().await else {
                return;
            };
            // The request failed while it waited (timed out, its connection closed): nothing to serve
            if !channel.is_open() {
                return;
            }
            // When no result can be had (stopping, or the Handler panicked) the channel is dropped and the peer sees a
            // failure.
            let served = ctx
                .blocking(move |h| {
                    let response = wire::serve(h, request);
                    let bytes = borsh::object_length(&response).unwrap_or(usize::MAX);
                    (response, bytes)
                })
                .await;
            if let Some((response, bytes)) = served {
                ctx.send(Command::Respond {
                    request_id,
                    channel,
                    response,
                    bytes,
                    permit,
                });
            }
        });
    }

    /// A response is ready: send it when its bytes fit the budgets of the responses being sent (D-132), otherwise drop
    /// it (the peer sees a failure and asks again later).
    fn respond(
        &mut self,
        request_id: InboundRequestId,
        channel: ResponseChannel<Response>,
        response: Response,
        bytes: usize,
    ) {
        match self.serving.reserve(request_id, bytes) {
            Reserve::Fits => {
                // A failure means the request timed out or the connection is closed; its InboundFailure follows.
                if self
                    .swarm
                    .behaviour_mut()
                    .sync
                    .send_response(channel, response)
                    .is_err()
                {
                    self.serving.finish(&request_id);
                }
            }
            // The request failed meanwhile (timeout, connection closed)
            Reserve::Gone => {}
            Reserve::Full => {
                if let Some(source) = self.serving.source(&request_id) {
                    self.dropped_request(source, &format!("a response of {bytes} bytes"));
                }
                self.serving.finish(&request_id);
            }
        }
    }

    /// An inbound request dropped over the serving limits (D-132): logged at an interval.
    fn dropped_request(&mut self, source: Source, why: &str) {
        if self.dropped_requests.due(now()) {
            eprintln!(
                "[ksana-p2p] dropping sync requests ({} since the last report; the latest from {source}: {why}; {} \
                 requests in flight, {} bytes of responses being sent, D-132)",
                self.dropped_requests.take(),
                self.serving.len(),
                self.serving.bytes()
            );
        }
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::Publish { kind, data } => self.publish(kind, data),
            Command::Dial(addr) => self.dial(addr),
            Command::Block(peer) => self.block(peer),
            Command::Unblock(peer) => self.unblock(peer),
            Command::RequestSync => self.wake_sessions(true),
            // Handled in event_loop.
            Command::Stop => {}
            Command::Request {
                peer,
                session,
                request,
                reply,
            } => match self.peers.get(&peer) {
                Some(entry) if entry.session == session => {
                    let id = self
                        .swarm
                        .behaviour_mut()
                        .sync
                        .send_request(&peer, RequestFrame::Valid(request));
                    self.pending.insert(id, reply);
                }
                _ => {
                    let _ = reply.send(Err(RequestError::Gone));
                }
            },
            Command::Respond {
                request_id,
                channel,
                response,
                bytes,
                permit,
            } => {
                self.respond(request_id, channel, response, bytes);
                drop(permit);
            }
            Command::Verdict {
                kind,
                id,
                from,
                source,
                verdict,
                size,
            } => self.on_verdict(kind, id, from, source, verdict, size),
            Command::Penalize { peer, reason } => self.penalize(peer, &reason),
            Command::GenesisMismatch { peer, genesis } => {
                eprintln!(
                    "[ksana-p2p] genesis block {} of peer {peer} does not match this network: disconnecting and rejecting",
                    hex::encode(genesis)
                );
                self.blocked.insert(peer);
                self.publish_rejected();
                self.swarm.behaviour_mut().block_list.block_peer(peer);
            }
        }
    }

    fn publish(&mut self, kind: TopicKind, data: Vec<u8>) {
        let topic = match kind {
            TopicKind::Block => self.blocks_topic.clone(),
            TopicKind::Op => self.ops_topic.clone(),
        };
        match self.swarm.behaviour_mut().gossipsub.publish(topic, data) {
            // Neither a duplicate (already broadcast or received from elsewhere) nor the lack of subscribers is an
            // error: peers obtain it through sync.
            Ok(_) | Err(PublishError::Duplicate) | Err(PublishError::NoPeersSubscribedToTopic) => {}
            Err(e) => eprintln!("[ksana-p2p] broadcasting {kind} failed: {e}"),
        }
    }

    fn dial(&mut self, addr: Multiaddr) {
        let opts = match peer_id_of(&addr) {
            Some(peer) if peer == self.local_peer_id => return,
            Some(peer) => DialOpts::peer_id(peer)
                .addresses(vec![addr.clone()])
                .condition(PeerCondition::DisconnectedAndNotDialing)
                .build(),
            None => DialOpts::unknown_peer_id().address(addr.clone()).build(),
        };
        match self.swarm.dial(opts) {
            Ok(()) | Err(DialError::DialPeerConditionFalse(_)) => {}
            Err(e) => eprintln!("[ksana-p2p] dialing {addr} failed: {e}"),
        }
    }

    fn block(&mut self, peer: PeerId) {
        if self.blocked.insert(peer) {
            eprintln!("[ksana-p2p] isolating peer {peer}");
        }
        self.publish_rejected();
        self.swarm.behaviour_mut().block_list.block_peer(peer);
    }

    fn unblock(&mut self, peer: PeerId) {
        let was_blocked = self.blocked.remove(&peer);
        let ban = self.bans.remove(&peer);
        let was_banned = ban.is_some();
        // The address banned with the peer is lifted with it, unless another peer banned from there holds it
        if let Some(group) = ban.and_then(|ban| ban.group) {
            if !self.bans.values().any(|other| other.group == Some(group)) {
                self.ip_bans.remove(&group);
                self.swarm.behaviour_mut().gate.unban(&group);
            }
        }
        self.scores.remove(&peer);
        self.publish_rejected();
        self.swarm.behaviour_mut().block_list.unblock_peer(peer);
        if was_blocked || was_banned {
            eprintln!("[ksana-p2p] restoring peer {peer}");
        }
        self.redial();
    }

    /// Score 1 point against the peer; at the threshold disconnect and ban it.
    fn penalize(&mut self, peer: PeerId, reason: &str) {
        if self.blocked.contains(&peer) || self.bans.contains_key(&peer) {
            return;
        }
        let score = self.scores.entry(peer).or_insert(0);
        *score = score.saturating_add(1);
        let score = *score;
        let reason: String = reason.chars().take(MAX_REASON_CHARS).collect();
        eprintln!(
            "[ksana-p2p] peer {peer} penalized 1 point ({score}/{}): {reason}",
            self.tuning.ban_threshold
        );
        if score < self.tuning.ban_threshold {
            return;
        }
        self.scores.remove(&peer);
        // A peer connected from a public address bans the address too (D-132): another identity from there is not
        // taken in while the ban lasts
        let group = self.source_of(&peer).ip();
        match now().checked_add(self.tuning.ban_duration) {
            Some(until) => {
                self.bans.insert(peer, Ban { until, group });
                if let Some(group) = group {
                    let expiry = self.ip_bans.entry(group).or_insert(until);
                    *expiry = (*expiry).max(until);
                    self.swarm.behaviour_mut().gate.ban(group);
                }
            }
            // The duration is not representable: treat as permanent rejection.
            None => {
                self.blocked.insert(peer);
            }
        }
        self.publish_rejected();
        let address = group.map_or(String::new(), |group| format!(" and its address {group}"));
        eprintln!(
            "[ksana-p2p] disconnecting and banning peer {peer}{address} ({} s)",
            self.tuning.ban_duration.as_secs()
        );
        self.swarm.behaviour_mut().block_list.block_peer(peer);
    }

    /// Lift expired bans (the score was already cleared when banning).
    fn expire_bans(&mut self) {
        let now = now();
        let lifted: Vec<IpGroup> = self
            .ip_bans
            .iter()
            .filter(|(_, until)| **until <= now)
            .map(|(group, _)| *group)
            .collect();
        for group in lifted {
            self.ip_bans.remove(&group);
            eprintln!("[ksana-p2p] ban of address {group} expired");
            self.swarm.behaviour_mut().gate.unban(&group);
        }
        let expired: Vec<PeerId> = self
            .bans
            .iter()
            .filter(|(_, ban)| ban.until <= now)
            .map(|(peer, _)| *peer)
            .collect();
        if expired.is_empty() {
            return;
        }
        for peer in expired {
            self.bans.remove(&peer);
            if !self.blocked.contains(&peer) {
                eprintln!("[ksana-p2p] ban of peer {peer} expired");
                self.swarm.behaviour_mut().block_list.unblock_peer(peer);
            }
        }
        self.publish_rejected();
    }

    /// Dial the bootstrap peers that are neither connected nor rejected.
    fn redial(&mut self) {
        let mut dials = Vec::new();
        for (index, entry) in self.bootstrap.iter().enumerate() {
            match entry.configured {
                Some(peer) => {
                    if !self.needs_no_dial(&peer) {
                        let opts = DialOpts::peer_id(peer)
                            .addresses(vec![entry.addr.clone()])
                            .condition(PeerCondition::DisconnectedAndNotDialing)
                            .build();
                        dials.push((None, opts));
                    }
                }
                None => {
                    if entry.dialing.is_none()
                        && !entry.learned.is_some_and(|peer| self.needs_no_dial(&peer))
                    {
                        let opts = DialOpts::unknown_peer_id()
                            .address(entry.addr.clone())
                            .build();
                        dials.push((Some(index), opts));
                    }
                }
            }
        }
        for (index, opts) in dials {
            let id = opts.connection_id();
            // Already dialing and similar cases return an error; try again next round.
            if self.swarm.dial(opts).is_ok() {
                if let Some(entry) = index.and_then(|i| self.bootstrap.get_mut(i)) {
                    entry.dialing = Some(id);
                }
            }
        }
    }

    /// Connected, rejected, or this node itself: no dial needed.
    fn needs_no_dial(&self, peer: &PeerId) -> bool {
        *peer == self.local_peer_id
            || self.blocked.contains(peer)
            || self.bans.contains_key(peer)
            || self.swarm.is_connected(peer)
    }

    fn wake(&self, peer: PeerId, force: bool) {
        if let Some(entry) = self.peers.get(&peer) {
            let _ = entry.wake.send(force);
        }
    }

    fn wake_sessions(&self, force: bool) {
        for entry in self.peers.values() {
            let _ = entry.wake.send(force);
        }
    }
}

/// TCP + Noise + Yamux, plus DNS: `/dns4` and `/dns` addresses (devnet bootstraps by service name) are resolved with
/// the system resolver on every dial.
/// When the system resolver configuration cannot be read, fall back to a transport supporting only IP addresses (and
/// log it) rather than failing startup.
fn build_swarm(
    keypair: identity::Keypair,
    topics: &Topics<'_>,
    request_timeout: Duration,
    connections: &ConnectionLimits,
) -> Result<Swarm<Behaviour>, P2pError> {
    let tcp = || {
        SwarmBuilder::with_existing_identity(keypair.clone())
            .with_tokio()
            .with_tcp(
                tcp::Config::default().nodelay(true),
                noise::Config::new,
                yamux::Config::default,
            )
            .map_err(|e| P2pError::Init(format!("transport: {e}")))
    };
    let behaviour =
        |key: &identity::Keypair| build_behaviour(key, topics, request_timeout, connections);
    let protocol_err = |e: libp2p::BehaviourBuilderError| P2pError::Init(format!("protocol: {e}"));
    let swarm = match tcp()?.with_dns() {
        Ok(builder) => builder
            .with_behaviour(behaviour)
            .map_err(protocol_err)?
            .with_swarm_config(|c| c.with_idle_connection_timeout(IDLE_CONNECTION_TIMEOUT))
            .build(),
        Err(e) => {
            eprintln!("[ksana-p2p] cannot read the system DNS configuration ({e}), /dns addresses are unavailable");
            tcp()?
                .with_behaviour(behaviour)
                .map_err(protocol_err)?
                .with_swarm_config(|c| c.with_idle_connection_timeout(IDLE_CONNECTION_TIMEOUT))
                .build()
        }
    };
    Ok(swarm)
}

/// The two gossip topics and the network's `MAX_OP_BYTES`.
struct Topics<'a> {
    blocks: &'a IdentTopic,
    ops: &'a IdentTopic,
    max_op_bytes: usize,
}

fn build_behaviour(
    key: &identity::Keypair,
    topics: &Topics<'_>,
    request_timeout: Duration,
    connections: &ConnectionLimits,
) -> Result<Behaviour, Box<dyn std::error::Error + Send + Sync>> {
    let gossip_config = gossipsub::ConfigBuilder::default()
        .validate_messages()
        .max_transmit_size(wire::MAX_GOSSIP_BYTES.saturating_add(GOSSIP_FRAME_OVERHEAD))
        // An op message longer than any valid op is refused by gossipsub's codec, before it reaches the queue (D-132)
        .max_transmit_size_for_topic(
            topics.max_op_bytes.saturating_add(GOSSIP_MESSAGE_OVERHEAD),
            topics.ops.hash(),
        )
        .message_id_fn(content_message_id)
        .history_length(GOSSIP_HISTORY_LENGTH)
        .publish_queue_duration(GOSSIP_QUEUE_DURATION)
        .forward_queue_duration(GOSSIP_QUEUE_DURATION)
        .build()?;
    let mut gossipsub = gossipsub::Behaviour::new(
        gossipsub::MessageAuthenticity::Signed(key.clone()),
        gossip_config,
    )?;
    gossipsub.subscribe(topics.blocks)?;
    gossipsub.subscribe(topics.ops)?;
    let sync = request_response::Behaviour::with_codec(
        ScoringCodec::default(),
        [(wire::SYNC_PROTOCOL, ProtocolSupport::Full)],
        request_response::Config::default()
            .with_request_timeout(request_timeout)
            .with_max_concurrent_streams(MAX_SYNC_STREAMS),
    );
    let identify = identify::Behaviour::new(
        identify::Config::new(IDENTIFY_PROTOCOL.to_string(), key.public())
            .with_agent_version(format!("ksana-p2p/{}", env!("CARGO_PKG_VERSION"))),
    );
    let limits = connection_limits::ConnectionLimits::default()
        .with_max_established_incoming(Some(connections.max_inbound))
        .with_max_pending_incoming(Some(connections.max_pending_inbound))
        .with_max_established_per_peer(Some(connections.max_per_peer));
    Ok(Behaviour {
        block_list: allow_block_list::Behaviour::default(),
        limits: connection_limits::Behaviour::new(limits),
        gate: Gate::new(usize::try_from(connections.max_inbound_per_ip).unwrap_or(usize::MAX)),
        identify,
        ping: ping::Behaviour::default(),
        gossipsub,
        sync,
    })
}

/// Gossip message ID: `sha256(topic ‖ 0 ‖ sha256(content))`. Content-addressed: the same block / op is processed
/// only once no matter who published it, and a message this node published is dropped as a duplicate when it comes
/// back.
fn content_message_id(message: &gossipsub::Message) -> MessageId {
    let mut preimage = message.topic.as_str().as_bytes().to_vec();
    preimage.push(0);
    preimage.extend_from_slice(&sha256(&message.data));
    MessageId::from(sha256(&preimage).to_vec())
}

/// The `/p2p/<peer id>` in an address.
fn peer_id_of(addr: &Multiaddr) -> Option<PeerId> {
    addr.iter().find_map(|protocol| match protocol {
        multiaddr::Protocol::P2p(peer) => Some(peer),
        _ => None,
    })
}

fn ticker(period: Duration) -> Interval {
    let period = period.max(MIN_TICK);
    let mut interval = tokio::time::interval_at(next_tick(period), period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval
}

fn next_tick(period: Duration) -> Instant {
    let now = now();
    now.checked_add(period).unwrap_or(now)
}

/// The current monotonic time: used only for timers and ban expiry, never enters any block or state.
fn now() -> Instant {
    Instant::now()
}
