//! Network integration (M4, D-040, D-041): implements [`ksana_p2p::Handler`] and wraps the running network layer
//! ([`Network`]).
//!
//! - Sync (`/ksana/sync/2`, D-063): Status carries the DAG tips that can still be merged, none anchored or merging a
//!   block anchored off the BTC best chain, and in place of such tips the blocks below them that templates merge
//!   ([`Chain::status_tips`], D-104), GetBlocks returns the blocks the peer may lack per [`Chain::sync_hashes`]; the
//!   peer thereby obtains our side blocks so it can merge them.
//! - Received blocks go to [`Chain::accept_block`]: those received by gossip are checked by the acceptance rules
//!   (`now` is the local time), those in sync responses are handled as historical blocks. After a main-chain change
//!   the entry pool is maintained and new-block notifications are emitted ([`ops::chain_changed`]). When the anchor is
//!   newer than the local BTC snapshot, sync with bitcoind first and retry.
//! - Verdict on a block: valid and new (including side-chain blocks whose work does not win) → Accept; duplicate,
//!   rejected only because of the local view ([`NodeError::Rejected`]) or local fault → Ignore; invalid block →
//!   Reject (the source peer loses score); unknown parent → MissingParent (sync from the source peer).
//! - Received ops are admitted per [`ops::admit`]: objectively invalid → Reject; known, invalid under the local state,
//!   not meeting this node's policy or local fault → Ignore.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ksana_mempool::Mempool;
use ksana_p2p::{Handler, Multiaddr, OpMessage, P2p, PeerId, Status, Verdict};
use ksana_primitives::{Block, VmOp};
use ksana_rpc::BlockSummary;
use tokio::sync::broadcast;

use crate::chain::{Accepted, Chain};
use crate::ops::{self, Rejection};
use crate::{unix_now, NodeError};

/// When the anchor is newer than the local BTC snapshot: the number of retries after syncing with bitcoind and the
/// interval between them (BTC blocks take time to propagate through the network).
const BTC_RETRIES: u32 = 5;
const BTC_RETRY_DELAY: Duration = Duration::from_millis(200);

/// Network handling on the node side.
pub struct NetHandler {
    /// The chain.
    pub chain: Arc<Chain>,
    /// The entry pool.
    pub mempool: Arc<Mutex<Mempool>>,
    /// New-block notifications.
    pub events: broadcast::Sender<BlockSummary>,
}

impl NetHandler {
    fn sync_btc(&self) -> Result<(), NodeError> {
        while self.chain.sync_btc()? {}
        ops::apply_chain_changes(&self.chain, &self.mempool, &self.events)
    }

    /// GetBlocks (D-063): the blocks given by [`Chain::sync_hashes`], as many as fit a response (D-132).
    fn try_blocks_after(
        &self,
        locator: &[[u8; 32]],
        after: Option<&[u8; 32]>,
        max: u32,
    ) -> Result<Vec<Block>, NodeError> {
        let max = usize::try_from(max).unwrap_or(usize::MAX);
        let store = self.chain.store();
        let mut out = Vec::new();
        let mut bytes: usize = 0;
        for hash in self.chain.sync_hashes(locator, after, max)? {
            let block = store
                .block(&hash)?
                .ok_or_else(|| NodeError::Storage("block in the DAG is missing".into()))?;
            // The response carries only the leading blocks that fit: no more are loaded (D-132)
            let len = borsh::object_length(&block).unwrap_or(usize::MAX);
            match bytes.checked_add(len) {
                Some(total) if total <= ksana_p2p::wire::MAX_SYNC_MESSAGE_BYTES => bytes = total,
                _ => break,
            }
            out.push(block);
        }
        Ok(out)
    }
}

impl Handler for NetHandler {
    fn status(&self) -> Status {
        let genesis = self.chain.genesis().hash();
        let tips = self
            .chain
            .status_tips(ksana_p2p::wire::MAX_STATUS_TIPS)
            .unwrap_or_else(|e| {
                eprintln!("[ksana-node] failed to read the DAG tips: {e}");
                Vec::new()
            });
        match self.chain.tip() {
            Ok(tip) => Status {
                genesis,
                tip_height: tip.header.height,
                tip_hash: tip.hash,
                tip_work: tip.chain_work.to_le_bytes(),
                tips,
            },
            Err(e) => {
                eprintln!("[ksana-node] failed to read the main-chain tip: {e}");
                Status {
                    genesis,
                    tip_height: 0,
                    tip_hash: genesis,
                    tip_work: [0; 32],
                    tips: Vec::new(),
                }
            }
        }
    }

    fn locator(&self) -> Vec<[u8; 32]> {
        self.chain.locator().unwrap_or_else(|e| {
            eprintln!("[ksana-node] failed to build the block locator: {e}");
            vec![self.chain.genesis().hash()]
        })
    }

    fn blocks_after(&self, locator: &[[u8; 32]], after: Option<&[u8; 32]>, max: u32) -> Vec<Block> {
        self.try_blocks_after(locator, after, max)
            .unwrap_or_else(|e| {
                eprintln!("[ksana-node] failed to read blocks for sync: {e}");
                Vec::new()
            })
    }

    fn block(&self, hash: &[u8; 32]) -> Option<Block> {
        self.chain.store().block(hash).ok().flatten()
    }

    fn unknown(&self, hashes: &[[u8; 32]]) -> Vec<[u8; 32]> {
        hashes
            .iter()
            .filter(|h| {
                // A block pruned from memory is known too (D-097): it is not fetched again
                matches!(self.chain.knows(h), Ok(false))
                    && matches!(self.chain.is_invalid(h), Ok(false))
            })
            .copied()
            .collect()
    }

    fn missing_parents(&self) -> Vec<[u8; 32]> {
        self.chain
            .missing_parents()
            .unwrap_or_default()
            .into_iter()
            .filter(|h| matches!(self.chain.is_invalid(h), Ok(false)))
            .collect()
    }

    fn mempool(&self, max: u32) -> Vec<OpMessage> {
        let Ok(pool) = self.mempool.lock() else {
            return Vec::new();
        };
        pool.pending(usize::try_from(max).unwrap_or(usize::MAX))
            .into_iter()
            .map(|op| OpMessage { op })
            .collect()
    }

    fn on_block(&self, _from: PeerId, block: Block, gossip: bool) -> Verdict {
        let now = gossip.then(unix_now);
        let height = block.header.height;
        let mut retries = 0;
        loop {
            // By reference: a block from a peer can take hundreds of MiB once decoded (M26), it is not copied per attempt
            match self.chain.accept_block(&block, now) {
                Ok(report) => {
                    if let Err(e) = ops::chain_changed(
                        &self.chain,
                        &self.mempool,
                        &self.events,
                        &report.connected,
                        &report.disconnected,
                    ) {
                        eprintln!("[ksana-node] failed to maintain the entry pool after a main-chain change: {e}");
                    }
                    return match report.outcome {
                        Accepted::Extended(_) | Accepted::Reorganized(_) | Accepted::Side => {
                            Verdict::Accept
                        }
                        Accepted::Duplicate => Verdict::Ignore,
                        Accepted::Orphan => Verdict::MissingParent,
                    };
                }
                Err(NodeError::InvalidBlock(m)) => return Verdict::Reject(m),
                Err(NodeError::Btc(m)) if retries < BTC_RETRIES => {
                    // Most likely the anchor is newer than the local BTC snapshot: sync and retry
                    retries = retries.saturating_add(1);
                    if let Err(e) = self.sync_btc() {
                        eprintln!("[ksana-node] block at height {height} not accepted ({m}), syncing with bitcoind failed: {e}");
                        return Verdict::Ignore;
                    }
                    std::thread::sleep(BTC_RETRY_DELAY);
                }
                Err(e) => {
                    eprintln!("[ksana-node] block at height {height} not accepted: {e}");
                    return Verdict::Ignore;
                }
            }
        }
    }

    fn on_op(&self, _from: PeerId, msg: OpMessage) -> Verdict {
        let op_id = match msg.op.op_id() {
            Ok(id) => id,
            Err(e) => return Verdict::Reject(format!("VmOp encoding failed: {e}")),
        };
        let known = self
            .mempool
            .lock()
            .map(|p| p.contains(&op_id))
            .unwrap_or(true);
        if known || !matches!(self.chain.store().op_location(&op_id), Ok(None)) {
            return Verdict::Ignore;
        }
        match ops::admit(&self.chain, &self.mempool, msg.op) {
            Ok(_) => Verdict::Accept,
            Err(Rejection::Invalid(m)) => Verdict::Reject(m),
            Err(Rejection::Stale(_) | Rejection::Policy(_) | Rejection::Local(_)) => {
                Verdict::Ignore
            }
        }
    }
}

/// The running network layer: broadcasts this node's blocks and ops, lets tests isolate nodes. Calls after
/// [`Network::stop`] do nothing.
pub struct Network {
    p2p: Mutex<Option<P2p>>,
    local_peer_id: PeerId,
}

impl Network {
    /// Wrap a started network layer.
    pub fn new(p2p: P2p) -> Self {
        Network {
            local_peer_id: p2p.local_peer_id(),
            p2p: Mutex::new(Some(p2p)),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&P2p) -> R) -> Option<R> {
        self.p2p.lock().ok()?.as_ref().map(f)
    }

    /// This node's peer id.
    pub fn local_peer_id(&self) -> PeerId {
        self.local_peer_id
    }

    /// The addresses actually listened on (without the /p2p suffix).
    pub fn listen_addrs(&self) -> Vec<Multiaddr> {
        self.with(P2p::listen_addrs).unwrap_or_default()
    }

    /// The connected peers.
    pub fn connected_peers(&self) -> Vec<PeerId> {
        self.with(P2p::connected_peers).unwrap_or_default()
    }

    /// The peers subscribed to the block topic.
    pub fn gossip_peers(&self) -> Vec<PeerId> {
        self.with(P2p::gossip_peers).unwrap_or_default()
    }

    /// Broadcast a block.
    pub fn publish_block(&self, block: &Block) {
        self.with(|p| p.publish_block(block));
    }

    /// Broadcast an op.
    pub fn publish_op(&self, op: &VmOp) {
        let msg = OpMessage { op: op.clone() };
        self.with(|p| p.publish_op(&msg));
    }

    /// Disconnect from and refuse connections with that peer (used by tests to create a network partition).
    pub fn block_peer(&self, peer: PeerId) {
        self.with(|p| p.block_peer(peer));
    }

    /// Undo [`Network::block_peer`].
    pub fn unblock_peer(&self, peer: PeerId) {
        self.with(|p| p.unblock_peer(peer));
    }

    /// Dial.
    pub fn dial(&self, addr: Multiaddr) {
        self.with(|p| p.dial(addr));
    }

    /// Immediately ask the connected peers for Status, and sync from the peers with more blue work or with DAG tips
    /// we do not have.
    pub fn request_sync(&self) {
        self.with(P2p::request_sync);
    }

    /// Stop the network layer and wait for the network thread to exit (must not be called while holding the chain or
    /// entry pool lock).
    pub fn stop(&self) {
        let p2p = self.p2p.lock().ok().and_then(|mut guard| guard.take());
        drop(p2p);
    }
}
