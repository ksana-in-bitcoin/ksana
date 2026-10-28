//! Backend implementation of `ksana-rpc` (SPEC §11.2; formats in `docs/RPC.md`).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bitcoin::hashes::Hash;
use jsonrpsee::core::RpcResult;
use jsonrpsee::types::ErrorObjectOwned;
use ksana_btc::tracker::{carrier_of, BtcSnapshot};
use ksana_execution::builtin;
use ksana_execution::keys::{self, AccountState, AssetMeta, ContractMeta, LockedFee, UnlockCursor};
use ksana_execution::{AssetChange, ChangeKind, Event, OpStatus, Receipt};
use ksana_mempool::{fee_rate, op_cost, Mempool};
use ksana_primitives::asset::{capability_names, status_names};
use ksana_primitives::{
    Action, Block, BlockHeader, Entry, FeeMode, Params, Rules, SigScheme, VmOp, KSA_ASSET_ID,
    VMOP_VERSION,
};
use ksana_rpc::{
    codes, error, index_not_built, AccountAssetJson, AccountAssetsJson, AccountJson, ApprovalJson,
    ApprovalsJson, AssetChangeJson, AssetJson, AssetStatusJson, Backend, BalanceJson, BlockId,
    BlockJson, BlockRef, BlockSummary, BtcConfirmationJson, CallRequestJson, ChainInfoJson,
    ContractJson, EstimateFeeJson, EventJson, HolderJson, HoldersJson, LockedFeeJson, NftJson,
    NftsJson, OpListJson, OpPosition, OpRefJson, OpState, OpStatusJson, PathBEstimateJson,
    ReceiptJson, SettlementJson, SettlementState, SubmitResult, TokenJson, UpgradeJson,
};
use tokio::sync::broadcast;

use crate::chain::{Chain, Tip};
use crate::ops::{self, Rejection, Relay};
use crate::producer::{FeePolicy, TemplateSync};
use crate::settlement;
use crate::standards::{Approval, Declared, Standard};
use crate::store::OpOutcome;

/// Maximum time to wait after submitting an op for the block production loop to take it into the template.
const TEMPLATE_WAIT: Duration = Duration::from_secs(5);

/// Maximum number of read-only calls (`ksana_call`) running at the same time on a node (local policy, M26): each may
/// run up to `BLOCK_GAS_LIMIT` gas on its own thread; further calls are refused as busy until one finishes.
const MAX_CONCURRENT_CALLS: usize = 4;

/// One of the [`MAX_CONCURRENT_CALLS`] places of a node, given back when dropped.
struct CallSlot<'a>(&'a AtomicUsize);

impl<'a> CallSlot<'a> {
    /// Takes a place, `running` counting the calls running now; `None` when all are taken.
    fn take(running: &'a AtomicUsize) -> Option<CallSlot<'a>> {
        running
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_CONCURRENT_CALLS).then(|| count.saturating_add(1))
            })
            .ok()
            .map(|_| CallSlot(running))
    }
}

impl Drop for CallSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// RPC backend.
pub struct NodeRpc {
    /// The chain.
    pub chain: Arc<Chain>,
    /// Entry pool.
    pub mempool: Arc<Mutex<Mempool>>,
    /// Template generation announcements while this node produces blocks (`None` when it does not).
    pub sync: Option<Arc<TemplateSync>>,
    /// Whether block production is paused (submissions do not wait for the template while paused).
    pub paused: Arc<AtomicBool>,
    /// New block notifications.
    pub events: broadcast::Sender<BlockSummary>,
    /// Forwards newly accepted ops to the network (`None` when networking is not enabled).
    pub relay: Option<Relay>,
    /// The fee rate policy of this node's settlement transactions (D-102): `ksana_estimateFee` reports the rate it gives
    /// now as a reference for path C and top-up transactions (D-054).
    pub fees: FeePolicy,
    /// The read-only calls running now (at most [`MAX_CONCURRENT_CALLS`]); starts at 0.
    pub calls: AtomicUsize,
}

/// Number of main-chain blocks `ksana_estimateFee` looks back over when collecting path B fee rates (D-054).
const FEE_HISTORY_BLOCKS: u64 = 20;

/// `max_gas` of the ordinary transfer that `ksana_estimateFee` uses to convert the suggested amount (gas of the
/// built-in transfer action, `ksana_vm::gas::ASSET_TRANSFER_ACTION`).
const REFERENCE_TRANSFER_GAS: u64 = 10_000;

/// Cost ([`op_cost`]) of an ordinary transfer (KSA, BIP340 signature) in the packing budget: `ksana_estimateFee` uses
/// it to convert the suggested fee rate into a suggested amount.
fn reference_transfer_cost(params: &Params, rules: &Rules) -> u64 {
    let op = VmOp {
        version: VMOP_VERSION,
        chain_id: params.chain_id,
        account: [0; 32],
        nonce: 0,
        fee: FeeMode::Token { amount: 0 },
        max_gas: REFERENCE_TRANSFER_GAS,
        action: Action::AssetTransfer {
            asset: KSA_ASSET_ID,
            to: [0; 32],
            amount: 0,
        },
        sig: SigScheme::Bip340([0; 64]),
    };
    let len = op.encode().map(|bytes| bytes.len()).unwrap_or(0);
    op_cost(params, rules, len, op.max_gas)
}

fn internal(e: impl std::fmt::Display) -> ErrorObjectOwned {
    error(codes::INTERNAL, e.to_string())
}

fn node_err(e: impl std::fmt::Display) -> ErrorObjectOwned {
    internal(e)
}

/// A receipt in JSON; `asset_changes` tells whether it records its changes to built-in assets (receipts stored before
/// D-122 do not).
fn receipt_json(r: &Receipt, asset_changes: bool) -> ReceiptJson {
    let (success, err) = match &r.status {
        OpStatus::Success => (true, None),
        OpStatus::Failed(reason) => (false, Some(reason.clone())),
        OpStatus::Skipped(reason) => (false, Some(format!("skipped: {reason}"))),
    };
    ReceiptJson {
        op_id: r.op_id,
        success,
        error: err,
        gas_used: r.gas_used,
        output: r.output.clone(),
        events: r.events.iter().map(event_json).collect(),
        asset_changes: asset_changes.then(|| r.asset_changes.iter().map(change_json).collect()),
    }
}

fn change_json(c: &AssetChange) -> AssetChangeJson {
    let (asset, events) = (c.asset, c.events);
    match c.kind {
        ChangeKind::Transfer { from, to, amount } => AssetChangeJson::Transfer {
            asset,
            from,
            to,
            amount,
            events,
        },
        ChangeKind::Funds { from, to, amount } => AssetChangeJson::Funds {
            asset,
            from,
            to,
            amount,
            events,
        },
        ChangeKind::Payout { from, to, amount } => AssetChangeJson::Payout {
            asset,
            from,
            to,
            amount,
            events,
        },
        ChangeKind::Issue { to, amount } => AssetChangeJson::Issue {
            asset,
            to,
            amount,
            events,
        },
        ChangeKind::Burn { from, amount } => AssetChangeJson::Burn {
            asset,
            from,
            amount,
            events,
        },
        ChangeKind::Create { capabilities } => AssetChangeJson::Create {
            asset,
            capabilities: capability_names(capabilities),
            events,
        },
        ChangeKind::Freeze { account, frozen } => AssetChangeJson::Freeze {
            asset,
            account,
            frozen,
            events,
        },
        ChangeKind::Allow { account, allowed } => AssetChangeJson::Allow {
            asset,
            account,
            allowed,
            events,
        },
        ChangeKind::Pause { paused } => AssetChangeJson::Pause {
            asset,
            paused,
            events,
        },
        ChangeKind::Renounce { capabilities } => AssetChangeJson::Renounce {
            asset,
            capabilities: capability_names(capabilities),
            events,
        },
    }
}

fn event_json(e: &Event) -> EventJson {
    EventJson {
        contract: e.contract,
        data: e.data.clone(),
    }
}

fn block_ref(header: &BlockHeader, hash: [u8; 32]) -> BlockRef {
    BlockRef {
        height: header.height,
        hash,
    }
}

/// The block a page of the standards index describes (D-120).
fn tip_ref((hash, height): ([u8; 32], u64)) -> BlockRef {
    BlockRef { height, hash }
}

/// The position the standards index keeps a declaration at.
fn position_of((height, slot, entry): (u64, u32, u32)) -> OpPosition {
    OpPosition {
        height,
        slot,
        entry,
    }
}

/// The record of a contract the standards index follows (D-120): a KS-20 token or a KS-721 collection.
fn followed(declared: Option<Declared>) -> RpcResult<(Declared, Standard)> {
    let declared = declared.ok_or_else(|| {
        error(
            codes::NOT_FOUND,
            "not a token: no contract with this ID declared a standard",
        )
    })?;
    let standard = declared.standard.ok_or_else(|| {
        error(
            codes::NOT_FOUND,
            "not a token: the contract's declaration names neither KS-20 nor KS-721 first",
        )
    })?;
    Ok((declared, standard))
}

fn hex_next(next: Option<Vec<u8>>) -> Option<String> {
    next.map(hex::encode)
}

impl NodeRpc {
    fn pool(&self) -> RpcResult<std::sync::MutexGuard<'_, Mempool>> {
        self.mempool
            .lock()
            .map_err(|_| internal("entry pool lock poisoned"))
    }

    /// Whether the main-chain block at height `height` (and the blocks it merges) has entered the settled closure
    /// (§10.4, D-038, D-063), and whether it is BTC final (on the finalized chain, D-082, decided at a BTC block with
    /// `BTC_FINALITY_CONFS` confirmations, D-114).
    fn finality(&self, height: u64) -> RpcResult<(bool, bool)> {
        let main = self.chain.main_chain().map_err(node_err)?;
        if height < main.base() {
            // Below the main chain's window (D-097): every block there has landed, and lies below the pruning root,
            // which is BTC final
            return Ok((true, true));
        }
        let snapshot = self.chain.btc().snapshot().map_err(node_err)?;
        let Some((btc_tip, _)) = snapshot.tip() else {
            return Ok((false, false));
        };
        let settled =
            crate::btc::prefix_height(&snapshot, &main, height).is_some_and(|p| p <= btc_tip);
        let btc_final = match main.get(height) {
            Some(entry) => self
                .chain
                .is_btc_final(&entry.hash, height)
                .map_err(node_err)?,
            None => false,
        };
        Ok((settled, btc_final))
    }

    /// Whether the main-chain block at height `height` is BTC final: the second half of [`NodeRpc::finality`] alone,
    /// in constant time, without the walk over the main chain's window that the settled closure takes. An op's status
    /// needs only this, and an operation listing asks it for up to `MAX_OPS_LIMIT` heights a page (D-111).
    fn btc_final(&self, height: u64) -> RpcResult<bool> {
        let main = self.chain.main_chain().map_err(node_err)?;
        if height < main.base() {
            return Ok(true);
        }
        let snapshot = self.chain.btc().snapshot().map_err(node_err)?;
        if snapshot.tip().is_none() {
            return Ok(false);
        }
        match main.get(height) {
            Some(entry) => self
                .chain
                .is_btc_final(&entry.hash, height)
                .map_err(node_err),
            None => Ok(false),
        }
    }

    /// A page of the operation index (D-111): the ops listed under `account` (every op with `None`), with their status
    /// as `ksana_getOpStatus` gives it on the main chain.
    fn op_list(
        &self,
        account: Option<&[u8; 32]>,
        before: Option<OpPosition>,
        limit: usize,
    ) -> RpcResult<OpListJson> {
        let page = self
            .chain
            .store()
            .list_ops(account, before, limit)
            .map_err(node_err)?
            .ok_or_else(|| {
                // Not an internal error: until a later start builds the index this node has no listings, as one from
                // before D-111 has none, and a client falls back the same way on the same code
                error(
                    codes::METHOD_NOT_FOUND,
                    "the operation index is not built: the node builds it at startup, and failed to (see its log, D-111)",
                )
            })?;
        let mut btc_final = std::collections::BTreeMap::new();
        let mut ops = Vec::with_capacity(page.ops.len());
        for listed in page.ops {
            let height = listed.position.height;
            let status = match listed.op.outcome {
                OpOutcome::Failed => OpState::Failed,
                OpOutcome::Skipped => OpState::Skipped,
                OpOutcome::Success => {
                    let decided = match btc_final.get(&height) {
                        Some(decided) => *decided,
                        None => {
                            let decided = self.btc_final(height)?;
                            btc_final.insert(height, decided);
                            decided
                        }
                    };
                    if decided {
                        OpState::BtcFinal
                    } else {
                        OpState::FastConfirmed
                    }
                }
            };
            ops.push(OpRefJson {
                op_id: listed.op.op_id,
                status,
                block: BlockRef {
                    height,
                    hash: listed.op.block,
                },
                index: listed.position.entry,
                position: listed.position,
            });
        }
        let (hash, height) = page.tip;
        Ok(OpListJson {
            ops,
            next: page.next,
            at: BlockRef { height, hash },
        })
    }

    fn block_json(&self, hash: [u8; 32], block: Block) -> RpcResult<BlockJson> {
        let stored = self.chain.store().receipts(&hash).map_err(node_err)?;
        let changes = stored.as_ref().is_some_and(|s| s.asset_changes);
        let receipts = stored.map(|s| s.receipts).unwrap_or_default();
        let accepted_height = self.chain.accepted_height(&hash).map_err(node_err)?;
        let merged = match self.chain.ghostdag(&hash).map_err(node_err)? {
            Some(data) => data.mergeset_ordered,
            // Pruned from memory (D-097): a main-chain block's merge set comes from its stored summary
            None => match accepted_height {
                Some(height) => self
                    .chain
                    .main_entry_at(height)
                    .map_err(node_err)?
                    .filter(|entry| entry.hash == hash)
                    .map(|entry| entry.merged.iter().map(|m| m.hash).collect())
                    .unwrap_or_default(),
                None => Vec::new(),
            },
        };
        Ok(BlockJson {
            hash,
            receipts: receipts.iter().map(|r| receipt_json(r, changes)).collect(),
            header: block.header,
            entries: block.entries,
            settlement: block.settlement,
            accepted_height,
            merged,
        })
    }
}

impl Backend for NodeRpc {
    fn submit_op(&self, op: Vec<u8>) -> RpcResult<SubmitResult> {
        let op = VmOp::decode(&op, self.chain.params())
            .map_err(|e| error(codes::INVALID_PARAMS, format!("VmOp decode failed: {e}")))?;
        let (op_id, generation) =
            ops::admit(&self.chain, &self.mempool, op.clone()).map_err(|e| match e {
                Rejection::Invalid(m) | Rejection::Stale(m) => error(codes::OP_INVALID, m),
                Rejection::Policy(m) => error(codes::OP_REJECTED, m),
                Rejection::Local(m) => internal(m),
            })?;
        if let Some(relay) = &self.relay {
            relay(&op);
        }
        // Wait for the block production loop to take the new op into the template: the next block produced after
        // that has certainly considered it (not awaited while block production is paused)
        if let Some(sync) = &self.sync {
            if !self.paused.load(Ordering::Relaxed) {
                sync.wait_for(generation, TEMPLATE_WAIT);
            }
        }
        Ok(SubmitResult { op_id })
    }

    fn op_status(&self, op_id: [u8; 32]) -> RpcResult<OpStatusJson> {
        if self.pool()?.contains(&op_id) {
            return Ok(OpStatusJson {
                status: OpState::Pending,
                block: None,
                index: None,
                receipt: None,
            });
        }
        let store = self.chain.store();
        let Some(location) = store.op_location(&op_id).map_err(node_err)? else {
            return Err(error(codes::NOT_FOUND, "unknown op"));
        };
        let stored = store
            .receipts(&location.block)
            .map_err(node_err)?
            .ok_or_else(|| internal("block receipts missing"))?;
        let receipt = usize::try_from(location.index)
            .ok()
            .and_then(|i| stored.receipts.get(i))
            .ok_or_else(|| internal("receipt index out of range"))?;
        let status = match receipt.status {
            OpStatus::Failed(_) => OpState::Failed,
            OpStatus::Skipped(_) => OpState::Skipped,
            OpStatus::Success => {
                if self.btc_final(location.height)? {
                    OpState::BtcFinal
                } else {
                    OpState::FastConfirmed
                }
            }
        };
        Ok(OpStatusJson {
            status,
            block: Some(BlockRef {
                height: location.height,
                hash: location.block,
            }),
            index: Some(location.index),
            receipt: Some(receipt_json(receipt, stored.asset_changes)),
        })
    }

    fn block(&self, id: BlockId) -> RpcResult<BlockJson> {
        let store = self.chain.store();
        let hash = match id {
            BlockId::Latest => self.chain.tip().map_err(node_err)?.hash,
            BlockId::Hash(hash) => hash,
            BlockId::Height(height) => store
                .hash_at(height)
                .map_err(node_err)?
                .ok_or_else(|| error(codes::NOT_FOUND, "no block at that height"))?,
        };
        let block = store
            .block(&hash)
            .map_err(node_err)?
            .ok_or_else(|| error(codes::NOT_FOUND, "unknown block"))?;
        self.block_json(hash, block)
    }

    fn account(&self, account: [u8; 32]) -> RpcResult<AccountJson> {
        self.chain
            .with_state(|state, tip| {
                // Under the read lock the main chain and the tip are consistent (both are updated under the same
                // write lock when a new block is committed)
                let main = self.chain.main_chain().map_err(node_err)?;
                let snapshot = self.chain.btc().snapshot().map_err(node_err)?;
                account_json(state, tip, &main, &snapshot, self.chain.params(), account)
            })
            .map_err(node_err)?
    }

    fn asset(&self, asset: [u8; 32]) -> RpcResult<AssetJson> {
        self.chain
            .with_state(|state, tip| {
                let meta: AssetMeta = match state.get(&keys::asset(&asset)).map_err(internal)? {
                    Some(bytes) => borsh::from_slice(&bytes)
                        .map_err(|e| internal(format!("state value corrupted: {e}")))?,
                    None => return Err(error(codes::NOT_FOUND, "asset does not exist")),
                };
                Ok(AssetJson {
                    asset,
                    name: meta.name,
                    symbol: meta.symbol,
                    decimals: meta.decimals,
                    total_supply: meta.total_supply,
                    issuer: meta.issuer,
                    capabilities: capability_names(meta.capabilities),
                    paused: meta.paused,
                    at: block_ref(&tip.header, tip.hash),
                })
            })
            .map_err(node_err)?
    }

    fn asset_status(&self, asset: [u8; 32], account: [u8; 32]) -> RpcResult<AssetStatusJson> {
        self.chain
            .with_state(|state, tip| {
                let read = |key: Vec<u8>| -> RpcResult<Option<Vec<u8>>> {
                    state.get(&key).map_err(internal)
                };
                let corrupted = |e: std::io::Error| internal(format!("state value corrupted: {e}"));
                let meta: AssetMeta = match read(keys::asset(&asset))? {
                    Some(bytes) => borsh::from_slice(&bytes).map_err(corrupted)?,
                    None => return Err(error(codes::NOT_FOUND, "asset does not exist")),
                };
                let status: u8 = match read(keys::asset_status(&asset, &account))? {
                    Some(bytes) => borsh::from_slice(&bytes).map_err(corrupted)?,
                    None => 0,
                };
                Ok(AssetStatusJson {
                    asset,
                    account,
                    status: status_names(status),
                    can_send: builtin::can_send(&meta, &account, status),
                    can_receive: builtin::can_receive(&meta, &account, status),
                    at: block_ref(&tip.header, tip.hash),
                })
            })
            .map_err(node_err)?
    }

    fn call(&self, req: CallRequestJson) -> RpcResult<ReceiptJson> {
        let Some(_slot) = CallSlot::take(&self.calls) else {
            return Err(error(
                jsonrpsee::types::error::SERVER_IS_BUSY_CODE,
                format!(
                    "{MAX_CONCURRENT_CALLS} read-only calls are already running; try again later"
                ),
            ));
        };
        let (tip, view) = self.chain.tip_and_view().map_err(node_err)?;
        let params = self.chain.params();
        let snapshot = self.chain.btc().snapshot().map_err(node_err)?;
        let btc = self
            .chain
            .btc_view(tip.header.btc_anchor.clone(), snapshot, false)
            .map_err(node_err)?;
        // Rules follow the lower bound of the next block's DAA score; a read-only call executes no unlock entry, so
        // no unlockable segment is needed
        let daa_score = self
            .chain
            .ghostdag(&tip.hash)
            .map_err(node_err)?
            .map_or(0, |d| d.daa_score.saturating_add(1));
        let env = ksana_execution::BlockEnv {
            height: tip.header.height.saturating_add(1),
            btc_anchor: tip.header.btc_anchor.clone(),
            producer: [0; 32],
            timestamp: tip.header.timestamp,
            rules: params.rules(daa_score),
            unlockable: Vec::new(),
        };
        // A call can use no more gas than a block (M26): a call that never returns would otherwise run as long as the
        // caller asks
        let limit = env.rules.block_gas_limit;
        let max_gas = match req.max_gas {
            None => limit,
            Some(max_gas) if max_gas <= limit => max_gas,
            Some(max_gas) => {
                return Err(error(
                    codes::INVALID_PARAMS,
                    format!("max_gas {max_gas} exceeds BLOCK_GAS_LIMIT ({limit})"),
                ))
            }
        };
        let receipt = self
            .chain
            .executor()
            .simulate_call(
                Arc::new(view),
                Arc::new(btc),
                &env,
                req.caller.unwrap_or([0; 32]),
                req.contract,
                req.method,
                req.args,
                req.funds,
                max_gas,
            )
            .map_err(internal)?;
        Ok(receipt_json(&receipt, true))
    }

    fn estimate_fee(&self) -> RpcResult<EstimateFeeJson> {
        let params = self.chain.params();
        let (min_token_fee, min_fee_per_kib, rules) = {
            let pool = self.pool()?;
            let policy = pool.policy();
            (policy.min_token_fee, policy.min_fee_per_kib, *pool.rules())
        };
        let feerate = self
            .fees
            .rate(params, self.chain.btc().client().as_ref(), false);

        // Path B: the median (the larger middle value for an even count) over the last FEE_HISTORY_BLOCKS main-chain
        // blocks of each block's lowest path B entry fee rate (sponsored fee per KiB of cost, the same ordering as
        // packing), not below this node's minimum packing fee rate; the suggested amount is that of an ordinary
        // transfer converted at this rate, not below the minimum sponsored fee for packing
        let tip = self.chain.tip().map_err(node_err)?;
        let store = self.chain.store();
        let mut rates = Vec::new();
        let from = tip
            .header
            .height
            .saturating_sub(FEE_HISTORY_BLOCKS.saturating_sub(1));
        for height in from..=tip.header.height {
            let Some(hash) = store.hash_at(height).map_err(node_err)? else {
                continue;
            };
            let Some(block) = store.block(&hash).map_err(node_err)? else {
                continue;
            };
            let lowest = block
                .entries
                .iter()
                .filter_map(|e| match e {
                    Entry::Sponsored { op } => match op.fee {
                        FeeMode::Token { amount } => {
                            let len = op.encode().ok()?.len();
                            Some(fee_rate(amount, op_cost(params, &rules, len, op.max_gas)))
                        }
                        _ => None,
                    },
                    _ => None,
                })
                .min();
            rates.extend(lowest);
        }
        rates.sort_unstable();
        let per_kib = rates
            .get(rates.len() / 2)
            .copied()
            .unwrap_or(0)
            .max(min_fee_per_kib);
        let reference = u128::from(reference_transfer_cost(params, &rules));
        let amount = per_kib
            .saturating_mul(reference)
            .div_ceil(1024)
            .max(min_token_fee);
        Ok(EstimateFeeJson {
            feerate_sat_vb: feerate,
            path_b: PathBEstimateJson { amount, per_kib },
            at: block_ref(&tip.header, tip.hash),
        })
    }

    fn settlement(&self, id: BlockId) -> RpcResult<SettlementJson> {
        let store = self.chain.store();
        let hash = match id {
            BlockId::Latest => self.chain.tip().map_err(node_err)?.hash,
            BlockId::Hash(hash) => hash,
            BlockId::Height(height) => store
                .hash_at(height)
                .map_err(node_err)?
                .ok_or_else(|| error(codes::NOT_FOUND, "no block at that height"))?,
        };
        let block = store
            .block(&hash)
            .map_err(node_err)?
            .ok_or_else(|| error(codes::NOT_FOUND, "unknown block"))?;
        let needs_body = settlement::needs_body(&block.entries);
        // Settlement transactions: the ones the block carries; for a block that carries none but needs a body, the
        // ones carried by the block that carries its body (D-077)
        let (txs, carrier) = match &block.settlement {
            Some(txs) => (Some(txs.clone()), None),
            None if needs_body => match self.chain.body_carrier(&hash).map_err(node_err)? {
                Some(carrier) => (carrier.settlement.clone(), Some(carrier)),
                None => (None, None),
            },
            None => {
                return Err(error(
                    codes::NOT_FOUND,
                    "that block has no settlement transactions",
                ))
            }
        };
        let pair = txs
            .as_ref()
            .map(settlement::decode)
            .transpose()
            .map_err(node_err)?;
        // The reveal transaction is the one that carries the payload (D-081)
        let txid = pair.as_ref().map(|p| p.reveal.compute_txid());
        let commit_txid = pair.as_ref().map(|p| p.commit.compute_txid());
        let snapshot = self.chain.btc().snapshot().map_err(node_err)?;
        let btc_tip = snapshot.tip().map(|(h, _)| h).unwrap_or(0);
        // Carrier (D-095): answered by the current BTC best chain and the current main chain
        let main = self.chain.main_chain().map_err(node_err)?;
        let carrier_account =
            carrier_of(self.chain.params(), &snapshot, &main, None, &hash).map_err(internal)?;
        // The transaction that lands this block's data on BTC; for a block needing no body (an empty block carrying
        // the batch's settlement transactions) it is the settlement transaction the block carries
        let landed = if needs_body {
            snapshot.settlement_of(&hash)
        } else {
            txid.map(|t| t.to_byte_array())
                .and_then(|t| snapshot.ksn1_height(&t).map(|h| (t, h)))
        };
        let confirmed = landed.map(|(txid, height)| BtcConfirmationJson {
            txid,
            height,
            block: snapshot.hash_at(height).unwrap_or([0; 32]),
            confirmations: btc_tip.saturating_sub(height).saturating_add(1),
        });
        let record_key = carrier.as_ref().map_or(hash, |c| c.header.hash());
        let record = store.settlement(&record_key).map_err(node_err)?;
        let status = if confirmed.is_some() {
            SettlementState::Confirmed
        } else if record.is_some_and(|r| r.broadcast) {
            SettlementState::Broadcast
        } else {
            SettlementState::Pending
        };
        // Judged by the main-chain block that accepted it (a merged block enters the settled closure at the height
        // of the main-chain block that merges it, D-063)
        let (settled, btc_final) = match self.chain.accepted_height(&hash).map_err(node_err)? {
            Some(height) => self.finality(height)?,
            None => (false, false),
        };
        Ok(SettlementJson {
            block: block_ref(&block.header, hash),
            txid: txid.map(|t| t.to_byte_array()),
            txid_rpc: txid.map(|t| t.to_string()),
            commit_txid: commit_txid.map(|t| t.to_byte_array()),
            commit_txid_rpc: commit_txid.map(|t| t.to_string()),
            txs,
            carrier_account,
            carrier: carrier.map(|c| block_ref(&c.header, c.header.hash())),
            status,
            confirmed,
            settled,
            btc_final,
        })
    }

    fn account_ops(
        &self,
        account: [u8; 32],
        before: Option<OpPosition>,
        limit: usize,
    ) -> RpcResult<OpListJson> {
        self.op_list(Some(&account), before, limit)
    }

    fn latest_ops(&self, before: Option<OpPosition>, limit: usize) -> RpcResult<OpListJson> {
        self.op_list(None, before, limit)
    }

    fn chain_info(&self) -> RpcResult<ChainInfoJson> {
        let tip = self.chain.tip().map_err(node_err)?;
        let params = self.chain.params();
        let daa_score = self
            .chain
            .ghostdag(&tip.hash)
            .map_err(node_err)?
            .map_or(0, |d| d.daa_score);
        let (final_hash, final_height) = self.chain.btc_final().map_err(node_err)?;
        Ok(ChainInfoJson {
            chain_id: params.chain_id,
            tip: block_ref(&tip.header, tip.hash),
            daa_score,
            protocol_version: tip.header.version,
            next_upgrade: params.next_upgrade(daa_score).map(|u| UpgradeJson {
                version: u.version,
                daa_score: u.daa_score,
            }),
            max_known_version: params.max_known_version(),
            finalized: BlockRef {
                height: final_height,
                hash: final_hash,
            },
        })
    }

    fn subscribe_blocks(&self) -> broadcast::Receiver<BlockSummary> {
        self.events.subscribe()
    }

    fn contract(&self, contract: [u8; 32]) -> RpcResult<ContractJson> {
        self.chain
            .with_state(|state, tip| {
                let meta: ContractMeta =
                    match state.get(&keys::contract(&contract)).map_err(internal)? {
                        Some(bytes) => borsh::from_slice(&bytes)
                            .map_err(|e| internal(format!("state value corrupted: {e}")))?,
                        None => return Err(error(codes::NOT_FOUND, "contract does not exist")),
                    };
                // Under the read lock the index describes the state's tip: both change under the same write lock
                let (declared, _) = self
                    .chain
                    .store()
                    .declared(&contract)
                    .map_err(node_err)?
                    .ok_or_else(index_not_built)?;
                let (standards, declared) = match declared {
                    Some(declared) => (declared.ids, Some(position_of(declared.at))),
                    None => (Vec::new(), None),
                };
                Ok(ContractJson {
                    contract,
                    code_hash: meta.code_hash,
                    deployer: meta.deployer,
                    standards,
                    declared,
                    at: block_ref(&tip.header, tip.hash),
                })
            })
            .map_err(node_err)?
    }

    fn account_assets(
        &self,
        account: [u8; 32],
        after: Option<Vec<u8>>,
        limit: usize,
    ) -> RpcResult<AccountAssetsJson> {
        // The cursor: 0 and an asset ID, or 1 and a contract ID (the built-in assets list first)
        let after = match after.as_deref().map(<[u8]>::split_first) {
            None => None,
            Some(Some((kind @ (0 | 1), id))) => match <[u8; 32]>::try_from(id) {
                Ok(id) => Some((*kind, id)),
                Err(_) => return Err(error(codes::INVALID_PARAMS, "after must be a page's next")),
            },
            Some(_) => return Err(error(codes::INVALID_PARAMS, "after must be a page's next")),
        };
        self.chain
            .with_state(|state, tip| {
                let mut assets = Vec::new();
                let mut more = false;
                if !matches!(after, Some((1, _))) {
                    for (asset, balance) in builtin_balances(state, &account)? {
                        if matches!(after, Some((0, after)) if asset <= after) {
                            continue;
                        }
                        if assets.len() >= limit {
                            more = true;
                            break;
                        }
                        assets.push(AccountAssetJson::Asset { asset, balance });
                    }
                }
                let after_contract = match after {
                    Some((1, contract)) => Some(contract),
                    _ => None,
                };
                // Asked even when the built-in assets fill the page: the method answers alike whether or not the
                // index is built, and the page tells whether a contract follows them
                let room = limit.saturating_sub(assets.len());
                let page = self
                    .chain
                    .store()
                    .account_amounts(&account, after_contract.as_ref(), room.max(1))
                    .map_err(node_err)?
                    .ok_or_else(index_not_built)?;
                if !more {
                    if room == 0 {
                        more = !page.items.is_empty();
                    } else {
                        more = page.next.is_some();
                        for (contract, standard, amount) in page.items {
                            assets.push(match standard {
                                Standard::Ks20 => AccountAssetJson::Token {
                                    contract,
                                    balance: amount,
                                },
                                Standard::Ks721 => AccountAssetJson::Nft {
                                    contract,
                                    count: amount,
                                },
                            });
                        }
                    }
                }
                let next = if more {
                    assets.last().map(|asset| {
                        let (kind, id) = match asset {
                            AccountAssetJson::Asset { asset, .. } => (0u8, asset),
                            AccountAssetJson::Token { contract, .. }
                            | AccountAssetJson::Nft { contract, .. } => (1u8, contract),
                        };
                        hex::encode([[kind].as_slice(), id.as_slice()].concat())
                    })
                } else {
                    None
                };
                Ok(AccountAssetsJson {
                    assets,
                    next,
                    at: block_ref(&tip.header, tip.hash),
                })
            })
            .map_err(node_err)?
    }

    fn token(&self, contract: [u8; 32]) -> RpcResult<TokenJson> {
        let (declared, tip) = self
            .chain
            .store()
            .declared(&contract)
            .map_err(node_err)?
            .ok_or_else(index_not_built)?;
        let (declared, standard) = followed(declared)?;
        let (name, symbol, decimals) = match declared.metadata {
            Some(metadata) => (
                Some(metadata.name),
                Some(metadata.symbol),
                metadata.decimals,
            ),
            None => (None, None, None),
        };
        Ok(TokenJson {
            contract,
            standard: standard.id().to_owned(),
            extensions: declared.ids.into_iter().skip(1).collect(),
            name,
            symbol,
            decimals,
            supply: declared.supply,
            holders: declared.holders,
            declared: position_of(declared.at),
            irregular: declared.irregular,
            at: tip_ref(tip),
        })
    }

    fn token_holders(
        &self,
        contract: [u8; 32],
        after: Option<Vec<u8>>,
        limit: usize,
    ) -> RpcResult<HoldersJson> {
        // Under the read lock both reads describe the same tip
        self.chain
            .with_state(|_, _| {
                let store = self.chain.store();
                let (declared, _) = store
                    .declared(&contract)
                    .map_err(node_err)?
                    .ok_or_else(index_not_built)?;
                followed(declared)?;
                let page = store
                    .holders(&contract, after.as_deref(), limit)
                    .map_err(node_err)?
                    .ok_or_else(index_not_built)?;
                Ok(HoldersJson {
                    holders: page
                        .items
                        .into_iter()
                        .map(|(balance, account)| HolderJson { account, balance })
                        .collect(),
                    next: hex_next(page.next),
                    at: tip_ref(page.tip),
                })
            })
            .map_err(node_err)?
    }

    fn nfts(
        &self,
        account: [u8; 32],
        contract: Option<[u8; 32]>,
        after: Option<Vec<u8>>,
        limit: usize,
    ) -> RpcResult<NftsJson> {
        let page = self
            .chain
            .store()
            .nfts(&account, contract.as_ref(), after.as_deref(), limit)
            .map_err(node_err)?
            .ok_or_else(index_not_built)?;
        Ok(NftsJson {
            nfts: page
                .items
                .into_iter()
                .map(|(contract, token_id)| NftJson { contract, token_id })
                .collect(),
            next: hex_next(page.next),
            at: tip_ref(page.tip),
        })
    }

    fn approvals(
        &self,
        account: [u8; 32],
        after: Option<Vec<u8>>,
        limit: usize,
    ) -> RpcResult<ApprovalsJson> {
        let page = self
            .chain
            .store()
            .approvals(&account, after.as_deref(), limit)
            .map_err(node_err)?
            .ok_or_else(index_not_built)?;
        Ok(ApprovalsJson {
            approvals: page
                .items
                .into_iter()
                .map(|approval| match approval {
                    Approval::Allowance {
                        contract,
                        spender,
                        amount,
                        expires_at,
                    } => ApprovalJson::Allowance {
                        contract,
                        spender,
                        amount,
                        expires_at,
                    },
                    Approval::Operator {
                        contract,
                        operator,
                        expires_at,
                    } => ApprovalJson::Operator {
                        contract,
                        operator,
                        expires_at,
                    },
                    Approval::Token {
                        contract,
                        token_id,
                        spender,
                    } => ApprovalJson::Token {
                        contract,
                        token_id,
                        spender,
                    },
                })
                .collect(),
            next: hex_next(page.next),
            at: tip_ref(page.tip),
        })
    }
}

/// The built-in assets `account` holds, by asset ID. Balance keys are 0x02 ‖ asset_id ‖ account (§10.1): this reads the
/// account's balance in each asset (0x03 ‖ asset_id), one read per asset rather than a scan of every account's
/// balances (F-29, D-132).
fn builtin_balances(
    state: &ksana_state::StateDb,
    account: &[u8; 32],
) -> RpcResult<Vec<([u8; 32], u128)>> {
    let mut balances = Vec::new();
    for (key, _) in state.scan_prefix(&[keys::PREFIX_ASSET]).map_err(internal)? {
        let Some(asset) = key.get(1..).and_then(|a| <[u8; 32]>::try_from(a).ok()) else {
            continue;
        };
        let Some(value) = state
            .get(&keys::balance(&asset, account))
            .map_err(internal)?
        else {
            continue;
        };
        let amount: u128 = borsh::from_slice(&value)
            .map_err(|e| internal(format!("state value corrupted: {e}")))?;
        balances.push((asset, amount));
    }
    Ok(balances)
}

fn account_json(
    state: &ksana_state::StateDb,
    tip: &Tip,
    main: &crate::btc::MainChain,
    snapshot: &BtcSnapshot,
    params: &Params,
    account: [u8; 32],
) -> RpcResult<AccountJson> {
    let decode_err = |e: std::io::Error| internal(format!("state value corrupted: {e}"));
    let account_state: AccountState = match state.get(&keys::account(&account)).map_err(internal)? {
        Some(bytes) => borsh::from_slice(&bytes).map_err(decode_err)?,
        None => AccountState::default(),
    };

    let balances = builtin_balances(state, &account)?
        .into_iter()
        .map(|(asset, amount)| BalanceJson { asset, amount })
        .collect();

    // Sponsored fees and block rewards not yet unlocked (§8.5, D-074, D-079): the sponsored fee records of the blocks
    // this account produced after the unlock cursor, plus the block rewards to be paid out at unlock (simulating the
    // KSA_MAX_SUPPLY cap in acceptance order); the parts this account will receive as carrier or block share account
    // are counted too (D-089, D-095)
    let cursor = match state.get(&keys::unlock_cursor()).map_err(internal)? {
        Some(bytes) => {
            let c: UnlockCursor = borsh::from_slice(&bytes).map_err(decode_err)?;
            Some((c.height, c.index))
        }
        None => None,
    };
    let supply = match state.get(&keys::asset(&KSA_ASSET_ID)).map_err(internal)? {
        Some(bytes) => {
            borsh::from_slice::<AssetMeta>(&bytes)
                .map_err(decode_err)?
                .total_supply
        }
        None => 0,
    };
    // Block share account (D-089): without one configured the whole reward goes to the producer
    let share_account = match state.get(&keys::block_share_account()).map_err(internal)? {
        Some(bytes) => Some(borsh::from_slice::<[u8; 32]>(&bytes).map_err(decode_err)?),
        None => None,
    };
    let mut issuable = params.ksa_max_supply.saturating_sub(supply);
    let mut locked_fees = Vec::new();
    // The blocks after the cursor and their carriers are looked up in an index from the cursor's height on (D-132)
    let from = cursor.map_or(1, |(height, _)| height.max(1));
    let mut carriers = crate::btc::Carriers::new(params, snapshot, main, from);
    for item in crate::btc::unlock_segment(main, cursor, u64::MAX).map_err(internal)? {
        // Carrier (D-095, by the current BTC best chain and the current main chain; when it is the producer itself
        // the part goes to the producer)
        let carrier = carriers
            .carrier(&item.hash)
            .map_err(internal)?
            .filter(|c| *c != item.producer);
        // A block of weight N gets N reward slots (D-084), split per slot into three parts for the producer, the block
        // share account and the carrier (D-089, D-095); at the cap the share's part is guaranteed first, then the
        // carrier's
        let (reward, carrier_reward, share) = if item.rewarded {
            let split = params.block_rewards_split_at(item.daa_score, item.weight);
            let share_part = if share_account.is_some() {
                split.share
            } else {
                0
            };
            let carrier_part = if carrier.is_some() { split.carrier } else { 0 };
            let issued = split.total().min(issuable);
            let share = share_part.min(issued);
            let carrier_reward = carrier_part.min(issued.saturating_sub(share));
            (
                issued.saturating_sub(share).saturating_sub(carrier_reward),
                carrier_reward,
                share,
            )
        } else {
            (0, 0, 0)
        };
        issuable = issuable
            .saturating_sub(reward)
            .saturating_sub(carrier_reward)
            .saturating_sub(share);
        let mut amount = if share_account == Some(account) {
            share
        } else {
            0
        };
        if item.producer == account || carrier == Some(account) {
            let fee = match state
                .get(&keys::locked_fee(item.height, item.index))
                .map_err(internal)?
            {
                Some(bytes) => {
                    borsh::from_slice::<LockedFee>(&bytes)
                        .map_err(decode_err)?
                        .amount
                }
                None => 0,
            };
            // The sponsored fee is split between carrier and producer by the unlocked block's own rules (D-095)
            let carrier_fee = if carrier.is_some() {
                params.carrier_fee_at(item.daa_score, fee)
            } else {
                0
            };
            if item.producer == account {
                amount = amount
                    .saturating_add(reward)
                    .saturating_add(fee.saturating_sub(carrier_fee));
            }
            if carrier == Some(account) {
                amount = amount
                    .saturating_add(carrier_reward)
                    .saturating_add(carrier_fee);
            }
        }
        if amount > 0 {
            locked_fees.push(LockedFeeJson {
                block_hash: item.hash,
                amount,
            });
        }
    }

    Ok(AccountJson {
        account,
        nonce: account_state.nonce,
        balances,
        locked_fees,
        at: block_ref(&tip.header, tip.hash),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ksana_primitives::asset::{CAP_FREEZE, CAP_MINT, CAP_PAUSE};
    use serde_json::json;

    #[test]
    fn every_kind_of_change_has_its_json() {
        let (asset, a, b) = ([7u8; 32], [1u8; 32], [2u8; 32]);
        let (asset_hex, a_hex, b_hex) = (hex::encode(asset), hex::encode(a), hex::encode(b));
        let json_of = |kind| {
            serde_json::to_value(change_json(&AssetChange {
                events: 3,
                asset,
                kind,
            }))
            .unwrap()
        };
        let kinds = [
            (
                ChangeKind::Transfer {
                    from: a,
                    to: b,
                    amount: 30,
                },
                json!({ "kind": "transfer", "asset": asset_hex, "from": a_hex, "to": b_hex, "amount": "30", "events": 3 }),
            ),
            (
                ChangeKind::Funds {
                    from: a,
                    to: b,
                    amount: u128::MAX,
                },
                json!({
                    "kind": "funds", "asset": asset_hex, "from": a_hex, "to": b_hex,
                    "amount": u128::MAX.to_string(), "events": 3,
                }),
            ),
            (
                ChangeKind::Payout {
                    from: b,
                    to: a,
                    amount: 7,
                },
                json!({ "kind": "payout", "asset": asset_hex, "from": b_hex, "to": a_hex, "amount": "7", "events": 3 }),
            ),
            (
                ChangeKind::Issue { to: b, amount: 100 },
                json!({ "kind": "issue", "asset": asset_hex, "to": b_hex, "amount": "100", "events": 3 }),
            ),
            (
                ChangeKind::Burn { from: b, amount: 2 },
                json!({ "kind": "burn", "asset": asset_hex, "from": b_hex, "amount": "2", "events": 3 }),
            ),
            (
                ChangeKind::Create {
                    capabilities: CAP_MINT | CAP_PAUSE,
                },
                json!({ "kind": "create", "asset": asset_hex, "capabilities": ["mint", "pause"], "events": 3 }),
            ),
            (
                ChangeKind::Freeze {
                    account: b,
                    frozen: false,
                },
                json!({ "kind": "freeze", "asset": asset_hex, "account": b_hex, "frozen": false, "events": 3 }),
            ),
            (
                ChangeKind::Allow {
                    account: b,
                    allowed: true,
                },
                json!({ "kind": "allow", "asset": asset_hex, "account": b_hex, "allowed": true, "events": 3 }),
            ),
            (
                ChangeKind::Pause { paused: true },
                json!({ "kind": "pause", "asset": asset_hex, "paused": true, "events": 3 }),
            ),
            (
                ChangeKind::Pause { paused: false },
                json!({ "kind": "pause", "asset": asset_hex, "paused": false, "events": 3 }),
            ),
            (
                ChangeKind::Renounce {
                    capabilities: CAP_FREEZE | CAP_PAUSE,
                },
                json!({ "kind": "renounce", "asset": asset_hex, "capabilities": ["freeze", "pause"], "events": 3 }),
            ),
            // a renounce of capabilities the asset no longer had is recorded with none (D-122)
            (
                ChangeKind::Renounce { capabilities: 0 },
                json!({ "kind": "renounce", "asset": asset_hex, "capabilities": [], "events": 3 }),
            ),
        ];
        for (kind, json) in kinds {
            assert_eq!(json_of(kind), json);
        }
    }
}
