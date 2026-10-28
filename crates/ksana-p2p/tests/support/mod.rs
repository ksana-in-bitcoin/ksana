//! Shared parts of the network tests: an in-memory chain (implementing `Handler`), block / op construction, node
//! startup and polling waits.
//!
//! The test chain's rules (unrelated to PoW, which the p2p layer does not care about):
//! - a block with `header.version == 99` is invalid (Reject);
//! - a known block (including orphans) is Ignore; when a parent is unknown, the block goes into the orphan pool and
//!   MissingParent is returned, and it is connected once the parent arrives;
//!   Reject when the height is not the height of `parents[0]` + 1;
//! - all other blocks are stored (DAG); the longest chain along `parents[0]` is the main chain (on equal length the
//!   one received first is kept), work = main-chain height;
//!   tips = blocks without children; GetBlocks returns the blocks not in `past(F) ∪ {F}`, ascending by
//!   `(height, hash)` (side blocks can be switched off, see [`TestChain::set_serve_side_blocks`]).

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use ksana_p2p::{
    wire, Handler, Keypair, Multiaddr, OpMessage, P2p, P2pConfig, PeerId, Status, Tuning, Verdict,
};
use ksana_primitives::{
    Action, Block, BlockHeader, BtcAnchor, FeeMode, SettlementTxs, SigScheme, VmOp,
};
use libp2p::multiaddr::Protocol;

/// The version number marking an invalid block.
pub const BAD_VERSION: u32 = 99;

/// One `on_block` call.
#[derive(Clone, Debug)]
pub struct BlockEvent {
    pub from: PeerId,
    pub hash: [u8; 32],
    pub height: u64,
    pub gossip: bool,
    pub verdict: Verdict,
}

/// One `on_op` call.
#[derive(Clone, Debug)]
pub struct OpEvent {
    pub from: PeerId,
    pub msg: OpMessage,
    pub verdict: Verdict,
}

struct Inner {
    genesis: [u8; 32],
    blocks: BTreeMap<[u8; 32], Block>,
    /// Blocks whose parent is unknown.
    orphans: BTreeMap<[u8; 32], Block>,
    /// The block hashes at each height of the main chain; `main[0]` is the genesis block.
    main: Vec<[u8; 32]>,
    /// Whether GetBlocks returns blocks not on the main chain (when off, the peer can only fetch side blocks by hash).
    serve_side_blocks: bool,
    mempool: Vec<OpMessage>,
    block_events: Vec<BlockEvent>,
    op_events: Vec<OpEvent>,
    blocks_after_calls: usize,
    status_calls: usize,
    mempool_calls: usize,
}

/// An in-memory chain.
pub struct TestChain {
    inner: Mutex<Inner>,
}

impl TestChain {
    pub fn new(genesis: Block) -> Arc<TestChain> {
        let hash = genesis.header.hash();
        let mut blocks = BTreeMap::new();
        blocks.insert(hash, genesis);
        Arc::new(TestChain {
            inner: Mutex::new(Inner {
                genesis: hash,
                blocks,
                orphans: BTreeMap::new(),
                main: vec![hash],
                serve_side_blocks: true,
                mempool: Vec::new(),
                block_events: Vec::new(),
                op_events: Vec::new(),
                blocks_after_calls: 0,
                status_calls: 0,
                mempool_calls: 0,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    pub fn genesis_hash(&self) -> [u8; 32] {
        self.lock().genesis
    }

    pub fn tip_height(&self) -> u64 {
        (self.lock().main.len() - 1) as u64
    }

    pub fn tip(&self) -> Block {
        let inner = self.lock();
        inner.blocks[inner.main.last().unwrap()].clone()
    }

    pub fn main_chain(&self) -> Vec<[u8; 32]> {
        self.lock().main.clone()
    }

    /// The block at `height` on the main chain.
    pub fn block_at(&self, height: u64) -> Block {
        let inner = self.lock();
        inner.blocks[&inner.main[height as usize]].clone()
    }

    pub fn has_block(&self, hash: &[u8; 32]) -> bool {
        self.lock().blocks.contains_key(hash)
    }

    /// Number of blocks in the DAG (including the genesis block, excluding orphans).
    pub fn block_count(&self) -> usize {
        self.lock().blocks.len()
    }

    /// Number of orphans.
    pub fn orphan_count(&self) -> usize {
        self.lock().orphans.len()
    }

    /// The DAG tips (blocks without children), sorted by hash.
    pub fn tips(&self) -> Vec<[u8; 32]> {
        self.lock().tips()
    }

    /// Whether GetBlocks returns blocks not on the main chain.
    pub fn set_serve_side_blocks(&self, serve: bool) {
        self.lock().serve_side_blocks = serve;
    }

    /// Local "block production": store any block (all parents known) in the DAG and update the main chain by the
    /// rules.
    pub fn add_block(&self, block: Block) {
        assert_eq!(self.lock().accept_block(block), Verdict::Accept);
    }

    /// Local "block production": append `n` blocks to the main-chain tip (`salt` distinguishes blocks produced by
    /// different nodes) and return them.
    pub fn extend(&self, n: u64, salt: u64) -> Vec<Block> {
        let mut inner = self.lock();
        let mut out = Vec::new();
        for _ in 0..n {
            let parent = inner.blocks[inner.main.last().unwrap()].clone();
            let block = child(&parent, salt);
            let hash = block.header.hash();
            inner.blocks.insert(hash, block.clone());
            inner.main.push(hash);
            out.push(block);
        }
        out
    }

    /// Local "block production": connect the given block (its parent must be the current tip) to the main chain.
    pub fn add_local(&self, block: Block) {
        let mut inner = self.lock();
        assert_eq!(block.header.parents, vec![*inner.main.last().unwrap()]);
        let hash = block.header.hash();
        inner.blocks.insert(hash, block);
        inner.main.push(hash);
    }

    pub fn add_to_mempool(&self, msg: OpMessage) {
        self.lock().mempool.push(msg);
    }

    pub fn block_events(&self) -> Vec<BlockEvent> {
        self.lock().block_events.clone()
    }

    pub fn op_events(&self) -> Vec<OpEvent> {
        self.lock().op_events.clone()
    }

    pub fn blocks_after_calls(&self) -> usize {
        self.lock().blocks_after_calls
    }

    /// How many times `status()` was called (the peer's Status requests and our comparison before syncing).
    pub fn status_calls(&self) -> usize {
        self.lock().status_calls
    }

    /// How many times `mempool()` was called: the peer's session fetches the entry pool at the end of its first round
    /// after connecting (Status → sync → entry pool).
    pub fn mempool_calls(&self) -> usize {
        self.lock().mempool_calls
    }

    /// The current status (not counted in `status_calls`).
    pub fn status_now(&self) -> Status {
        self.lock().status()
    }
}

impl Handler for TestChain {
    fn status(&self) -> Status {
        let mut inner = self.lock();
        inner.status_calls += 1;
        inner.status()
    }

    fn locator(&self) -> Vec<[u8; 32]> {
        let inner = self.lock();
        let tip = (inner.main.len() - 1) as u64;
        wire::locator_heights(tip)
            .into_iter()
            .map(|h| inner.main[h as usize])
            .collect()
    }

    fn blocks_after(&self, locator: &[[u8; 32]], after: Option<&[u8; 32]>, max: u32) -> Vec<Block> {
        let mut inner = self.lock();
        inner.blocks_after_calls += 1;
        let on_main = |hash: &&[u8; 32]| {
            inner
                .blocks
                .get(*hash)
                .is_some_and(|b| inner.main.get(b.header.height as usize) == Some(*hash))
        };
        let fork = locator
            .iter()
            .find(on_main)
            .copied()
            .unwrap_or(inner.genesis);
        // past(F) ∪ {F}
        let mut past = BTreeSet::new();
        let mut stack = vec![fork];
        while let Some(hash) = stack.pop() {
            if past.insert(hash) {
                stack.extend(inner.blocks[&hash].header.parents.iter().copied());
            }
        }
        let key = |hash: &[u8; 32]| (inner.blocks[hash].header.height, *hash);
        let cursor = match after {
            Some(a) if inner.blocks.contains_key(a) => Some(key(a)),
            Some(_) => return Vec::new(),
            None => None,
        };
        let main: BTreeSet<[u8; 32]> = inner.main.iter().copied().collect();
        let mut out: Vec<[u8; 32]> = inner
            .blocks
            .keys()
            .filter(|h| !past.contains(*h))
            .filter(|h| inner.serve_side_blocks || main.contains(*h))
            .filter(|h| cursor.is_none_or(|c| key(h) > c))
            .copied()
            .collect();
        out.sort_by_key(key);
        out.into_iter()
            .take(max as usize)
            .map(|hash| inner.blocks[&hash].clone())
            .collect()
    }

    fn unknown(&self, hashes: &[[u8; 32]]) -> Vec<[u8; 32]> {
        let inner = self.lock();
        hashes
            .iter()
            .filter(|h| !inner.blocks.contains_key(*h))
            .copied()
            .collect()
    }

    fn missing_parents(&self) -> Vec<[u8; 32]> {
        let inner = self.lock();
        let missing: BTreeSet<[u8; 32]> = inner
            .orphans
            .values()
            .flat_map(|b| b.header.parents.iter())
            .filter(|p| !inner.blocks.contains_key(*p))
            .copied()
            .collect();
        missing.into_iter().collect()
    }

    fn block(&self, hash: &[u8; 32]) -> Option<Block> {
        self.lock().blocks.get(hash).cloned()
    }

    fn mempool(&self, max: u32) -> Vec<OpMessage> {
        let mut inner = self.lock();
        inner.mempool_calls += 1;
        inner.mempool.iter().take(max as usize).cloned().collect()
    }

    fn on_block(&self, from: PeerId, block: Block, gossip: bool) -> Verdict {
        let mut inner = self.lock();
        let hash = block.header.hash();
        let height = block.header.height;
        let verdict = inner.accept_block(block);
        inner.block_events.push(BlockEvent {
            from,
            hash,
            height,
            gossip,
            verdict: verdict.clone(),
        });
        verdict
    }

    fn on_op(&self, from: PeerId, msg: OpMessage) -> Verdict {
        let mut inner = self.lock();
        let verdict = if msg.op.max_gas == 0 {
            Verdict::Reject("max_gas is 0".to_string())
        } else if inner.mempool.contains(&msg) {
            Verdict::Ignore
        } else {
            inner.mempool.push(msg.clone());
            Verdict::Accept
        };
        inner.op_events.push(OpEvent {
            from,
            msg,
            verdict: verdict.clone(),
        });
        verdict
    }
}

impl Inner {
    fn status(&self) -> Status {
        let height = (self.main.len() - 1) as u64;
        let mut tip_work = [0u8; 32];
        tip_work[..8].copy_from_slice(&height.to_le_bytes());
        let mut tips = self.tips();
        tips.sort_by_key(|h| std::cmp::Reverse((self.blocks[h].header.height, *h)));
        tips.truncate(wire::MAX_STATUS_TIPS);
        Status {
            genesis: self.genesis,
            tip_height: height,
            tip_hash: *self.main.last().unwrap(),
            tip_work,
            tips,
        }
    }

    fn tips(&self) -> Vec<[u8; 32]> {
        let parents: BTreeSet<[u8; 32]> = self
            .blocks
            .values()
            .flat_map(|b| b.header.parents.iter().copied())
            .collect();
        self.blocks
            .keys()
            .filter(|h| !parents.contains(*h))
            .copied()
            .collect()
    }

    fn accept_block(&mut self, block: Block) -> Verdict {
        if block.header.version == BAD_VERSION {
            return Verdict::Reject("invalid block marked by the test".to_string());
        }
        let hash = block.header.hash();
        if self.blocks.contains_key(&hash) || self.orphans.contains_key(&hash) {
            return Verdict::Ignore;
        }
        if block
            .header
            .parents
            .iter()
            .any(|p| !self.blocks.contains_key(p))
        {
            self.orphans.insert(hash, block);
            return Verdict::MissingParent;
        }
        let verdict = self.connect(block);
        // Orphans whose parents have all arrived are connected in turn
        while let Some(ready) = self
            .orphans
            .iter()
            .find(|(_, b)| b.header.parents.iter().all(|p| self.blocks.contains_key(p)))
            .map(|(h, _)| *h)
        {
            let orphan = self.orphans.remove(&ready).unwrap();
            let _ = self.connect(orphan);
        }
        verdict
    }

    fn connect(&mut self, block: Block) -> Verdict {
        let hash = block.header.hash();
        let parent = &self.blocks[&block.header.parents[0]];
        if parent.header.height + 1 != block.header.height {
            return Verdict::Reject("height not consecutive".to_string());
        }
        let height = block.header.height;
        self.blocks.insert(hash, block);
        if height as usize >= self.main.len() {
            // A longer chain: walk back along the parents to the fork point on the main chain and rebuild the main
            // chain.
            let mut branch = vec![hash];
            let mut cursor = hash;
            loop {
                let parent = self.blocks[&cursor].header.parents[0];
                let parent_height = self.blocks[&parent].header.height as usize;
                if self.main.get(parent_height) == Some(&parent) {
                    self.main.truncate(parent_height + 1);
                    break;
                }
                branch.push(parent);
                cursor = parent;
            }
            self.main.extend(branch.into_iter().rev());
        }
        Verdict::Accept
    }
}

/// The genesis block (a different `seed` makes a different network).
pub fn genesis(seed: u64) -> Block {
    Block {
        header: BlockHeader {
            version: 1,
            height: 0,
            parents: vec![],
            btc_anchor: BtcAnchor {
                height: 0,
                hash: [0u8; 32],
            },
            entries_root: [0u8; 32],
            state_root: [0u8; 32],
            producer: [0u8; 32],
            timestamp: 0,
            bits: 0,
            nonce: seed,
        },
        entries: Vec::new(),
        settlement: None,
    }
}

/// A child of `parent` (no PoW needed).
pub fn child(parent: &Block, salt: u64) -> Block {
    let mut header = parent.header.clone();
    header.version = 1;
    header.height = parent.header.height + 1;
    header.parents = vec![parent.header.hash()];
    header.nonce = salt;
    header.timestamp = parent.header.timestamp + 1;
    Block {
        header,
        entries: Vec::new(),
        settlement: Some(SettlementTxs {
            commit: vec![0xab; 32],
            reveal: vec![0xcd; 32],
        }),
    }
}

/// An invalid block carrying the test marker.
pub fn bad_block(parent: &Block, salt: u64) -> Block {
    let mut block = child(parent, salt);
    block.header.version = BAD_VERSION;
    block
}

/// A test op whose encoding is `len` bytes long (at least the length of one without arguments, about 150 bytes).
pub fn op_of_len(nonce: u64, len: usize) -> OpMessage {
    let call = |args: Vec<u8>| OpMessage {
        op: VmOp {
            version: 1,
            chain_id: 1,
            account: [3u8; 32],
            nonce,
            fee: FeeMode::Token { amount: 5 },
            max_gas: 100_000,
            action: Action::Call {
                contract: [6u8; 32],
                method: "store".to_string(),
                args,
                funds: Vec::new(),
            },
            sig: SigScheme::Bip340([9u8; 64]),
        },
    };
    let short = borsh::to_vec(&call(Vec::new())).unwrap().len();
    let msg = call(vec![0x77; len - short]);
    assert_eq!(borsh::to_vec(&msg).unwrap().len(), len);
    msg
}

/// A test op: `nonce` distinguishes ops.
pub fn op(nonce: u64) -> OpMessage {
    OpMessage {
        op: VmOp {
            version: 1,
            chain_id: 1,
            account: [3u8; 32],
            nonce,
            fee: FeeMode::Token { amount: 5 },
            max_gas: 100_000,
            action: Action::AssetTransfer {
                asset: [0u8; 32],
                to: [4u8; 32],
                amount: 1,
            },
            sig: SigScheme::Bip340([9u8; 64]),
        },
    }
}

/// A test node.
pub struct TestNode {
    pub p2p: P2p,
    pub chain: Arc<TestChain>,
}

impl TestNode {
    pub fn start(chain: Arc<TestChain>, bootstrap: Vec<Multiaddr>, tuning: Tuning) -> TestNode {
        Self::start_on(
            chain,
            "/ip4/127.0.0.1/tcp/0".parse().unwrap(),
            bootstrap,
            tuning,
        )
    }

    pub fn start_on(
        chain: Arc<TestChain>,
        listen: Multiaddr,
        bootstrap: Vec<Multiaddr>,
        tuning: Tuning,
    ) -> TestNode {
        Self::start_with_op_limit(
            chain,
            listen,
            bootstrap,
            tuning,
            ksana_primitives::params::DEVNET.max_op_bytes,
        )
    }

    /// Like [`TestNode::start_on`], with `max_op_bytes` in place of the network's `MAX_OP_BYTES`.
    pub fn start_with_op_limit(
        chain: Arc<TestChain>,
        listen: Multiaddr,
        bootstrap: Vec<Multiaddr>,
        tuning: Tuning,
        max_op_bytes: usize,
    ) -> TestNode {
        let config = P2pConfig {
            keypair: Keypair::generate_ed25519(),
            listen: vec![listen],
            peers: bootstrap,
            genesis: chain.genesis_hash(),
            max_op_bytes,
        };
        let p2p = P2p::start_with(config, tuning, chain.clone()).unwrap();
        assert!(!p2p.listen_addrs().is_empty());
        TestNode { p2p, chain }
    }

    pub fn peer_id(&self) -> PeerId {
        self.p2p.local_peer_id()
    }

    /// The listen address with the /p2p suffix (as a bootstrap address for other nodes).
    pub fn addr(&self) -> Multiaddr {
        self.p2p.listen_addrs()[0]
            .clone()
            .with(Protocol::P2p(self.peer_id()))
    }

    /// The TCP port listened on.
    pub fn port(&self) -> u16 {
        self.p2p.listen_addrs()[0]
            .iter()
            .find_map(|p| match p {
                Protocol::Tcp(port) => Some(port),
                _ => None,
            })
            .unwrap()
    }

    pub fn is_connected_to(&self, other: &TestNode) -> bool {
        self.p2p.connected_peers().contains(&other.peer_id())
    }

    pub fn gossips_with(&self, other: &TestNode) -> bool {
        self.p2p.gossip_peers().contains(&other.peer_id())
    }
}

/// Periodic Status queries essentially off (the tests rely only on the query at connection time and on gossip), fast
/// bootstrap redials.
pub fn quiet_tuning() -> Tuning {
    Tuning {
        status_interval: Duration::from_secs(3600),
        redial_interval: Duration::from_millis(300),
        ..Tuning::default()
    }
}

/// Poll until the condition holds; fail on timeout.
pub fn wait_until(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let step = Duration::from_millis(20);
    let steps = timeout.as_millis() / step.as_millis();
    for _ in 0..steps {
        if cond() {
            return;
        }
        std::thread::sleep(step);
    }
    assert!(cond(), "timed out waiting: {what}");
}

/// The condition holds throughout `duration`.
pub fn holds_for(what: &str, duration: Duration, mut cond: impl FnMut() -> bool) {
    let step = Duration::from_millis(20);
    let steps = duration.as_millis() / step.as_millis();
    for _ in 0..steps {
        assert!(cond(), "condition no longer holds: {what}");
        std::thread::sleep(step);
    }
}

/// The two nodes are connected to each other and each sees the other subscribed to the block topic (blocks published
/// afterwards can be delivered directly).
pub fn wait_gossip_ready(a: &TestNode, b: &TestNode) {
    wait_until("gossip ready", Duration::from_secs(10), || {
        a.is_connected_to(b) && b.is_connected_to(a) && a.gossips_with(b) && b.gossips_with(a)
    });
}

/// Wait until `node`'s entry pool has been fetched by peers at least `calls` times: the first round of the peer's
/// session (Status → sync → entry pool) has finished, and afterwards (with periodic queries off) the peer asks for
/// Status no more.
pub fn wait_mempool_served(node: &TestNode, calls: usize) {
    wait_until(
        "first round of the peer's session finished",
        Duration::from_secs(10),
        || node.chain.mempool_calls() >= calls,
    );
}
