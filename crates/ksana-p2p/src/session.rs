//! The async tasks that call the [`Handler`]: one session task per connected peer (Status, genesis check, block sync,
//! entry pool), plus one validation task per gossip topic. The Handler's methods all run on the blocking thread pool,
//! and the results are sent back to the event loop through [`Command`].
//!
//! A session task runs sequentially, so a peer has at most one sync at a time.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ksana_primitives::Block;
use libp2p::gossipsub::MessageId;
use libp2p::PeerId;
use tokio::sync::{mpsc, oneshot};

use crate::limit::Source;
use crate::wire::{self, Request, Response, MAX_BLOCKS_PER_REQUEST, MAX_MEMPOOL_OPS};
use crate::worker::{Command, RequestError, TopicKind};
use crate::{lock, Handler, OpMessage, Status, Verdict};

/// A gossip message awaiting the Handler's verdict.
pub(crate) struct GossipItem {
    pub(crate) id: MessageId,
    pub(crate) from: PeerId,
    /// Where the bytes it holds in its queue are counted (D-132).
    pub(crate) source: Source,
    pub(crate) data: Vec<u8>,
}

/// Context shared by the tasks.
#[derive(Clone)]
pub(crate) struct TaskCtx {
    handler: Arc<dyn Handler>,
    commands: mpsc::UnboundedSender<Command>,
    stopping: Arc<AtomicBool>,
    genesis: [u8; 32],
    /// The network's `MAX_OP_BYTES`: the length limit of a gossip op (D-132).
    max_op_bytes: usize,
    /// The peers currently banned or blocked, as published by the event loop: their gossip still in the validation
    /// queues is dropped unvalidated.
    rejected: Arc<Mutex<BTreeSet<PeerId>>>,
}

impl TaskCtx {
    pub(crate) fn new(
        handler: Arc<dyn Handler>,
        commands: mpsc::UnboundedSender<Command>,
        stopping: Arc<AtomicBool>,
        genesis: [u8; 32],
        max_op_bytes: usize,
    ) -> TaskCtx {
        TaskCtx {
            handler,
            commands,
            stopping,
            genesis,
            max_op_bytes,
            rejected: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    pub(crate) fn send(&self, command: Command) {
        // Silently dropped when the event loop has exited.
        let _ = self.commands.send(command);
    }

    pub(crate) fn set_stopping(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Replaces the set of banned and blocked peers (called by the event loop whenever it changes).
    pub(crate) fn set_rejected(&self, peers: BTreeSet<PeerId>) {
        *lock(&self.rejected) = peers;
    }

    fn is_rejected(&self, peer: &PeerId) -> bool {
        lock(&self.rejected).contains(peer)
    }

    /// Call the Handler on the blocking thread pool. Returns `None` when the network layer is stopping or the Handler
    /// panicked.
    pub(crate) async fn blocking<R, F>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&dyn Handler) -> R + Send + 'static,
        R: Send + 'static,
    {
        if self.is_stopping() {
            return None;
        }
        let handler = Arc::clone(&self.handler);
        match tokio::task::spawn_blocking(move || f(handler.as_ref())).await {
            Ok(result) => Some(result),
            Err(e) => {
                if e.is_panic() {
                    eprintln!("[ksana-p2p] Handler call panicked");
                }
                None
            }
        }
    }

    /// Send a sync request to the peer through the event loop and wait for the response.
    async fn request(
        &self,
        peer: PeerId,
        session: u64,
        request: Request,
    ) -> Result<Response, RequestError> {
        if self.is_stopping() {
            return Err(RequestError::Gone);
        }
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Request {
                peer,
                session,
                request,
                reply,
            })
            .map_err(|_| RequestError::Gone)?;
        response.await.unwrap_or(Err(RequestError::Gone))
    }

    fn penalize(&self, peer: PeerId, reason: String) {
        self.send(Command::Penalize { peer, reason });
    }
}

/// Gossip validation task: decode the messages one by one in arrival order (checking the length limit first), hand
/// them to the Handler, and send the verdicts back to the event loop. The messages of a peer banned or blocked while
/// they were queued are dropped without decoding or validation (M26), like those beyond the queue limit (Ignore).
pub(crate) async fn validate_gossip(
    ctx: TaskCtx,
    kind: TopicKind,
    mut queue: mpsc::UnboundedReceiver<GossipItem>,
) {
    while let Some(GossipItem {
        id,
        from,
        source,
        data,
    }) = queue.recv().await
    {
        let size = data.len();
        let max_op_bytes = ctx.max_op_bytes;
        let verdict = if ctx.is_rejected(&from) {
            Some(Verdict::Ignore)
        } else {
            ctx.blocking(move |handler| judge_gossip(handler, kind, from, &data, max_op_bytes))
                .await
        };
        ctx.send(Command::Verdict {
            kind,
            id,
            from,
            source,
            verdict,
            size,
        });
    }
}

fn judge_gossip(
    handler: &dyn Handler,
    kind: TopicKind,
    from: PeerId,
    data: &[u8],
    max_op_bytes: usize,
) -> Verdict {
    match kind {
        TopicKind::Block => match wire::decode_block(data) {
            Ok(block) => handler.on_block(from, block, true),
            Err(e) => Verdict::Reject(format!("cannot decode: {e}")),
        },
        TopicKind::Op => match wire::decode_op(data, max_op_bytes) {
            Ok(msg) => handler.on_op(from, msg),
            Err(e) => Verdict::Reject(format!("cannot decode: {e}")),
        },
    }
}

/// Fetching blocks by hash: at most this many requests per sync (`GetBlock` counts 1 token, far below the peer's
/// request quota, D-059).
const MAX_FETCH_PER_SYNC: usize = 64;
/// Maximum number of rounds of fetching by hash (each round fetches the missing parents of orphans once).
const MAX_FETCH_ROUNDS: usize = 16;

/// The result of one sync.
enum SyncOutcome {
    /// The peer's main chain has no more chain work than ours, we have all the peer's tips, and we have no orphans: no
    /// sync.
    NotNeeded,
    /// Accepted blocks were received.
    Progress,
    /// The sync finished or stopped without any block being accepted: do not sync from this peer again while its
    /// status is unchanged.
    NoProgress,
    /// A request failed or the peer misbehaved (already penalized); retry as usual next time.
    Interrupted,
    /// The session has ended.
    Gone,
}

/// The sync-relevant part of the peer's status: while the tip and the DAG tips are unchanged, a previous sync that
/// yielded nothing is not repeated.
type Fingerprint = ([u8; 32], Vec<[u8; 32]>);

/// The session with one connected peer: ask for its Status when the connection is established and every time we are
/// woken; on a genesis mismatch have the event loop disconnect and reject the peer; sync blocks from it when needed
/// ([`sync_if_needed`]); fetch the peer's entry pool once after the first successful Status query.
pub(crate) async fn run_session(
    ctx: TaskCtx,
    peer: PeerId,
    session: u64,
    mut wake: mpsc::UnboundedReceiver<bool>,
) {
    let mut mempool_fetched = false;
    // The peer's status when the last sync from it yielded nothing: while unchanged, do not download again (except on
    // `request_sync`).
    let mut stale: Option<Fingerprint> = None;
    let mut force = false;
    loop {
        let status = match ctx.request(peer, session, Request::Status).await {
            Ok(Response::Status(status)) => Some(status),
            Ok(_) => {
                ctx.penalize(
                    peer,
                    "Status request got a response of another type".to_string(),
                );
                None
            }
            Err(RequestError::Gone) => return,
            Err(RequestError::Failed(_)) => None,
        };
        if let Some(mut status) = status {
            if status.genesis != ctx.genesis {
                ctx.send(Command::GenesisMismatch {
                    peer,
                    genesis: status.genesis,
                });
                return;
            }
            status.tips.truncate(wire::MAX_STATUS_TIPS);
            let fingerprint = (status.tip_hash, status.tips.clone());
            if force || stale.as_ref() != Some(&fingerprint) {
                match sync_if_needed(&ctx, peer, session, &status).await {
                    SyncOutcome::Gone => return,
                    SyncOutcome::NoProgress => stale = Some(fingerprint),
                    SyncOutcome::Progress | SyncOutcome::Interrupted => stale = None,
                    SyncOutcome::NotNeeded => {}
                }
            }
            if !mempool_fetched {
                mempool_fetched = true;
                if !fetch_mempool(&ctx, peer, session).await {
                    return;
                }
            }
        }
        // Wait for the next wake-up (periodic query, request_sync, gossip block with an unknown parent), merging
        // backlogged wake-ups.
        match wake.recv().await {
            Some(f) => force = f,
            None => return,
        }
        while let Ok(f) = wake.try_recv() {
            force |= f;
        }
    }
}

/// Blocks received and accepted during one sync.
#[derive(Default)]
struct Tally {
    received: u64,
    accepted: u64,
}

/// Sync from the peer when needed (D-063): when the peer's main-chain tip has more chain work (D-100), the peer's DAG
/// tips include blocks we lack, or we have orphans.
/// First fetch GetBlocks in batches with a locator and a cursor (the peer returns the blocks we may lack, ascending by
/// `(blue_work, hash)`), then fetch by hash the peer's tips we still lack and the missing parents of orphans.
async fn sync_if_needed(ctx: &TaskCtx, peer: PeerId, session: u64, theirs: &Status) -> SyncOutcome {
    let tips = theirs.tips.clone();
    let Some((ours, unknown, missing)) = ctx
        .blocking(move |handler| {
            (
                handler.status(),
                handler.unknown(&tips),
                handler.missing_parents(),
            )
        })
        .await
    else {
        return SyncOutcome::Gone;
    };
    let ahead = wire::work_gt(&theirs.tip_work, &ours.tip_work);
    if !ahead && unknown.is_empty() && missing.is_empty() {
        return SyncOutcome::NotNeeded;
    }
    let mut tally = Tally::default();
    let mut outcome = None;
    if ahead || !unknown.is_empty() {
        eprintln!(
            "[ksana-p2p] syncing blocks from {peer}: peer height {}, our height {}, {} of the peer's tips we lack",
            theirs.tip_height,
            ours.tip_height,
            unknown.len()
        );
        outcome = fetch_blocks(ctx, peer, session, theirs, &mut tally).await;
    }
    if outcome.is_none() {
        outcome = fetch_by_hash(ctx, peer, session, theirs, &mut tally).await;
    }
    if tally.received > 0 {
        eprintln!(
            "[ksana-p2p] sync from {peer} finished: {} blocks received, {} accepted",
            tally.received, tally.accepted
        );
    }
    match outcome {
        Some(outcome) => outcome,
        None if tally.accepted > 0 => SyncOutcome::Progress,
        None => SyncOutcome::NoProgress,
    }
}

/// Fetch GetBlocks in batches with a locator and a cursor and hand them to the Handler in order, until the peer has no
/// more blocks or a block is judged invalid (penalizing the peer).
/// Returns `None` on a normal end.
async fn fetch_blocks(
    ctx: &TaskCtx,
    peer: PeerId,
    session: u64,
    theirs: &Status,
    tally: &mut Tally,
) -> Option<SyncOutcome> {
    // Round limit: not reached normally (when the peer produces many more blocks during the sync, the next Status
    // query continues). The DAG may have more blocks than the main-chain height; estimate at most 16 blocks per height.
    let max_rounds = theirs
        .tip_height
        .saturating_add(1)
        .saturating_mul(16)
        .checked_div(u64::from(MAX_BLOCKS_PER_REQUEST))
        .unwrap_or(0)
        .saturating_add(4);
    let mut rounds: u64 = 0;
    let mut after: Option<[u8; 32]> = None;
    loop {
        if rounds >= max_rounds {
            return None;
        }
        rounds = rounds.saturating_add(1);
        let locator = ctx.blocking(|handler| handler.locator()).await?;
        let request = Request::GetBlocks {
            locator,
            after,
            max: MAX_BLOCKS_PER_REQUEST,
        };
        let blocks = match ctx.request(peer, session, request).await {
            Ok(Response::Blocks(blocks)) => blocks,
            Ok(_) => {
                ctx.penalize(
                    peer,
                    "GetBlocks request got a response of another type".to_string(),
                );
                return Some(SyncOutcome::Interrupted);
            }
            Err(RequestError::Gone) => return Some(SyncOutcome::Gone),
            Err(RequestError::Failed(e)) => {
                eprintln!("[ksana-p2p] syncing blocks from {peer} failed: {e}");
                return Some(SyncOutcome::Interrupted);
            }
        };
        if blocks.len() > usize::try_from(MAX_BLOCKS_PER_REQUEST).unwrap_or(usize::MAX) {
            ctx.penalize(peer, format!("GetBlocks returned {} blocks", blocks.len()));
            return Some(SyncOutcome::Interrupted);
        }
        let batch_last = blocks.last().map(|block| block.header.hash())?;
        if after == Some(batch_last) {
            // The peer gave no new blocks.
            return None;
        }
        if let Some(end) = feed(ctx, peer, blocks, tally).await {
            return Some(end);
        }
        after = Some(batch_last);
    }
}

/// Fetch by hash (GetBlock): the peer's tips we still lack and the missing parents of orphans, re-reading the missing
/// list each round, at most [`MAX_FETCH_ROUNDS`] rounds and [`MAX_FETCH_PER_SYNC`] requests in total; stops when the
/// peer lacks them or nothing new is missing. Returns `None` on a normal end.
async fn fetch_by_hash(
    ctx: &TaskCtx,
    peer: PeerId,
    session: u64,
    theirs: &Status,
    tally: &mut Tally,
) -> Option<SyncOutcome> {
    let tips = theirs.tips.clone();
    let mut asked: BTreeSet<[u8; 32]> = BTreeSet::new();
    for round in 0..MAX_FETCH_ROUNDS {
        let tips = tips.clone();
        let (mut wanted, missing) = ctx
            .blocking(move |handler| {
                // The first round also fetches the peer's tips still lacking after GetBlocks
                let unknown = if round == 0 {
                    handler.unknown(&tips)
                } else {
                    Vec::new()
                };
                (unknown, handler.missing_parents())
            })
            .await?;
        wanted.extend(missing);
        wanted.retain(|hash| !asked.contains(hash));
        wanted.sort_unstable();
        wanted.dedup();
        let budget = MAX_FETCH_PER_SYNC.saturating_sub(asked.len());
        wanted.truncate(budget);
        if wanted.is_empty() {
            return None;
        }
        let mut fetched = Vec::new();
        for hash in wanted {
            asked.insert(hash);
            match ctx.request(peer, session, Request::GetBlock { hash }).await {
                Ok(Response::Block(Some(block))) => {
                    if block.header.hash() != hash {
                        ctx.penalize(peer, "GetBlock returned a different block".to_string());
                        return Some(SyncOutcome::Interrupted);
                    }
                    fetched.push(block);
                }
                Ok(Response::Block(None)) => {}
                Ok(_) => {
                    ctx.penalize(
                        peer,
                        "GetBlock request got a response of another type".to_string(),
                    );
                    return Some(SyncOutcome::Interrupted);
                }
                Err(RequestError::Gone) => return Some(SyncOutcome::Gone),
                Err(RequestError::Failed(e)) => {
                    eprintln!("[ksana-p2p] fetching a block from {peer} failed: {e}");
                    return Some(SyncOutcome::Interrupted);
                }
            }
        }
        if fetched.is_empty() {
            return None;
        }
        if let Some(end) = feed(ctx, peer, fetched, tally).await {
            return Some(end);
        }
    }
    None
}

/// Hand the received blocks to the Handler and count them; penalize the peer when a block is judged invalid. Returns
/// `None` when all were processed.
async fn feed(
    ctx: &TaskCtx,
    peer: PeerId,
    blocks: Vec<Block>,
    tally: &mut Tally,
) -> Option<SyncOutcome> {
    tally.received = tally
        .received
        .saturating_add(u64::try_from(blocks.len()).unwrap_or(u64::MAX));
    let stopping = Arc::clone(&ctx.stopping);
    let Some(fed) = ctx
        .blocking(move |handler| feed_blocks(handler, peer, blocks, &stopping))
        .await
    else {
        return Some(SyncOutcome::Gone);
    };
    tally.accepted = tally.accepted.saturating_add(fed.accepted);
    match fed.end {
        FeedEnd::All => None,
        FeedEnd::Rejected(reason) => {
            ctx.penalize(peer, format!("sync received an invalid block: {reason}"));
            Some(SyncOutcome::Interrupted)
        }
        FeedEnd::Stopped => Some(SyncOutcome::Gone),
    }
}

/// Why [`feed_blocks`] ended.
enum FeedEnd {
    All,
    Rejected(String),
    Stopped,
}

struct Fed {
    accepted: u64,
    end: FeedEnd,
}

/// Hand the blocks received by sync to the Handler in order (runs on a blocking thread), stopping at an invalid block;
/// blocks with unknown parents are put into the orphan pool by the Handler, and the parents are fetched by hash later.
fn feed_blocks(
    handler: &dyn Handler,
    peer: PeerId,
    blocks: Vec<Block>,
    stopping: &AtomicBool,
) -> Fed {
    let mut accepted: u64 = 0;
    for block in blocks {
        if stopping.load(Ordering::SeqCst) {
            return Fed {
                accepted,
                end: FeedEnd::Stopped,
            };
        }
        match handler.on_block(peer, block, false) {
            Verdict::Accept => accepted = accepted.saturating_add(1),
            Verdict::Ignore | Verdict::MissingParent => {}
            Verdict::Reject(reason) => {
                return Fed {
                    accepted,
                    end: FeedEnd::Rejected(reason),
                }
            }
        }
    }
    Fed {
        accepted,
        end: FeedEnd::All,
    }
}

/// Fetch the peer's entry pool and hand the ops to the Handler one by one. Invalid ops in one response cost only 1
/// point (an op's validity depends on our state).
/// Returns `false` when the session has ended.
async fn fetch_mempool(ctx: &TaskCtx, peer: PeerId, session: u64) -> bool {
    let request = Request::GetMempool {
        max: MAX_MEMPOOL_OPS,
    };
    let ops = match ctx.request(peer, session, request).await {
        Ok(Response::Mempool(ops)) => ops,
        Ok(_) => {
            ctx.penalize(
                peer,
                "GetMempool request got a response of another type".to_string(),
            );
            return true;
        }
        Err(RequestError::Gone) => return false,
        Err(RequestError::Failed(e)) => {
            eprintln!("[ksana-p2p] fetching the entry pool of {peer} failed: {e}");
            return true;
        }
    };
    if ops.len() > usize::try_from(MAX_MEMPOOL_OPS).unwrap_or(usize::MAX) {
        ctx.penalize(peer, format!("GetMempool returned {} ops", ops.len()));
        return true;
    }
    if ops.is_empty() {
        return true;
    }
    let stopping = Arc::clone(&ctx.stopping);
    match ctx
        .blocking(move |handler| feed_ops(handler, peer, ops, &stopping))
        .await
    {
        None => false,
        Some(Some(reason)) => {
            ctx.penalize(peer, format!("entry pool contains an invalid op: {reason}"));
            true
        }
        Some(None) => true,
    }
}

/// Hand the ops of an entry pool response to the Handler one by one, returning the reason of the first one judged
/// invalid.
fn feed_ops(
    handler: &dyn Handler,
    peer: PeerId,
    ops: Vec<OpMessage>,
    stopping: &AtomicBool,
) -> Option<String> {
    let mut rejected = None;
    for msg in ops {
        if stopping.load(Ordering::SeqCst) {
            break;
        }
        if let Verdict::Reject(reason) = handler.on_op(peer, msg) {
            rejected.get_or_insert(reason);
        }
    }
    rejected
}
