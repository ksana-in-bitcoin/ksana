//! An in-memory mock BTC chain (feature `mock`), for tests only.
//!
//! [`MockBtc`] implements [`BtcClient`]; the parts the node uses have the same semantics as bitcoind, but it **does not
//! verify scripts and signatures** and does not check proof of work either:
//!
//! - Blocks are real `bitcoin::Block`s: the genesis block is regtest's genesis block; each later block's
//!   `prev_blockhash` points at its parent, `merkle_root` is computed from the transactions, `bits` is regtest's
//!   [`REGTEST_BITS`], `nonce` is 0, and the time strictly increases ([`BLOCK_INTERVAL_SECS`] seconds per block).
//!   The coinbase's scriptSig starts with the BIP34 height followed by a global serial number (so coinbases at the
//!   same height on different branches differ too), pays [`COINBASE_VALUE_SAT`] to [`coinbase_script`], and carries
//!   a witness commitment (BIP141) like Bitcoin Core does.
//! - Maintains the UTXO set (as in Bitcoin Core, unspendable outputs such as `OP_RETURN` do not enter it), the mempool
//!   (in insertion order) and the transaction index (best chain + mempool).
//! - Transaction checks ([`MockBtc::submit`], `test_accept` (a group of transactions checked in order, later ones may
//!   spend the outputs of earlier ones), [`MockBtc::mine_block`], returning to the mempool after a reorg): has inputs
//!   and outputs, is not a coinbase, no duplicate inputs; every input exists and is unspent (an on-chain UTXO, or the
//!   output of a mempool transaction when entering the mempool); a conflict with a mempool transaction is rejected
//!   (**no RBF support**, unlike the full-RBF default of Bitcoin Core 28); coinbase outputs are mature
//!   ([`COINBASE_MATURITY`]); the total output does not exceed the total input. Rejection reasons use Bitcoin Core's
//!   strings.
//! - Reorg: [`MockBtc::reorg`] disconnects a number of blocks from the tip, the still-valid non-coinbase transactions
//!   return to the mempool, and another branch can be mined afterwards. Disconnected blocks can still be looked up
//!   through `block` / `header` (`in_best_chain = false`), and [`MockBtc::reconnect`] connects them again after
//!   another reorg (BTC going back to the branch it left).
//! - Fees: no estimate by default (like a fresh regtest), [`MockBtc::set_fee_estimate`] sets one; with
//!   [`MockBtc::set_min_feerate`] [`MockBtc::mine`] includes only the transactions whose ancestor package pays that
//!   rate (a child can pay for its parents, CPFP), the others stay in the mempool. What bitcoind tells of its mempool
//!   is set by [`MockBtc::set_mempool_info`], whatever the mock mempool holds: nothing by default.
//! - Fully deterministic: no random numbers and no system time; the same sequence of operations yields the same block
//!   hashes.

use std::collections::{BTreeMap, BTreeSet};
use std::mem;
use std::sync::{Mutex, MutexGuard, PoisonError};

use bitcoin::block::{Header, Version as BlockVersion};
use bitcoin::hashes::{sha256d, Hash};
use bitcoin::script::Builder;
use bitcoin::transaction::Version;
use bitcoin::{
    absolute, Amount, Block, BlockHash, CompactTarget, Network, OutPoint, Script, ScriptBuf,
    Sequence, Transaction, TxIn, TxMerkleNode, TxOut, Txid, Witness,
};

use crate::client::{BtcClient, BtcError, HeaderInfo, MempoolInfo, ScannedUtxo, TxInfo, UtxoInfo};

/// regtest's difficulty target (compact format).
pub const REGTEST_BITS: u32 = 0x207f_ffff;
/// The coinbase amount of every block (sats).
pub const COINBASE_VALUE_SAT: u64 = 50 * 100_000_000;
/// The confirmations a coinbase output needs before it can be spent (same as Bitcoin Core).
pub const COINBASE_MATURITY: u32 = 100;
/// The time interval between adjacent blocks (seconds).
pub const BLOCK_INTERVAL_SECS: u32 = 600;

/// Block version: the BIP9 version-bits prefix, same as the blocks Bitcoin Core produces on regtest.
const BLOCK_VERSION: i32 = 0x2000_0000;
/// The witness reserved value (the only witness element of the coinbase input).
const WITNESS_RESERVED_VALUE: [u8; 32] = [0; 32];
/// The script prefix of the witness commitment output: `OP_RETURN OP_PUSHBYTES_36 0xaa21a9ed`.
const WITNESS_COMMITMENT_PREFIX: [u8; 6] = [0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed];
/// Locking scripts longer than this are unspendable (Bitcoin Core `MAX_SCRIPT_SIZE`).
const MAX_SCRIPT_SIZE: usize = 10_000;
/// Placeholder "height" of mempool transaction outputs (used only for the coinbase maturity check, and the mempool
/// has no coinbases).
const MEMPOOL_HEIGHT: u32 = u32::MAX;

/// The locking script every mock block's coinbase output pays to: `OP_TRUE`.
pub fn coinbase_script() -> ScriptBuf {
    Builder::new()
        .push_opcode(bitcoin::opcodes::OP_TRUE)
        .into_script()
}

/// The locking script of the output spent by the virtual input of [`MockBtc::fund`] (never used by real
/// transactions).
fn fund_source_script() -> ScriptBuf {
    Builder::new().push_slice(b"ksana/mock/fund").into_script()
}

/// Outputs that are unspendable and do not enter the UTXO set (Bitcoin Core `CScript::IsUnspendable`).
fn is_unspendable(script: &Script) -> bool {
    script.is_op_return() || script.len() > MAX_SCRIPT_SIZE
}

/// One output in the UTXO set.
#[derive(Clone, Debug)]
struct Coin {
    txout: TxOut,
    height: u32,
    coinbase: bool,
}

/// A known block (including disconnected ones).
#[derive(Debug)]
struct Stored {
    block: Block,
    height: u32,
    /// The outputs removed from the UTXO set when connecting, restored when disconnecting; cleared once the block is
    /// disconnected.
    undo: Vec<(OutPoint, Coin)>,
}

/// What the miner knows of a mempool transaction: its fee, virtual size and parents in the mempool.
struct MinerEntry {
    fee: u64,
    vsize: u64,
    parents: Vec<[u8; 32]>,
}

/// The mempool: transactions in insertion order, parents always before children.
#[derive(Clone, Debug, Default)]
struct Mempool {
    order: Vec<[u8; 32]>,
    txs: BTreeMap<[u8; 32], Transaction>,
    /// Output spent by a mempool transaction → the transaction spending it.
    spends: BTreeMap<OutPoint, [u8; 32]>,
}

impl Mempool {
    fn add(&mut self, txid: [u8; 32], tx: Transaction) {
        for input in &tx.input {
            self.spends.insert(input.previous_output, txid);
        }
        self.order.push(txid);
        self.txs.insert(txid, tx);
    }

    /// A spendable output of a mempool transaction.
    fn output(&self, outpoint: &OutPoint) -> Option<&TxOut> {
        let tx = self.txs.get(&outpoint.txid.to_byte_array())?;
        let out = tx.output.get(usize::try_from(outpoint.vout).ok()?)?;
        (!is_unspendable(&out.script_pubkey)).then_some(out)
    }

    /// All transactions in insertion order.
    fn snapshot(&self) -> Vec<Transaction> {
        self.order
            .iter()
            .filter_map(|txid| self.txs.get(txid).cloned())
            .collect()
    }

    /// Clears the mempool and takes out all transactions in insertion order.
    fn take(&mut self) -> Vec<Transaction> {
        let mut txs = mem::take(&mut self.txs);
        self.spends.clear();
        mem::take(&mut self.order)
            .into_iter()
            .filter_map(|txid| txs.remove(&txid))
            .collect()
    }
}

/// The lookup result of the output an input refers to.
enum Prevout {
    Found(Coin),
    /// Already spent by another transaction in the mempool.
    Conflict,
    /// Does not exist or already spent.
    Missing,
}

/// Context-free checks (the corresponding parts of Bitcoin Core's `CheckTransaction`, plus "must not be a coinbase").
fn check_structure(tx: &Transaction) -> Result<(), String> {
    if tx.input.is_empty() {
        return Err("bad-txns-vin-empty".into());
    }
    if tx.output.is_empty() {
        return Err("bad-txns-vout-empty".into());
    }
    let mut total = Amount::ZERO;
    for out in &tx.output {
        if out.value > Amount::MAX_MONEY {
            return Err("bad-txns-vout-toolarge".into());
        }
        total = total
            .checked_add(out.value)
            .filter(|t| *t <= Amount::MAX_MONEY)
            .ok_or_else(|| "bad-txns-txouttotal-toolarge".to_string())?;
    }
    let mut seen = BTreeSet::new();
    if !tx.input.iter().all(|i| seen.insert(i.previous_output)) {
        return Err("bad-txns-inputs-duplicate".into());
    }
    if tx.is_coinbase() {
        return Err("coinbase".into());
    }
    if tx.input.iter().any(|i| i.previous_output.is_null()) {
        return Err("bad-txns-prevout-null".into());
    }
    Ok(())
}

/// Input checks: the previous outputs exist and are unspent, coinbases are mature, the total output does not exceed
/// the total input. `spend_height` is the height of the block the transaction is in (or is about to enter). Called
/// only after [`check_structure`] passed.
fn check_inputs(
    tx: &Transaction,
    spend_height: u32,
    mut prevout: impl FnMut(&OutPoint) -> Prevout,
) -> Result<(), String> {
    let mut total_in = Amount::ZERO;
    for input in &tx.input {
        let coin = match prevout(&input.previous_output) {
            Prevout::Found(coin) => coin,
            Prevout::Conflict => return Err("txn-mempool-conflict".into()),
            Prevout::Missing => return Err("bad-txns-inputs-missingorspent".into()),
        };
        if coin.coinbase && spend_height.saturating_sub(coin.height) < COINBASE_MATURITY {
            return Err("bad-txns-premature-spend-of-coinbase".into());
        }
        total_in = total_in
            .checked_add(coin.txout.value)
            .filter(|t| *t <= Amount::MAX_MONEY)
            .ok_or_else(|| "bad-txns-inputvalues-outofrange".to_string())?;
    }
    let total_out = tx
        .output
        .iter()
        .try_fold(Amount::ZERO, |acc, out| acc.checked_add(out.value))
        .ok_or_else(|| "bad-txns-txouttotal-toolarge".to_string())?;
    if total_in < total_out {
        return Err("bad-txns-in-belowout".into());
    }
    Ok(())
}

/// A block being assembled: the transactions added in turn may spend only on-chain UTXOs and the outputs of earlier
/// transactions in this block.
struct Draft {
    height: u32,
    txs: Vec<Transaction>,
    txids: BTreeSet<[u8; 32]>,
    created: BTreeMap<OutPoint, Coin>,
    spent: BTreeSet<OutPoint>,
}

impl Draft {
    fn new(chain: &Chain) -> Self {
        Draft {
            height: chain.height().saturating_add(1),
            txs: Vec::new(),
            txids: BTreeSet::new(),
            created: BTreeMap::new(),
            spent: BTreeSet::new(),
        }
    }

    /// Checks `tx` and adds it to this block; on failure the block is unchanged.
    fn push(&mut self, chain: &Chain, tx: Transaction) -> Result<(), String> {
        let txid = tx.compute_txid();
        let id = txid.to_byte_array();
        check_structure(&tx)?;
        if chain.tx_index.contains_key(&id) {
            return Err("bad-txns-BIP30".into());
        }
        if self.txids.contains(&id) {
            return Err("bad-txns-duplicate".into());
        }
        check_inputs(&tx, self.height, |outpoint| {
            if self.spent.contains(outpoint) {
                return Prevout::Missing;
            }
            match self
                .created
                .get(outpoint)
                .or_else(|| chain.utxos.get(outpoint))
            {
                Some(coin) => Prevout::Found(coin.clone()),
                None => Prevout::Missing,
            }
        })?;
        for input in &tx.input {
            self.spent.insert(input.previous_output);
        }
        for (vout, out) in (0u32..).zip(&tx.output) {
            if !is_unspendable(&out.script_pubkey) {
                let coin = Coin {
                    txout: out.clone(),
                    height: self.height,
                    coinbase: false,
                };
                self.created.insert(OutPoint { txid, vout }, coin);
            }
        }
        self.txids.insert(id);
        self.txs.push(tx);
        Ok(())
    }
}

/// The whole state of the mock chain.
#[derive(Debug)]
struct Chain {
    /// All known blocks (including disconnected ones), indexed by hash.
    blocks: BTreeMap<[u8; 32], Stored>,
    /// The best chain: index = height. Contains at least the genesis block.
    best: Vec<[u8; 32]>,
    utxos: BTreeMap<OutPoint, Coin>,
    /// Transaction on the best chain → (block hash, index within the block).
    tx_index: BTreeMap<[u8; 32], ([u8; 32], usize)>,
    mempool: Mempool,
    /// The number of blocks built (written into the coinbase so that the coinbases of all branches differ).
    blocks_built: u64,
    /// The number of `fund` calls (to generate distinct virtual inputs).
    funded: u64,
    /// The timestamp of the latest block; it only ever increases, so the blocks of a new branch after a reorg are
    /// later too.
    time: u32,
    /// When true, all [`BtcClient`] methods return a local fault.
    offline: bool,
    /// The transactions of [`MockBtc::hold`]: they stay in the mempool and [`MockBtc::mine`] does not include them
    /// (simulating a fee rate that is too low).
    held: BTreeSet<[u8; 32]>,
    /// [`MockBtc::set_fee_estimate`]: the fee rate estimates (sat/vB) by confirmation target.
    fee_estimates: BTreeMap<u16, u64>,
    /// [`MockBtc::set_min_feerate`]: the lowest fee rate (sat/vB) of an ancestor package [`MockBtc::mine`] includes.
    min_feerate: u64,
    /// [`MockBtc::set_mempool_info`]: what bitcoind tells of its mempool.
    mempool_info: Option<MempoolInfo>,
}

impl Chain {
    fn new() -> Self {
        let genesis = bitcoin::constants::genesis_block(Network::Regtest);
        let hash = genesis.block_hash().to_byte_array();
        // the clock starts at the system time at creation (rather than the regtest genesis block's 2011): fast-lane
        // block timestamps are taken from the system time, and they may be at most MAX_TIMESTAMP_AHEAD_OF_ANCHOR ahead
        // of the anchored BTC block's time (D-083)
        let time = unix_now().max(genesis.header.time);
        // same as Bitcoin Core: the genesis block's coinbase does not enter the UTXO set or the transaction index.
        let stored = Stored {
            block: genesis,
            height: 0,
            undo: Vec::new(),
        };
        Chain {
            blocks: BTreeMap::from([(hash, stored)]),
            best: vec![hash],
            utxos: BTreeMap::new(),
            tx_index: BTreeMap::new(),
            mempool: Mempool::default(),
            blocks_built: 0,
            funded: 0,
            time,
            offline: false,
            held: BTreeSet::new(),
            fee_estimates: BTreeMap::new(),
            min_feerate: 0,
            mempool_info: None,
        }
    }

    fn height(&self) -> u32 {
        u32::try_from(self.best.len().saturating_sub(1)).unwrap_or(u32::MAX)
    }

    fn tip(&self) -> [u8; 32] {
        self.best.last().copied().unwrap_or_default()
    }

    fn best_at(&self, height: u32) -> Option<[u8; 32]> {
        self.best.get(usize::try_from(height).ok()?).copied()
    }

    fn confirmations(&self, height: u32) -> u32 {
        self.height().saturating_sub(height).saturating_add(1)
    }

    /// Looks up a previous output from the mempool's point of view.
    fn mempool_prevout(&self, outpoint: &OutPoint) -> Prevout {
        if self.mempool.spends.contains_key(outpoint) {
            return Prevout::Conflict;
        }
        if let Some(coin) = self.utxos.get(outpoint) {
            return Prevout::Found(coin.clone());
        }
        match self.mempool.output(outpoint) {
            Some(out) => Prevout::Found(Coin {
                txout: out.clone(),
                height: MEMPOOL_HEIGHT,
                coinbase: false,
            }),
            None => Prevout::Missing,
        }
    }

    /// The mempool admission check; when `add` is true and it passes, the transaction is added to the mempool.
    fn accept(&mut self, tx: Transaction, add: bool) -> Result<[u8; 32], String> {
        let txid = tx.compute_txid().to_byte_array();
        check_structure(&tx)?;
        if self.mempool.txs.contains_key(&txid) {
            return Err("txn-already-in-mempool".into());
        }
        if self.tx_index.contains_key(&txid) {
            return Err("txn-already-known".into());
        }
        let spend_height = self.height().saturating_add(1);
        check_inputs(&tx, spend_height, |outpoint| self.mempool_prevout(outpoint))?;
        if add {
            self.mempool.add(txid, tx);
        }
        Ok(txid)
    }

    /// The mempool transactions a miner takes into the next block, in mempool order (parents first): none that is held
    /// or descends from a held one, and only those whose ancestor package pays `min_feerate`, the package being the
    /// transaction with its mempool ancestors not taken yet (like Bitcoin Core's block assembly: a child can pay for
    /// its parents, CPFP).
    fn minable(&self) -> Vec<Transaction> {
        let txs = self.mempool.snapshot();
        let mut info: BTreeMap<[u8; 32], MinerEntry> = BTreeMap::new();
        let mut blocked: BTreeSet<[u8; 32]> = BTreeSet::new();
        for tx in &txs {
            let txid = tx.compute_txid().to_byte_array();
            let mut total_in = 0u64;
            let mut parents = Vec::new();
            for input in &tx.input {
                let value = match self.utxos.get(&input.previous_output) {
                    Some(coin) => coin.txout.value.to_sat(),
                    None => {
                        parents.push(input.previous_output.txid.to_byte_array());
                        self.mempool
                            .output(&input.previous_output)
                            .map_or(0, |out| out.value.to_sat())
                    }
                };
                total_in = total_in.saturating_add(value);
            }
            let total_out = tx
                .output
                .iter()
                .map(|out| out.value.to_sat())
                .fold(0u64, u64::saturating_add);
            if self.held.contains(&txid) || parents.iter().any(|p| blocked.contains(p)) {
                blocked.insert(txid);
            }
            let vsize = tx.weight().to_vbytes_ceil();
            info.insert(
                txid,
                MinerEntry {
                    fee: total_in.saturating_sub(total_out),
                    vsize,
                    parents,
                },
            );
        }
        let mut taken: BTreeSet<[u8; 32]> = BTreeSet::new();
        loop {
            let mut progress = false;
            for tx in &txs {
                let txid = tx.compute_txid().to_byte_array();
                if taken.contains(&txid) || blocked.contains(&txid) {
                    continue;
                }
                let mut package = BTreeSet::new();
                let mut stack = vec![txid];
                while let Some(next) = stack.pop() {
                    if taken.contains(&next) || !package.insert(next) {
                        continue;
                    }
                    if let Some(entry) = info.get(&next) {
                        stack.extend(entry.parents.iter().copied());
                    }
                }
                let (fee, vsize) = package
                    .iter()
                    .filter_map(|t| info.get(t))
                    .fold((0u64, 0u64), |(f, v), entry| {
                        (f.saturating_add(entry.fee), v.saturating_add(entry.vsize))
                    });
                if fee >= self.min_feerate.saturating_mul(vsize) {
                    taken.extend(package);
                    progress = true;
                }
            }
            if !progress {
                break;
            }
        }
        txs.into_iter()
            .filter(|tx| taken.contains(&tx.compute_txid().to_byte_array()))
            .collect()
    }

    /// Clears the mempool and re-admits `candidates` in order; transactions no longer valid (already on the chain,
    /// conflicting, depending on an invalidated transaction, immature coinbase) are dropped.
    fn refill_mempool(&mut self, candidates: Vec<Transaction>) {
        self.mempool = Mempool::default();
        for tx in candidates {
            // rejected means dropped.
            let _ = self.accept(tx, true);
        }
    }

    /// Connects a new block containing `txs` on top of the tip (`txs` already checked per [`Draft`]) and returns the
    /// block hash.
    fn connect(&mut self, txs: Vec<Transaction>) -> [u8; 32] {
        let height = self.height().saturating_add(1);
        self.blocks_built = self.blocks_built.saturating_add(1);
        self.time = self.time.saturating_add(BLOCK_INTERVAL_SECS);
        let block = build_block(self.tip(), height, self.blocks_built, self.time, txs);
        self.attach(block, height)
    }

    /// Connects the disconnected block `hash` again on top of the tip, whose child it must be; its transactions are
    /// checked against the chain as in [`MockBtc::mine_block`].
    fn reconnect(&mut self, hash: &[u8; 32]) -> Result<(), String> {
        let height = self.height().saturating_add(1);
        let block = match self.blocks.get(hash) {
            Some(stored)
                if stored.height == height
                    && stored.block.header.prev_blockhash.to_byte_array() == self.tip() =>
            {
                stored.block.clone()
            }
            Some(_) => return Err("the block is not a child of the tip".into()),
            None => return Err("unknown block".into()),
        };
        let mut draft = Draft::new(self);
        for tx in block.txdata.iter().filter(|tx| !tx.is_coinbase()) {
            draft.push(self, tx.clone())?;
        }
        self.attach(block, height);
        Ok(())
    }

    /// Connects `block` at `height` on top of the tip (its transactions already checked per [`Draft`]) and returns its
    /// hash.
    fn attach(&mut self, block: Block, height: u32) -> [u8; 32] {
        let hash = block.block_hash().to_byte_array();
        let mut undo = Vec::new();
        for (index, tx) in block.txdata.iter().enumerate() {
            let txid = tx.compute_txid();
            let coinbase = tx.is_coinbase();
            if !coinbase {
                for input in &tx.input {
                    if let Some(coin) = self.utxos.remove(&input.previous_output) {
                        undo.push((input.previous_output, coin));
                    }
                }
            }
            for (vout, out) in (0u32..).zip(&tx.output) {
                if !is_unspendable(&out.script_pubkey) {
                    let coin = Coin {
                        txout: out.clone(),
                        height,
                        coinbase,
                    };
                    self.utxos.insert(OutPoint { txid, vout }, coin);
                }
            }
            self.tx_index.insert(txid.to_byte_array(), (hash, index));
        }
        self.best.push(hash);
        self.blocks.insert(
            hash,
            Stored {
                block,
                height,
                undo,
            },
        );
        let pending = self.mempool.take();
        self.refill_mempool(pending);
        hash
    }

    /// Disconnects the tip block (never the genesis block) and returns its hash and its non-coinbase transactions. The
    /// mempool is unchanged.
    fn disconnect_tip(&mut self) -> Option<([u8; 32], Vec<Transaction>)> {
        if self.best.len() <= 1 {
            return None;
        }
        let hash = self.best.pop()?;
        let stored = self.blocks.get_mut(&hash)?;
        // restore the spent outputs first, then delete the outputs this block created: an output created and spent
        // within this block therefore does not remain.
        for (outpoint, coin) in mem::take(&mut stored.undo) {
            self.utxos.insert(outpoint, coin);
        }
        let mut txs = Vec::new();
        for tx in &stored.block.txdata {
            let txid = tx.compute_txid();
            for vout in (0u32..).take(tx.output.len()) {
                self.utxos.remove(&OutPoint { txid, vout });
            }
            self.tx_index.remove(&txid.to_byte_array());
            if !tx.is_coinbase() {
                txs.push(tx.clone());
            }
        }
        Some((hash, txs))
    }

    fn transaction(&self, txid: &[u8; 32]) -> Option<TxInfo> {
        if let Some((hash, index)) = self.tx_index.get(txid) {
            let tx = self
                .blocks
                .get(hash)
                .and_then(|stored| stored.block.txdata.get(*index))?;
            return Some(TxInfo {
                tx: tx.clone(),
                block: Some(*hash),
            });
        }
        self.mempool.txs.get(txid).map(|tx| TxInfo {
            tx: tx.clone(),
            block: None,
        })
    }
}

/// Builds the block at height `height` with parent `prev`: the coinbase first, followed by `txs` in order.
/// The current system time (seconds): the header time of the mock chain, so that tests producing blocks by local time
/// satisfy the anchor time bound of timestamps (D-083); the mock chain is not consensus code.
#[allow(clippy::disallowed_methods)]
fn unix_now() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u32::try_from(d.as_secs()).unwrap_or(u32::MAX))
        .unwrap_or(0)
}

fn build_block(
    prev: [u8; 32],
    height: u32,
    serial: u64,
    time: u32,
    txs: Vec<Transaction>,
) -> Block {
    let script_sig = Builder::new()
        .push_int(i64::from(height))
        .push_slice(serial.to_le_bytes())
        .into_script();
    let coinbase = Transaction {
        version: Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig,
            sequence: Sequence::MAX,
            witness: Witness::from_slice(&[WITNESS_RESERVED_VALUE]),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(COINBASE_VALUE_SAT),
            script_pubkey: coinbase_script(),
        }],
    };
    let mut txdata = vec![coinbase];
    txdata.extend(txs);
    let mut block = Block {
        header: Header {
            version: BlockVersion::from_consensus(BLOCK_VERSION),
            prev_blockhash: BlockHash::from_byte_array(prev),
            merkle_root: TxMerkleNode::all_zeros(),
            time,
            bits: CompactTarget::from_consensus(REGTEST_BITS),
            nonce: 0,
        },
        txdata,
    };
    // the witness Merkle root treats the coinbase's wtxid as all zeros, so adding the commitment output to the coinbase
    // afterwards does not affect it.
    if let Some(witness_root) = block.witness_root() {
        let commitment = Block::compute_witness_commitment(&witness_root, &WITNESS_RESERVED_VALUE);
        let script = WITNESS_COMMITMENT_PREFIX
            .iter()
            .chain(commitment.as_byte_array())
            .copied()
            .collect::<Vec<u8>>();
        if let Some(coinbase) = block.txdata.first_mut() {
            coinbase.output.push(TxOut {
                value: Amount::ZERO,
                script_pubkey: ScriptBuf::from_bytes(script),
            });
        }
    }
    if let Some(merkle_root) = block.compute_merkle_root() {
        block.header.merkle_root = merkle_root;
    }
    block
}

/// The in-memory mock BTC chain, see the module docs. All methods take `&self` (locking internally) and can be shared
/// between threads (commonly used as `Arc<MockBtc>`).
///
/// The test helper methods ([`fund`](Self::fund), [`mine`](Self::mine) etc.) simulate the BTC network itself and
/// remain usable after [`set_offline`](Self::set_offline); only the [`BtcClient`] methods return a local fault.
#[derive(Debug)]
pub struct MockBtc {
    chain: Mutex<Chain>,
}

impl Default for MockBtc {
    fn default() -> Self {
        Self::new()
    }
}

impl MockBtc {
    /// A chain with only the regtest genesis block (height 0).
    pub fn new() -> Self {
        MockBtc {
            chain: Mutex::new(Chain::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Chain> {
        self.chain.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn online(&self) -> Result<MutexGuard<'_, Chain>, BtcError> {
        let chain = self.lock();
        if chain.offline {
            return Err(BtcError::Rpc("the mock bitcoind is unavailable".into()));
        }
        Ok(chain)
    }

    /// Creates a transaction paying `values` (sats, one per output) to `script` and mines it alone into a new block
    /// (mempool transactions are not included); returns the outpoints of the outputs.
    ///
    /// The transaction's only input spends a virtual output: it appears in the UTXO set with a script no real
    /// transaction uses and is spent by this transaction right away; when the funding block is reorged out, this
    /// transaction returns to the mempool like any other. When `values` is empty or the total exceeds the BTC supply,
    /// no block is mined and an empty list is returned.
    pub fn fund(&self, script: &Script, values: &[u64]) -> Vec<OutPoint> {
        let mut guard = self.lock();
        let chain = &mut *guard;
        chain.funded = chain.funded.saturating_add(1);
        let seed = [b"ksana/mock/fund/".as_slice(), &chain.funded.to_le_bytes()].concat();
        let source = OutPoint {
            txid: Txid::from_byte_array(sha256d::Hash::hash(&seed).to_byte_array()),
            vout: 0,
        };
        let Some(total) = values
            .iter()
            .try_fold(Amount::ZERO, |acc, v| acc.checked_add(Amount::from_sat(*v)))
        else {
            return Vec::new();
        };
        let tx = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: source,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: values
                .iter()
                .map(|v| TxOut {
                    value: Amount::from_sat(*v),
                    script_pubkey: script.to_owned(),
                })
                .collect(),
        };
        let txid = tx.compute_txid();
        let coin = Coin {
            txout: TxOut {
                value: total,
                script_pubkey: fund_source_script(),
            },
            height: chain.height(),
            coinbase: false,
        };
        chain.utxos.insert(source, coin);
        let mut draft = Draft::new(chain);
        if draft.push(chain, tx).is_err() {
            chain.utxos.remove(&source);
            return Vec::new();
        }
        chain.connect(draft.txs);
        (0u32..)
            .zip(values)
            .map(|(vout, _)| OutPoint { txid, vout })
            .collect()
    }

    /// Adds the transaction to the mempool (checks: see the module docs) and returns the transaction ID (internal byte
    /// order); returns the reason when rejected.
    pub fn submit(&self, tx: Transaction) -> Result<[u8; 32], String> {
        self.lock().accept(tx, true)
    }

    /// Mines `n` blocks and returns their hashes (ascending by height); the first block includes the mempool
    /// transactions in order, except the held ones ([`hold`](Self::hold)) and, with a minimum fee rate
    /// ([`set_min_feerate`](Self::set_min_feerate)), those whose ancestor package pays less.
    pub fn mine(&self, n: u32) -> Vec<[u8; 32]> {
        let mut guard = self.lock();
        let chain = &mut *guard;
        let mut hashes = Vec::new();
        for i in 0..n {
            let mut draft = Draft::new(chain);
            if i == 0 {
                for tx in chain.minable() {
                    // the mempool is always consistent with the tip and the parents come first, so this does not
                    // fail; should it fail the transaction is just not included.
                    let _ = draft.push(chain, tx);
                }
            }
            hashes.push(chain.connect(draft.txs));
        }
        hashes
    }

    /// Mines a block containing exactly `txs` (in order, after the coinbase) and returns the block hash. The
    /// transactions are checked against the on-chain state (they cannot spend mempool outputs); when any of them fails
    /// no block is mined and the reason is returned. The included transactions are removed from the mempool, and the
    /// mempool transactions conflicting with them (and their descendants) are evicted.
    pub fn mine_block(&self, txs: Vec<Transaction>) -> Result<[u8; 32], String> {
        let mut guard = self.lock();
        let chain = &mut *guard;
        let mut draft = Draft::new(chain);
        for tx in txs {
            draft.push(chain, tx)?;
        }
        Ok(chain.connect(draft.txs))
    }

    /// Disconnects the top `depth` blocks of the best chain (at most down to just above the genesis block) and returns
    /// their hashes (ascending by height).
    ///
    /// Spent outputs are restored and newly created outputs deleted; the non-coinbase transactions of these blocks are
    /// put back at the front of the mempool in their original order, then the original mempool transactions are
    /// re-admitted in turn, and transactions no longer valid are dropped. The disconnected blocks can still be
    /// queried, with `in_best_chain` false.
    pub fn reorg(&self, depth: u32) -> Vec<[u8; 32]> {
        let mut guard = self.lock();
        let chain = &mut *guard;
        let mut hashes = Vec::new();
        let mut returned = Vec::new();
        for _ in 0..depth {
            let Some((hash, txs)) = chain.disconnect_tip() else {
                break;
            };
            hashes.push(hash);
            returned.push(txs);
        }
        hashes.reverse();
        returned.reverse();
        let mut candidates: Vec<Transaction> = returned.into_iter().flatten().collect();
        candidates.extend(chain.mempool.take());
        chain.refill_mempool(candidates);
        hashes
    }

    /// Connects blocks disconnected by [`reorg`](Self::reorg) again, `hashes` ascending by height from a child of the
    /// tip (BTC going back to a branch it left). Their transactions are checked against the chain as in
    /// [`mine_block`](Self::mine_block), and the mempool transactions they confirm or conflict with are dropped. On the
    /// first block that cannot be connected the reason is returned, with the blocks before it connected.
    pub fn reconnect(&self, hashes: &[[u8; 32]]) -> Result<(), String> {
        let mut guard = self.lock();
        let chain = &mut *guard;
        for hash in hashes {
            chain.reconnect(hash)?;
        }
        Ok(())
    }

    /// The height of the best-chain tip.
    pub fn tip_height(&self) -> u32 {
        self.lock().height()
    }

    /// The block hash of the best-chain tip.
    pub fn tip_hash(&self) -> [u8; 32] {
        self.lock().tip()
    }

    /// All transactions in the mempool (in insertion order).
    pub fn mempool(&self) -> Vec<Transaction> {
        self.lock().mempool.snapshot()
    }

    /// Evicts the transaction and all of its descendants from the mempool (simulating the node dropping it); returns
    /// false when the transaction is not in the mempool.
    pub fn evict(&self, txid: &[u8; 32]) -> bool {
        let mut guard = self.lock();
        let chain = &mut *guard;
        if !chain.mempool.txs.contains_key(txid) {
            return false;
        }
        let rest = chain
            .mempool
            .take()
            .into_iter()
            .filter(|tx| tx.compute_txid().to_byte_array() != *txid)
            .collect();
        chain.refill_mempool(rest);
        true
    }

    /// Keeps the transaction in the mempool without [`mine`](Self::mine) including it (simulating a fee rate too low
    /// to land for a long time); [`release`](Self::release) undoes it. Also effective for transactions not yet
    /// broadcast.
    pub fn hold(&self, txid: &[u8; 32]) {
        self.lock().held.insert(*txid);
    }

    /// Undoes [`hold`](Self::hold).
    pub fn release(&self, txid: &[u8; 32]) {
        self.lock().held.remove(txid);
    }

    /// Sets bitcoind's fee rate estimate (sat/vB) for the confirmation targets from `target` up to the next target
    /// set; `None` removes it. A target with no estimate set at or below it has none (like a fresh regtest).
    pub fn set_fee_estimate(&self, target: u16, rate: Option<u64>) {
        let mut chain = self.lock();
        match rate {
            Some(rate) => chain.fee_estimates.insert(target, rate),
            None => chain.fee_estimates.remove(&target),
        };
    }

    /// Sets the lowest fee rate (sat/vB) of the transactions [`mine`](Self::mine) includes, by ancestor package (a child
    /// can pay for its parents, CPFP): the others stay in the mempool. 0, the default, includes everything not held.
    pub fn set_min_feerate(&self, rate: u64) {
        self.lock().min_feerate = rate;
    }

    /// Sets what bitcoind tells of its mempool ([`BtcClient::mempool_info`], D-117), whatever the mock mempool holds;
    /// `None`, the default, tells nothing.
    pub fn set_mempool_info(&self, info: Option<MempoolInfo>) {
        self.lock().mempool_info = info;
    }

    /// Clears the mempool (simulating a bitcoind restart without keeping the mempool).
    pub fn clear_mempool(&self) {
        self.lock().mempool = Mempool::default();
    }

    /// Simulates bitcoind being unavailable: when true, all [`BtcClient`] methods return [`BtcError::Rpc`].
    pub fn set_offline(&self, offline: bool) {
        self.lock().offline = offline;
    }
}

impl BtcClient for MockBtc {
    fn tip(&self) -> Result<(u32, [u8; 32]), BtcError> {
        let chain = self.online()?;
        Ok((chain.height(), chain.tip()))
    }

    fn block_hash(&self, height: u32) -> Result<Option<[u8; 32]>, BtcError> {
        Ok(self.online()?.best_at(height))
    }

    fn block(&self, hash: &[u8; 32]) -> Result<Option<Block>, BtcError> {
        Ok(self
            .online()?
            .blocks
            .get(hash)
            .map(|stored| stored.block.clone()))
    }

    fn header(&self, hash: &[u8; 32]) -> Result<Option<HeaderInfo>, BtcError> {
        let chain = self.online()?;
        Ok(chain.blocks.get(hash).map(|stored| HeaderInfo {
            height: stored.height,
            in_best_chain: chain.best_at(stored.height) == Some(*hash),
            header: stored.block.header,
        }))
    }

    fn transaction(&self, txid: &[u8; 32]) -> Result<Option<TxInfo>, BtcError> {
        Ok(self.online()?.transaction(txid))
    }

    fn utxo(
        &self,
        outpoint: &OutPoint,
        include_mempool: bool,
    ) -> Result<Option<UtxoInfo>, BtcError> {
        let chain = self.online()?;
        if include_mempool && chain.mempool.spends.contains_key(outpoint) {
            return Ok(None);
        }
        if let Some(coin) = chain.utxos.get(outpoint) {
            return Ok(Some(UtxoInfo {
                txout: coin.txout.clone(),
                confirmations: chain.confirmations(coin.height),
                coinbase: coin.coinbase,
            }));
        }
        if !include_mempool {
            return Ok(None);
        }
        Ok(chain.mempool.output(outpoint).map(|out| UtxoInfo {
            txout: out.clone(),
            confirmations: 0,
            coinbase: false,
        }))
    }

    fn scan_utxos(&self, script: &Script) -> Result<Vec<ScannedUtxo>, BtcError> {
        let chain = self.online()?;
        let mut found: Vec<ScannedUtxo> = chain
            .utxos
            .iter()
            .filter(|(_, coin)| coin.txout.script_pubkey.as_script() == script)
            .map(|(outpoint, coin)| ScannedUtxo {
                outpoint: *outpoint,
                txout: coin.txout.clone(),
                height: coin.height,
                coinbase: coin.coinbase,
            })
            .collect();
        found.sort_by_key(|u| (u.height, u.outpoint));
        Ok(found)
    }

    /// Tries to add each to the mempool in turn (children may spend the outputs of parents in the package), then
    /// restores the original mempool.
    fn test_accept(&self, txs: &[&Transaction]) -> Result<Result<(), String>, BtcError> {
        let mut chain = self.online()?;
        let saved = chain.mempool.clone();
        let mut result = Ok(());
        for tx in txs {
            if let Err(reason) = chain.accept((*tx).clone(), true) {
                result = Err(reason);
                break;
            }
        }
        chain.mempool = saved;
        Ok(result)
    }

    fn broadcast(&self, tx: &Transaction) -> Result<Result<(), String>, BtcError> {
        let mut chain = self.online()?;
        let txid = tx.compute_txid().to_byte_array();
        if chain.mempool.txs.contains_key(&txid) || chain.tx_index.contains_key(&txid) {
            return Ok(Ok(()));
        }
        Ok(chain.accept(tx.clone(), true).map(|_| ()))
    }

    fn in_mempool(&self, txid: &[u8; 32]) -> Result<bool, BtcError> {
        Ok(self.online()?.mempool.txs.contains_key(txid))
    }

    fn mempool_txids(&self) -> Result<Vec<[u8; 32]>, BtcError> {
        Ok(self.online()?.mempool.order.clone())
    }

    /// The mock chain's txindex is always in sync with the tip.
    fn txindex_height(&self) -> Result<Option<u32>, BtcError> {
        Ok(Some(self.online()?.height()))
    }

    /// The estimate set for the highest target at or below `target` ([`MockBtc::set_fee_estimate`]).
    fn estimate_feerate(&self, target: u16) -> Result<Option<u64>, BtcError> {
        Ok(self
            .online()?
            .fee_estimates
            .range(..=target)
            .next_back()
            .map(|(_, rate)| *rate))
    }

    /// What [`MockBtc::set_mempool_info`] set.
    fn mempool_info(&self) -> Result<Option<MempoolInfo>, BtcError> {
        Ok(self.online()?.mempool_info)
    }
}
