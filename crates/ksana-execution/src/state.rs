//! State access of the execution layer: read-only underlying state + this block's write buffer + the current op's
//! write buffer.

use std::collections::BTreeMap;
use std::sync::Arc;

/// Full state key → `Some(value)` (write) or `None` (delete).
pub use ksana_state::WriteSet;

/// The parent state that block execution is based on (read-only). Implementations must be a consistent view of one
/// fixed version.
pub trait StateSource: Send + Sync + 'static {
    /// Reads a full key; `Err` means a storage-layer failure (a fatal error, not a failed op).
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String>;
}

/// In-memory state (used by tests and tools).
#[derive(Clone, Debug, Default)]
pub struct MemState(pub BTreeMap<Vec<u8>, Vec<u8>>);

impl MemState {
    /// Applies a set of writes.
    pub fn apply(&mut self, writes: &WriteSet) {
        for (k, v) in writes {
            match v {
                Some(v) => {
                    self.0.insert(k.clone(), v.clone());
                }
                None => {
                    self.0.remove(k);
                }
            }
        }
    }
}

impl StateSource for MemState {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        Ok(self.0.get(key).cloned())
    }
}

/// A pinned-version view of the persistent state (used by the node and indexer when executing blocks).
impl StateSource for ksana_state::StateView {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        ksana_state::StateRead::get(self, key).map_err(|e| e.to_string())
    }
}

/// Contract event (does not enter the state root).
#[derive(Clone, Debug, PartialEq, Eq, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub struct Event {
    /// The contract that emitted the event.
    pub contract: [u8; 32],
    /// Event data.
    pub data: Vec<u8>,
}

/// A change an op made to a built-in asset (D-122; does not enter the state root): a movement of the asset or one of
/// the management actions of D-121, recorded once it succeeded, in the order the op made it.
#[derive(Clone, Debug, PartialEq, Eq, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub struct AssetChange {
    /// How many of the op's events were recorded before it: the receipt's events and asset changes interleave in this
    /// order.
    pub events: u32,
    /// The asset.
    pub asset: [u8; 32],
    /// What happened to it.
    pub kind: ChangeKind,
}

/// What an [`AssetChange`] did (D-122). The movements are the paths through which D-121's rules move an asset; KSA's
/// fees, unlocks and rewards change balances outside them and are not recorded, nor is a movement of 0, which changes
/// nothing (KS-20 refuses one, D-120).
#[derive(Clone, Debug, PartialEq, Eq, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub enum ChangeKind {
    /// The `AssetTransfer` action: `amount` from its signer `from` to `to`.
    Transfer {
        /// The signer.
        from: [u8; 32],
        /// The receiving account.
        to: [u8; 32],
        /// The amount.
        amount: u128,
    },
    /// Funds attached to a `Call` or a `Deploy` (or to the RPC's `ksana_call`) by its signer, or by a contract to
    /// `call_contract`, `try_call_contract` or `create_contract`: `amount` from `from` to the contract `to`.
    Funds {
        /// The signer, or the calling contract.
        from: [u8; 32],
        /// The contract called or created.
        to: [u8; 32],
        /// The amount.
        amount: u128,
    },
    /// `asset_transfer`: the contract `from` paid `amount` to `to`.
    Payout {
        /// The paying contract.
        from: [u8; 32],
        /// The receiving account.
        to: [u8; 32],
        /// The amount.
        amount: u128,
    },
    /// `asset_issue` or `asset_create`: the issuing contract issued `amount` to `to`.
    Issue {
        /// The receiving account.
        to: [u8; 32],
        /// The amount.
        amount: u128,
    },
    /// `asset_burn`: `amount` burned from `from`.
    Burn {
        /// The account the amount was burned from.
        from: [u8; 32],
        /// The amount.
        amount: u128,
    },
    /// The issuing contract created the asset with `capabilities` (`asset_create`, or the first `asset_issue` with
    /// `mint` alone, D-121); the issuance follows as a [`ChangeKind::Issue`] unless it is of 0.
    Create {
        /// The capabilities declared.
        capabilities: u8,
    },
    /// `asset_freeze`: the issuing contract froze `account`, or unfroze it.
    Freeze {
        /// The account.
        account: [u8; 32],
        /// Whether it is frozen now.
        frozen: bool,
    },
    /// `asset_allow`: the issuing contract allowed `account`, or no longer allows it.
    Allow {
        /// The account.
        account: [u8; 32],
        /// Whether it is allowed now.
        allowed: bool,
    },
    /// `asset_pause`: the issuing contract paused the asset, or resumed it.
    Pause {
        /// Whether it is paused now.
        paused: bool,
    },
    /// `asset_renounce`: the issuing contract gave up `capabilities` for good, those of the call the asset still had;
    /// giving up `pause` resumes the asset.
    Renounce {
        /// The capabilities given up.
        capabilities: u8,
    },
}

impl ChangeKind {
    /// The amount a movement moved; `None` for the management actions.
    fn amount(&self) -> Option<u128> {
        match self {
            ChangeKind::Transfer { amount, .. }
            | ChangeKind::Funds { amount, .. }
            | ChangeKind::Payout { amount, .. }
            | ChangeKind::Issue { amount, .. }
            | ChangeKind::Burn { amount, .. } => Some(*amount),
            ChangeKind::Create { .. }
            | ChangeKind::Freeze { .. }
            | ChangeKind::Allow { .. }
            | ChangeKind::Pause { .. }
            | ChangeKind::Renounce { .. } => None,
        }
    }
}

/// Three-layer state: parent state ← writes committed by this block ← writes of the current op.
///
/// Nested checkpoints can be set within the current op (`try_call_contract`, D-073): reverting a checkpoint restores
/// the keys modified after it and drops the events and asset changes recorded after it.
pub struct Overlay {
    base: Arc<dyn StateSource>,
    block: WriteSet,
    op: WriteSet,
    log: Box<OpLog>,
}

/// One item of the undo log: the key and its state in `op` before it was overwritten (the outer `None` means it was
/// not in `op` before).
type Undo = (Vec<u8>, Option<Option<Vec<u8>>>);

/// Events, asset changes and checkpoints of the current op (D-073, D-122).
#[derive(Default)]
struct OpLog {
    events: Vec<Event>,
    changes: Vec<AssetChange>,
    /// (undo log length, event count, asset change count) at each checkpoint level.
    checkpoints: Vec<(usize, usize, usize)>,
    /// Undo log: recorded only while a checkpoint exists.
    undo: Vec<Undo>,
}

impl Overlay {
    /// An empty buffer with `base` as the parent state.
    pub fn new(base: Arc<dyn StateSource>) -> Self {
        Overlay {
            base,
            block: WriteSet::new(),
            op: WriteSet::new(),
            log: Box::default(),
        }
    }

    /// Read: current op → this block → parent state.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        if let Some(v) = self.op.get(key) {
            return Ok(v.clone());
        }
        if let Some(v) = self.block.get(key) {
            return Ok(v.clone());
        }
        self.base.get(key)
    }

    /// Writes into the current op's buffer.
    pub fn set(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.write(key, Some(value));
    }

    /// Deletes in the current op's buffer.
    pub fn delete(&mut self, key: Vec<u8>) {
        self.write(key, None);
    }

    fn write(&mut self, key: Vec<u8>, value: Option<Vec<u8>>) {
        if !self.log.checkpoints.is_empty() {
            let previous = self.op.get(&key).cloned();
            self.log.undo.push((key.clone(), previous));
        }
        self.op.insert(key, value);
    }

    /// Sets a checkpoint: later writes, events and asset changes can be reverted with [`Overlay::revert_checkpoint`].
    /// Can be nested.
    pub fn checkpoint(&mut self) {
        let mark = (
            self.log.undo.len(),
            self.log.events.len(),
            self.log.changes.len(),
        );
        self.log.checkpoints.push(mark);
    }

    /// Reverts the writes, events and asset changes after the most recent checkpoint and removes that checkpoint.
    /// Returns an error when there is no checkpoint.
    pub fn revert_checkpoint(&mut self) -> Result<(), String> {
        let (undo_len, events_len, changes_len) = self
            .log
            .checkpoints
            .pop()
            .ok_or("no checkpoint to revert")?;
        while self.log.undo.len() > undo_len {
            if let Some((key, previous)) = self.log.undo.pop() {
                match previous {
                    Some(v) => {
                        self.op.insert(key, v);
                    }
                    None => {
                        self.op.remove(&key);
                    }
                }
            }
        }
        self.log.events.truncate(events_len);
        self.log.changes.truncate(changes_len);
        Ok(())
    }

    /// Keeps the writes, events and asset changes after the most recent checkpoint (merging them into the outer level)
    /// and removes that checkpoint. Returns an error when there is no checkpoint.
    pub fn release_checkpoint(&mut self) -> Result<(), String> {
        self.log
            .checkpoints
            .pop()
            .ok_or("no checkpoint to release")?;
        if self.log.checkpoints.is_empty() {
            self.log.undo.clear();
        }
        Ok(())
    }

    /// Records an event (belonging to the current op).
    pub fn emit(&mut self, event: Event) {
        self.log.events.push(event);
    }

    /// Records a change the current op made to `asset` (D-122), after the events recorded so far; a movement of 0 is
    /// left out.
    pub fn record(&mut self, asset: [u8; 32], kind: ChangeKind) {
        if kind.amount() == Some(0) {
            return;
        }
        let events = u32::try_from(self.log.events.len()).unwrap_or(u32::MAX);
        self.log.changes.push(AssetChange {
            events,
            asset,
            kind,
        });
    }

    /// The current op succeeded: its writes are merged into this block; returns this op's events and asset changes.
    pub fn commit_op(&mut self) -> (Vec<Event>, Vec<AssetChange>) {
        let log = std::mem::take(&mut *self.log);
        let op = std::mem::take(&mut self.op);
        self.block.extend(op);
        (log.events, log.changes)
    }

    /// The current op failed: discards all of its writes, events and asset changes.
    pub fn discard_op(&mut self) {
        *self.log = OpLog::default();
        self.op.clear();
    }

    /// All writes of this block (the current op must have been committed or discarded).
    pub fn into_block_writes(self) -> WriteSet {
        self.block
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlay() -> Overlay {
        let mut base = MemState::default();
        base.0.insert(b"base".to_vec(), b"0".to_vec());
        Overlay::new(Arc::new(base))
    }

    fn event(byte: u8) -> Event {
        Event {
            contract: [byte; 32],
            data: vec![byte],
        }
    }

    fn get(ov: &Overlay, key: &[u8]) -> Option<Vec<u8>> {
        ov.get(key).unwrap()
    }

    #[test]
    fn nested_checkpoints_revert_only_their_own_writes() {
        let mut ov = overlay();
        ov.set(b"a".to_vec(), b"1".to_vec());
        ov.emit(event(1));

        ov.checkpoint();
        ov.set(b"a".to_vec(), b"2".to_vec());
        ov.delete(b"base".to_vec());
        ov.set(b"b".to_vec(), b"x".to_vec());
        ov.emit(event(2));

        ov.checkpoint();
        ov.set(b"b".to_vec(), b"y".to_vec());
        ov.set(b"c".to_vec(), b"z".to_vec());
        ov.emit(event(3));
        // inner level fails: only the inner level is reverted
        ov.revert_checkpoint().unwrap();
        assert_eq!(get(&ov, b"b"), Some(b"x".to_vec()));
        assert_eq!(get(&ov, b"c"), None);

        ov.checkpoint();
        ov.set(b"d".to_vec(), b"w".to_vec());
        // inner level succeeds: merged into the outer level, reverted together with it when the outer level is reverted
        ov.release_checkpoint().unwrap();
        assert_eq!(get(&ov, b"d"), Some(b"w".to_vec()));
        ov.revert_checkpoint().unwrap();
        assert_eq!(get(&ov, b"a"), Some(b"1".to_vec()));
        assert_eq!(
            get(&ov, b"base"),
            Some(b"0".to_vec()),
            "the deletion was reverted, back to the parent state's value"
        );
        assert_eq!(get(&ov, b"b"), None);
        assert_eq!(get(&ov, b"d"), None);
        assert!(ov.revert_checkpoint().is_err(), "no checkpoint");
        assert!(ov.release_checkpoint().is_err());

        // writes outside the checkpoints are committed as usual
        assert_eq!(ov.commit_op(), (vec![event(1)], Vec::new()));
        let writes = ov.into_block_writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes.get(b"a".as_slice()), Some(&Some(b"1".to_vec())));
    }

    #[test]
    fn released_checkpoints_keep_writes_and_events() {
        let mut ov = overlay();
        ov.checkpoint();
        ov.set(b"a".to_vec(), b"1".to_vec());
        ov.emit(event(1));
        ov.release_checkpoint().unwrap();
        assert_eq!(ov.commit_op(), (vec![event(1)], Vec::new()));
        assert_eq!(get(&ov, b"a"), Some(b"1".to_vec()));
    }

    fn pause(paused: bool) -> ChangeKind {
        ChangeKind::Pause { paused }
    }

    fn change(events: u32, asset: u8, paused: bool) -> AssetChange {
        AssetChange {
            events,
            asset: [asset; 32],
            kind: pause(paused),
        }
    }

    #[test]
    fn asset_changes_count_the_events_before_them_and_go_with_their_checkpoint() {
        let mut ov = overlay();
        ov.record([1; 32], pause(true));
        ov.emit(event(1));
        ov.record([2; 32], pause(true));
        // a movement of 0 changes nothing
        let nothing = ChangeKind::Burn {
            from: [9; 32],
            amount: 0,
        };
        ov.record([2; 32], nothing);

        ov.checkpoint();
        ov.emit(event(2));
        ov.record([3; 32], pause(true));
        // the inner level fails: its event and asset change go with it
        ov.revert_checkpoint().unwrap();

        ov.checkpoint();
        ov.record([4; 32], pause(false));
        ov.emit(event(3));
        ov.release_checkpoint().unwrap();
        assert_eq!(
            ov.commit_op(),
            (
                vec![event(1), event(3)],
                vec![change(0, 1, true), change(1, 2, true), change(1, 4, false)]
            )
        );

        // a failed op keeps none of them, and the next op starts counting its own events from 0
        ov.emit(event(4));
        ov.record([5; 32], pause(true));
        ov.discard_op();
        ov.emit(event(5));
        ov.record([6; 32], pause(true));
        assert_eq!(ov.commit_op(), (vec![event(5)], vec![change(1, 6, true)]));
    }
}
