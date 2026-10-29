//! Adversarial tests (M26 security audit): a peer, a BTC user or an RPC caller must not be able to stop this node's
//! block production, republishing or rescues, or tie it up with read-only calls. Without the network: node instances
//! share one mock BTC chain, produce blocks with the test PoW `Sha256d` and are driven step by step.
//!
//! - An anchor-invalid block (D-056) whose forced-inclusion entry names no native transaction on the BTC best chain, or
//!   one without the confirmations forced inclusion asks for, is refused when it arrives (D-100), and merged once the
//!   transaction has them, so it does not stop production.
//! - Forced-inclusion entries are mandatory and not bounded by the payload size (§7.4): an unlock or path B entry that
//!   would make the body too large for one payload waits, and a settlement payload that would exceed
//!   `MAX_BLOCK_DATA_BYTES` drops the other blocks' data or is skipped, instead of failing production.
//! - A republish payload is sized by its exact encoding, payee included (D-095).
//! - `ksana_call` runs at most `BLOCK_GAS_LIMIT` gas and a bounded number of simulations at once.
//! - Ops that cannot alter the packing candidates, such as ops behind a nonce gap, do not make the producer rebuild its
//!   template (D-130).
//! - A settlement claim in the BTC mempool holds a rescue back only when it carries the block's exact payload at a fee
//!   rate not below this node's, and only for a while (D-057).
//! - Settlement transactions a BTC reorg confirmed again at the block's anchor height, where their payload does not
//!   count (D-100), come from a reorg that took the block's anchor off the BTC best chain: the block leaves the main
//!   chain (D-104), and its op is included again by a block whose own settlement lands.
//! - A low fee does not hold a block's data back (D-102): this node's fee rate follows bitcoind's estimate within its
//!   floor and cap, a producer bumps its own settlement waiting below that rate through the reveal's P2A output (CPFP),
//!   and a block's own settlement waiting in the mempool below another node's rate holds that node's rescue back no
//!   more than another party's claim does. While bitcoind's mempool has room, the node's settlements and rescues pay
//!   the floor, and a pair the miners leave waiting all the same is bumped to the estimate or past its own rate,
//!   whatever bitcoind estimates (D-117).
//! - Another party's child holding a reveal's P2A output first (pinning) does not hold a block's data back: the
//!   payload of the producer's own settlement, a rescue or a republish goes out again in a new pair of this node's at
//!   the bump's rate, and nothing more is spent on a pair whose block's data has landed (D-131).

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use bitcoin::hashes::Hash;
use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey};
use bitcoin::{absolute, transaction, Amount, Sequence, Transaction, TxIn, TxOut, Witness};
use common::mock_node::{native_tx, signed_native};
use common::side::setup;
use common::{transfer_op, user, User};
use ksana_btc::client::{BtcClient, MempoolInfo};
use ksana_btc::mock::{coinbase_script, MockBtc};
use ksana_btc::payload::{attached_payload, envelope_payload};
use ksana_btc::settlement::{draft, finalize, producer_script, ProducerUtxo, SettlementPair};
use ksana_consensus::{Consensus, Sha256d};
use ksana_crypto::{entries_root, sign_vmop};
use ksana_execution::keys::{self, AccountState};
use ksana_execution::StateSource;
use ksana_mempool::{Mempool, Policy};
use ksana_node::chain::{Accepted, Chain};
use ksana_node::genesis::GenesisFile;
use ksana_node::producer::{
    FeePolicy, Funding, Producer, TemplateSync, DEFAULT_FEE_TARGET, ROOMY_MEMPOOL_VSIZE,
};
use ksana_node::rescue::{Rescuer, BUMP_AFTER, CLAIM_PATIENCE};
use ksana_node::rpc::NodeRpc;
use ksana_node::NodeError;
use ksana_primitives::hash::sha256;
use ksana_primitives::{
    Action, Block, BlockHeader, BtcAnchor, Entry, FeeMode, Params, Payload, SigScheme, VmOp,
    DEVNET, KSA_ASSET_ID, VMOP_VERSION,
};
use ksana_rpc::{Backend, CallRequestJson};
use tokio::sync::broadcast;

/// One node instance: chain (test PoW `Sha256d`), entry pool and a producer whose loop is not running.
struct Instance {
    chain: Arc<Chain>,
    mempool: Arc<Mutex<Mempool>>,
    producer: Producer,
    _dir: tempfile::TempDir,
}

fn btc_keypair(seed: &str) -> Keypair {
    let secret = SecretKey::from_slice(&sha256(format!("adv/liveness/{seed}").as_bytes())).unwrap();
    Keypair::from_secret_key(&Secp256k1::new(), &secret)
}

impl Instance {
    /// With `funded`, the producer's funding address has 4 confirmed UTXOs and every block that needs a body carries
    /// its own settlement transactions (settlement batching off, D-077); otherwise the producer has no funding wallet.
    fn open(
        mock: &Arc<MockBtc>,
        genesis: &GenesisFile,
        name: &str,
        params: &Params,
        funded: bool,
    ) -> Instance {
        let dir = tempfile::tempdir().unwrap();
        let chain = Arc::new(
            Chain::open_with_algorithm(
                dir.path(),
                params.clone(),
                genesis,
                mock.clone(),
                Some(Arc::new(Sha256d)),
            )
            .unwrap(),
        );
        let funding = funded.then(|| {
            let funding =
                Funding::new(btc_keypair(&format!("{name}/funding")), 2).with_batching(false);
            mock.fund(&funding.script, &[400_000; 4]);
            funding
        });
        while chain.sync_btc().unwrap() {}
        let mempool = Arc::new(Mutex::new(Mempool::new(params.clone(), Policy::default())));
        let producer = Producer {
            chain: chain.clone(),
            mempool: mempool.clone(),
            producer: user(&format!("adv/liveness/{name}/producer")).account,
            funding,
            pack: true,
            sync: Arc::new(TemplateSync::default()),
            events: broadcast::channel(16).0,
            shutdown: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            min_interval: Duration::ZERO,
            announce: None,
            cache: Default::default(),
        };
        Instance {
            chain,
            mempool,
            producer,
            _dir: dir,
        }
    }

    /// Syncs with the mock BTC chain and maintains the entry pool like the node does.
    fn sync(&self) {
        while self.chain.sync_btc().unwrap() {}
        ksana_node::ops::apply_chain_changes(&self.chain, &self.mempool, &self.producer.events)
            .unwrap();
    }

    /// Produces one block, retrying while the template is stale; a production error is returned as is.
    fn produce(&self) -> Result<Block, NodeError> {
        self.sync();
        for _ in 0..20 {
            if let Some(block) = self.producer.step()? {
                return Ok(block);
            }
        }
        panic!("block production made no progress");
    }

    fn submit(&self, op: VmOp) {
        let (_, view) = self.chain.tip_and_view().unwrap();
        let state: Arc<dyn StateSource> = Arc::new(view);
        self.mempool.lock().unwrap().insert(op, &state).unwrap();
    }

    /// Accepts a block as a historical block (a sync response, D-056).
    fn accept_history(&self, block: &Block) -> Result<Accepted, NodeError> {
        self.sync();
        self.chain
            .accept_block(block.clone(), None)
            .map(|report| report.outcome)
    }

    /// The KSA balance and the nonce of `u` at the main-chain tip.
    fn account(&self, u: &User) -> (u128, u64) {
        self.chain
            .with_state(|state, _| {
                let balance = state
                    .get(&keys::balance(&KSA_ASSET_ID, &u.account))
                    .unwrap()
                    .map_or(0, |v| borsh::from_slice::<u128>(&v).unwrap());
                let nonce = state
                    .get(&keys::account(&u.account))
                    .unwrap()
                    .map_or(0, |v| borsh::from_slice::<AccountState>(&v).unwrap().nonce);
                (balance, nonce)
            })
            .unwrap()
    }
}

/// Produces the two main-chain blocks a forged block on the genesis block stays a side block against: it has the chain
/// work of one block (D-100), so against a single main-chain block the hashes would decide.
fn two_main_blocks(node: &Instance) {
    for _ in 0..2 {
        assert!(node.produce().unwrap().entries.is_empty());
    }
}

/// A block on the genesis block anchored at `anchor` with `entries`, solved with the test PoW: a side block nobody
/// executes until a later block merges it (D-063), so its state root is arbitrary.
fn forge(node: &Instance, anchor: BtcAnchor, entries: Vec<Entry>) -> Block {
    forge_on(node, node.chain.genesis(), anchor, entries)
}

/// The same on `parent`.
fn forge_on(
    node: &Instance,
    parent: &BlockHeader,
    anchor: BtcAnchor,
    entries: Vec<Entry>,
) -> Block {
    let chain = &node.chain;
    let parent_hash = parent.hash();
    let daa_score = chain.ghostdag(&parent_hash).unwrap().unwrap().daa_score + 1;
    let mut header = BlockHeader {
        version: chain.params().protocol_version(daa_score),
        height: parent.height + 1,
        parents: vec![parent_hash],
        btc_anchor: anchor,
        entries_root: entries_root(&entries).unwrap(),
        state_root: [7; 32],
        producer: user("adv/liveness/forger").account,
        timestamp: parent.timestamp + 1,
        bits: chain.consensus().next_bits(parent, daa_score, 1).unwrap(),
        nonce: 0,
    };
    assert!(chain
        .consensus()
        .solve(&mut header, 1, 0, u64::MAX)
        .unwrap());
    Block {
        header,
        entries,
        settlement: None,
    }
}

/// An anchor that never existed on BTC, at `height`.
fn forged_anchor(height: u32) -> BtcAnchor {
    BtcAnchor {
        height,
        hash: [0x42; 32],
    }
}

/// A path-C op (any signature binding): enough for a `Forced` entry, whose op is only checked when executed.
fn native_op(seed: &str) -> VmOp {
    let (from, to) = (user(&format!("{seed}/from")), user(&format!("{seed}/to")));
    let input = bitcoin::OutPoint {
        txid: bitcoin::Txid::from_byte_array([0x5a; 32]),
        vout: 0,
    };
    signed_native(&from, 0, &input, to.account, 1)
}

/// `count` native transactions (path C) in one new BTC block (after the block funding them): each spends its own
/// output and carries an op that fails at execution (a native transaction is recognized without checking the op's
/// signature, §7.4, D-086). Returns their IDs in block order.
fn native_flood(mock: &MockBtc, count: usize) -> Vec<[u8; 32]> {
    let inputs = mock.fund(&coinbase_script(), &vec![10_000; count]);
    let spammer = user("adv/liveness/spammer");
    let txs: Vec<Transaction> = inputs
        .iter()
        .zip(0u64..)
        .map(|(input, nonce)| {
            let op = VmOp {
                version: VMOP_VERSION,
                chain_id: DEVNET.chain_id,
                account: spammer.account,
                nonce,
                fee: FeeMode::BtcNative,
                max_gas: 10_000,
                action: Action::AssetTransfer {
                    asset: KSA_ASSET_ID,
                    to: spammer.account,
                    amount: 1,
                },
                sig: SigScheme::Bip340([0; 64]),
            };
            let payload = Payload::NativeOp { op }.encode(&DEVNET).unwrap();
            Transaction {
                version: transaction::Version::TWO,
                lock_time: absolute::LockTime::ZERO,
                input: vec![TxIn {
                    previous_output: *input,
                    script_sig: bitcoin::ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                }],
                output: vec![TxOut {
                    value: Amount::ZERO,
                    script_pubkey: ksana_btc::data::data_script(&DEVNET.magic, &payload).unwrap(),
                }],
            }
        })
        .collect();
    let ids = txs
        .iter()
        .map(|tx| tx.compute_txid().to_byte_array())
        .collect();
    mock.mine_block(txs).unwrap();
    ids
}

fn forced_txids(block: &Block) -> Vec<[u8; 32]> {
    block
        .entries
        .iter()
        .filter_map(|e| match e {
            Entry::Forced { btc_txid, .. } => Some(*btc_txid),
            _ => None,
        })
        .collect()
}

fn unlock_of(block: &Block) -> Option<(u64, u32)> {
    block.entries.iter().find_map(|e| match e {
        Entry::Unlock { height, index } => Some((*height, *index)),
        _ => None,
    })
}

/// The payload of the block's settlement transactions, with its encoded length.
fn settlement_payload(params: &Params, block: &Block) -> (Payload, usize) {
    let pair = SettlementPair::decode(block.settlement.as_ref().unwrap()).unwrap();
    let bytes = attached_payload(params, &pair).unwrap();
    (Payload::decode(&bytes, params).unwrap(), bytes.len())
}

/// An anchor-invalid block (D-056, accepted as a sync response once its anchor lags the BTC tip by more than
/// `MAX_ANCHOR_LAG + ANCHOR_DEPTH`) whose forced-inclusion entry names a transaction that is not a native transaction
/// on the BTC best chain, unknown or plain, is refused when it arrives (D-100), so no producer or indexer ever has to
/// merge it: with only forced-inclusion entries it is invalid (they are not the ones its window gives); with a body
/// this copy is refused without marking the block invalid (the snapshot holds only native transactions, and an
/// unknown one may still be confirmed). Production goes on.
#[test]
fn an_anchor_invalid_block_naming_no_native_transaction_is_refused() {
    let (mock, genesis) = setup(&[]);
    let a = Instance::open(&mock, &genesis, "no-native", &DEVNET, false);
    two_main_blocks(&a);
    let plain = mock.fund(&coinbase_script(), &[10_000])[0];
    mock.mine(DEVNET.max_anchor_lag + DEVNET.anchor_depth + 1);
    let (alice, bob) = (
        user("adv/liveness/no-native/alice"),
        user("adv/liveness/no-native/bob"),
    );
    for txid in [[0xab; 32], plain.txid.to_byte_array()] {
        let forced = Entry::Forced {
            btc_txid: txid,
            op: native_op("adv/liveness/no-native"),
        };
        let bare = forge(
            &a,
            forged_anchor(genesis.btc_anchor.height),
            vec![forced.clone()],
        );
        let refused = a.accept_history(&bare);
        assert!(
            matches!(&refused, Err(NodeError::InvalidBlock(m)) if m.contains("D-100")),
            "{refused:?}"
        );
        assert!(a.chain.is_invalid(&bare.header.hash()).unwrap());
        let bodied = forge(
            &a,
            forged_anchor(genesis.btc_anchor.height),
            vec![
                forced,
                Entry::Sponsored {
                    op: transfer_op(&alice, 0, 1, bob.account, 1),
                },
            ],
        );
        let refused = a.accept_history(&bodied);
        assert!(
            matches!(&refused, Err(NodeError::Rejected(m)) if m.contains("not a native transaction")),
            "{refused:?}"
        );
        assert!(!a.chain.is_invalid(&bodied.header.hash()).unwrap());
    }
    let block = a.produce().unwrap();
    assert_eq!(a.chain.tip().unwrap().hash, block.header.hash());
}

/// An anchor-invalid side block whose forced-inclusion entry is a native transaction confirmed in the last
/// `ANCHOR_DEPTH` BTC blocks, above the anchor of the producer's next block: the transaction does not yet have the
/// confirmations forced inclusion asks for, so this copy is refused without marking the block (D-100). Once BTC
/// advances `ANCHOR_DEPTH` blocks the block is accepted as a side block, but no template merges it: no block merging a
/// block anchored off the BTC best chain could become BTC-final (D-101), so the producer's next block leaves it out and
/// force-includes the transaction itself.
#[test]
fn an_anchor_invalid_block_waits_until_its_forced_transaction_is_deep_enough() {
    let params = Params {
        anchor_depth: 2,
        ..DEVNET
    };
    let (alice, bob, carol, dave) = (
        user("adv/liveness/late/alice"),
        user("adv/liveness/late/bob"),
        user("adv/liveness/late/carol"),
        user("adv/liveness/late/dave"),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000), (&carol, 100)]);
    let a = Instance::open(&mock, &genesis, "late", &params, false);
    mock.mine(3);
    two_main_blocks(&a);
    let (native, op) = native_tx(&mock, |input| {
        signed_native(&carol, 0, input, dave.account, 5)
    });
    mock.mine_block(vec![native.clone()]).unwrap();
    let txid = native.compute_txid().to_byte_array();
    let forged = forge(
        &a,
        forged_anchor(genesis.btc_anchor.height),
        vec![
            Entry::Forced { btc_txid: txid, op },
            Entry::Sponsored {
                op: transfer_op(&alice, 0, 2, bob.account, 5),
            },
        ],
    );
    let refused = a.accept_history(&forged);
    assert!(
        matches!(refused, Err(NodeError::Rejected(_))),
        "{refused:?}"
    );
    assert!(!a.chain.is_invalid(&forged.header.hash()).unwrap());

    mock.mine(params.anchor_depth);
    assert_eq!(a.accept_history(&forged).unwrap(), Accepted::Side);
    let block = a.produce().unwrap();
    assert!(!block.header.parents.contains(&forged.header.hash()));
    assert_eq!(forced_txids(&block), vec![txid]);
    assert_eq!(a.account(&bob).0, 0);
    assert_eq!(a.account(&dave).0, 5);
    assert_eq!(a.account(&carol).1, 1);
}

/// Second audit, P4a-2: a block on an old main-chain block is a tip beyond the merge depth of the main-chain tip for
/// good, cheap to create and never mergeable on the main chain (a merged block needs `blue_score + merge_depth` at
/// least the selected parent's). The template no longer tries such tips, and it merges the tips around the boundary
/// as before: with the main-chain tip at blue score 6 and merge depth 4, the merge-depth root is the main-chain block
/// at blue score 2 (D-097); the tip on it is merged, the tip on the block below it (within the merge depth by blue
/// score, but below the root) is tried and left out, and the tips on the genesis block are not tried.
#[test]
fn tips_beyond_the_merge_depth_are_not_merged_and_do_not_stop_a_merge() {
    let params = Params {
        merge_depth: 4,
        ..DEVNET
    };
    let (alice, bob) = (
        user("adv/liveness/dead-tips/alice"),
        user("adv/liveness/dead-tips/bob"),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Instance::open(&mock, &genesis, "dead-tips", &params, false);
    let mut main = vec![a.chain.genesis().clone()];
    for _ in 0..6 {
        main.push(a.produce().unwrap().header);
    }
    let score = |header: &BlockHeader| {
        a.chain
            .ghostdag(&header.hash())
            .unwrap()
            .unwrap()
            .blue_score
    };
    assert_eq!(score(&main[6]), 6);
    assert_eq!(score(&main[2]), 2);

    // Side blocks with one transfer each (forge_on solves at weight 1, which an empty block would not have)
    let side = |parent: &BlockHeader, nonce: u64| {
        let op = transfer_op(&alice, nonce, 1, bob.account, 1);
        let entries = vec![Entry::Sponsored { op }];
        let block = forge_on(&a, parent, parent.btc_anchor.clone(), entries);
        assert_eq!(a.accept_history(&block).unwrap(), Accepted::Side);
        block.header.hash()
    };
    let on_root = side(&main[2], 0);
    let below_root = side(&main[1], 1);
    let dead: Vec<_> = (2..22).map(|nonce| side(&main[0], nonce)).collect();
    let tips = a.chain.dag_tips().unwrap();
    assert!([on_root, below_root]
        .iter()
        .chain(&dead)
        .all(|t| tips.contains(t)));

    let block = a.produce().unwrap();
    let parents = &block.header.parents;
    assert_eq!(parents.len(), 2);
    assert!(parents.contains(&main[6].hash()) && parents.contains(&on_root));
    // The others stay tips nobody merges
    let next = a.produce().unwrap();
    assert_eq!(next.header.parents, vec![block.header.hash()]);
    let tips = a.chain.dag_tips().unwrap();
    assert!(std::iter::once(&below_root)
        .chain(&dead)
        .all(|t| tips.contains(t)));
}

/// A BTC block full of native transactions: their forced-inclusion entries alone take more than one payload (§7.4).
/// The unlock entry due in the same block would make it need a body that no payload can carry; the funded producer
/// used to fail every round assembling its settlement payload. Now the unlock waits for the next block, and the block
/// with only forced-inclusion entries needs no body.
#[test]
fn an_unlock_that_would_make_the_body_too_large_waits_for_a_later_block() {
    let params = Params {
        max_block_data_bytes: 2_048,
        ..DEVNET
    };
    let (alice, bob) = (
        user("adv/liveness/unlock/alice"),
        user("adv/liveness/unlock/bob"),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Instance::open(&mock, &genesis, "unlock", &params, true);
    a.submit(transfer_op(&alice, 0, 5, bob.account, 10));
    assert!(a.produce().unwrap().settlement.is_some());
    for _ in 0..params.settlement_depth {
        a.produce().unwrap();
    }
    // Block 1's settlement lands: its position can be unlocked
    a.chain.process_settlements().unwrap();
    mock.mine(1);
    let flood = native_flood(&mock, 70);

    let block = a
        .produce()
        .expect("forced-inclusion entries must not stop production");
    assert_eq!(forced_txids(&block), flood);
    assert_eq!(block.entries.len(), flood.len(), "the unlock waits");
    assert!(block.settlement.is_none(), "the block needs no body");
    let next = a.produce().unwrap();
    assert_eq!(unlock_of(&next), Some((1, 0)));
    let (_, len) = settlement_payload(&params, &next);
    assert!(len <= params.max_block_data_bytes);
}

/// The same flood with an unlock that still fits in this block's body, while the other blocks' headers waiting to be
/// carried (a long run of empty blocks, D-075) do not fit next to it: the settlement payload carries this block's
/// data only, instead of failing.
#[test]
fn a_settlement_payload_too_large_with_the_carried_data_carries_only_its_own() {
    let params = Params {
        max_block_data_bytes: 2_048,
        ..DEVNET
    };
    let (alice, bob) = (
        user("adv/liveness/carry/alice"),
        user("adv/liveness/carry/bob"),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Instance::open(&mock, &genesis, "carry", &params, true);
    a.submit(transfer_op(&alice, 0, 5, bob.account, 10));
    assert!(a.produce().unwrap().settlement.is_some());
    for _ in 0..60 {
        a.produce().unwrap();
    }
    a.chain.process_settlements().unwrap();
    mock.mine(1);
    let flood = native_flood(&mock, 40);

    let block = a
        .produce()
        .expect("forced-inclusion entries must not stop production");
    assert_eq!(forced_txids(&block), flood);
    assert_eq!(unlock_of(&block), Some((1, 0)));
    let (payload, len) = settlement_payload(&params, &block);
    assert!(len <= params.max_block_data_bytes);
    let Payload::Settlement {
        headers, bodies, ..
    } = payload
    else {
        panic!("not a settlement payload");
    };
    assert_eq!(headers, vec![block.header.clone()]);
    assert!(bodies.is_empty());
}

/// The flood again, with another producer's body waiting to be carried (its producer has no funding wallet, D-095):
/// the block has only forced-inclusion entries and its own settlement payload would exceed `MAX_BLOCK_DATA_BYTES`
/// with or without that body. It carries no settlement transactions; the next block carries the body.
#[test]
fn a_block_whose_settlement_payload_cannot_fit_carries_none() {
    let params = Params {
        max_block_data_bytes: 2_048,
        ..DEVNET
    };
    let (alice, bob) = (
        user("adv/liveness/skip/alice"),
        user("adv/liveness/skip/bob"),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Instance::open(&mock, &genesis, "skip-a", &params, true);
    let u = Instance::open(&mock, &genesis, "skip-u", &params, false);
    u.submit(transfer_op(&alice, 0, 5, bob.account, 10));
    let carried = u.produce().unwrap();
    assert!(carried.settlement.is_none());
    assert!(matches!(
        a.accept_history(&carried).unwrap(),
        Accepted::Extended(_)
    ));
    let flood = native_flood(&mock, 70);

    let block = a
        .produce()
        .expect("forced-inclusion entries must not stop production");
    assert_eq!(forced_txids(&block), flood);
    assert_eq!(block.entries.len(), flood.len());
    assert!(block.settlement.is_none());
    let next = a.produce().unwrap();
    let (payload, len) = settlement_payload(&params, &next);
    assert!(len <= params.max_block_data_bytes);
    let Payload::Settlement {
        headers, bodies, ..
    } = payload
    else {
        panic!("not a settlement payload");
    };
    let index = headers
        .iter()
        .position(|h| *h == carried.header)
        .expect("the other producer's header is carried");
    assert!(bodies.iter().any(|b| b.header as usize == index));
}

/// The `Headers` payloads in the mock BTC mempool, with their encoded length.
fn republished(mock: &MockBtc, params: &Params) -> Vec<(Vec<BlockHeader>, usize)> {
    mock.mempool()
        .iter()
        .filter_map(|tx| {
            let bytes = envelope_payload(params, tx)?;
            match Payload::decode(&bytes, params).ok()? {
                Payload::Headers { headers, .. } => Some((headers, bytes.len())),
                _ => None,
            }
        })
        .collect()
}

/// A long run of empty blocks whose headers no settlement payload will carry (more than `MAX_SETTLEMENT_HEADERS − 1`,
/// D-047, D-075): the republish takes as many as fit. Its size used to be estimated without the 32-byte payee
/// (D-095), so the payload came out a few bytes too large and no republish was ever sent.
#[test]
fn a_republish_of_a_long_run_of_empty_blocks_fits_in_one_payload() {
    let params = Params {
        max_block_data_bytes: 1_024,
        max_settlement_headers: 60,
        ..DEVNET
    };
    let (mock, genesis) = setup(&[]);
    let a = Instance::open(&mock, &genesis, "republish", &params, false);
    for _ in 0..130 {
        assert!(a.produce().unwrap().entries.is_empty());
    }
    let funding = Funding::new(btc_keypair("republish/rescuer"), 2);
    mock.fund(&funding.script, &[400_000; 4]);
    a.sync();
    let payee = user("adv/liveness/republish/payee").account;
    Rescuer::new(a.chain.clone(), funding, None, Some(payee))
        .run_once()
        .unwrap();

    let sent = republished(&mock, &params);
    assert_eq!(sent.len(), 1, "one republish transaction is broadcast");
    let (headers, len) = &sent[0];
    assert!(*len <= params.max_block_data_bytes, "{len} bytes");
    let heights: Vec<u64> = headers.iter().map(|h| h.height).collect();
    assert_eq!(heights, (1..=heights.len() as u64).collect::<Vec<_>>());
    assert!(
        heights.len() > 50,
        "as many headers as fit: {}",
        heights.len()
    );
}

/// `ksana_call` (SPEC §11.2): `max_gas` defaults to `BLOCK_GAS_LIMIT` and may not exceed it — a contract call that
/// never returns used to run as long as the caller asked; and only a bounded number of simulations run at once, the
/// others are refused as busy.
#[test]
fn ksana_call_is_bounded_by_the_block_gas_limit_and_runs_few_at_once() {
    let alice = user("adv/liveness/call/alice");
    let (mock, genesis) = setup(&[(&alice, 1_000_000)]);
    let a = Instance::open(&mock, &genesis, "call", &DEVNET, false);
    let code = ksana_test_contracts::PROBE.to_vec();
    let contract = ksana_execution::builtin::contract_id(&alice.account, 0, &sha256(&code));
    let mut deploy = VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account: alice.account,
        nonce: 0,
        fee: FeeMode::Token { amount: 100 },
        max_gas: 2_000_000,
        action: Action::Deploy {
            code,
            init_args: borsh::to_vec(&Vec::<u8>::new()).unwrap(),
            funds: Vec::new(),
        },
        sig: SigScheme::Bip340([0; 64]),
    };
    deploy.sig = sign_vmop(&deploy, &alice.kp, &[3; 32]).unwrap();
    a.submit(deploy);
    assert_eq!(a.produce().unwrap().entries.len(), 1);
    let rpc = Arc::new(NodeRpc {
        chain: Arc::clone(&a.chain),
        mempool: Arc::clone(&a.mempool),
        sync: None,
        paused: Arc::new(AtomicBool::new(false)),
        events: a.producer.events.clone(),
        relay: None,
        fees: FeePolicy::new(1),
        calls: AtomicUsize::new(0),
    });
    let spin = |max_gas: Option<u64>| CallRequestJson {
        contract,
        method: "spin".into(),
        args: Vec::new(),
        funds: Vec::new(),
        caller: None,
        max_gas,
    };
    let limit = DEVNET.block_gas_limit;

    let receipt = rpc.call(spin(None)).unwrap();
    assert!(!receipt.success);
    assert_eq!(receipt.gas_used, limit);
    assert_eq!(rpc.call(spin(Some(limit))).unwrap().gas_used, limit);
    let refused = rpc
        .call(spin(Some(3 * limit)))
        .expect_err("max_gas above BLOCK_GAS_LIMIT must be refused");
    assert_eq!(refused.code(), ksana_rpc::codes::INVALID_PARAMS);

    let callers = 12;
    let barrier = Arc::new(Barrier::new(callers));
    let handles: Vec<_> = (0..callers)
        .map(|_| {
            let (rpc, barrier, request) = (Arc::clone(&rpc), Arc::clone(&barrier), spin(None));
            std::thread::spawn(move || {
                barrier.wait();
                rpc.call(request).map(|_| ()).map_err(|e| e.code())
            })
        })
        .collect();
    let results: Vec<Result<(), i32>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let busy = results
        .iter()
        .filter(|r| **r == Err(jsonrpsee::types::error::SERVER_IS_BUSY_CODE))
        .count();
    assert!(
        busy > 0 && results.iter().any(Result::is_ok),
        "some calls run, the others are refused as busy: {results:?}"
    );
    assert_eq!(
        results.iter().filter(|r| r.is_err()).count(),
        busy,
        "{results:?}"
    );
}

/// Ops that cannot alter the packing candidates leave the template alone (D-130, A2-08): while ops behind a nonce gap
/// pour into the entry pool, the producer keeps solving one template until its lifetime ends. Before, every op admitted
/// made the template stale after one PoW chunk, so anyone could keep every producer rebuilding for ops no block takes.
#[test]
fn ops_behind_a_nonce_gap_do_not_keep_the_producer_rebuilding() {
    let spammers: Vec<User> = (0..40)
        .map(|i| user(&format!("adv/liveness/churn/{i}")))
        .collect();
    let allocations: Vec<(&User, u128)> = spammers.iter().map(|u| (u, 1_000)).collect();
    let (mock, mut genesis) = setup(&allocations);
    // A target the test PoW does not meet while the test runs, so that a step solves until its template is stale (ASERT
    // is anchored at the genesis block: with its timestamp a month back, the next target would be the PoW limit)
    genesis.timestamp = ksana_node::unix_now();
    genesis.bits = Some(0x1c00_ffff);
    let node = Instance::open(&mock, &genesis, "churn", &DEVNET, false);
    node.sync();
    // Each spammer's nonce 0 is missing: its ops at 1 to 8 are behind a gap, admitted a few milliseconds apart
    let ops: Vec<VmOp> = spammers
        .iter()
        .flat_map(|u| (1..=8).map(move |nonce| transfer_op(u, nonce, 1, [9; 32], 1)))
        .collect();
    let count = ops.len();
    let generation = node.mempool.lock().unwrap().generation();
    let (_, view) = node.chain.tip_and_view().unwrap();
    let state: Arc<dyn StateSource> = Arc::new(view);
    let mempool = Arc::clone(&node.mempool);
    let churn = std::thread::spawn(move || {
        for op in ops {
            mempool.lock().unwrap().insert(op, &state).unwrap();
            std::thread::sleep(Duration::from_millis(4));
        }
    });

    let started = ksana_node::monotonic_now();
    assert!(node.producer.step().unwrap().is_none());
    let solved = started.elapsed();
    churn.join().unwrap();
    // The template's lifetime of 2 s ended the step, not the churn
    assert!(
        solved >= Duration::from_secs(2),
        "the step ended after {solved:?}"
    );
    let pool = node.mempool.lock().unwrap();
    assert_eq!((pool.len(), pool.generation()), (count, generation));
}

/// Spends the first funding input of the block's settlement transactions on BTC (the producer's wallet used it
/// elsewhere): those transactions can never enter the mempool again and the block awaits a rescue (§7.3, D-045).
fn spend_settlement_input(mock: &MockBtc, block: &Block) {
    let pair = SettlementPair::decode(block.settlement.as_ref().unwrap()).unwrap();
    mock.mine_block(vec![Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: pair.commit.input[0].previous_output,
            script_sig: bitcoin::ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(1_000),
            script_pubkey: coinbase_script(),
        }],
    }])
    .unwrap();
}

/// A commit / reveal pair (D-081) whose envelope carries `payload`, funded by a fresh key at `feerate` sat/vB and put
/// into the BTC mempool: anyone can send such a pair.
fn send_claim(mock: &MockBtc, seed: &str, payload: &[u8], feerate: u64) -> SettlementPair {
    let key = btc_keypair(seed);
    let xonly = key.x_only_public_key().0;
    let script = producer_script(&xonly);
    let outpoint = mock.fund(&script, &[200_000])[0];
    let utxo = ProducerUtxo {
        outpoint,
        txout: TxOut {
            value: Amount::from_sat(200_000),
            script_pubkey: script.clone(),
        },
    };
    let draft = draft(&DEVNET, &[utxo], script, &xonly, payload, feerate).unwrap();
    let pair = finalize(&DEVNET, draft, payload, &key).unwrap();
    mock.submit(pair.commit.clone()).unwrap();
    mock.submit(pair.reveal.clone()).unwrap();
    pair
}

/// A funded producer's block whose settlement transactions cannot be broadcast (their funding input was spent) and
/// that has reached `SETTLEMENT_DEPTH`, with this producer's rescuer (it rescues its own blocks at once, D-057).
fn stuck_block(seed: &str) -> (Arc<MockBtc>, Instance, Block, Rescuer) {
    let (alice, bob) = (
        user(&format!("adv/liveness/{seed}/alice")),
        user(&format!("adv/liveness/{seed}/bob")),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Instance::open(&mock, &genesis, seed, &DEVNET, true);
    a.submit(transfer_op(&alice, 0, 2, bob.account, 7));
    let stuck = a.produce().unwrap();
    spend_settlement_input(&mock, &stuck);
    for _ in 0..DEVNET.settlement_depth {
        a.produce().unwrap();
    }
    let rescuer = Rescuer::new(
        a.chain.clone(),
        a.producer.funding.clone().unwrap(),
        Some(a.producer.producer),
        None,
    );
    (mock, a, stuck, rescuer)
}

/// The block's settlement payload.
fn payload_of(block: &Block) -> Vec<u8> {
    let pair = SettlementPair::decode(block.settlement.as_ref().unwrap()).unwrap();
    attached_payload(&DEVNET, &pair).unwrap()
}

/// The reveal transactions in the BTC mempool carrying exactly the block's settlement payload, other than `except`.
fn rescues(mock: &MockBtc, block: &Block, except: &[[u8; 32]]) -> Vec<Transaction> {
    let payload = payload_of(block);
    mock.mempool()
        .into_iter()
        .filter(|tx| !except.contains(&tx.compute_txid().to_byte_array()))
        .filter(|tx| envelope_payload(&DEVNET, tx).as_ref() == Some(&payload))
        .collect()
}

/// A transaction in the BTC mempool naming the stuck block as the last header of a settlement payload that carries
/// none of its entries (it would land nothing) used to stop every node from rescuing the block.
#[test]
fn a_fake_settlement_claim_in_the_mempool_does_not_stop_a_rescue() {
    let (mock, a, stuck, rescuer) = stuck_block("fake-claim");
    let fake = Payload::Settlement {
        headers: vec![stuck.header.clone()],
        entries: Vec::new(),
        bodies: Vec::new(),
    }
    .encode(&DEVNET)
    .unwrap();
    send_claim(&mock, "fake-claim/adversary", &fake, 1);
    a.sync();
    rescuer.run_once().unwrap();
    assert_eq!(rescues(&mock, &stuck, &[]).len(), 1, "the block is rescued");
}

/// A claim carrying the block's exact payload but paying a lower fee rate than this node's rescue would (it may never
/// confirm) does not stop the rescue either.
#[test]
fn a_settlement_claim_paying_less_than_this_node_does_not_stop_a_rescue() {
    let (mock, a, stuck, rescuer) = stuck_block("cheap-claim");
    let claim = send_claim(&mock, "cheap-claim/other", &payload_of(&stuck), 1);
    a.sync();
    rescuer.run_once().unwrap();
    assert_eq!(
        rescues(&mock, &stuck, &[claim.txid()]).len(),
        1,
        "the block is rescued by this node too"
    );
}

/// A claim carrying the block's exact payload at this node's fee rate holds the rescue back (another node is rescuing
/// it, D-057), but only for a while: when it has not confirmed `CLAIM_PATIENCE` (`RESCUE_ORDER_WINDOW + RESCUE_DELAY`)
/// BTC blocks after this node first saw it, this node rescues the block as well.
#[test]
fn a_settlement_claim_that_does_not_confirm_stops_holding_a_rescue_back() {
    let (mock, a, stuck, rescuer) = stuck_block("held-claim");
    let claim = send_claim(&mock, "held-claim/other", &payload_of(&stuck), 2);
    mock.hold(&claim.commit_txid());
    mock.hold(&claim.txid());
    a.sync();
    rescuer.run_once().unwrap();
    assert!(
        rescues(&mock, &stuck, &[claim.txid()]).is_empty(),
        "another node's rescue in the mempool is honored"
    );
    mock.mine(CLAIM_PATIENCE - 1);
    a.sync();
    rescuer.run_once().unwrap();
    assert!(rescues(&mock, &stuck, &[claim.txid()]).is_empty());
    mock.mine(1);
    a.sync();
    rescuer.run_once().unwrap();
    assert!(mock
        .mempool()
        .iter()
        .any(|tx| tx.compute_txid().to_byte_array() == claim.txid()));
    assert_eq!(
        rescues(&mock, &stuck, &[claim.txid()]).len(),
        1,
        "a claim that does not confirm stops holding the rescue back"
    );
}

/// A BTC reorg can confirm a block's settlement transactions again at or below the block's anchor height, where their
/// payload does not count (D-100): they never land the block's data, although bitcoind knows them and a rebroadcast
/// reports success. Such a reorg has taken the block's anchor off the BTC best chain, so the block leaves the main
/// chain (D-104) and nothing rescues it (the rescue of second audit P4b-1 no longer has a block to land): its op is
/// included again by a block anchored on the new branch, whose own settlement lands.
#[test]
fn a_settlement_confirmed_at_its_anchor_height_after_a_btc_reorg_leaves_its_block_behind() {
    let (alice, bob) = (
        user("adv/liveness/remined/alice"),
        user("adv/liveness/remined/bob"),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Instance::open(&mock, &genesis, "remined", &DEVNET, true);
    // a BTC block above the funding one for the block to anchor to
    mock.mine(1);
    let op = transfer_op(&alice, 0, 2, bob.account, 7);
    a.submit(op.clone());
    let block = a.produce().unwrap();
    let anchor = block.header.btc_anchor.clone();
    assert_eq!(anchor.height, mock.tip_height());
    for _ in 0..DEVNET.settlement_depth {
        a.produce().unwrap();
    }
    a.chain.process_settlements().unwrap();
    let pair = SettlementPair::decode(block.settlement.as_ref().unwrap()).unwrap();
    assert!(mock
        .mempool()
        .iter()
        .any(|tx| tx.compute_txid().to_byte_array() == pair.txid()));
    // the anchor block is reorged out, and its replacement at the same height confirms the settlement
    mock.reorg(1);
    mock.mine(1);
    assert_eq!(mock.tip_height(), anchor.height);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert!(!snapshot.contains(&anchor));
    assert_eq!(
        snapshot.ksn1_height(&pair.txid()),
        None,
        "confirmed without counting"
    );
    // the block and the ones on top of it left the main chain, and the op is back in the entry pool
    assert_eq!(a.chain.tip().unwrap().header.height, 0);
    assert_eq!(a.account(&bob), (0, 0));
    assert!(a.mempool.lock().unwrap().contains(&op.op_id().unwrap()));
    let rescuer = Rescuer::new(
        a.chain.clone(),
        a.producer.funding.clone().unwrap(),
        Some(a.producer.producer),
        None,
    );
    rescuer.run_once().unwrap();
    assert!(
        rescues(&mock, &block, &[]).is_empty(),
        "a block off the main chain is not rescued"
    );
    // a block anchored on the new branch includes the op again, and its settlement lands
    let again = a.produce().unwrap();
    assert_eq!(again.header.height, 1);
    assert_ne!(again.header.btc_anchor, anchor);
    assert!(
        matches!(again.entries.as_slice(), [Entry::Sponsored { op: included }] if *included == op),
        "{:?}",
        again.entries
    );
    for _ in 0..DEVNET.settlement_depth {
        a.produce().unwrap();
    }
    a.chain.process_settlements().unwrap();
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert!(a
        .chain
        .carried_on_chain(&again, &snapshot)
        .unwrap()
        .is_some());
    assert_eq!(a.account(&bob).0, 7);
}

/// The fee rate of this node's BTC transactions is bitcoind's estimate for its confirmation target kept within the
/// floor and the cap; the floor when there is no estimate or bitcoind cannot be asked; the next-block estimate while a
/// lighter competing chain is on BTC (D-102, D-091).
#[test]
fn the_fee_rate_follows_the_estimate_within_the_floor_and_the_cap() {
    let mock = MockBtc::new();
    let fees = FeePolicy {
        floor: 2,
        cap: 50,
        target: DEFAULT_FEE_TARGET,
    };
    assert_eq!(fees.rate(&DEVNET, &mock, false), 2, "no estimate");
    mock.set_fee_estimate(DEFAULT_FEE_TARGET, Some(7));
    assert_eq!(fees.rate(&DEVNET, &mock, false), 7);
    assert_eq!(fees.rate(&DEVNET, &mock, true), 2, "no next-block estimate");
    mock.set_fee_estimate(1, Some(30));
    assert_eq!(fees.rate(&DEVNET, &mock, true), 30, "urgent");
    assert_eq!(fees.rate(&DEVNET, &mock, false), 7);
    mock.set_fee_estimate(1, Some(500));
    assert_eq!(fees.rate(&DEVNET, &mock, true), 50, "the cap");
    mock.set_fee_estimate(DEFAULT_FEE_TARGET, Some(1));
    assert_eq!(fees.rate(&DEVNET, &mock, false), 2, "the floor");
    mock.set_offline(true);
    assert_eq!(fees.rate(&DEVNET, &mock, true), 2, "bitcoind unavailable");
}

/// While bitcoind's mempool holds at most half a block, the next BTC block takes all of it, so this node's BTC
/// transactions pay the floor, or the mempool's own minimum when that is higher, up to the cap, whatever the estimate;
/// not while a lighter competing chain is on BTC, and not when the mempool holds more or cannot be read (D-117).
#[test]
fn the_fee_rate_is_the_floor_while_the_mempool_has_room() {
    let mock = MockBtc::new();
    let fees = FeePolicy {
        floor: 2,
        cap: 50,
        target: DEFAULT_FEE_TARGET,
    };
    mock.set_fee_estimate(1, Some(30));
    mock.set_fee_estimate(DEFAULT_FEE_TARGET, Some(100));
    let mempool = |vsize, min_feerate| {
        mock.set_mempool_info(Some(MempoolInfo { vsize, min_feerate }));
    };
    assert_eq!(
        fees.rate(&DEVNET, &mock, false),
        50,
        "nothing known of the mempool"
    );
    mempool(ROOMY_MEMPOOL_VSIZE, 1);
    assert_eq!(fees.rate(&DEVNET, &mock, false), 2, "half a block");
    assert_eq!(fees.room_rate(&DEVNET, &mock), Some(2));
    assert_eq!(fees.estimated_rate(&DEVNET, &mock, false), 50);
    assert_eq!(
        fees.rate(&DEVNET, &mock, true),
        30,
        "racing a competing chain"
    );
    mempool(1_000, 7);
    assert_eq!(fees.rate(&DEVNET, &mock, false), 7, "the mempool's minimum");
    mempool(1_000, 80);
    assert_eq!(fees.rate(&DEVNET, &mock, false), 50, "up to the cap");
    mempool(ROOMY_MEMPOOL_VSIZE + 1, 1);
    assert_eq!(
        fees.rate(&DEVNET, &mock, false),
        50,
        "more than half a block"
    );
    assert_eq!(fees.room_rate(&DEVNET, &mock), None);
    mock.set_offline(true);
    assert_eq!(fees.room_rate(&DEVNET, &mock), None, "bitcoind unavailable");
    assert_eq!(fees.rate(&DEVNET, &mock, false), 2);
}

/// A pair the room left waiting is bumped to at least its own rate doubled for every BTC block it has waited from
/// `BUMP_AFTER` on, kept within the floor and the cap (D-117).
#[test]
fn the_escalated_rate_doubles_within_the_floor_and_the_cap() {
    let fees = FeePolicy {
        floor: 2,
        cap: 50,
        target: DEFAULT_FEE_TARGET,
    };
    assert_eq!(fees.escalated(&DEVNET, 2, 1), 4);
    assert_eq!(fees.escalated(&DEVNET, 2, 3), 16);
    assert_eq!(fees.escalated(&DEVNET, 2, 5), 50, "the cap");
    assert_eq!(fees.escalated(&DEVNET, 0, 1), 2, "the floor");
    assert_eq!(fees.escalated(&DEVNET, 3, 64), 50, "no overflow");
    assert_eq!(fees.escalated(&DEVNET, u64::MAX, 1), 50);
}

/// The fees (sats) a settlement pair pays: what its funding inputs bring in, less what the commit's change and the
/// reveal's outputs keep.
fn pair_fee(mock: &MockBtc, pair: &SettlementPair) -> u64 {
    let commit = pair.commit.compute_txid();
    let value = |outpoint: &bitcoin::OutPoint| {
        let amount = if outpoint.txid == commit {
            pair.commit.output[outpoint.vout as usize].value
        } else {
            mock.utxo(outpoint, false).unwrap().unwrap().txout.value
        };
        amount.to_sat()
    };
    [&pair.commit, &pair.reveal]
        .into_iter()
        .map(|tx| {
            let input: u64 = tx.input.iter().map(|i| value(&i.previous_output)).sum();
            let output: u64 = tx.output.iter().map(|o| o.value.to_sat()).sum();
            input - output
        })
        .sum()
}

/// The vsize of both transactions of a settlement pair.
fn pair_vsize(pair: &SettlementPair) -> u64 {
    (pair.commit.vsize() + pair.reveal.vsize()) as u64
}

/// The fee rate (sat/vB, rounded down) a settlement pair pays.
fn pair_feerate(mock: &MockBtc, pair: &SettlementPair) -> u64 {
    pair_fee(mock, pair) / pair_vsize(pair)
}

/// The fee rate (sat/vB, rounded down) a settlement pair and its CPFP child pay together; before the child's funding
/// input is spent.
fn package_feerate(mock: &MockBtc, pair: &SettlementPair, child: &Transaction) -> u64 {
    let value = |outpoint: &bitcoin::OutPoint| {
        let amount = if outpoint.txid.to_byte_array() == pair.txid() {
            pair.reveal.output[outpoint.vout as usize].value
        } else {
            mock.utxo(outpoint, false).unwrap().unwrap().txout.value
        };
        amount.to_sat()
    };
    let input: u64 = child.input.iter().map(|i| value(&i.previous_output)).sum();
    let output: u64 = child.output.iter().map(|o| o.value.to_sat()).sum();
    (pair_fee(mock, pair) + input - output) / (pair_vsize(pair) + child.vsize() as u64)
}

/// Whether the transaction is in the BTC mempool.
fn in_mempool(mock: &MockBtc, txid: &[u8; 32]) -> bool {
    mock.mempool()
        .iter()
        .any(|tx| tx.compute_txid().to_byte_array() == *txid)
}

/// The transactions in the BTC mempool spending the reveal's P2A output (output 0): CPFP children.
fn children(mock: &MockBtc, pair: &SettlementPair) -> Vec<Transaction> {
    mock.mempool()
        .into_iter()
        .filter(|tx| {
            tx.input.iter().any(|input| {
                input.previous_output.txid.to_byte_array() == pair.txid()
                    && input.previous_output.vout == 0
            })
        })
        .collect()
}

/// A funded producer's block whose settlement transactions, paying the producer's floor of 2 sat/vB and sent once the
/// block reached `SETTLEMENT_DEPTH`, wait in the BTC mempool: with `cheap`, miners take nothing below 5 sat/vB;
/// otherwise miners pass the pair over whatever it pays.
fn waiting_block(seed: &str, cheap: bool) -> (Arc<MockBtc>, Instance, Block, SettlementPair) {
    let (alice, bob) = (
        user(&format!("adv/liveness/{seed}/alice")),
        user(&format!("adv/liveness/{seed}/bob")),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Instance::open(&mock, &genesis, seed, &DEVNET, true);
    if cheap {
        mock.set_min_feerate(5);
    }
    a.submit(transfer_op(&alice, 0, 2, bob.account, 7));
    let block = a.produce().unwrap();
    let pair = SettlementPair::decode(block.settlement.as_ref().unwrap()).unwrap();
    if !cheap {
        mock.hold(&pair.commit_txid());
        mock.hold(&pair.txid());
    }
    for _ in 0..DEVNET.settlement_depth {
        a.produce().unwrap();
    }
    a.chain.process_settlements().unwrap();
    assert!(in_mempool(&mock, &pair.txid()));
    mock.mine(1);
    a.sync();
    assert!(in_mempool(&mock, &pair.txid()), "not mined");
    (mock, a, block, pair)
}

/// A producer's settlement left waiting in the BTC mempool by a fee spike is bumped through the reveal's P2A output
/// once it has waited `BUMP_AFTER` BTC blocks (D-102): the child lifts the package to the producer's rate, now the
/// estimate, and miners take the pair with it. Nothing raised a stuck fee before.
#[test]
fn a_settlement_left_behind_by_a_fee_spike_is_bumped_by_cpfp() {
    let (mock, a, block, pair) = waiting_block("bump", true);
    let rescuer = Rescuer::new(
        a.chain.clone(),
        a.producer.funding.clone().unwrap(),
        Some(a.producer.producer),
        None,
    );
    mock.set_fee_estimate(DEFAULT_FEE_TARGET, Some(10));
    rescuer.run_once().unwrap();
    for _ in 1..BUMP_AFTER {
        mock.mine(1);
        a.sync();
        rescuer.run_once().unwrap();
    }
    assert!(
        children(&mock, &pair).is_empty(),
        "not bumped before BUMP_AFTER"
    );
    assert!(
        rescues(&mock, &block, &[pair.txid()]).is_empty(),
        "the producer does not rescue its own waiting block"
    );
    mock.mine(1);
    a.sync();
    rescuer.run_once().unwrap();
    let child = children(&mock, &pair);
    assert_eq!(child.len(), 1, "bumped");
    let child_txid = child[0].compute_txid().to_byte_array();
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    let (tip, _) = snapshot.tip().unwrap();
    assert_eq!(
        snapshot.ksn1_height(&pair.txid()),
        Some(tip),
        "the pair landed"
    );
    assert!(!in_mempool(&mock, &child_txid), "with its child");
    assert!(a
        .chain
        .carried_on_chain(&block, &snapshot)
        .unwrap()
        .is_some());
}

/// What bitcoind tells of a mempool with room for the next BTC block (D-117): 1 000 vB waiting, the relay minimum of
/// 1 sat/vB.
const ROOM: MempoolInfo = MempoolInfo {
    vsize: 1_000,
    min_feerate: 1,
};

/// A funded producer's block whose settlement, assembled while bitcoind's mempool has room and bitcoind estimates
/// `estimate` (sat/vB) for the producer's target, pays the floor of 2 sat/vB (D-117), and waits in the BTC mempool
/// once sent at `SETTLEMENT_DEPTH`, as miners take nothing below `min_feerate`; with the producer's rescuer, which has
/// seen it waiting.
fn room_block(
    seed: &str,
    estimate: Option<u64>,
    min_feerate: u64,
) -> (Arc<MockBtc>, Instance, Block, SettlementPair, Rescuer) {
    let (alice, bob) = (
        user(&format!("adv/liveness/{seed}/alice")),
        user(&format!("adv/liveness/{seed}/bob")),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Instance::open(&mock, &genesis, seed, &DEVNET, true);
    mock.set_fee_estimate(DEFAULT_FEE_TARGET, estimate);
    mock.set_mempool_info(Some(ROOM));
    mock.set_min_feerate(min_feerate);
    a.submit(transfer_op(&alice, 0, 2, bob.account, 7));
    let block = a.produce().unwrap();
    let pair = SettlementPair::decode(block.settlement.as_ref().unwrap()).unwrap();
    assert_eq!(pair_feerate(&mock, &pair), 2, "the floor, not the estimate");
    for _ in 0..DEVNET.settlement_depth {
        a.produce().unwrap();
    }
    a.chain.process_settlements().unwrap();
    assert!(in_mempool(&mock, &pair.txid()));
    let rescuer = Rescuer::new(
        a.chain.clone(),
        a.producer.funding.clone().unwrap(),
        Some(a.producer.producer),
        None,
    );
    rescuer.run_once().unwrap();
    (mock, a, block, pair, rescuer)
}

/// Whether the pair confirmed in the BTC tip.
fn landed_at_tip(a: &Instance, pair: &SettlementPair) -> bool {
    let snapshot = a.chain.btc().snapshot().unwrap();
    let (tip, _) = snapshot.tip().unwrap();
    snapshot.ksn1_height(&pair.txid()) == Some(tip)
}

/// While bitcoind's mempool has room, a producer's settlement pays the floor whatever the estimate (D-117). When the
/// miners leave it waiting all the same (here they take nothing below 5 sat/vB), it is bumped to the estimate of 10,
/// above twice its own rate, once it has waited `BUMP_AFTER` BTC blocks, the mempool's room notwithstanding, and lands
/// with its child.
#[test]
fn a_settlement_pays_the_floor_while_the_mempool_has_room() {
    let (mock, a, _, pair, rescuer) = room_block("room", Some(10), 5);
    for _ in 0..BUMP_AFTER {
        assert!(
            children(&mock, &pair).is_empty(),
            "not bumped before BUMP_AFTER"
        );
        mock.mine(1);
        a.sync();
        assert!(in_mempool(&mock, &pair.txid()), "not mined");
        rescuer.run_once().unwrap();
    }
    let child = children(&mock, &pair);
    assert_eq!(child.len(), 1, "bumped");
    assert_eq!(
        package_feerate(&mock, &pair, &child[0]),
        10,
        "to the estimate"
    );
    mock.mine(1);
    a.sync();
    assert!(landed_at_tip(&a, &pair), "the pair landed");
}

/// A settlement the room left waiting is bumped even when bitcoind has no estimate (a new data directory, a
/// `-blocksonly` bitcoind) or estimates no more than the floor the pair pays (D-117, the review of the change): once the
/// pair has waited `BUMP_AFTER` BTC blocks while the mempool had room, its package is lifted to twice the pair's own
/// rate. Here miners take nothing below 3 sat/vB; a bump to the estimate alone, the floor the pair pays, never came.
#[test]
fn a_settlement_the_room_left_waiting_is_bumped_without_an_estimate() {
    let (mock, a, _, pair, rescuer) = room_block("room-blind", None, 3);
    for _ in 0..BUMP_AFTER {
        assert!(
            children(&mock, &pair).is_empty(),
            "not bumped before BUMP_AFTER"
        );
        mock.mine(1);
        a.sync();
        assert!(in_mempool(&mock, &pair.txid()), "not mined");
        rescuer.run_once().unwrap();
    }
    let child = children(&mock, &pair);
    assert_eq!(child.len(), 1, "bumped");
    assert_eq!(
        package_feerate(&mock, &pair, &child[0]),
        4,
        "twice the pair's rate"
    );
    mock.mine(1);
    a.sync();
    assert!(landed_at_tip(&a, &pair), "the pair landed");
}

/// Only the BTC blocks a pair has waited through while the mempool had room count against its rate (D-117): without
/// room it waits for the estimate as before (D-102), here none above the floor it pays, however long; once the room is
/// there, it is bumped to twice its rate after `BUMP_AFTER` BTC blocks more, not at once to four times its rate or more
/// for blocks that had no space for it.
#[test]
fn only_the_blocks_a_pair_waited_with_room_count_against_its_rate() {
    let (mock, a, _, pair, rescuer) = room_block("room-gone", None, 3);
    mock.set_mempool_info(Some(MempoolInfo {
        vsize: ROOMY_MEMPOOL_VSIZE + 1,
        min_feerate: 1,
    }));
    for _ in 0..=BUMP_AFTER {
        mock.mine(1);
        a.sync();
        rescuer.run_once().unwrap();
        assert!(
            children(&mock, &pair).is_empty(),
            "without room the estimate, the floor the pair pays"
        );
    }
    mock.set_mempool_info(Some(ROOM));
    for _ in 0..BUMP_AFTER {
        rescuer.run_once().unwrap();
        assert!(
            children(&mock, &pair).is_empty(),
            "not bumped before BUMP_AFTER BTC blocks with room"
        );
        mock.mine(1);
        a.sync();
        assert!(in_mempool(&mock, &pair.txid()), "not mined");
    }
    rescuer.run_once().unwrap();
    let child = children(&mock, &pair);
    assert_eq!(child.len(), 1, "bumped");
    assert_eq!(
        package_feerate(&mock, &pair, &child[0]),
        4,
        "twice the pair's rate"
    );
    mock.mine(1);
    a.sync();
    assert!(landed_at_tip(&a, &pair), "the pair landed");
}

/// The rate a pair the room left waiting is bumped to doubles again for every further BTC block it waits (D-117):
/// bumped first after `BUMP_AFTER` + 1 BTC blocks (the mock has no RBF, so a first child stands in for the one
/// replacing an earlier child), it is lifted to four times its rate, which miners taking nothing below 5 sat/vB accept.
#[test]
fn a_pair_the_room_left_waiting_longer_is_bumped_further() {
    let (mock, a, _, pair, rescuer) = room_block("room-longer", None, 5);
    mock.mine(BUMP_AFTER + 1);
    a.sync();
    assert!(in_mempool(&mock, &pair.txid()), "not mined");
    rescuer.run_once().unwrap();
    let child = children(&mock, &pair);
    assert_eq!(child.len(), 1, "bumped");
    assert_eq!(
        package_feerate(&mock, &pair, &child[0]),
        8,
        "four times the pair's rate"
    );
    mock.mine(1);
    a.sync();
    assert!(landed_at_tip(&a, &pair), "the pair landed");
}

/// While bitcoind's mempool has room, a rescue pays the floor like a settlement whatever the estimate (D-117): it is a
/// new transaction of this node, and so are its republishes and funding splits.
#[test]
fn a_rescue_pays_the_floor_while_the_mempool_has_room() {
    let (mock, a, stuck, rescuer) = stuck_block("room-rescue");
    mock.set_fee_estimate(DEFAULT_FEE_TARGET, Some(10));
    mock.set_mempool_info(Some(ROOM));
    a.sync();
    rescuer.run_once().unwrap();
    let rescue = rescues(&mock, &stuck, &[]);
    assert_eq!(rescue.len(), 1, "rescued");
    let reveal = rescue[0].clone();
    let commit_txid = reveal.input[0].previous_output.txid;
    let commit = mock
        .mempool()
        .into_iter()
        .find(|tx| tx.compute_txid() == commit_txid)
        .unwrap();
    assert_eq!(
        pair_feerate(&mock, &SettlementPair { commit, reveal }),
        2,
        "the floor, not the estimate"
    );
}

/// A CPFP child that left the BTC mempool after its pair confirmed without it is not sent again (D-102, the review of
/// M28): it would only pay the package's deficit to sweep the P2A output.
#[test]
fn a_child_left_behind_by_its_confirmed_pair_is_not_sent_again() {
    let (mock, a, _, pair) = waiting_block("bump-left", true);
    let rescuer = Rescuer::new(
        a.chain.clone(),
        a.producer.funding.clone().unwrap(),
        Some(a.producer.producer),
        None,
    );
    mock.set_fee_estimate(DEFAULT_FEE_TARGET, Some(10));
    rescuer.run_once().unwrap();
    for _ in 0..BUMP_AFTER {
        mock.mine(1);
        a.sync();
        rescuer.run_once().unwrap();
    }
    let child = children(&mock, &pair);
    assert_eq!(child.len(), 1, "bumped");
    let child_txid = child[0].compute_txid().to_byte_array();
    // A miner takes the pair without its child, and the child then leaves the mempool (a bitcoind restart)
    mock.mine_block(vec![pair.commit.clone(), pair.reveal.clone()])
        .unwrap();
    assert!(mock.evict(&child_txid));
    a.sync();
    rescuer.run_once().unwrap();
    assert!(children(&mock, &pair).is_empty(), "not sent again");
    mock.mine(1);
    a.sync();
    rescuer.run_once().unwrap();
    assert!(children(&mock, &pair).is_empty());
}

/// A CPFP child of a pair whose block left the main chain is not sent again (D-104 item f): once a BTC reorg took the
/// block's anchor off the best chain, the node retreats below the block and no longer settles it, so a child that left
/// the mempool would only pay for a pair nobody needs, although the pair still waits there.
#[test]
fn a_child_of_a_pair_whose_block_left_the_main_chain_is_not_sent_again() {
    let (alice, bob) = (
        user("adv/liveness/bump-retreat/alice"),
        user("adv/liveness/bump-retreat/bob"),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Instance::open(&mock, &genesis, "bump-retreat", &DEVNET, true);
    mock.set_min_feerate(5);
    // The block anchors above the BTC block that confirmed the funding, which the reorg below then leaves in place
    mock.mine(1);
    a.submit(transfer_op(&alice, 0, 2, bob.account, 7));
    let block = a.produce().unwrap();
    let pair = SettlementPair::decode(block.settlement.as_ref().unwrap()).unwrap();
    for _ in 0..DEVNET.settlement_depth {
        a.produce().unwrap();
    }
    a.chain.process_settlements().unwrap();
    mock.mine(1);
    a.sync();
    assert!(in_mempool(&mock, &pair.txid()), "not mined");
    let rescuer = Rescuer::new(
        a.chain.clone(),
        a.producer.funding.clone().unwrap(),
        Some(a.producer.producer),
        None,
    );
    mock.set_fee_estimate(DEFAULT_FEE_TARGET, Some(10));
    rescuer.run_once().unwrap();
    for _ in 0..BUMP_AFTER {
        mock.mine(1);
        a.sync();
        rescuer.run_once().unwrap();
    }
    let child = children(&mock, &pair);
    assert_eq!(child.len(), 1, "bumped");
    let child_txid = child[0].compute_txid().to_byte_array();

    // A BTC reorg takes the block's anchor off the best chain while the pair and its child wait in the mempool
    for txid in [pair.commit_txid(), pair.txid(), child_txid] {
        mock.hold(&txid);
    }
    let depth = mock.tip_height() - block.header.btc_anchor.height + 1;
    mock.reorg(depth);
    mock.mine(depth + 1);
    a.sync();
    assert_eq!(
        a.chain.tip().unwrap().header.height,
        block.header.height - 1,
        "retreated below the block"
    );
    assert!(in_mempool(&mock, &pair.txid()) && in_mempool(&mock, &child_txid));

    // The child leaves the mempool (a bitcoind restart) while the pair still waits: it is not sent again
    assert!(mock.evict(&child_txid));
    rescuer.run_once().unwrap();
    assert!(children(&mock, &pair).is_empty(), "not sent again");
    assert!(in_mempool(&mock, &pair.txid()));
    mock.mine(1);
    a.sync();
    rescuer.run_once().unwrap();
    assert!(children(&mock, &pair).is_empty());
}

/// Another party's child of the pair, put into the BTC mempool first: it spends the reveal's P2A output (anyone can,
/// D-102) and `source`, an output of its own of 100 000 sats, and pays 1 sat/vB for about 20 000 vB, so that a child
/// of this node's would have to pay more than its whole fee to replace it (BIP125 rule 3; the mock refuses any
/// conflict), and the package stays below what the miners take.
fn pin(mock: &MockBtc, pair: &SettlementPair, source: bitcoin::OutPoint) -> [u8; 32] {
    let anchor = bitcoin::OutPoint {
        txid: pair.reveal.compute_txid(),
        vout: 0,
    };
    let data = bitcoin::script::PushBytesBuf::try_from(vec![0u8; 20_000]).unwrap();
    let mut tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: [anchor, source]
            .into_iter()
            .map(|previous_output| TxIn {
                previous_output,
                script_sig: bitcoin::ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            })
            .collect(),
        output: vec![
            TxOut {
                value: Amount::ZERO,
                script_pubkey: bitcoin::ScriptBuf::new_op_return(data),
            },
            TxOut {
                value: Amount::ZERO,
                script_pubkey: coinbase_script(),
            },
        ],
    };
    let input = pair.reveal.output[0].value.to_sat() + 100_000;
    tx.output[1].value = Amount::from_sat(input - tx.vsize() as u64);
    mock.submit(tx).unwrap()
}

/// The settlement pair of a reveal transaction in the BTC mempool, whose commit transaction waits there too.
fn pair_in_mempool(mock: &MockBtc, reveal: Transaction) -> SettlementPair {
    let commit_txid = reveal.input[0].previous_output.txid;
    let commit = mock
        .mempool()
        .into_iter()
        .find(|tx| tx.compute_txid() == commit_txid)
        .unwrap();
    SettlementPair { commit, reveal }
}

/// Whether the funding inputs of the pair are reserved by this node.
fn reserved(a: &Instance, pair: &SettlementPair) -> bool {
    let reserved = a.chain.reserved_inputs().unwrap();
    pair.commit.input.iter().all(|input| {
        reserved.contains(&(
            input.previous_output.txid.to_byte_array(),
            input.previous_output.vout,
        ))
    })
}

/// A lone producer's settlement left waiting by a fee spike, whose P2A output another party's large child at a low rate
/// took first (pinning), goes out again in a new pair of the producer's at the bump's rate once it has waited
/// `BUMP_AFTER` BTC blocks (D-131): bitcoind refuses the producer's CPFP child, which would have to pay more than that
/// child's whole fee, and the producer does not rescue its own waiting settlement (D-102), so the block's data used to
/// wait for as long as the pinned pair did. The new pair lands it, and nothing more is spent on the pinned one.
#[test]
fn a_settlement_whose_p2a_output_another_party_holds_goes_out_again() {
    let (mock, a, block, pair) = waiting_block("pinned", true);
    let source = mock.fund(&coinbase_script(), &[100_000])[0];
    let pinning = pin(&mock, &pair, source);
    a.sync();
    let rescuer = Rescuer::new(
        a.chain.clone(),
        a.producer.funding.clone().unwrap(),
        Some(a.producer.producer),
        None,
    );
    mock.set_fee_estimate(DEFAULT_FEE_TARGET, Some(10));
    rescuer.run_once().unwrap();
    for _ in 1..BUMP_AFTER {
        mock.mine(1);
        a.sync();
        rescuer.run_once().unwrap();
    }
    assert!(
        rescues(&mock, &block, &[pair.txid()]).is_empty(),
        "not before BUMP_AFTER"
    );
    mock.mine(1);
    a.sync();
    rescuer.run_once().unwrap();
    let held: Vec<[u8; 32]> = children(&mock, &pair)
        .iter()
        .map(|tx| tx.compute_txid().to_byte_array())
        .collect();
    assert_eq!(held, [pinning], "the producer's child was refused");
    let again = rescues(&mock, &block, &[pair.txid()]);
    assert_eq!(again.len(), 1, "its payload went out again");
    let again = pair_in_mempool(&mock, again[0].clone());
    assert_eq!(pair_feerate(&mock, &again), 10, "at the bump's rate");
    rescuer.run_once().unwrap();
    assert_eq!(rescues(&mock, &block, &[pair.txid()]).len(), 1, "only once");

    mock.mine(1);
    a.sync();
    assert!(landed_at_tip(&a, &again));
    assert!(in_mempool(&mock, &pair.txid()) && in_mempool(&mock, &pinning));
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert!(a
        .chain
        .carried_on_chain(&block, &snapshot)
        .unwrap()
        .is_some());
    for _ in 0..BUMP_AFTER {
        mock.mine(1);
        a.sync();
        rescuer.run_once().unwrap();
    }
    assert_eq!(children(&mock, &pair).len(), 1, "not bumped");
    assert!(
        rescues(&mock, &block, &[pair.txid()]).is_empty(),
        "not sent again"
    );

    // The block's record closes once the data has BTC_FINALITY_CONFS confirmations; the pinned pair's funding stays
    // reserved while it waits (D-104), or the producer would pick it for a block it then had to discard
    mock.mine(DEVNET.btc_finality_confs);
    a.sync();
    a.chain.process_settlements().unwrap();
    rescuer.run_once().unwrap();
    let hash = block.header.hash();
    assert!(a
        .chain
        .open_settlements()
        .unwrap()
        .iter()
        .all(|(open, _)| *open != hash));
    assert!(in_mempool(&mock, &pair.txid()) && reserved(&a, &pair));
    assert!(mock.evict(&pair.commit_txid()));
    a.chain.process_settlements().unwrap();
    assert!(!reserved(&a, &pair), "released once it is gone");
}

/// A producer's settlement waiting in the BTC mempool is not bumped once its block's data has landed through another
/// party's transaction (D-131): the child would only pay for a payload already on BTC. It used to be bumped until the
/// block's record closed, `BTC_FINALITY_CONFS` BTC blocks later.
#[test]
fn a_settlement_whose_data_landed_otherwise_is_not_bumped() {
    let (mock, a, block, pair) = waiting_block("landed-otherwise", true);
    send_claim(&mock, "landed-otherwise/claim", &payload_of(&block), 10);
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert!(a
        .chain
        .carried_on_chain(&block, &snapshot)
        .unwrap()
        .is_some());
    let rescuer = Rescuer::new(
        a.chain.clone(),
        a.producer.funding.clone().unwrap(),
        Some(a.producer.producer),
        None,
    );
    mock.set_fee_estimate(DEFAULT_FEE_TARGET, Some(10));
    rescuer.run_once().unwrap();
    for _ in 0..BUMP_AFTER {
        mock.mine(1);
        a.sync();
        rescuer.run_once().unwrap();
    }
    assert!(in_mempool(&mock, &pair.txid()));
    assert!(children(&mock, &pair).is_empty(), "not bumped");
    assert!(
        rescues(&mock, &block, &[pair.txid()]).is_empty(),
        "nor sent again"
    );
}

/// A rescue whose P2A output another party's child took first goes out again the same way, in its place (D-131); the
/// funding of the pinned rescue stays reserved while it waits in the BTC mempool (D-104).
#[test]
fn a_rescue_whose_p2a_output_another_party_holds_goes_out_again() {
    let (mock, a, stuck, rescuer) = stuck_block("pinned-rescue");
    mock.set_min_feerate(5);
    let source = mock.fund(&coinbase_script(), &[100_000])[0];
    a.sync();
    rescuer.run_once().unwrap();
    let rescue = rescues(&mock, &stuck, &[]);
    assert_eq!(rescue.len(), 1, "rescued at the floor");
    let first = pair_in_mempool(&mock, rescue[0].clone());
    assert_eq!(pair_feerate(&mock, &first), 2);
    pin(&mock, &first, source);
    mock.set_fee_estimate(DEFAULT_FEE_TARGET, Some(10));
    for _ in 1..BUMP_AFTER {
        mock.mine(1);
        a.sync();
        rescuer.run_once().unwrap();
    }
    assert_eq!(
        rescues(&mock, &stuck, &[]).len(),
        1,
        "not before BUMP_AFTER"
    );
    mock.mine(1);
    a.sync();
    rescuer.run_once().unwrap();
    let again = rescues(&mock, &stuck, &[first.txid()]);
    assert_eq!(again.len(), 1, "its payload went out again");
    let again = pair_in_mempool(&mock, again[0].clone());
    assert_eq!(pair_feerate(&mock, &again), 10, "at the bump's rate");
    assert!(reserved(&a, &first) && reserved(&a, &again));

    mock.mine(1);
    a.sync();
    assert!(landed_at_tip(&a, &again));
    assert!(in_mempool(&mock, &first.txid()));
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert!(a
        .chain
        .carried_on_chain(&stuck, &snapshot)
        .unwrap()
        .is_some());
    for _ in 0..BUMP_AFTER {
        mock.mine(1);
        a.sync();
        rescuer.run_once().unwrap();
    }
    assert!(
        rescues(&mock, &stuck, &[first.txid()]).is_empty(),
        "not sent again"
    );
    assert!(reserved(&a, &first), "while the pinned rescue waits");
}

/// A republish whose P2A output another party's child took first goes out again the same way, with the same headers
/// (D-131).
#[test]
fn a_republish_whose_p2a_output_another_party_holds_goes_out_again() {
    let params = Params {
        max_block_data_bytes: 1_024,
        max_settlement_headers: 60,
        ..DEVNET
    };
    let (mock, genesis) = setup(&[]);
    let a = Instance::open(&mock, &genesis, "pinned-republish", &params, false);
    for _ in 0..130 {
        assert!(a.produce().unwrap().entries.is_empty());
    }
    let funding = Funding::new(btc_keypair("pinned-republish/rescuer"), 2);
    mock.fund(&funding.script, &[400_000; 4]);
    let source = mock.fund(&coinbase_script(), &[100_000])[0];
    mock.set_min_feerate(5);
    a.sync();
    let payee = user("adv/liveness/pinned-republish/payee").account;
    let rescuer = Rescuer::new(a.chain.clone(), funding, None, Some(payee));
    rescuer.run_once().unwrap();
    let reveal = mock
        .mempool()
        .into_iter()
        .find(|tx| envelope_payload(&params, tx).is_some())
        .unwrap();
    let first = pair_in_mempool(&mock, reveal);
    let sent = republished(&mock, &params);
    assert_eq!(sent.len(), 1, "republished at the floor");
    let headers = sent[0].0.clone();
    pin(&mock, &first, source);
    mock.set_fee_estimate(DEFAULT_FEE_TARGET, Some(10));
    for _ in 1..BUMP_AFTER {
        mock.mine(1);
        a.sync();
        rescuer.run_once().unwrap();
    }
    assert_eq!(
        republished(&mock, &params).len(),
        1,
        "not before BUMP_AFTER"
    );
    mock.mine(1);
    a.sync();
    rescuer.run_once().unwrap();
    let again: Vec<Transaction> = mock
        .mempool()
        .into_iter()
        .filter(|tx| tx.compute_txid().to_byte_array() != first.txid())
        .filter(|tx| envelope_payload(&params, tx).is_some())
        .collect();
    assert_eq!(again.len(), 1, "its payload went out again");
    assert_eq!(
        envelope_payload(&params, &again[0]),
        envelope_payload(&params, &first.reveal)
    );
    let again = pair_in_mempool(&mock, again[0].clone());
    assert_eq!(pair_feerate(&mock, &again), 10, "at the bump's rate");

    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    for header in &headers {
        let landing = snapshot.data_landing(&header.hash()).unwrap();
        assert_eq!(landing.txid, again.txid(), "landed by the new pair");
    }
    rescuer.run_once().unwrap();
    let carrying = republished(&mock, &params)
        .into_iter()
        .filter(|(sent, _)| *sent == headers)
        .count();
    assert_eq!(carrying, 1, "only the pinned republish carries them");
}

/// A block's own settlement waiting in the BTC mempool below this node's fee rate does not hold this node's rescue back
/// (D-102): the node rescues the block in its turn in the rescue order. It used to wait for as long as the settlement
/// stayed in the mempool.
#[test]
fn a_settlement_waiting_below_this_nodes_fee_rate_is_rescued() {
    let (mock, a, block, pair) = waiting_block("cheap-own", true);
    // Another node following the same chain, producing nothing, paying the estimate of 10 sat/vB
    let funding = Funding::new(btc_keypair("cheap-own/rescuer"), 2);
    mock.fund(&funding.script, &[400_000; 2]);
    a.sync();
    mock.set_fee_estimate(DEFAULT_FEE_TARGET, Some(10));
    let rescuer = Rescuer::new(a.chain.clone(), funding, None, None);
    rescuer.run_once().unwrap();
    // A node outside the rescue order waits RESCUE_ORDER_WINDOW + RESCUE_DELAY BTC blocks, CLAIM_PATIENCE
    mock.mine(CLAIM_PATIENCE - 1);
    a.sync();
    rescuer.run_once().unwrap();
    assert!(rescues(&mock, &block, &[pair.txid()]).is_empty());
    mock.mine(1);
    a.sync();
    rescuer.run_once().unwrap();
    let rescue = rescues(&mock, &block, &[pair.txid()]);
    assert_eq!(rescue.len(), 1, "rescued at this node's rate");
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert!(snapshot
        .ksn1_height(&rescue[0].compute_txid().to_byte_array())
        .is_some());
    assert!(in_mempool(&mock, &pair.txid()), "the original still waits");
    assert!(a
        .chain
        .carried_on_chain(&block, &snapshot)
        .unwrap()
        .is_some());
}

/// A block's own settlement waiting in the BTC mempool at this node's fee rate holds the rescue back like another
/// party's claim, for `CLAIM_PATIENCE` BTC blocks after this node first saw it waiting; then the node rescues the block
/// in its turn in the rescue order (D-102).
#[test]
fn a_settlement_that_waits_too_long_is_rescued() {
    let (mock, a, block, pair) = waiting_block("held-own", false);
    let funding = Funding::new(btc_keypair("held-own/rescuer"), 2);
    mock.fund(&funding.script, &[400_000; 2]);
    a.sync();
    let rescuer = Rescuer::new(a.chain.clone(), funding, None, None);
    rescuer.run_once().unwrap();
    // CLAIM_PATIENCE BTC blocks of patience, then RESCUE_ORDER_WINDOW + RESCUE_DELAY (as many again) for a node
    // outside the rescue order
    for _ in 1..2 * CLAIM_PATIENCE {
        mock.mine(1);
        a.sync();
        rescuer.run_once().unwrap();
        assert!(rescues(&mock, &block, &[pair.txid()]).is_empty());
    }
    mock.mine(1);
    a.sync();
    rescuer.run_once().unwrap();
    let rescue = rescues(&mock, &block, &[pair.txid()]);
    assert_eq!(rescue.len(), 1, "rescued");
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert!(snapshot
        .ksn1_height(&rescue[0].compute_txid().to_byte_array())
        .is_some());
    assert!(a
        .chain
        .carried_on_chain(&block, &snapshot)
        .unwrap()
        .is_some());
}
