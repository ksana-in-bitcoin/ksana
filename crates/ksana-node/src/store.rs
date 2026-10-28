//! Block store (SPEC §10.2): blocks (every block in the DAG), receipts, main-chain height index, op index and
//! settlement records. Local format, not part of consensus.
//!
//! | Column family | Key | Value |
//! | --- | --- | --- |
//! | `blocks` | block hash | Borsh(`Block`) |
//! | `dag` | insertion sequence number (8 bytes big-endian) | block hash: blocks are inserted into the DAG in this order (parents always before children), the GHOSTDAG data is rebuilt from it at startup (D-063), or from a pruning checkpoint on (D-097) |
//! | `dag_seq` | block hash | its insertion sequence number (8 bytes big-endian): which blocks this node has seen, also after they were pruned from memory, and where to continue serving the insertion order to peers (D-097) |
//! | `receipts` | block hash | [`RECEIPTS_MAGIC`], the format byte 2, then Borsh(`Vec<Receipt>`): the receipts of the block's entries when accepted on the current main chain (main-chain blocks and merged blocks), with their changes to built-in assets (D-122); a value without the magic is Borsh of the receipts a version before D-122 stored, without them |
//! | `main` | height (8 bytes big-endian) | hash of the block at that height on the main chain (virtual selected chain) |
//! | `summaries` | height (8 bytes big-endian) | Borsh(`MainEntry`): the summary of the main-chain block at that height and of the blocks it merges (D-097: the main chain below the in-memory window) |
//! | `accepted` | block hash | Borsh(`(u64, BlockSummary)`): the height of the main-chain block that accepted the block, and its summary (D-097) |
//! | `ops` | op_id | Borsh(`OpLocation`) (when the same op is included several times, the first executed position is kept) |
//! | `op_seq` | position (`height ‖ slot ‖ entry`, 8 + 4 + 4 bytes big-endian) | Borsh(`IndexedOp`): every op entry of the blocks the main chain accepted, at its position in the main chain's execution order (D-111) |
//! | `account_ops` | account ‖ position | empty: the positions of the ops listed under the account in `op_seq` (D-111) |
//! | `standards` | see `crate::standards` | the index of the token standards' events (D-120): declared contracts, balances and holders, NFTs, approvals |
//! | `standards_undo` | height (8 bytes big-endian) ‖ number (4 bytes big-endian) | Borsh(`(Vec<u8>, Option<Vec<u8>>)`): a key of `standards` that height changed, and its value before (D-120) |
//! | `settlements` | block hash | Borsh(`SettlementRecord`): the settlement transactions carried by blocks accepted on the main chain (D-037) |
//! | `verdicts` | block hash | the version of the verdict rules (1 byte, `ksana_btc::verdict::VERDICT_VERSION`), then Borsh(`(bool, [u8; 32])`): the verdict of executing the block on top of its selected parent (valid or not) and the BTC block hash it was given on (D-101; a cache, written without waiting for the disk: a lost one, or one kept under another version, is judged again) |
//! | `meta` | `"tip"` / `"genesis"` / `"config"` / `"index"` | main-chain tip hash / genesis block hash / genesis config fingerprint / the tip the operation index and the standards index describe, and its height (D-111, D-120) |
//!
//! Connecting a block to the main chain (together with the receipts, op index, acceptance index and settlement records
//! of the blocks it merges, and its summary) and the new tip are written in one synchronous write batch; when the
//! main chain is rolled back, one batch first moves the tip back to the fork point (deleting the height index and the
//! summaries above it and the receipts, op index, acceptance index and settlement records of the disconnected
//! blocks), then the new main chain is connected block by block.
//!
//! The operation index (`op_seq`, `account_ops`, D-111) and the standards index (`standards`, `standards_undo`, D-120)
//! are written in the same batches while they describe the stored main chain (`meta` `"index"` holds the tip and its
//! height): connecting a block applies the standard events of its receipts and keeps, by height, the value each key of
//! `standards` had before, and rolling back restores them. Otherwise (a data directory from a version without them,
//! which has the operation index's marker `"op_index"` of D-111 or none, or a build that did not finish) both are left
//! alone until [`ChainStore::rebuild_index`] builds them again at the next start.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use borsh::{BorshDeserialize, BorshSerialize};
use ksana_btc::verdict;
use ksana_execution::{AssetChange, ChangeKind, Event, OpStatus, Receipt};
use ksana_primitives::hash::sha256;
use ksana_primitives::{Action, Block, Entry, VmOp, KSA_ASSET_ID};
use ksana_rpc::OpPosition;
use ksana_state::rocks::{self, DbMemory};
use rocksdb::{Direction, IteratorMode, WriteBatch, WriteOptions, DB};

use crate::btc::{BlockSummary, MainEntry};
use crate::standards::{self, Approval, Declared, Standard};
use crate::NodeError;

const CF_BLOCKS: &str = "blocks";
const CF_DAG: &str = "dag";
const CF_DAG_SEQ: &str = "dag_seq";
const CF_RECEIPTS: &str = "receipts";
const CF_MAIN: &str = "main";
const CF_SUMMARIES: &str = "summaries";
const CF_ACCEPTED: &str = "accepted";
const CF_OPS: &str = "ops";
const CF_SETTLEMENTS: &str = "settlements";
const CF_VERDICTS: &str = "verdicts";
const CF_OP_SEQ: &str = "op_seq";
const CF_ACCOUNT_OPS: &str = "account_ops";
const CF_STANDARDS: &str = "standards";
const CF_STANDARDS_UNDO: &str = "standards_undo";
const CF_META: &str = "meta";

const COLUMN_FAMILIES: [&str; 15] = [
    CF_BLOCKS,
    CF_DAG,
    CF_DAG_SEQ,
    CF_RECEIPTS,
    CF_MAIN,
    CF_SUMMARIES,
    CF_ACCEPTED,
    CF_OPS,
    CF_SETTLEMENTS,
    CF_VERDICTS,
    CF_OP_SEQ,
    CF_ACCOUNT_OPS,
    CF_STANDARDS,
    CF_STANDARDS_UNDO,
    CF_META,
];
const META_TIP: &[u8] = b"tip";
const META_GENESIS: &[u8] = b"genesis";
const META_CONFIG: &[u8] = b"config";
const META_INDEX: &[u8] = b"index";
/// The marker of the operation index of a version without the standards index (D-111): a data directory that has it
/// gets both indexes built again.
const META_LEGACY_OP_INDEX: &[u8] = b"op_index";

/// The most entries of the operation index one listing looks at beyond the ones it returns (D-111): the skipped
/// inclusions of an op executed elsewhere are passed over, and a page that meets more of them than this ends early,
/// with the position it reached as the start of the next one.
const MAX_HIDDEN_PER_PAGE: usize = 1_024;

/// Where an op is located.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct OpLocation {
    /// The block that includes the op.
    pub block: [u8; 32],
    /// Height of the main-chain block that accepted the block (a block on the main chain has its own height, a merged
    /// block has the height of the main-chain block that merges it, D-063).
    pub height: u64,
    /// Index among the entries.
    pub index: u32,
    /// The entry was skipped (the lenient mode of D-063): the op was not executed.
    pub skipped: bool,
}

/// The settlement transactions carried by a block accepted on the main chain (the transactions themselves are in the
/// block).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SettlementRecord {
    /// Height of the main-chain block that accepted the block (D-063; a block on the main chain has its own height).
    pub height: u64,
    /// Settlement transaction ID (internal byte order).
    pub txid: [u8; 32],
    /// Producer (funding) inputs, which cannot be used for funding again before confirmation (D-037).
    pub funding_inputs: Vec<([u8; 32], u32)>,
    /// Whether it has been broadcast.
    pub broadcast: bool,
}

/// A block accepted on the main chain: the block and the receipts of its entries.
pub struct AcceptedBlock<'a> {
    /// The block.
    pub block: &'a Block,
    /// Receipts, one per entry.
    pub receipts: &'a [Receipt],
}

/// What became of an op at its position in the operation index (D-111): the status of its receipt there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum OpOutcome {
    /// Executed successfully.
    Success,
    /// Executed, and failed (the fee is still charged, the state is rolled back).
    Failed,
    /// Skipped (the lenient mode of D-063): not executed.
    Skipped,
}

/// An op entry of a block the main chain accepted, as the operation index keeps it (D-111).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct IndexedOp {
    /// The op ID.
    pub op_id: [u8; 32],
    /// The block that includes the entry.
    pub block: [u8; 32],
    /// What became of the op there.
    pub outcome: OpOutcome,
    /// The accounts it is listed under: the signer, the receiving account of a transfer, the contract called, those
    /// its standard events name with the contracts that emitted them (D-120), and those its changes to built-in assets
    /// name with the assets (D-122).
    pub accounts: Vec<[u8; 32]>,
}

/// One op of a page of the operation index (D-111).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedOp {
    /// Its position on the main chain.
    pub position: OpPosition,
    /// The entry.
    pub op: IndexedOp,
}

/// A page of the operation index (D-111), read from one snapshot of the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpPage {
    /// The ops, newest first.
    pub ops: Vec<ListedOp>,
    /// Where the next page starts (the listing continues below it); `None` when nothing is left below the page.
    pub next: Option<OpPosition>,
    /// The main-chain tip the page describes: its hash and height.
    pub tip: ([u8; 32], u64),
}

/// A page of a listing of the standards index (D-120), read from one snapshot of the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexPage<T> {
    /// The items.
    pub items: Vec<T>,
    /// The cursor of the last item when more follow it: passed back as `after`, the listing continues after it.
    pub next: Option<Vec<u8>>,
    /// The main-chain tip the page describes: its hash and height.
    pub tip: ([u8; 32], u64),
}

/// Block store.
pub struct ChainStore {
    db: DB,
    /// The next DAG insertion sequence number.
    next_seq: Mutex<u64>,
}

fn decode<T: BorshDeserialize>(bytes: &[u8]) -> Result<T, NodeError> {
    borsh::from_slice(bytes).map_err(|e| NodeError::Storage(format!("stored data is corrupt: {e}")))
}

/// The start of a block's receipts as stored since D-122, before the format byte: the largest `u32`. A value stored
/// before is Borsh of the receipts, which starts with their number as a `u32`, and no block has that many entries, so
/// it never starts so.
pub const RECEIPTS_MAGIC: [u8; 4] = u32::MAX.to_le_bytes();

/// The format of the receipts written after [`RECEIPTS_MAGIC`]: Borsh(`Vec<Receipt>`) with the asset changes (D-122).
const RECEIPTS_FORMAT: u8 = 2;

/// A receipt as a version before D-122 stored it: [`Receipt`] without its asset changes.
#[derive(BorshDeserialize)]
struct ReceiptBefore {
    op_id: Option<[u8; 32]>,
    status: OpStatus,
    gas_used: u64,
    output: Vec<u8>,
    events: Vec<Event>,
}

/// The receipts of a block as the store keeps them (D-122).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredReceipts {
    /// One per entry.
    pub receipts: Vec<Receipt>,
    /// Whether they record the entries' changes to built-in assets: receipts a version before D-122 stored do not,
    /// and their `asset_changes` are empty.
    pub asset_changes: bool,
}

fn encode_receipts(receipts: &[Receipt]) -> Result<Vec<u8>, NodeError> {
    let mut bytes = RECEIPTS_MAGIC.to_vec();
    bytes.push(RECEIPTS_FORMAT);
    borsh::to_writer(&mut bytes, receipts).map_err(|e| NodeError::Internal(e.to_string()))?;
    Ok(bytes)
}

fn decode_receipts(bytes: &[u8]) -> Result<StoredReceipts, NodeError> {
    let Some(rest) = bytes.strip_prefix(RECEIPTS_MAGIC.as_slice()) else {
        let before: Vec<ReceiptBefore> = decode(bytes)?;
        let receipts = before
            .into_iter()
            .map(|r| Receipt {
                op_id: r.op_id,
                status: r.status,
                gas_used: r.gas_used,
                output: r.output,
                events: r.events,
                asset_changes: Vec::new(),
            })
            .collect();
        return Ok(StoredReceipts {
            receipts,
            asset_changes: false,
        });
    };
    match rest.split_first() {
        Some((&RECEIPTS_FORMAT, receipts)) => Ok(StoredReceipts {
            receipts: decode(receipts)?,
            asset_changes: true,
        }),
        _ => Err(NodeError::Storage(
            "stored receipts are in a format this version does not know".into(),
        )),
    }
}

fn encode<T: BorshSerialize>(value: &T) -> Result<Vec<u8>, NodeError> {
    borsh::to_vec(value).map_err(|e| NodeError::Internal(e.to_string()))
}

fn seq_of(bytes: &[u8]) -> Result<u64, NodeError> {
    let seq: [u8; 8] = bytes
        .try_into()
        .map_err(|_| NodeError::Storage("DAG sequence number is not 8 bytes".into()))?;
    Ok(u64::from_be_bytes(seq))
}

fn hash32(bytes: &[u8]) -> Result<[u8; 32], NodeError> {
    bytes
        .try_into()
        .map_err(|_| NodeError::Storage("stored hash is not 32 bytes".into()))
}

/// The key of a position in `op_seq` (and after the account in `account_ops`): height, slot and entry, big-endian, so
/// that keys sort in the main chain's execution order.
fn position_key(position: OpPosition) -> Vec<u8> {
    let mut key = Vec::with_capacity(16);
    key.extend_from_slice(&position.height.to_be_bytes());
    key.extend_from_slice(&position.slot.to_be_bytes());
    key.extend_from_slice(&position.entry.to_be_bytes());
    key
}

fn position_of(key: &[u8]) -> Result<OpPosition, NodeError> {
    let corrupt = || NodeError::Storage("operation index key is corrupt".into());
    let (height, rest) = key.split_first_chunk::<8>().ok_or_else(corrupt)?;
    let (slot, rest) = rest.split_first_chunk::<4>().ok_or_else(corrupt)?;
    let entry: [u8; 4] = rest.try_into().map_err(|_| corrupt())?;
    Ok(OpPosition {
        height: u64::from_be_bytes(*height),
        slot: u32::from_be_bytes(*slot),
        entry: u32::from_be_bytes(entry),
    })
}

fn account_key(account: &[u8; 32], position: OpPosition) -> Vec<u8> {
    let mut key = Vec::with_capacity(48);
    key.extend_from_slice(account);
    key.extend_from_slice(&position_key(position));
    key
}

/// The value of `meta` `"index"`: the tip the operation index and the standards index describe and its height.
fn index_marker(tip: &[u8; 32], height: u64) -> Vec<u8> {
    let mut value = Vec::with_capacity(40);
    value.extend_from_slice(tip);
    value.extend_from_slice(&height.to_be_bytes());
    value
}

fn marker_of(value: &[u8]) -> Result<([u8; 32], u64), NodeError> {
    let corrupt = || NodeError::Storage("operation index marker is corrupt".into());
    let (tip, height) = value.split_first_chunk::<32>().ok_or_else(corrupt)?;
    let height: [u8; 8] = height.try_into().map_err(|_| corrupt())?;
    Ok((*tip, u64::from_be_bytes(height)))
}

/// The accounts an op is listed under in the operation index by its action (D-111): the signer, the receiving account of
/// a transfer and the contract called, the same as the web page's own index of the blocks it reads. For an op that
/// succeeded, the contracts whose standard events the standards index applied and the accounts those events name join
/// them in `stage_block_ops` (D-120), and so do the accounts and assets its asset changes name ([`changed`], D-122),
/// except the contract a deployment creates ([`deployed`]).
fn op_accounts(op: &VmOp) -> Vec<[u8; 32]> {
    let other = match &op.action {
        Action::AssetTransfer { to, .. } => Some(*to),
        Action::Call { contract, .. } => Some(*contract),
        Action::Deploy { .. } => None,
    };
    let mut accounts = vec![op.account];
    accounts.extend(other.filter(|other| *other != op.account));
    accounts
}

/// The accounts and assets an op's changes to built-in assets name (D-122): the two sides of a movement, the account a
/// freeze or an allowlist change names, and each asset but KSA, so that a built-in asset's own listing is its history.
/// Never the zero account, which no one holds, and KSA's ID is the same 32 zero bytes.
fn changed(changes: &[AssetChange]) -> Vec<[u8; 32]> {
    let mut named = Vec::new();
    for change in changes {
        named.push(change.asset);
        match &change.kind {
            ChangeKind::Transfer { from, to, .. }
            | ChangeKind::Funds { from, to, .. }
            | ChangeKind::Payout { from, to, .. } => named.extend([*from, *to]),
            ChangeKind::Issue { to, .. } => named.push(*to),
            ChangeKind::Burn { from, .. } => named.push(*from),
            ChangeKind::Freeze { account, .. } | ChangeKind::Allow { account, .. } => {
                named.push(*account)
            }
            ChangeKind::Create { .. } | ChangeKind::Pause { .. } | ChangeKind::Renounce { .. } => {}
        }
    }
    named.retain(|account| *account != KSA_ASSET_ID);
    named
}

/// The contract a deployment creates. A deployment stays off that contract's listing even when its `init` declares a
/// standard or the deployment attaches funds (D-111 lists it under its signer only): a page that looks for a contract's
/// deployment below the earliest op listed under the contract keeps finding it.
fn deployed(op: &VmOp) -> Option<[u8; 32]> {
    match &op.action {
        Action::Deploy { code, .. } => Some(ksana_execution::builtin::contract_id(
            &op.account,
            op.nonce,
            &sha256(code),
        )),
        Action::AssetTransfer { .. } | Action::Call { .. } => None,
    }
}

fn slot_of(slot: usize) -> Result<u32, NodeError> {
    u32::try_from(slot).map_err(|e| NodeError::Internal(e.to_string()))
}

/// The cursor of a page of the standards index: the key suffix of its last entry, when more follow.
fn next_of(entries: &[(Vec<u8>, Vec<u8>)], more: bool) -> Option<Vec<u8>> {
    more.then(|| entries.last().map(|(suffix, _)| suffix.clone()))
        .flatten()
}

/// The key of a change in `standards_undo`: the height and the change's number there.
fn undo_key(height: u64, number: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(12);
    key.extend_from_slice(&height.to_be_bytes());
    key.extend_from_slice(&number.to_be_bytes());
    key
}

/// The writes of the standards index (D-120) staged for one batch, read back before the store's: `writes` holds every
/// key written since the overlay began or last staged them, `before` the value each key had before the current height
/// first wrote it, for that height's undo log.
struct Overlay<'s> {
    store: &'s ChainStore,
    writes: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    before: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
}

impl standards::Kv for Overlay<'_> {
    fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, NodeError> {
        match self.writes.get(key) {
            Some(value) => Ok(value.clone()),
            None => self.store.get(CF_STANDARDS, key),
        }
    }

    fn set(&mut self, key: Vec<u8>, value: Option<Vec<u8>>) -> Result<(), NodeError> {
        if !self.before.contains_key(&key) {
            let before = self.get(&key)?;
            self.before.insert(key.clone(), before);
        }
        self.writes.insert(key, value);
        Ok(())
    }
}

impl<'s> Overlay<'s> {
    fn new(store: &'s ChainStore) -> Self {
        Overlay {
            store,
            writes: BTreeMap::new(),
            before: BTreeMap::new(),
        }
    }

    /// Stages the undo log of `height`, the value before of each key it changed, and starts the next height.
    fn end_height(&mut self, batch: &mut WriteBatch, height: u64) -> Result<(), NodeError> {
        let undo = self.store.cf(CF_STANDARDS_UNDO)?;
        let mut number = 0u32;
        for (key, before) in std::mem::take(&mut self.before) {
            if self.writes.get(&key) == Some(&before) {
                continue;
            }
            batch.put_cf(undo, undo_key(height, number), encode(&(key, before))?);
            number = number.checked_add(1).ok_or_else(|| {
                NodeError::Internal(format!(
                    "too many changes of the standards index at height {height}"
                ))
            })?;
        }
        Ok(())
    }

    /// Stages the writes: the store reads them once the batch is written.
    fn stage(&mut self, batch: &mut WriteBatch) -> Result<(), NodeError> {
        let cf = self.store.cf(CF_STANDARDS)?;
        for (key, value) in std::mem::take(&mut self.writes) {
            match value {
                Some(value) => batch.put_cf(cf, key, value),
                None => batch.delete_cf(cf, key),
            }
        }
        Ok(())
    }
}

impl ChainStore {
    /// Open (create when it does not exist), within the RocksDB memory bounds of `memory`, whose block cache it shares
    /// with the state store (D-103).
    pub fn open(path: &Path, memory: &DbMemory) -> Result<Self, NodeError> {
        let db = memory.open(path, &COLUMN_FAMILIES)?;
        let next_seq = {
            let cf = db
                .cf_handle(CF_DAG)
                .ok_or_else(|| NodeError::Storage(format!("missing column family {CF_DAG}")))?;
            match db.iterator_cf(cf, rocksdb::IteratorMode::End).next() {
                Some(item) => {
                    let (key, _) = item?;
                    seq_of(&key)?
                        .checked_add(1)
                        .ok_or_else(|| NodeError::Storage("DAG sequence number overflow".into()))?
                }
                None => 0,
            }
        };
        Ok(ChainStore {
            db,
            next_seq: Mutex::new(next_seq),
        })
    }

    /// The bytes RocksDB's write buffers of this database hold (D-103).
    pub fn memtable_bytes(&self) -> Result<u64, NodeError> {
        Ok(rocks::memtable_bytes(&self.db, &COLUMN_FAMILIES)?)
    }

    fn cf(&self, name: &str) -> Result<&rocksdb::ColumnFamily, NodeError> {
        self.db
            .cf_handle(name)
            .ok_or_else(|| NodeError::Storage(format!("missing column family {name}")))
    }

    fn get(&self, cf: &str, key: &[u8]) -> Result<Option<Vec<u8>>, NodeError> {
        Ok(self.db.get_cf(self.cf(cf)?, key)?)
    }

    /// The main-chain tip.
    pub fn tip(&self) -> Result<Option<[u8; 32]>, NodeError> {
        self.get(CF_META, META_TIP)?.map(|b| hash32(&b)).transpose()
    }

    /// The genesis block hash.
    pub fn genesis(&self) -> Result<Option<[u8; 32]>, NodeError> {
        self.get(CF_META, META_GENESIS)?
            .map(|b| hash32(&b))
            .transpose()
    }

    /// The genesis config fingerprint.
    pub fn config_fingerprint(&self) -> Result<Option<[u8; 32]>, NodeError> {
        self.get(CF_META, META_CONFIG)?
            .map(|b| hash32(&b))
            .transpose()
    }

    /// A block.
    pub fn block(&self, hash: &[u8; 32]) -> Result<Option<Block>, NodeError> {
        self.get(CF_BLOCKS, hash)?.map(|b| decode(&b)).transpose()
    }

    /// The receipts of a block (D-122: those stored before D-122 without their asset changes).
    pub fn receipts(&self, hash: &[u8; 32]) -> Result<Option<StoredReceipts>, NodeError> {
        self.get(CF_RECEIPTS, hash)?
            .map(|b| decode_receipts(&b))
            .transpose()
    }

    /// Every block in the DAG, in insertion order (parents always before children, the genesis block first).
    pub fn dag_order(&self) -> Result<Vec<[u8; 32]>, NodeError> {
        Ok(self
            .dag_from(0, usize::MAX)?
            .into_iter()
            .map(|(_, hash)| hash)
            .collect())
    }

    /// The blocks inserted into the DAG with sequence number `from` or later, in insertion order, at most `max` of
    /// them, with their sequence numbers.
    pub fn dag_from(&self, from: u64, max: usize) -> Result<Vec<(u64, [u8; 32])>, NodeError> {
        let mut out = Vec::new();
        let start = from.to_be_bytes();
        for item in self.db.iterator_cf(
            self.cf(CF_DAG)?,
            rocksdb::IteratorMode::From(&start, rocksdb::Direction::Forward),
        ) {
            if out.len() >= max {
                break;
            }
            let (key, value) = item?;
            out.push((seq_of(&key)?, hash32(&value)?));
        }
        Ok(out)
    }

    /// The insertion sequence number of a block this node has inserted into its DAG (also after it was pruned from
    /// memory, D-097); `None` for a block never inserted.
    pub fn dag_seq(&self, hash: &[u8; 32]) -> Result<Option<u64>, NodeError> {
        self.get(CF_DAG_SEQ, hash)?.map(|b| seq_of(&b)).transpose()
    }

    /// The sequence number the next block inserted into the DAG gets.
    pub fn next_seq(&self) -> Result<u64, NodeError> {
        self.next_seq
            .lock()
            .map(|seq| *seq)
            .map_err(|_| NodeError::Internal("DAG sequence number lock is poisoned".into()))
    }

    /// The summary of the main-chain block at `height` and of the blocks it merges (D-097).
    pub fn summary_at(&self, height: u64) -> Result<Option<MainEntry>, NodeError> {
        self.get(CF_SUMMARIES, &height.to_be_bytes())?
            .map(|b| decode(&b))
            .transpose()
    }

    /// The height of the main-chain block that accepted block `hash` (its own height for a main-chain block) and its
    /// summary; `None` when the main chain does not accept it (D-097).
    pub fn accepted(&self, hash: &[u8; 32]) -> Result<Option<(u64, BlockSummary)>, NodeError> {
        self.get(CF_ACCEPTED, hash)?.map(|b| decode(&b)).transpose()
    }

    /// Writes the summary of the main-chain block at `height` and the acceptance index of the blocks it accepts (for a
    /// data directory whose index is not complete yet: the genesis block, or a main chain connected by an older
    /// version).
    pub fn put_main_index(&self, height: u64, entry: &MainEntry) -> Result<(), NodeError> {
        let mut batch = WriteBatch::default();
        self.stage_main_index(&mut batch, height, entry)?;
        self.write(batch)
    }

    fn stage_main_index(
        &self,
        batch: &mut WriteBatch,
        height: u64,
        entry: &MainEntry,
    ) -> Result<(), NodeError> {
        batch.put_cf(self.cf(CF_SUMMARIES)?, height.to_be_bytes(), encode(entry)?);
        for block in entry.accepted() {
            batch.put_cf(self.cf(CF_ACCEPTED)?, block.hash, encode(&(height, block))?);
        }
        Ok(())
    }

    /// The hash of the block at a height on the main chain.
    pub fn hash_at(&self, height: u64) -> Result<Option<[u8; 32]>, NodeError> {
        self.get(CF_MAIN, &height.to_be_bytes())?
            .map(|b| hash32(&b))
            .transpose()
    }

    /// Where an op is located (main chain).
    pub fn op_location(&self, op_id: &[u8; 32]) -> Result<Option<OpLocation>, NodeError> {
        self.get(CF_OPS, op_id)?.map(|b| decode(&b)).transpose()
    }

    /// Whether the operation index (D-111) and the standards index (D-120) describe the stored main chain.
    pub fn index_current(&self) -> Result<bool, NodeError> {
        Ok(self.described_tip(&self.db.snapshot())?.is_some())
    }

    /// The tip of the stored main chain and its height, when the operation index and the standards index describe it:
    /// `meta` `"index"` names the tip, and no marker of a version without the standards index is left. `None`
    /// otherwise.
    fn described_tip(
        &self,
        snapshot: &rocksdb::Snapshot<'_>,
    ) -> Result<Option<([u8; 32], u64)>, NodeError> {
        let meta = self.cf(CF_META)?;
        let tip = snapshot
            .get_cf(meta, META_TIP)?
            .map(|b| hash32(&b))
            .transpose()?;
        let marker = snapshot
            .get_cf(meta, META_INDEX)?
            .map(|b| marker_of(&b))
            .transpose()?;
        let legacy = snapshot.get_cf(meta, META_LEGACY_OP_INDEX)?.is_some();
        Ok(match (marker, tip) {
            (Some((indexed, height)), Some(tip)) if indexed == tip && !legacy => {
                Some((tip, height))
            }
            _ => None,
        })
    }

    /// A page of the operation index (D-111), newest first: the ops listed under `account` (every op with `None`)
    /// below position `before` (from the newest with `None`), at most `limit` of them, all read from one snapshot of
    /// the store. An entry of an op the `ops` index places elsewhere (a skipped inclusion of an op executed at another
    /// position, or another inclusion of an op skipped before) is passed over, at most [`MAX_HIDDEN_PER_PAGE`] of them
    /// per page. `None` while the index does not describe the stored main chain.
    pub fn list_ops(
        &self,
        account: Option<&[u8; 32]>,
        before: Option<OpPosition>,
        limit: usize,
    ) -> Result<Option<OpPage>, NodeError> {
        let snapshot = self.db.snapshot();
        let Some(tip) = self.described_tip(&snapshot)? else {
            return Ok(None);
        };
        let seq = self.cf(CF_OP_SEQ)?;
        let ops = self.cf(CF_OPS)?;
        let (cf, prefix) = match account {
            Some(account) => (self.cf(CF_ACCOUNT_OPS)?, account.to_vec()),
            None => (seq, Vec::new()),
        };
        let mut seek = prefix.clone();
        match before {
            Some(before) => seek.extend_from_slice(&position_key(before)),
            None => seek.extend_from_slice(&[0xff; 16]),
        }
        let limit = limit.max(1);
        let mut page = Vec::new();
        let mut hidden = 0usize;
        let mut last = None;
        let mut next = None;
        for item in snapshot.iterator_cf(cf, IteratorMode::From(&seek, Direction::Reverse)) {
            let (key, value) = item?;
            let Some(rest) = key.strip_prefix(prefix.as_slice()) else {
                break;
            };
            let position = position_of(rest)?;
            if before.is_some_and(|before| position >= before) {
                continue;
            }
            if page.len() >= limit || hidden >= MAX_HIDDEN_PER_PAGE {
                // More below the page: the next one continues below the last position looked at
                next = last;
                break;
            }
            last = Some(position);
            let op: IndexedOp = match account {
                Some(_) => decode(&snapshot.get_cf(seq, position_key(position))?.ok_or_else(
                    || {
                        NodeError::Storage(format!(
                            "operation index has no entry at position {position}"
                        ))
                    },
                )?)?,
                None => decode(&value)?,
            };
            let location: Option<OpLocation> = snapshot
                .get_cf(ops, op.op_id)?
                .map(|b| decode(&b))
                .transpose()?;
            if location.is_some_and(|l| l.block == op.block && l.index == position.entry) {
                page.push(ListedOp { position, op });
            } else {
                hidden = hidden.saturating_add(1);
            }
        }
        Ok(Some(OpPage {
            ops: page,
            next,
            tip,
        }))
    }

    /// The record the standards index keeps of `contract` (D-120): `Some` once it made its declaration. `None` while
    /// the index does not describe the stored main chain; otherwise with the tip it describes.
    #[allow(clippy::type_complexity)]
    pub fn declared(
        &self,
        contract: &[u8; 32],
    ) -> Result<Option<(Option<Declared>, ([u8; 32], u64))>, NodeError> {
        let snapshot = self.db.snapshot();
        let Some(tip) = self.described_tip(&snapshot)? else {
            return Ok(None);
        };
        let declared = snapshot
            .get_cf(self.cf(CF_STANDARDS)?, standards::token_key(contract))?
            .map(|b| standards::declared_of(&b))
            .transpose()?;
        Ok(Some((declared, tip)))
    }

    /// The entries of the standards index under `prefix` after the key suffix `after` (from the first with `None`), in
    /// key order or, `descending`, the reverse, at most `limit` of them: their suffixes and values, and whether more
    /// follow.
    #[allow(clippy::type_complexity)]
    fn standards_page(
        &self,
        snapshot: &rocksdb::Snapshot<'_>,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
        descending: bool,
    ) -> Result<(Vec<(Vec<u8>, Vec<u8>)>, bool), NodeError> {
        let mut seek = prefix.to_vec();
        match after {
            Some(after) => seek.extend_from_slice(after),
            // Above every key under the prefix: the suffixes are shorter
            None if descending => seek.extend_from_slice(&[0xff; 128]),
            None => {}
        }
        let direction = if descending {
            Direction::Reverse
        } else {
            Direction::Forward
        };
        let mut items = Vec::new();
        for item in
            snapshot.iterator_cf(self.cf(CF_STANDARDS)?, IteratorMode::From(&seek, direction))
        {
            let (key, value) = item?;
            let Some(suffix) = key.strip_prefix(prefix) else {
                break;
            };
            if after.is_some_and(|after| suffix == after) {
                continue;
            }
            if items.len() >= limit.max(1) {
                return Ok((items, true));
            }
            items.push((suffix.to_vec(), value.to_vec()));
        }
        Ok((items, false))
    }

    /// The tokens and NFT collections `account` holds by the standards index (D-120), by contract ID after `after`, at
    /// most `limit`: each contract, its standard and the amount (a balance, or a number of NFTs). `None` while the
    /// index does not describe the stored main chain.
    #[allow(clippy::type_complexity)]
    pub fn account_amounts(
        &self,
        account: &[u8; 32],
        after: Option<&[u8; 32]>,
        limit: usize,
    ) -> Result<Option<IndexPage<([u8; 32], Standard, u128)>>, NodeError> {
        let snapshot = self.db.snapshot();
        let Some(tip) = self.described_tip(&snapshot)? else {
            return Ok(None);
        };
        let prefix = standards::balances_prefix(account);
        let (entries, more) = self.standards_page(
            &snapshot,
            &prefix,
            after.map(|a| a.as_slice()),
            limit,
            false,
        )?;
        let mut items = Vec::with_capacity(entries.len());
        for (suffix, value) in &entries {
            let contract = hash32(suffix)?;
            let standard = snapshot
                .get_cf(self.cf(CF_STANDARDS)?, standards::token_key(&contract))?
                .map(|b| standards::declared_of(&b))
                .transpose()?
                .and_then(|d| d.standard)
                .ok_or_else(|| {
                    NodeError::Storage(
                        "the standards index holds an amount of a contract it does not follow"
                            .into(),
                    )
                })?;
            items.push((contract, standard, standards::amount_from(value)?));
        }
        Ok(Some(IndexPage {
            next: next_of(&entries, more),
            items,
            tip,
        }))
    }

    /// The holders of `contract` by the standards index (D-120), the largest amount first (the same amount by account,
    /// the highest first), after `after` (an amount and an account), at most `limit`. `None` while the index does not
    /// describe the stored main chain.
    #[allow(clippy::type_complexity)]
    pub fn holders(
        &self,
        contract: &[u8; 32],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Option<IndexPage<(u128, [u8; 32])>>, NodeError> {
        let snapshot = self.db.snapshot();
        let Some(tip) = self.described_tip(&snapshot)? else {
            return Ok(None);
        };
        let prefix = standards::holders_prefix(contract);
        let (entries, more) = self.standards_page(&snapshot, &prefix, after, limit, true)?;
        let items = entries
            .iter()
            .map(|(suffix, _)| standards::holder_from(suffix))
            .collect::<Result<_, _>>()?;
        Ok(Some(IndexPage {
            next: next_of(&entries, more),
            items,
            tip,
        }))
    }

    /// The NFTs `account` holds by the standards index (D-120), of `contract` only when given, by contract and token
    /// ID after `after` (a contract and a token ID), at most `limit`. `None` while the index does not describe the
    /// stored main chain.
    #[allow(clippy::type_complexity)]
    pub fn nfts(
        &self,
        account: &[u8; 32],
        contract: Option<&[u8; 32]>,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Option<IndexPage<([u8; 32], [u8; 32])>>, NodeError> {
        let snapshot = self.db.snapshot();
        let Some(tip) = self.described_tip(&snapshot)? else {
            return Ok(None);
        };
        let mut prefix = standards::owned_prefix(account);
        // Within one contract the cursor's token ID follows it
        let after = match contract {
            Some(contract) => {
                prefix.extend_from_slice(contract);
                after.and_then(|a| a.strip_prefix(contract.as_slice()))
            }
            None => after,
        };
        let (entries, more) = self.standards_page(&snapshot, &prefix, after, limit, false)?;
        let items: Vec<([u8; 32], [u8; 32])> = entries
            .iter()
            .map(|(suffix, _)| match contract {
                Some(contract) => Ok((*contract, hash32(suffix)?)),
                None => standards::nft_from(suffix),
            })
            .collect::<Result<_, NodeError>>()?;
        let next = more
            .then(|| items.last().map(|(c, t)| [c.as_slice(), t].concat()))
            .flatten();
        Ok(Some(IndexPage { items, next, tip }))
    }

    /// The approvals `owner` gave by the standards index (D-120), by contract, kind and ID after `after`, at most
    /// `limit`. `None` while the index does not describe the stored main chain.
    pub fn approvals(
        &self,
        owner: &[u8; 32],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Option<IndexPage<Approval>>, NodeError> {
        let snapshot = self.db.snapshot();
        let Some(tip) = self.described_tip(&snapshot)? else {
            return Ok(None);
        };
        let prefix = standards::approvals_prefix(owner);
        let (entries, more) = self.standards_page(&snapshot, &prefix, after, limit, false)?;
        let items = entries
            .iter()
            .map(|(suffix, value)| standards::approval_from(suffix, value))
            .collect::<Result<_, _>>()?;
        Ok(Some(IndexPage {
            next: next_of(&entries, more),
            items,
            tip,
        }))
    }

    /// Builds the operation index (D-111) and the standards index (D-120) again from the stored main chain: clears
    /// them, indexes the blocks each main-chain height accepted with their receipts (from the stored summaries, which
    /// must be complete), and marks them as describing the tip. Written in several batches with the marker last: a
    /// build that stops midway starts over at the next start. Returns the tip's height.
    pub fn rebuild_index(&self) -> Result<u64, NodeError> {
        const UPDATES_PER_BATCH: usize = 16_384;
        let tip = self
            .tip()?
            .ok_or_else(|| NodeError::Storage("main-chain tip missing".into()))?;
        let tip_height = self
            .block(&tip)?
            .ok_or_else(|| NodeError::Storage("block of the main-chain tip is missing".into()))?
            .header
            .height;
        let mut batch = WriteBatch::default();
        batch.delete_cf(self.cf(CF_META)?, META_INDEX);
        batch.delete_cf(self.cf(CF_META)?, META_LEGACY_OP_INDEX);
        self.write(batch)?;
        for name in [CF_OP_SEQ, CF_ACCOUNT_OPS, CF_STANDARDS, CF_STANDARDS_UNDO] {
            let cf = self.cf(name)?;
            let mut batch = WriteBatch::default();
            for item in self.db.iterator_cf(cf, IteratorMode::Start) {
                let (key, _) = item?;
                batch.delete_cf(cf, key);
                if batch.len() >= UPDATES_PER_BATCH {
                    self.write(std::mem::take(&mut batch))?;
                }
            }
            self.write(batch)?;
        }
        let mut batch = WriteBatch::default();
        let mut overlay = Overlay::new(self);
        for height in 1..=tip_height {
            let entry = self.summary_at(height)?.ok_or_else(|| {
                NodeError::Storage(format!("main-chain summary missing at height {height}"))
            })?;
            for (slot, accepted) in entry.accepted().enumerate() {
                let block = self.block(&accepted.hash)?.ok_or_else(|| {
                    NodeError::Storage("block accepted by the main chain is missing".into())
                })?;
                let stored = self.receipts(&accepted.hash)?.ok_or_else(|| {
                    NodeError::Storage(
                        "receipts of a block accepted by the main chain are missing".into(),
                    )
                })?;
                self.stage_block_ops(
                    &mut batch,
                    &mut overlay,
                    height,
                    slot_of(slot)?,
                    &block,
                    &stored.receipts,
                )?;
            }
            overlay.end_height(&mut batch, height)?;
            if batch.len().saturating_add(overlay.writes.len()) >= UPDATES_PER_BATCH {
                overlay.stage(&mut batch)?;
                self.write(std::mem::take(&mut batch))?;
            }
        }
        overlay.stage(&mut batch)?;
        batch.put_cf(
            self.cf(CF_META)?,
            META_INDEX,
            index_marker(&tip, tip_height),
        );
        self.write(batch)?;
        Ok(tip_height)
    }

    /// Stages the operation index entries of the op entries of `block`, accepted at `height` in place `slot` of that
    /// main-chain block's acceptance order, with its receipts (D-111), and applies the standard events of the ops that
    /// succeeded to the standards index through `overlay` (D-120).
    fn stage_block_ops(
        &self,
        batch: &mut WriteBatch,
        overlay: &mut Overlay<'_>,
        height: u64,
        slot: u32,
        block: &Block,
        receipts: &[Receipt],
    ) -> Result<(), NodeError> {
        let hash = block.header.hash();
        for (index, entry) in block.entries.iter().enumerate() {
            let op = match entry {
                Entry::Forced { op, .. } | Entry::Sponsored { op } => op,
                Entry::Unlock { .. } => continue,
            };
            let receipt = receipts
                .get(index)
                .ok_or_else(|| NodeError::Internal("an entry has no receipt".into()))?;
            let outcome = match &receipt.status {
                OpStatus::Success => OpOutcome::Success,
                OpStatus::Failed(_) => OpOutcome::Failed,
                OpStatus::Skipped(_) => OpOutcome::Skipped,
            };
            let position = OpPosition {
                height,
                slot,
                entry: u32::try_from(index).map_err(|e| NodeError::Internal(e.to_string()))?,
            };
            let mut accounts = op_accounts(op);
            if outcome == OpOutcome::Success {
                let mut named = standards::apply(
                    overlay,
                    (position.height, position.slot, position.entry),
                    &receipt.events,
                )?;
                named.extend(changed(&receipt.asset_changes));
                let created = deployed(op);
                for account in named {
                    if !accounts.contains(&account) && Some(account) != created {
                        accounts.push(account);
                    }
                }
            }
            let indexed = IndexedOp {
                op_id: op.op_id()?,
                block: hash,
                outcome,
                accounts,
            };
            for account in &indexed.accounts {
                batch.put_cf(self.cf(CF_ACCOUNT_OPS)?, account_key(account, position), []);
            }
            batch.put_cf(
                self.cf(CF_OP_SEQ)?,
                position_key(position),
                encode(&indexed)?,
            );
        }
        Ok(())
    }

    /// Stages the return of the standards index to what it was below height `from` (D-120): the undo log from the
    /// newest change down to `from`, each key back to its value before, and the log's entries removed.
    fn stage_undo_from(&self, batch: &mut WriteBatch, from: u64) -> Result<(), NodeError> {
        let undo = self.cf(CF_STANDARDS_UNDO)?;
        let index = self.cf(CF_STANDARDS)?;
        let start = from.to_be_bytes();
        for item in self.db.iterator_cf(undo, IteratorMode::End) {
            let (key, value) = item?;
            if key.as_ref() < start.as_slice() {
                break;
            }
            let (changed, before): (Vec<u8>, Option<Vec<u8>>) = decode(&value)?;
            match before {
                Some(before) => batch.put_cf(index, changed, before),
                None => batch.delete_cf(index, changed),
            }
            batch.delete_cf(undo, key);
        }
        Ok(())
    }

    /// Stages the removal of the operation index entries at height `from` and above (D-111).
    fn stage_unindex_from(&self, batch: &mut WriteBatch, from: u64) -> Result<(), NodeError> {
        let start = position_key(OpPosition {
            height: from,
            slot: 0,
            entry: 0,
        });
        let seq = self.cf(CF_OP_SEQ)?;
        for item in self
            .db
            .iterator_cf(seq, IteratorMode::From(&start, Direction::Forward))
        {
            let (key, value) = item?;
            let position = position_of(&key)?;
            let indexed: IndexedOp = decode(&value)?;
            for account in &indexed.accounts {
                batch.delete_cf(self.cf(CF_ACCOUNT_OPS)?, account_key(account, position));
            }
            batch.delete_cf(seq, key);
        }
        Ok(())
    }

    /// A settlement record.
    pub fn settlement(&self, hash: &[u8; 32]) -> Result<Option<SettlementRecord>, NodeError> {
        self.get(CF_SETTLEMENTS, hash)?
            .map(|b| decode(&b))
            .transpose()
    }

    /// Every settlement record (loaded at startup).
    pub fn settlements(&self) -> Result<Vec<([u8; 32], SettlementRecord)>, NodeError> {
        let mut out = Vec::new();
        for item in self
            .db
            .iterator_cf(self.cf(CF_SETTLEMENTS)?, rocksdb::IteratorMode::Start)
        {
            let (key, value) = item?;
            out.push((hash32(&key)?, decode(&value)?));
        }
        Ok(out)
    }

    /// Update a settlement record (for example mark it as broadcast).
    pub fn put_settlement(
        &self,
        hash: &[u8; 32],
        record: &SettlementRecord,
    ) -> Result<(), NodeError> {
        let mut batch = WriteBatch::default();
        batch.put_cf(self.cf(CF_SETTLEMENTS)?, hash, encode(record)?);
        self.write(batch)
    }

    /// The verdict on a block kept on disk (D-101): valid or not, and the basis it was given on.
    pub fn verdict(&self, hash: &[u8; 32]) -> Result<Option<(bool, [u8; 32])>, NodeError> {
        Ok(self
            .get(CF_VERDICTS, hash)?
            .and_then(|b| verdict::decode_stored(&b)))
    }

    /// Keeps the verdict on a block given on `basis` (D-101). A cache: written without waiting for the disk, a verdict
    /// lost in a crash is judged again.
    pub fn put_verdict(
        &self,
        hash: &[u8; 32],
        valid: bool,
        basis: &[u8; 32],
    ) -> Result<(), NodeError> {
        Ok(self.db.put_cf(
            self.cf(CF_VERDICTS)?,
            hash,
            verdict::encode_stored(valid, basis),
        )?)
    }

    /// Store a new DAG block (the block and its insertion sequence number; the main chain is unchanged). The caller
    /// guarantees that the parents are all stored and that there is only one writer at a time.
    pub fn put_dag_block(&self, block: &Block) -> Result<(), NodeError> {
        let mut seq = self
            .next_seq
            .lock()
            .map_err(|_| NodeError::Internal("DAG sequence number lock is poisoned".into()))?;
        let hash = block.header.hash();
        let mut batch = WriteBatch::default();
        batch.put_cf(self.cf(CF_BLOCKS)?, hash, encode(block)?);
        batch.put_cf(self.cf(CF_DAG)?, seq.to_be_bytes(), hash);
        batch.put_cf(self.cf(CF_DAG_SEQ)?, hash, seq.to_be_bytes());
        self.write(batch)?;
        *seq = seq
            .checked_add(1)
            .ok_or_else(|| NodeError::Storage("DAG sequence number overflow".into()))?;
        Ok(())
    }

    /// Roll the main chain back to `fork` (the block below `heights`): delete the height index and the summaries of
    /// `heights`, delete the receipts, op index (only the entries pointing at these blocks), acceptance index and
    /// settlement records of the blocks in `disconnected` (the disconnected main-chain blocks and the blocks they
    /// merged), the operation index from the first of `heights` up (D-111) with the changes of the standards index
    /// there (D-120), and set the tip to `fork`. One synchronous batch.
    pub fn rewind_main(
        &self,
        fork: &[u8; 32],
        heights: std::ops::RangeInclusive<u64>,
        disconnected: &[Block],
    ) -> Result<(), NodeError> {
        let mut batch = WriteBatch::default();
        if self.index_current()? {
            let from = *heights.start();
            self.stage_unindex_from(&mut batch, from)?;
            self.stage_undo_from(&mut batch, from)?;
            batch.put_cf(
                self.cf(CF_META)?,
                META_INDEX,
                index_marker(fork, from.saturating_sub(1)),
            );
        }
        for height in heights {
            batch.delete_cf(self.cf(CF_MAIN)?, height.to_be_bytes());
            batch.delete_cf(self.cf(CF_SUMMARIES)?, height.to_be_bytes());
        }
        for block in disconnected {
            let hash = block.header.hash();
            batch.delete_cf(self.cf(CF_SETTLEMENTS)?, hash);
            batch.delete_cf(self.cf(CF_RECEIPTS)?, hash);
            batch.delete_cf(self.cf(CF_ACCEPTED)?, hash);
            for entry in &block.entries {
                let op = match entry {
                    Entry::Forced { op, .. } | Entry::Sponsored { op } => op,
                    Entry::Unlock { .. } => continue,
                };
                let op_id = op.op_id()?;
                let points_here = self
                    .op_location(&op_id)?
                    .is_some_and(|location| location.block == hash);
                if points_here {
                    batch.delete_cf(self.cf(CF_OPS)?, op_id);
                }
            }
        }
        batch.put_cf(self.cf(CF_META)?, META_TIP, fork);
        self.write(batch)
    }

    /// Write the genesis block (data directory initialization).
    pub fn put_genesis(
        &self,
        block: &Block,
        config_fingerprint: [u8; 32],
    ) -> Result<(), NodeError> {
        let hash = block.header.hash();
        let mut seq = self
            .next_seq
            .lock()
            .map_err(|_| NodeError::Internal("DAG sequence number lock is poisoned".into()))?;
        let mut batch = WriteBatch::default();
        batch.put_cf(self.cf(CF_META)?, META_GENESIS, hash);
        batch.put_cf(self.cf(CF_META)?, META_CONFIG, config_fingerprint);
        batch.put_cf(self.cf(CF_BLOCKS)?, hash, encode(block)?);
        batch.put_cf(self.cf(CF_DAG)?, seq.to_be_bytes(), hash);
        batch.put_cf(self.cf(CF_DAG_SEQ)?, hash, seq.to_be_bytes());
        batch.put_cf(self.cf(CF_RECEIPTS)?, hash, encode_receipts(&[])?);
        batch.put_cf(self.cf(CF_MAIN)?, 0u64.to_be_bytes(), hash);
        batch.put_cf(self.cf(CF_META)?, META_TIP, hash);
        // The genesis block has no entries: the empty indexes describe it (D-111, D-120)
        batch.put_cf(self.cf(CF_META)?, META_INDEX, index_marker(&hash, 0));
        self.write(batch)?;
        *seq = seq
            .checked_add(1)
            .ok_or_else(|| NodeError::Storage("DAG sequence number overflow".into()))?;
        Ok(())
    }

    /// Connect `chain` (an already stored DAG block, at height `chain.header.height`) to the main chain: the main-chain
    /// height index, its summary `entry` with the acceptance index of the blocks it accepts, the receipts, op index,
    /// operation index (D-111) and standards index (D-120) of the blocks in `accepted` (the merged blocks in execution
    /// order, `chain` itself last, the order of `entry.accepted()`), the settlement records, and the new tip, written in
    /// one synchronous batch.
    pub fn connect_main(
        &self,
        chain: &Block,
        entry: &MainEntry,
        accepted: &[AcceptedBlock<'_>],
        settlements: &[([u8; 32], SettlementRecord)],
    ) -> Result<(), NodeError> {
        let height = chain.header.height;
        let chain_hash = chain.header.hash();
        let mut batch = WriteBatch::default();
        batch.put_cf(self.cf(CF_MAIN)?, height.to_be_bytes(), chain_hash);
        self.stage_main_index(&mut batch, height, entry)?;
        // Positions written earlier in the same batch: not readable before the batch is committed
        let mut staged: std::collections::BTreeMap<[u8; 32], OpLocation> =
            std::collections::BTreeMap::new();
        for AcceptedBlock { block, receipts } in accepted {
            let hash = block.header.hash();
            batch.put_cf(self.cf(CF_RECEIPTS)?, hash, encode_receipts(receipts)?);
            for (index, entry) in block.entries.iter().enumerate() {
                let op = match entry {
                    Entry::Forced { op, .. } | Entry::Sponsored { op } => op,
                    Entry::Unlock { .. } => continue,
                };
                let op_id = op.op_id()?;
                let skipped = receipts
                    .get(index)
                    .is_some_and(|r| matches!(r.status, OpStatus::Skipped(_)));
                // When the same op is included several times, the first executed position is kept; only a skipped
                // record can be replaced by a later executed record
                let existing = match staged.get(&op_id) {
                    Some(location) => Some(location.clone()),
                    None => self.op_location(&op_id)?,
                };
                if existing.is_some_and(|location| !location.skipped || skipped) {
                    continue;
                }
                let location = OpLocation {
                    block: hash,
                    height,
                    index: u32::try_from(index).map_err(|e| NodeError::Internal(e.to_string()))?,
                    skipped,
                };
                batch.put_cf(self.cf(CF_OPS)?, op_id, encode(&location)?);
                staged.insert(op_id, location);
            }
        }
        for (hash, record) in settlements {
            batch.put_cf(self.cf(CF_SETTLEMENTS)?, hash, encode(record)?);
        }
        // The operation index and the standards index, while they describe the main chain this block extends (D-111,
        // D-120): the slots follow the summary's acceptance order, the order a rebuild reads
        if self.index_current()? {
            let in_order = entry
                .accepted()
                .map(|b| b.hash)
                .eq(accepted.iter().map(|a| a.block.header.hash()));
            if in_order {
                let mut overlay = Overlay::new(self);
                for (slot, AcceptedBlock { block, receipts }) in accepted.iter().enumerate() {
                    self.stage_block_ops(
                        &mut batch,
                        &mut overlay,
                        height,
                        slot_of(slot)?,
                        block,
                        receipts,
                    )?;
                }
                overlay.end_height(&mut batch, height)?;
                overlay.stage(&mut batch)?;
                batch.put_cf(
                    self.cf(CF_META)?,
                    META_INDEX,
                    index_marker(&chain_hash, height),
                );
            } else {
                eprintln!(
                    "[ksana-node] alert: the blocks accepted at height {height} are not in the order of the main chain's \
                     summary; the operation index and the standards index stop here and are built again at the next start \
                     (D-111, D-120)"
                );
            }
        }
        batch.put_cf(self.cf(CF_META)?, META_TIP, chain_hash);
        self.write(batch)
    }

    fn write(&self, batch: WriteBatch) -> Result<(), NodeError> {
        let mut opts = WriteOptions::default();
        opts.set_sync(true);
        Ok(self.db.write_opt(batch, &opts)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIP: [u8; 32] = [0x71; 32];
    const SHOWN: [u8; 32] = [0xb1; 32];

    fn open() -> (tempfile::TempDir, ChainStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ChainStore::open(dir.path(), &DbMemory::new()).unwrap();
        (dir, store)
    }

    fn at(height: u64, slot: u32, entry: u32) -> OpPosition {
        OpPosition {
            height,
            slot,
            entry,
        }
    }

    /// An op ID unique to the position.
    fn op_id(position: OpPosition) -> [u8; 32] {
        let mut id = [0u8; 32];
        for (byte, key) in id.iter_mut().zip(position_key(position)) {
            *byte = key;
        }
        id
    }

    /// Indexes an op at `position` under `accounts`; the `ops` index places it there when `shown`, in another block
    /// otherwise.
    fn put(store: &ChainStore, position: OpPosition, accounts: &[[u8; 32]], shown: bool) {
        let indexed = IndexedOp {
            op_id: op_id(position),
            block: SHOWN,
            outcome: OpOutcome::Success,
            accounts: accounts.to_vec(),
        };
        let location = OpLocation {
            block: if shown { SHOWN } else { [0xb2; 32] },
            height: position.height,
            index: position.entry,
            skipped: false,
        };
        let mut batch = WriteBatch::default();
        batch.put_cf(
            store.cf(CF_OP_SEQ).unwrap(),
            position_key(position),
            encode(&indexed).unwrap(),
        );
        for account in accounts {
            batch.put_cf(
                store.cf(CF_ACCOUNT_OPS).unwrap(),
                account_key(account, position),
                [],
            );
        }
        batch.put_cf(
            store.cf(CF_OPS).unwrap(),
            indexed.op_id,
            encode(&location).unwrap(),
        );
        store.write(batch).unwrap();
    }

    fn mark(store: &ChainStore, tip: [u8; 32], indexed: Option<[u8; 32]>) {
        let mut batch = WriteBatch::default();
        let meta = store.cf(CF_META).unwrap();
        batch.put_cf(meta, META_TIP, tip);
        match indexed {
            Some(indexed) => batch.put_cf(meta, META_INDEX, index_marker(&indexed, 9)),
            None => batch.delete_cf(meta, META_INDEX),
        }
        store.write(batch).unwrap();
    }

    fn positions(page: &OpPage) -> Vec<OpPosition> {
        page.ops.iter().map(|op| op.position).collect()
    }

    #[test]
    fn position_keys_sort_in_execution_order() {
        let ordered = [
            at(0, 0, 0),
            at(0, 0, 1),
            at(0, 0, u32::MAX),
            at(0, 1, 0),
            at(0, u32::MAX, u32::MAX),
            at(1, 0, 0),
            at(256, 0, 0),
            at(u64::MAX, u32::MAX, u32::MAX),
        ];
        for pair in ordered.windows(2) {
            assert!(pair[0] < pair[1]);
            assert!(position_key(pair[0]) < position_key(pair[1]), "{pair:?}");
        }
        for position in ordered {
            assert_eq!(position_of(&position_key(position)).unwrap(), position);
        }
        assert!(position_of(&[0; 15]).is_err());
        assert!(position_of(&[0; 17]).is_err());
        assert_eq!(
            marker_of(&index_marker(&TIP, 81)).unwrap(),
            (TIP, 81),
            "the marker holds the tip and its height"
        );
        assert!(marker_of(&[0; 39]).is_err());
    }

    #[test]
    fn a_listing_pages_down_from_the_newest_within_the_account() {
        let (_dir, store) = open();
        // Neighbouring keys: accounts one apart, below and above
        let (below, account, above) = ([0xa9; 32], [0xaa; 32], [0xab; 32]);
        put(&store, at(1, 0, 0), &[account], true);
        put(&store, at(1, 0, 1), &[below], true);
        put(&store, at(2, 0, 3), &[account, above], true);
        put(&store, at(2, 1, 0), &[above], true);
        put(&store, at(3, 0, 0), &[account], true);
        put(&store, at(4, 0, 0), &[below, above], true);
        mark(&store, TIP, Some(TIP));
        assert!(store.index_current().unwrap());

        let all = store.list_ops(Some(&account), None, 50).unwrap().unwrap();
        assert_eq!(positions(&all), [at(3, 0, 0), at(2, 0, 3), at(1, 0, 0)]);
        assert_eq!(all.next, None);
        assert_eq!(all.tip, (TIP, 9));
        assert_eq!(all.ops[0].op.op_id, op_id(at(3, 0, 0)));

        // Two at a time: `next` is the last position returned, and `before` lists strictly below it
        let first = store.list_ops(Some(&account), None, 2).unwrap().unwrap();
        assert_eq!(positions(&first), [at(3, 0, 0), at(2, 0, 3)]);
        assert_eq!(first.next, Some(at(2, 0, 3)));
        let second = store
            .list_ops(Some(&account), first.next, 2)
            .unwrap()
            .unwrap();
        assert_eq!(positions(&second), [at(1, 0, 0)]);
        assert_eq!(second.next, None);
        // A page that ends on the last entry says nothing is left
        let exact = store.list_ops(Some(&account), None, 3).unwrap().unwrap();
        assert_eq!(exact.next, None);
        // `before` need not be a position of the account
        let mid = store
            .list_ops(Some(&account), Some(at(2, 1, 0)), 50)
            .unwrap()
            .unwrap();
        assert_eq!(positions(&mid), [at(2, 0, 3), at(1, 0, 0)]);
        assert!(store
            .list_ops(Some(&account), Some(at(1, 0, 0)), 50)
            .unwrap()
            .unwrap()
            .ops
            .is_empty());

        let others = store.list_ops(Some(&above), None, 50).unwrap().unwrap();
        assert_eq!(positions(&others), [at(4, 0, 0), at(2, 1, 0), at(2, 0, 3)]);
        let nobody = store
            .list_ops(Some(&[0xac; 32]), None, 50)
            .unwrap()
            .unwrap();
        assert!(nobody.ops.is_empty());
        assert_eq!(nobody.next, None);

        // Every op, newest first
        let latest = store.list_ops(None, None, 4).unwrap().unwrap();
        assert_eq!(
            positions(&latest),
            [at(4, 0, 0), at(3, 0, 0), at(2, 1, 0), at(2, 0, 3)]
        );
        assert_eq!(latest.next, Some(at(2, 0, 3)));
        let rest = store.list_ops(None, latest.next, 4).unwrap().unwrap();
        assert_eq!(positions(&rest), [at(1, 0, 1), at(1, 0, 0)]);
        assert_eq!(rest.next, None);
    }

    #[test]
    fn entries_placed_elsewhere_are_passed_over_up_to_a_bound() {
        let (_dir, store) = open();
        let account = [0xaa; 32];
        put(&store, at(1, 0, 0), &[account], true);
        let hidden = MAX_HIDDEN_PER_PAGE.checked_add(5).unwrap();
        for height in 2..=u64::try_from(hidden).unwrap().checked_add(1).unwrap() {
            put(&store, at(height, 0, 0), &[account], false);
        }
        put(&store, at(u64::MAX, 0, 0), &[account], true);
        mark(&store, TIP, Some(TIP));

        let top = store.list_ops(Some(&account), None, 1).unwrap().unwrap();
        assert_eq!(positions(&top), [at(u64::MAX, 0, 0)]);
        assert_eq!(top.next, Some(at(u64::MAX, 0, 0)));
        // The page ends after looking at MAX_HIDDEN_PER_PAGE entries placed elsewhere, empty, and the next one
        // continues below the last of them
        let page = store
            .list_ops(Some(&account), top.next, 50)
            .unwrap()
            .unwrap();
        assert!(page.ops.is_empty());
        let reached = u64::try_from(hidden)
            .unwrap()
            .checked_sub(MAX_HIDDEN_PER_PAGE as u64)
            .unwrap()
            .checked_add(2)
            .unwrap();
        assert_eq!(page.next, Some(at(reached, 0, 0)));
        let last = store
            .list_ops(Some(&account), page.next, 50)
            .unwrap()
            .unwrap();
        assert_eq!(positions(&last), [at(1, 0, 0)]);
        assert_eq!(last.next, None);
    }

    /// Everything in the standards index.
    fn standards_dump(store: &ChainStore) -> Vec<(Vec<u8>, Vec<u8>)> {
        store
            .db
            .iterator_cf(store.cf(CF_STANDARDS).unwrap(), IteratorMode::Start)
            .map(|item| {
                let (key, value) = item.unwrap();
                (key.to_vec(), value.to_vec())
            })
            .collect()
    }

    fn event(contract: [u8; 32], data: Vec<u8>) -> ksana_execution::Event {
        ksana_execution::Event { contract, data }
    }

    /// Five heights of a token's and a collection's events, keys changed several times within a height and across
    /// heights.
    fn heights() -> Vec<Vec<ksana_execution::Event>> {
        let (token, collection): ([u8; 32], [u8; 32]) = ([0x7a; 32], [0x7b; 32]);
        let (a, b, c): ([u8; 32], [u8; 32], [u8; 32]) = ([0xa1; 32], [0xb2; 32], [0xc3; 32]);
        let zero = [0u8; 32];
        let id = [0x01u8; 32];
        let ks20 = |data: Vec<u8>| event(token, data);
        let ks721 = |data: Vec<u8>| event(collection, data);
        vec![
            vec![
                ks20(borsh::to_vec(&("standards", vec!["KS-20"])).unwrap()),
                ks20(borsh::to_vec(&("metadata", "Token", "TOK", 2u8)).unwrap()),
                ks20(borsh::to_vec(&("transfer", zero, a, 100u128)).unwrap()),
                ks721(borsh::to_vec(&("standards", vec!["KS-721"])).unwrap()),
            ],
            vec![
                ks20(borsh::to_vec(&("transfer", a, b, 40u128)).unwrap()),
                ks20(borsh::to_vec(&("transfer", b, c, 10u128)).unwrap()),
                ks20(borsh::to_vec(&("approve", a, b, 7u128, Some(9u64))).unwrap()),
                ks721(borsh::to_vec(&("transfer", zero, a, id)).unwrap()),
            ],
            vec![
                ks721(borsh::to_vec(&("approve", a, Some(c), id)).unwrap()),
                ks721(borsh::to_vec(&("operator", a, b, true, None::<u64>)).unwrap()),
                ks20(borsh::to_vec(&("transfer", c, zero, 10u128)).unwrap()),
            ],
            vec![
                ks721(borsh::to_vec(&("transfer", a, b, id)).unwrap()),
                ks20(borsh::to_vec(&("approve", a, b, 0u128, None::<u64>)).unwrap()),
                ks20(borsh::to_vec(&("transfer", a, a, 1u128)).unwrap()),
            ],
            vec![
                ks721(borsh::to_vec(&("transfer", b, zero, id)).unwrap()),
                ks20(borsh::to_vec(&("transfer", b, a, 999u128)).unwrap()),
                ks20(borsh::to_vec(&("metadata", "Token two", "TOK", 2u8)).unwrap()),
            ],
        ]
    }

    /// Applies `heights` from height 1 on, `per_batch` heights to a batch (as a rebuild writes them), and returns the
    /// standards index after each height that ends a batch.
    fn apply_heights(
        store: &ChainStore,
        heights: &[Vec<ksana_execution::Event>],
        per_batch: usize,
    ) -> Vec<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut dumps = vec![standards_dump(store)];
        let mut batch = WriteBatch::default();
        let mut overlay = Overlay::new(store);
        for (i, events) in heights.iter().enumerate() {
            let height = u64::try_from(i).unwrap().checked_add(1).unwrap();
            standards::apply(&mut overlay, (height, 0, 0), events).unwrap();
            overlay.end_height(&mut batch, height).unwrap();
            if i.checked_add(1).unwrap().checked_rem(per_batch).unwrap() == 0 {
                overlay.stage(&mut batch).unwrap();
                store.write(std::mem::take(&mut batch)).unwrap();
                dumps.push(standards_dump(store));
            }
        }
        overlay.stage(&mut batch).unwrap();
        store.write(batch).unwrap();
        dumps
    }

    #[test]
    fn the_undo_log_takes_the_standards_index_back_to_any_height() {
        let heights = heights();
        let (_dir, store) = open();
        let dumps = apply_heights(&store, &heights, 1);
        assert_eq!(dumps.len(), 6);
        assert!(dumps[0].is_empty());
        for pair in dumps.windows(2) {
            assert_ne!(pair[0], pair[1], "every height changes the index");
        }
        // Written several heights to a batch, the index and its undo log are the same
        for per_batch in [2, 5] {
            let (_dir, other) = open();
            apply_heights(&other, &heights, per_batch);
            assert_eq!(standards_dump(&other), dumps[5]);
            let undo = |store: &ChainStore| -> Vec<(Vec<u8>, Vec<u8>)> {
                store
                    .db
                    .iterator_cf(store.cf(CF_STANDARDS_UNDO).unwrap(), IteratorMode::Start)
                    .map(|item| {
                        let (k, v) = item.unwrap();
                        (k.to_vec(), v.to_vec())
                    })
                    .collect()
            };
            assert_eq!(undo(&other), undo(&store), "{per_batch} heights a batch");
        }
        // Back one height at a time
        for from in (1..=5u64).rev() {
            let mut batch = WriteBatch::default();
            store.stage_undo_from(&mut batch, from).unwrap();
            store.write(batch).unwrap();
            assert_eq!(
                standards_dump(&store),
                dumps[usize::try_from(from).unwrap().checked_sub(1).unwrap()],
                "below height {from}"
            );
        }
        // Back several heights at once, then forward again from there
        for from in 1..=5u64 {
            let (_dir, store) = open();
            apply_heights(&store, &heights, 1);
            let mut batch = WriteBatch::default();
            store.stage_undo_from(&mut batch, from).unwrap();
            store.write(batch).unwrap();
            let kept = usize::try_from(from).unwrap().checked_sub(1).unwrap();
            assert_eq!(standards_dump(&store), dumps[kept], "below height {from}");
            let mut batch = WriteBatch::default();
            let mut overlay = Overlay::new(&store);
            for (i, events) in heights.iter().enumerate().skip(kept) {
                let height = u64::try_from(i).unwrap().checked_add(1).unwrap();
                standards::apply(&mut overlay, (height, 0, 0), events).unwrap();
                overlay.end_height(&mut batch, height).unwrap();
            }
            overlay.stage(&mut batch).unwrap();
            store.write(batch).unwrap();
            assert_eq!(standards_dump(&store), dumps[5]);
        }
    }

    #[test]
    fn no_listing_while_the_index_describes_another_tip() {
        let (_dir, store) = open();
        put(&store, at(1, 0, 0), &[[0xaa; 32]], true);
        assert!(!store.index_current().unwrap());
        assert_eq!(store.list_ops(None, None, 50).unwrap(), None);
        mark(&store, TIP, None);
        assert!(!store.index_current().unwrap());
        assert_eq!(store.list_ops(None, None, 50).unwrap(), None);
        mark(&store, TIP, Some([0x72; 32]));
        assert!(!store.index_current().unwrap());
        assert_eq!(store.list_ops(Some(&[0xaa; 32]), None, 50).unwrap(), None);
        mark(&store, TIP, Some(TIP));
        assert_eq!(
            store.list_ops(None, None, 50).unwrap().unwrap().ops.len(),
            1
        );
    }

    #[test]
    fn receipts_carry_their_format_and_one_this_version_does_not_know_is_refused() {
        let receipt = Receipt {
            op_id: Some([1; 32]),
            status: OpStatus::Success,
            gas_used: 5,
            output: b"out".to_vec(),
            events: Vec::new(),
            asset_changes: vec![AssetChange {
                events: 0,
                asset: [4; 32],
                kind: ChangeKind::Pause { paused: true },
            }],
        };
        let bytes = encode_receipts(std::slice::from_ref(&receipt)).unwrap();
        assert_eq!(bytes[..5], [0xff, 0xff, 0xff, 0xff, 2]);
        assert_eq!(
            decode_receipts(&bytes).unwrap(),
            StoredReceipts {
                receipts: vec![receipt],
                asset_changes: true
            }
        );
        // the genesis block's receipts as a version before D-122 stored them
        assert_eq!(
            decode_receipts(&[0; 4]).unwrap(),
            StoredReceipts {
                receipts: Vec::new(),
                asset_changes: false
            }
        );
        let mut later = bytes.clone();
        later[4] = 3;
        let cut = bytes[..bytes.len() - 1].to_vec();
        for value in [later, cut, RECEIPTS_MAGIC.to_vec()] {
            assert!(
                matches!(decode_receipts(&value), Err(NodeError::Storage(_))),
                "{value:?}"
            );
        }
    }
}
