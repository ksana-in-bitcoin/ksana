//! Ksana node JSON-RPC (SPEC §11.2): based on jsonrpsee, supporting both HTTP and WebSocket.
//!
//! The interface and JSON format are in `docs/RPC.md`. This crate only defines the interface, the types and the server
//! shell; the implementation is provided by the node through [`Backend`].
//! The JSON conventions are the same as in `ksana-primitives`: byte strings are lowercase hex, `u64` / `u128` are
//! decimal strings.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::net::SocketAddr;
use std::sync::Arc;

use jsonrpsee::core::{async_trait, RpcResult, SubscriptionResult};
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::server::{
    BatchRequestConfig, PendingSubscriptionSink, Server, ServerConfig, ServerHandle,
    SubscriptionMessage,
};
use jsonrpsee::types::ErrorObjectOwned;
use ksana_primitives::json::{dec_u128, dec_u64, hex_array, hex_array_vec, hex_bytes};
use ksana_primitives::{BlockHeader, Entry, Funds, SettlementTxs};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Error codes (the JSON-RPC server-defined range).
pub mod codes {
    /// Malformed parameter (hex, encoding, length).
    pub const INVALID_PARAMS: i32 = -32602;
    /// Invalid op (consensus rules such as signature, nonce, balance).
    pub const OP_INVALID: i32 = -32010;
    /// The op does not meet this node's policy (fee too low, pool full, etc.).
    pub const OP_REJECTED: i32 = -32011;
    /// The queried object does not exist (op, block).
    pub const NOT_FOUND: i32 = -32004;
    /// The method does not exist or is not available: JSON-RPC's own code, which a node also answers to the operation
    /// listings while its operation index is not built, like a node from before them (D-111).
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// Internal node error.
    pub const INTERNAL: i32 = -32603;
}

/// Build an RPC error.
pub fn error(code: i32, message: impl Into<String>) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(code, message.into(), None::<()>)
}

/// The result of `ksana_submitOp`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitResult {
    /// The op ID.
    #[serde(with = "hex_array")]
    pub op_id: [u8; 32],
}

/// Op status (SPEC §10.4: fast-lane confirmation and BTC finality must be clearly distinguished).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpState {
    /// In the entry pool, not yet included.
    Pending,
    /// Included by a fast-lane block and executed successfully.
    FastConfirmed,
    /// The main-chain block that accepted the containing block is BTC-final: on the finalized chain, decided at a BTC
    /// block that has `BTC_FINALITY_CONFS` confirmations (D-082, D-114).
    BtcFinal,
    /// Included, but execution failed (the fee is still charged, the state is rolled back).
    Failed,
    /// Included only by a block merged by the main chain, and could not be executed against the state at merge time
    /// (duplicate inclusion, a conflict on the same nonce, insufficient balance, etc.); not executed and no fee
    /// charged (D-063). While still valid it returns to the entry pool and may be included again later.
    Skipped,
}

/// A block's height and hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockRef {
    /// Height.
    #[serde(with = "dec_u64")]
    pub height: u64,
    /// Block hash.
    #[serde(with = "hex_array")]
    pub hash: [u8; 32],
}

/// An event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventJson {
    /// The contract that emitted the event.
    #[serde(with = "hex_array")]
    pub contract: [u8; 32],
    /// Event data.
    #[serde(with = "hex_bytes")]
    pub data: Vec<u8>,
}

/// An entry receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptJson {
    /// Op ID (empty for unlock entries and read-only calls).
    #[serde(with = "hex_array_opt")]
    pub op_id: Option<[u8; 32]>,
    /// Whether it succeeded.
    pub success: bool,
    /// Failure reason.
    pub error: Option<String>,
    /// Gas consumed (`max_gas` on failure).
    #[serde(with = "dec_u64")]
    pub gas_used: u64,
    /// Return data.
    #[serde(with = "hex_bytes")]
    pub output: Vec<u8>,
    /// Events.
    pub events: Vec<EventJson>,
    /// The changes the op made to built-in assets, in the order it made them (D-122; empty on failure); `null` when the
    /// node did not record them: a node from before them, or a receipt it stored before it recorded them.
    #[serde(default)]
    pub asset_changes: Option<Vec<AssetChangeJson>>,
}

/// A change an op made to a built-in asset (D-122): a movement or a management action of D-121. `events` is how many
/// of the op's events came before it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AssetChangeJson {
    /// The `AssetTransfer` action: from its signer to `to`.
    Transfer {
        /// The asset.
        #[serde(with = "hex_array")]
        asset: [u8; 32],
        /// The signer.
        #[serde(with = "hex_array")]
        from: [u8; 32],
        /// The receiving account.
        #[serde(with = "hex_array")]
        to: [u8; 32],
        /// The amount.
        #[serde(with = "dec_u128")]
        amount: u128,
        /// The op's events before it.
        events: u32,
    },
    /// Funds attached to a `Call` or a `Deploy` (or to `ksana_call`) by its signer, or by a contract to the contract
    /// it calls or creates.
    Funds {
        /// The asset.
        #[serde(with = "hex_array")]
        asset: [u8; 32],
        /// The signer, or the calling contract.
        #[serde(with = "hex_array")]
        from: [u8; 32],
        /// The contract called or created.
        #[serde(with = "hex_array")]
        to: [u8; 32],
        /// The amount.
        #[serde(with = "dec_u128")]
        amount: u128,
        /// The op's events before it.
        events: u32,
    },
    /// A contract's payment (`asset_transfer`).
    Payout {
        /// The asset.
        #[serde(with = "hex_array")]
        asset: [u8; 32],
        /// The paying contract.
        #[serde(with = "hex_array")]
        from: [u8; 32],
        /// The receiving account.
        #[serde(with = "hex_array")]
        to: [u8; 32],
        /// The amount.
        #[serde(with = "dec_u128")]
        amount: u128,
        /// The op's events before it.
        events: u32,
    },
    /// An issuance by the asset's issuing contract (`asset_issue`, `asset_create`).
    Issue {
        /// The asset.
        #[serde(with = "hex_array")]
        asset: [u8; 32],
        /// The receiving account.
        #[serde(with = "hex_array")]
        to: [u8; 32],
        /// The amount.
        #[serde(with = "dec_u128")]
        amount: u128,
        /// The op's events before it.
        events: u32,
    },
    /// A burn (`asset_burn`).
    Burn {
        /// The asset.
        #[serde(with = "hex_array")]
        asset: [u8; 32],
        /// The account the amount was burned from.
        #[serde(with = "hex_array")]
        from: [u8; 32],
        /// The amount.
        #[serde(with = "dec_u128")]
        amount: u128,
        /// The op's events before it.
        events: u32,
    },
    /// The asset's creation, with the capabilities it declared (names in bit order, as `ksana_getAsset` gives them);
    /// its issuance follows as an `issue` unless it is of 0.
    Create {
        /// The asset.
        #[serde(with = "hex_array")]
        asset: [u8; 32],
        /// The capabilities.
        capabilities: Vec<String>,
        /// The op's events before it.
        events: u32,
    },
    /// The issuing contract froze an account, or unfroze it (`asset_freeze`).
    Freeze {
        /// The asset.
        #[serde(with = "hex_array")]
        asset: [u8; 32],
        /// The account.
        #[serde(with = "hex_array")]
        account: [u8; 32],
        /// Whether it is frozen now.
        frozen: bool,
        /// The op's events before it.
        events: u32,
    },
    /// The issuing contract allowed an account, or no longer allows it (`asset_allow`).
    Allow {
        /// The asset.
        #[serde(with = "hex_array")]
        asset: [u8; 32],
        /// The account.
        #[serde(with = "hex_array")]
        account: [u8; 32],
        /// Whether it is allowed now.
        allowed: bool,
        /// The op's events before it.
        events: u32,
    },
    /// The issuing contract paused the asset, or resumed it (`asset_pause`).
    Pause {
        /// The asset.
        #[serde(with = "hex_array")]
        asset: [u8; 32],
        /// Whether it is paused now.
        paused: bool,
        /// The op's events before it.
        events: u32,
    },
    /// The issuing contract gave up capabilities for good (`asset_renounce`): those it still had, by name, none when it
    /// had none of them; giving up `pause` resumes the asset.
    Renounce {
        /// The asset.
        #[serde(with = "hex_array")]
        asset: [u8; 32],
        /// The capabilities given up.
        capabilities: Vec<String>,
        /// The op's events before it.
        events: u32,
    },
}

/// The result of `ksana_getOpStatus`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpStatusJson {
    /// Status.
    pub status: OpState,
    /// The containing block (once included).
    pub block: Option<BlockRef>,
    /// Index among the block's entries (once included).
    pub index: Option<u32>,
    /// Receipt (once included).
    pub receipt: Option<ReceiptJson>,
}

/// The number of operations a page of `ksana_getAccountOps` / `ksana_getLatestOps` holds when the call gives no
/// `limit` (D-111).
pub const DEFAULT_OPS_LIMIT: u32 = 50;

/// The most operations a page of `ksana_getAccountOps` / `ksana_getLatestOps` holds (D-111).
pub const MAX_OPS_LIMIT: u32 = 500;

/// A position on the main chain (D-111): the height of the main-chain block that accepted a block, the block's place
/// in that main-chain block's acceptance order (the blocks it merges first, in consensus order, then the main-chain
/// block itself: the order of an unlock entry's `index`) and the index of an entry in the block. Positions sort in the
/// order the main chain executes the entries. Its JSON form is the string `height:slot:entry` in decimal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpPosition {
    /// Height of the main-chain block that accepted the block.
    pub height: u64,
    /// The block's place in that main-chain block's acceptance order.
    pub slot: u32,
    /// The entry's index in the block.
    pub entry: u32,
}

impl OpPosition {
    /// Parses `height:slot:entry` (decimal numbers without leading zeros).
    pub fn parse(s: &str) -> Result<Self, String> {
        fn number<T: std::str::FromStr>(part: &str) -> Option<T> {
            let canonical = !part.is_empty()
                && part.bytes().all(|b| b.is_ascii_digit())
                && (part == "0" || !part.starts_with('0'));
            canonical.then(|| part.parse().ok()).flatten()
        }
        let mut parts = s.split(':');
        let position = match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(height), Some(slot), Some(entry), None) => {
                match (number(height), number(slot), number(entry)) {
                    (Some(height), Some(slot), Some(entry)) => Some(OpPosition {
                        height,
                        slot,
                        entry,
                    }),
                    _ => None,
                }
            }
            _ => None,
        };
        position.ok_or_else(|| {
            format!("a position must be height:slot:entry in decimal, such as 81:0:2, not {s:?}")
        })
    }
}

impl std::fmt::Display for OpPosition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.height, self.slot, self.entry)
    }
}

impl Serialize for OpPosition {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for OpPosition {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        OpPosition::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// One operation of a listing (`ksana_getAccountOps`, `ksana_getLatestOps`, D-111).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpRefJson {
    /// The op ID.
    #[serde(with = "hex_array")]
    pub op_id: [u8; 32],
    /// Its status, as `ksana_getOpStatus` gives it: `fast_confirmed`, `btc_final`, `failed`, or `skipped` for an op
    /// the main chain includes only where it was skipped.
    pub status: OpState,
    /// The block that includes the op (`hash`) and the height of the main-chain block that accepted that block
    /// (`height`).
    pub block: BlockRef,
    /// Index among the block's entries.
    pub index: u32,
    /// Its position on the main chain: passed as `before`, a listing continues below it.
    pub position: OpPosition,
}

/// A page of a listing of operations on the main chain, from the newest position down (D-111).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpListJson {
    /// The operations, newest first.
    pub ops: Vec<OpRefJson>,
    /// Where the next page starts: passed as `before`, it lists the operations below this position; `null` once the
    /// listing reaches the genesis block.
    pub next: Option<OpPosition>,
    /// The main-chain tip the listing is based on.
    pub at: BlockRef,
}

/// The number of operations a page holds: `limit` when given (1 to [`MAX_OPS_LIMIT`]), [`DEFAULT_OPS_LIMIT`]
/// otherwise.
pub fn ops_limit(limit: Option<u32>) -> RpcResult<usize> {
    match limit.unwrap_or(DEFAULT_OPS_LIMIT) {
        limit @ 1..=MAX_OPS_LIMIT => {
            usize::try_from(limit).map_err(|_| error(codes::INVALID_PARAMS, "limit out of range"))
        }
        _ => Err(error(
            codes::INVALID_PARAMS,
            format!("limit must be 1 to {MAX_OPS_LIMIT}"),
        )),
    }
}

/// The length of a cursor of `ksana_getAccountAssets` (D-120): the kind (0 a built-in asset, 1 a contract) and the ID.
pub const ASSET_CURSOR_BYTES: usize = 33;
/// The length of a cursor of `ksana_getTokenHolders`: the amount (16 bytes big-endian) and the account.
pub const HOLDER_CURSOR_BYTES: usize = 48;
/// The length of a cursor of `ksana_getNfts`: the contract and the token ID.
pub const NFT_CURSOR_BYTES: usize = 64;
/// The length of a cursor of `ksana_getApprovals`: the contract, the kind (0 an allowance, 1 an operator, 2 a token's
/// approved account) and the spender, the operator or the token ID.
pub const APPROVAL_CURSOR_BYTES: usize = 65;

/// The result of `ksana_getContract` (D-120).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractJson {
    /// Contract ID.
    #[serde(with = "hex_array")]
    pub contract: [u8; 32],
    /// The SHA-256 of its code.
    #[serde(with = "hex_array")]
    pub code_hash: [u8; 32],
    /// The account that deployed it.
    #[serde(with = "hex_array")]
    pub deployer: [u8; 32],
    /// The IDs of the standards it declared (`docs/STANDARDS.md` §2), the standard's first; empty when it declared
    /// none, or a declaration that is not 1 to 16 IDs of 1 to 64 bytes.
    pub standards: Vec<String>,
    /// The position of the op whose events hold its declaration; `null` when it made none.
    pub declared: Option<OpPosition>,
    /// The block the query is based on.
    pub at: BlockRef,
}

/// One asset an account holds (`ksana_getAccountAssets`, D-120).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccountAssetJson {
    /// A built-in asset (KSA is 32 zero bytes).
    Asset {
        /// Asset ID.
        #[serde(with = "hex_array")]
        asset: [u8; 32],
        /// Available balance.
        #[serde(with = "dec_u128")]
        balance: u128,
    },
    /// A KS-20 token.
    Token {
        /// The token contract.
        #[serde(with = "hex_array")]
        contract: [u8; 32],
        /// The balance by the token's events.
        #[serde(with = "dec_u128")]
        balance: u128,
    },
    /// A KS-721 collection.
    Nft {
        /// The collection contract.
        #[serde(with = "hex_array")]
        contract: [u8; 32],
        /// How many of its NFTs the account holds.
        #[serde(with = "dec_u128")]
        count: u128,
    },
}

/// The result of `ksana_getAccountAssets` (D-120).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountAssetsJson {
    /// The account's assets: the built-in ones by asset ID, then the tokens and the NFT collections by contract ID.
    pub assets: Vec<AccountAssetJson>,
    /// Where the next page starts (pass it as `after`); `null` once the listing reached its end.
    pub next: Option<String>,
    /// The block the query is based on.
    pub at: BlockRef,
}

/// The result of `ksana_getToken` (D-120): what the node's index knows of a KS-20 token or a KS-721 collection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenJson {
    /// The contract.
    #[serde(with = "hex_array")]
    pub contract: [u8; 32],
    /// `"KS-20"` or `"KS-721"`.
    pub standard: String,
    /// The IDs of the extensions it declared.
    pub extensions: Vec<String>,
    /// Its name, from its latest `metadata` event; `null` before one.
    pub name: Option<String>,
    /// Its symbol.
    pub symbol: Option<String>,
    /// A KS-20 token's decimals (JSON number); `null` for a collection, or before a `metadata` event.
    pub decimals: Option<u8>,
    /// A token's supply (the sum of its balances), or how many NFTs of a collection exist, by its events.
    #[serde(with = "dec_u128")]
    pub supply: u128,
    /// How many accounts hold some.
    #[serde(with = "dec_u64")]
    pub holders: u64,
    /// The position of the op whose events hold its declaration.
    pub declared: OpPosition,
    /// Whether the index passed over an event of the standard it emitted (a transfer of more than the sender held by
    /// its events, of an NFT `from` did not own, a second declaration, metadata out of bounds): its balances may then
    /// differ from what it answers itself.
    pub irregular: bool,
    /// The block the query is based on.
    pub at: BlockRef,
}

/// One holder of a token (`ksana_getTokenHolders`, D-120).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HolderJson {
    /// The account.
    #[serde(with = "hex_array")]
    pub account: [u8; 32],
    /// Its balance, or the number of the collection's NFTs it holds.
    #[serde(with = "dec_u128")]
    pub balance: u128,
}

/// The result of `ksana_getTokenHolders` (D-120).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoldersJson {
    /// The holders, the largest balance first.
    pub holders: Vec<HolderJson>,
    /// Where the next page starts (pass it as `after`); `null` once the listing reached its end.
    pub next: Option<String>,
    /// The block the query is based on.
    pub at: BlockRef,
}

/// One NFT (`ksana_getNfts`, D-120).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NftJson {
    /// The collection.
    #[serde(with = "hex_array")]
    pub contract: [u8; 32],
    /// The token ID.
    #[serde(with = "hex_array")]
    pub token_id: [u8; 32],
}

/// The result of `ksana_getNfts` (D-120).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NftsJson {
    /// The NFTs, by collection and token ID.
    pub nfts: Vec<NftJson>,
    /// Where the next page starts (pass it as `after`); `null` once the listing reached its end.
    pub next: Option<String>,
    /// The block the query is based on.
    pub at: BlockRef,
}

/// One approval an account gave (`ksana_getApprovals`, D-120), as its latest event set it, expired or not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApprovalJson {
    /// A KS-20 allowance.
    Allowance {
        /// The token.
        #[serde(with = "hex_array")]
        contract: [u8; 32],
        /// The spender.
        #[serde(with = "hex_array")]
        spender: [u8; 32],
        /// The amount set; what is left is the token's `allowance` view.
        #[serde(with = "dec_u128")]
        amount: u128,
        /// The block timestamp from which it no longer counts; `null` never.
        #[serde(with = "dec_u64_opt")]
        expires_at: Option<u64>,
    },
    /// A KS-721 operator: it may move every NFT of the account in the collection.
    Operator {
        /// The collection.
        #[serde(with = "hex_array")]
        contract: [u8; 32],
        /// The operator.
        #[serde(with = "hex_array")]
        operator: [u8; 32],
        /// The block timestamp from which it no longer counts; `null` never.
        #[serde(with = "dec_u64_opt")]
        expires_at: Option<u64>,
    },
    /// The approved account of one NFT.
    Token {
        /// The collection.
        #[serde(with = "hex_array")]
        contract: [u8; 32],
        /// The token ID.
        #[serde(with = "hex_array")]
        token_id: [u8; 32],
        /// The approved account.
        #[serde(with = "hex_array")]
        spender: [u8; 32],
    },
}

/// The result of `ksana_getApprovals` (D-120).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalsJson {
    /// The approvals, by contract, then kind, then spender, operator or token ID.
    pub approvals: Vec<ApprovalJson>,
    /// Where the next page starts (pass it as `after`); `null` once the listing reached its end.
    pub next: Option<String>,
    /// The block the query is based on.
    pub at: BlockRef,
}

/// The error of a method of the standards index while the node's index is not built (D-120): the answer of a node
/// without the method, as for the operation listings (D-111).
pub fn index_not_built() -> ErrorObjectOwned {
    error(
        codes::METHOD_NOT_FOUND,
        "the index is not built: the node builds it at startup, and failed to (see its log, D-111, D-120)",
    )
}

/// The result of `ksana_getBlock`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockJson {
    /// Block hash.
    #[serde(with = "hex_array")]
    pub hash: [u8; 32],
    /// Block header.
    pub header: BlockHeader,
    /// Entries.
    pub entries: Vec<Entry>,
    /// Settlement transactions (commit and reveal, Bitcoin network serialization, D-081); absent for blocks without
    /// settlement transactions.
    pub settlement: Option<SettlementTxs>,
    /// Receipts corresponding one-to-one to the entries (when accepted by the main chain; for a merged block, the
    /// execution result at merge time).
    pub receipts: Vec<ReceiptJson>,
    /// Height of the main-chain block that accepted it (D-063): its own height for a main-chain block, the height of
    /// the merging main-chain block for a merged block; `null` when not accepted by the main chain.
    #[serde(with = "dec_u64_opt")]
    pub accepted_height: Option<u64>,
    /// The blocks merged by a main-chain block (the merge set minus the selected parent, in execution order, D-063);
    /// empty for other blocks.
    #[serde(with = "hex_array_vec")]
    pub merged: Vec<[u8; 32]>,
}

/// An asset balance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BalanceJson {
    /// Asset ID (32 zero bytes for KSA).
    #[serde(with = "hex_array")]
    pub asset: [u8; 32],
    /// Amount.
    #[serde(with = "dec_u128")]
    pub amount: u128,
}

/// A producer's not-yet-unlocked locked record (SPEC §8.5, D-074): one block's path-B sponsored fees and block reward.
/// The record is deleted on unlock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedFeeJson {
    /// The block that produced this income.
    #[serde(with = "hex_array")]
    pub block_hash: [u8; 32],
    /// Amount (KSA base units).
    #[serde(with = "dec_u128")]
    pub amount: u128,
}

/// The result of `ksana_getAccount`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountJson {
    /// Account (the 32-byte output key of a Taproot address, D-092).
    #[serde(with = "hex_array")]
    pub account: [u8; 32],
    /// The next usable nonce.
    #[serde(with = "dec_u64")]
    pub nonce: u64,
    /// Available balance per asset (assets with 0 excluded).
    pub balances: Vec<BalanceJson>,
    /// Not-yet-unlocked income (D-074): as producer, the producer's share of the sponsored fees and block rewards; as
    /// block share account, the per-block shares (D-089).
    pub locked_fees: Vec<LockedFeeJson>,
    /// The block the query is based on.
    pub at: BlockRef,
}

/// The result of `ksana_getAsset` (D-074): asset metadata, the capabilities its issuing contract holds and the pause
/// (D-121).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetJson {
    /// Asset ID (32 zero bytes for KSA).
    #[serde(with = "hex_array")]
    pub asset: [u8; 32],
    /// Name.
    pub name: String,
    /// Symbol (D-121).
    pub symbol: String,
    /// Decimals.
    pub decimals: u8,
    /// Total supply (base units). For KSA: the genesis allocation plus the block rewards booked so far (including
    /// those not yet unlocked) minus burns.
    #[serde(with = "dec_u128")]
    pub total_supply: u128,
    /// Issuing contract (32 zero bytes for KSA).
    #[serde(with = "hex_array")]
    pub issuer: [u8; 32],
    /// The capabilities the issuing contract still holds, by name: `mint`, `freeze`, `allowlist` and `pause`, in that
    /// order (D-121). Empty for KSA and for an asset whose supply is fixed and whose movements no one can stop.
    pub capabilities: Vec<String>,
    /// Whether the issuing contract paused the asset: then no account but the issuing contract sends it (D-121).
    pub paused: bool,
    /// The block the query is based on.
    pub at: BlockRef,
}

/// The result of `ksana_getAssetStatus` (D-121): an account's standing in an asset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetStatusJson {
    /// Asset ID.
    #[serde(with = "hex_array")]
    pub asset: [u8; 32],
    /// The account.
    #[serde(with = "hex_array")]
    pub account: [u8; 32],
    /// The account's status flags for the asset as recorded, by name: `frozen` and `allowed`, in that order. A flag
    /// counts only while the asset has the matching capability.
    pub status: Vec<String>,
    /// Whether the asset's rules let the account send or burn it now (its balance aside).
    pub can_send: bool,
    /// Whether the asset's rules let the account receive it now.
    pub can_receive: bool,
    /// The block the query is based on.
    pub at: BlockRef,
}

/// A scheduled network upgrade (D-076).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpgradeJson {
    /// The new protocol version.
    pub version: u32,
    /// Activation point: blocks whose DAA score is not below it use the new version.
    #[serde(with = "dec_u64")]
    pub daa_score: u64,
}

/// The result of `ksana_getChainInfo` (D-076): main-chain tip and protocol version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainInfoJson {
    /// Chain ID.
    pub chain_id: u32,
    /// Main-chain tip.
    pub tip: BlockRef,
    /// The tip's DAA score (D-063).
    #[serde(with = "dec_u64")]
    pub daa_score: u64,
    /// The tip's protocol version (block header version).
    pub protocol_version: u32,
    /// The next scheduled upgrade after the tip; `null` when there is none.
    pub next_upgrade: Option<UpgradeJson>,
    /// The highest protocol version this node's software supports: when the network's version is higher, the software
    /// needs upgrading.
    pub max_known_version: u32,
    /// The BTC-final tip (D-082, D-114): the highest block of the finalized chain decided at a BTC block that has
    /// `BTC_FINALITY_CONFS` confirmations; only a BTC reorg reaching that BTC block can take it back. The genesis block
    /// when there is none yet.
    pub finalized: BlockRef,
}

/// The parameters of `ksana_call`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallRequestJson {
    /// The called contract.
    #[serde(with = "hex_array")]
    pub contract: [u8; 32],
    /// Method name.
    pub method: String,
    /// Borsh-encoded arguments.
    #[serde(with = "hex_bytes")]
    pub args: Vec<u8>,
    /// Attached funds (D-073), transferred from `caller` to the contract; empty by default.
    #[serde(default)]
    pub funds: Vec<Funds>,
    /// The simulated caller; 32 zero bytes by default.
    #[serde(default, with = "hex_array_opt")]
    pub caller: Option<[u8; 32]>,
    /// Gas limit; `BLOCK_GAS_LIMIT` by default.
    #[serde(default, with = "dec_u64_opt")]
    pub max_gas: Option<u64>,
}

/// The state of the settlement transactions on BTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementState {
    /// Fewer than `SETTLEMENT_DEPTH` fast-lane confirmations, not yet broadcast (D-037).
    Pending,
    /// Broadcast, not yet confirmed.
    Broadcast,
    /// The block's data is confirmed on the BTC main chain (a transaction carrying its body or header, D-077).
    Confirmed,
}

/// The transaction confirmed on the BTC main chain that landed the block's data (the earliest one, D-077).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BtcConfirmationJson {
    /// Transaction ID (Bitcoin internal byte order, D-001).
    #[serde(with = "hex_array")]
    pub txid: [u8; 32],
    /// Height of the containing BTC block.
    pub height: u32,
    /// Hash of the containing BTC block (internal byte order).
    #[serde(with = "hex_array")]
    pub block: [u8; 32],
    /// Number of confirmations.
    pub confirmations: u32,
}

/// The result of `ksana_getSettlement` (SPEC §11.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettlementJson {
    /// The block.
    pub block: BlockRef,
    /// ID of the settlement's reveal transaction (which carries the payload, D-081) (Bitcoin internal byte order,
    /// D-001): the settlement transaction carried by the block; for a block without settlement transactions, the
    /// settlement transaction carried by the block that carries its body (`carrier`), or `null` while no block
    /// carries it yet (D-077).
    #[serde(with = "hex_array_opt")]
    pub txid: Option<[u8; 32]>,
    /// The same ID in the display byte order of bitcoind RPC and block explorers.
    pub txid_rpc: Option<String>,
    /// ID of the commit transaction (Bitcoin internal byte order).
    #[serde(with = "hex_array_opt")]
    pub commit_txid: Option<[u8; 32]>,
    /// The same ID in display byte order.
    pub commit_txid_rpc: Option<String>,
    /// The commit and reveal transactions (Bitcoin network serialization).
    pub txs: Option<SettlementTxs>,
    /// The carrier (D-095): the payee account of the sender of the payload that first landed the block's data on BTC:
    /// for a settlement payload the producer of the containing block, for a republish payload its payee (once
    /// `CARRIER_REPOST_DELAY` BTC blocks have passed); `null` when there is none (the reward goes to the producer).
    /// Answered according to the current BTC best chain and the current main chain.
    #[serde(with = "hex_array_opt")]
    pub carrier_account: Option<[u8; 32]>,
    /// For a block without settlement transactions, the block whose settlement payload carried its body (D-077);
    /// `null` for a block with settlement transactions.
    pub carrier: Option<BlockRef>,
    /// Status.
    pub status: SettlementState,
    /// The confirmed transaction that landed the block's data on BTC (the earliest one; may be a rescue settlement or
    /// a republish).
    pub confirmed: Option<BtcConfirmationJson>,
    /// Whether the block has entered the settled prefix (D-038).
    pub settled: bool,
    /// Whether the block is BTC-final (§10.4, D-082, D-114).
    pub btc_final: bool,
}

/// The new-block summary pushed by `ksana_subscribeBlocks`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockSummary {
    /// Height.
    #[serde(with = "dec_u64")]
    pub height: u64,
    /// Block hash.
    #[serde(with = "hex_array")]
    pub hash: [u8; 32],
    /// Number of entries.
    pub entries: u32,
    /// Timestamp (seconds).
    #[serde(with = "dec_u64")]
    pub timestamp: u64,
}

/// Query a block by height, hash or the latest block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockId {
    /// A height on the main chain.
    Height(u64),
    /// A block hash.
    Hash([u8; 32]),
    /// The main-chain tip.
    Latest,
}

impl BlockId {
    /// Parse `"latest"`, a decimal height or a 64-digit hex hash.
    pub fn parse(s: &str) -> Result<Self, ErrorObjectOwned> {
        if s == "latest" {
            return Ok(BlockId::Latest);
        }
        if s.len() == 64 {
            let mut hash = [0u8; 32];
            hex::decode_to_slice(s, &mut hash)
                .map_err(|e| error(codes::INVALID_PARAMS, format!("invalid block hash: {e}")))?;
            return Ok(BlockId::Hash(hash));
        }
        let canonical = !s.is_empty()
            && s.bytes().all(|b| b.is_ascii_digit())
            && (s == "0" || !s.starts_with('0'));
        if !canonical {
            return Err(error(
                codes::INVALID_PARAMS,
                "block identifier must be \"latest\", a decimal height or a 64-digit hex hash",
            ));
        }
        s.parse()
            .map(BlockId::Height)
            .map_err(|_| error(codes::INVALID_PARAMS, "height out of range"))
    }
}

/// Suggested values for path B.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathBEstimateJson {
    /// Suggested token amount (KSA base units): the amount for an ordinary transfer derived from `per_kib`, not below
    /// this node's minimum token amount for packing.
    #[serde(with = "dec_u128")]
    pub amount: u128,
    /// Suggested fee rate: token amount per KiB of cost (cost = max(encoded bytes, bytes equivalent to `max_gas`), the
    /// same ordering this node packs by); the median of the lowest fee rate of the path-B entries of each of the last
    /// 20 main-chain blocks, not below this node's minimum fee rate for packing.
    #[serde(with = "dec_u128")]
    pub per_kib: u128,
}

/// The result of `ksana_estimateFee` (D-054; the path-A part was removed along with path A, D-078).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EstimateFeeJson {
    /// The fee rate (sat/vB) this node funds settlements at: path C and top-up transactions may use it as a reference.
    #[serde(with = "dec_u64")]
    pub feerate_sat_vb: u64,
    /// Suggested token amount for path B.
    pub path_b: PathBEstimateJson,
    /// The block the estimate is based on (the tip).
    pub at: BlockRef,
}

/// The node-side implementation (all blocking calls; the server runs them on the blocking thread pool).
pub trait Backend: Send + Sync + 'static {
    /// Accept a Borsh-encoded VmOp (decoded by the node according to this network's params).
    fn submit_op(&self, op: Vec<u8>) -> RpcResult<SubmitResult>;
    /// Op status.
    fn op_status(&self, op_id: [u8; 32]) -> RpcResult<OpStatusJson>;
    /// A block.
    fn block(&self, id: BlockId) -> RpcResult<BlockJson>;
    /// An account.
    fn account(&self, account: [u8; 32]) -> RpcResult<AccountJson>;
    /// Asset metadata (D-074).
    fn asset(&self, asset: [u8; 32]) -> RpcResult<AssetJson>;
    /// An account's standing in an asset (D-121).
    fn asset_status(&self, asset: [u8; 32], account: [u8; 32]) -> RpcResult<AssetStatusJson>;
    /// A read-only call.
    fn call(&self, req: CallRequestJson) -> RpcResult<ReceiptJson>;
    /// A block's settlement transactions and their BTC landing status.
    fn settlement(&self, id: BlockId) -> RpcResult<SettlementJson>;
    /// Suggested path-B token amount and BTC fee rate (D-054).
    fn estimate_fee(&self) -> RpcResult<EstimateFeeJson>;
    /// Main-chain tip and protocol version (D-076).
    fn chain_info(&self) -> RpcResult<ChainInfoJson>;
    /// An account's operations on the main chain, newest first, at most `limit` below `before` (D-111).
    fn account_ops(
        &self,
        account: [u8; 32],
        before: Option<OpPosition>,
        limit: usize,
    ) -> RpcResult<OpListJson>;
    /// The main chain's operations, newest first, at most `limit` below `before` (D-111).
    fn latest_ops(&self, before: Option<OpPosition>, limit: usize) -> RpcResult<OpListJson>;
    /// New-block notifications.
    fn subscribe_blocks(&self) -> broadcast::Receiver<BlockSummary>;
    /// A contract's code hash, deployer and declaration (D-120). The methods of the standards index answer as a node
    /// without them unless the backend has them.
    fn contract(&self, _contract: [u8; 32]) -> RpcResult<ContractJson> {
        Err(index_not_built())
    }
    /// An account's assets after the cursor `after` ([`ASSET_CURSOR_BYTES`] bytes), at most `limit` (D-120).
    fn account_assets(
        &self,
        _account: [u8; 32],
        _after: Option<Vec<u8>>,
        _limit: usize,
    ) -> RpcResult<AccountAssetsJson> {
        Err(index_not_built())
    }
    /// A token or a collection (D-120).
    fn token(&self, _contract: [u8; 32]) -> RpcResult<TokenJson> {
        Err(index_not_built())
    }
    /// A token's holders after the cursor `after` ([`HOLDER_CURSOR_BYTES`] bytes), at most `limit` (D-120).
    fn token_holders(
        &self,
        _contract: [u8; 32],
        _after: Option<Vec<u8>>,
        _limit: usize,
    ) -> RpcResult<HoldersJson> {
        Err(index_not_built())
    }
    /// The NFTs an account holds, of `contract` only when given, after the cursor `after` ([`NFT_CURSOR_BYTES`]
    /// bytes), at most `limit` (D-120).
    fn nfts(
        &self,
        _account: [u8; 32],
        _contract: Option<[u8; 32]>,
        _after: Option<Vec<u8>>,
        _limit: usize,
    ) -> RpcResult<NftsJson> {
        Err(index_not_built())
    }
    /// The approvals an account gave after the cursor `after` ([`APPROVAL_CURSOR_BYTES`] bytes), at most `limit`
    /// (D-120).
    fn approvals(
        &self,
        _account: [u8; 32],
        _after: Option<Vec<u8>>,
        _limit: usize,
    ) -> RpcResult<ApprovalsJson> {
        Err(index_not_built())
    }
}

/// The JSON-RPC interface (method names carry the `ksana_` prefix).
#[rpc(server, client, namespace = "ksana")]
pub trait KsanaApi {
    /// Submit a path-B VmOp: `op` is the Borsh encoding in hex.
    #[method(name = "submitOp", blocking)]
    fn submit_op(&self, op: String) -> RpcResult<SubmitResult>;

    /// Op status: one of pending, fast_confirmed, btc_final, failed, plus the containing block.
    #[method(name = "getOpStatus", blocking)]
    fn get_op_status(&self, op_id: String) -> RpcResult<OpStatusJson>;

    /// Query a block by height, hash or `"latest"`.
    #[method(name = "getBlock", blocking)]
    fn get_block(&self, id: String) -> RpcResult<BlockJson>;

    /// Query the nonce, the balance of each asset, and the not-yet-unlocked sponsored fees and block rewards.
    #[method(name = "getAccount", blocking)]
    fn get_account(&self, account: String) -> RpcResult<AccountJson>;

    /// Query an asset's name, symbol, decimals, total supply, issuing contract, capabilities and pause (the KSA asset ID
    /// is 32 zero bytes, D-074, D-121).
    #[method(name = "getAsset", blocking)]
    fn get_asset(&self, asset: String) -> RpcResult<AssetJson>;

    /// Query an account's standing in an asset: its status flags and whether the asset's rules let it send and receive
    /// now (D-121).
    #[method(name = "getAssetStatus", blocking)]
    fn get_asset_status(&self, asset: String, account: String) -> RpcResult<AssetStatusJson>;

    /// Call a contract read-only, without going on chain.
    #[method(name = "call", blocking)]
    fn call(&self, request: CallRequestJson) -> RpcResult<ReceiptJson>;

    /// Query a block's settlement transactions and their BTC landing status (block identifier as in `getBlock`).
    #[method(name = "getSettlement", blocking)]
    fn get_settlement(&self, id: String) -> RpcResult<SettlementJson>;

    /// Suggested path-B token amount and BTC fee rate (no parameters, D-054).
    #[method(name = "estimateFee", blocking)]
    fn estimate_fee(&self) -> RpcResult<EstimateFeeJson>;

    /// Main-chain tip, protocol version and the next scheduled upgrade (D-076).
    #[method(name = "getChainInfo", blocking)]
    fn get_chain_info(&self) -> RpcResult<ChainInfoJson>;

    /// An account's operations on the main chain, newest first: those it signed, the asset transfers to it, the calls
    /// to it as a contract (D-111), and those whose standard events name it or were emitted by it (D-120). `before`
    /// continues below the `next` of an earlier page; `limit` is [`DEFAULT_OPS_LIMIT`] when not given, at most
    /// [`MAX_OPS_LIMIT`].
    #[method(name = "getAccountOps", blocking)]
    fn get_account_ops(
        &self,
        account: String,
        before: Option<OpPosition>,
        limit: Option<u32>,
    ) -> RpcResult<OpListJson>;

    /// The main chain's operations, newest first (D-111); `before` and `limit` as in `getAccountOps`.
    #[method(name = "getLatestOps", blocking)]
    fn get_latest_ops(
        &self,
        before: Option<OpPosition>,
        limit: Option<u32>,
    ) -> RpcResult<OpListJson>;

    /// Subscribe to new blocks over WebSocket.
    #[subscription(name = "subscribeBlocks" => "block", unsubscribe = "unsubscribeBlocks", item = BlockSummary)]
    async fn subscribe_blocks(&self) -> SubscriptionResult;

    /// A contract's code hash, deployer and the standards it declared (D-120).
    #[method(name = "getContract", blocking)]
    fn get_contract(&self, contract: String) -> RpcResult<ContractJson>;

    /// The assets an account holds: built-in assets, KS-20 tokens and KS-721 collections (D-120). `after` continues
    /// after the `next` of an earlier page; `limit` as in `getAccountOps`.
    #[method(name = "getAccountAssets", blocking)]
    fn get_account_assets(
        &self,
        account: String,
        after: Option<String>,
        limit: Option<u32>,
    ) -> RpcResult<AccountAssetsJson>;

    /// What the node's index knows of a KS-20 token or a KS-721 collection (D-120).
    #[method(name = "getToken", blocking)]
    fn get_token(&self, contract: String) -> RpcResult<TokenJson>;

    /// A token's or a collection's holders, the largest balance first (D-120); `after` and `limit` as in
    /// `getAccountAssets`.
    #[method(name = "getTokenHolders", blocking)]
    fn get_token_holders(
        &self,
        contract: String,
        after: Option<String>,
        limit: Option<u32>,
    ) -> RpcResult<HoldersJson>;

    /// The NFTs an account holds, of one collection when `contract` is given (D-120); `after` and `limit` as in
    /// `getAccountAssets`.
    #[method(name = "getNfts", blocking)]
    fn get_nfts(
        &self,
        account: String,
        contract: Option<String>,
        after: Option<String>,
        limit: Option<u32>,
    ) -> RpcResult<NftsJson>;

    /// The approvals an account gave: KS-20 allowances, KS-721 operators and the approved accounts of its NFTs
    /// (D-120); `after` and `limit` as in `getAccountAssets`.
    #[method(name = "getApprovals", blocking)]
    fn get_approvals(
        &self,
        account: String,
        after: Option<String>,
        limit: Option<u32>,
    ) -> RpcResult<ApprovalsJson>;
}

struct RpcImpl {
    backend: Arc<dyn Backend>,
}

fn decode_hex(s: &str, what: &str) -> RpcResult<Vec<u8>> {
    hex::decode(s).map_err(|e| error(codes::INVALID_PARAMS, format!("{what} is not hex: {e}")))
}

/// A cursor of a listing of the standards index (D-120): `bytes` bytes of hex.
fn decode_cursor(s: Option<String>, bytes: usize) -> RpcResult<Option<Vec<u8>>> {
    let Some(s) = s else { return Ok(None) };
    let cursor = decode_hex(&s, "after")?;
    if cursor.len() != bytes {
        return Err(error(
            codes::INVALID_PARAMS,
            format!("after must be {bytes} bytes of hex, a page's next"),
        ));
    }
    Ok(Some(cursor))
}

fn decode_id(s: &str, what: &str) -> RpcResult<[u8; 32]> {
    let mut out = [0u8; 32];
    hex::decode_to_slice(s, &mut out).map_err(|e| {
        error(
            codes::INVALID_PARAMS,
            format!("{what} must be 32 bytes of hex: {e}"),
        )
    })?;
    Ok(out)
}

#[async_trait]
impl KsanaApiServer for RpcImpl {
    fn submit_op(&self, op: String) -> RpcResult<SubmitResult> {
        let op = decode_hex(&op, "op")?;
        self.backend.submit_op(op)
    }

    fn get_op_status(&self, op_id: String) -> RpcResult<OpStatusJson> {
        self.backend.op_status(decode_id(&op_id, "op_id")?)
    }

    fn get_block(&self, id: String) -> RpcResult<BlockJson> {
        self.backend.block(BlockId::parse(&id)?)
    }

    fn get_account(&self, account: String) -> RpcResult<AccountJson> {
        self.backend.account(decode_id(&account, "account")?)
    }

    fn get_asset(&self, asset: String) -> RpcResult<AssetJson> {
        self.backend.asset(decode_id(&asset, "asset")?)
    }

    fn get_asset_status(&self, asset: String, account: String) -> RpcResult<AssetStatusJson> {
        let asset = decode_id(&asset, "asset")?;
        self.backend
            .asset_status(asset, decode_id(&account, "account")?)
    }

    fn call(&self, request: CallRequestJson) -> RpcResult<ReceiptJson> {
        self.backend.call(request)
    }

    fn get_settlement(&self, id: String) -> RpcResult<SettlementJson> {
        self.backend.settlement(BlockId::parse(&id)?)
    }

    fn estimate_fee(&self) -> RpcResult<EstimateFeeJson> {
        self.backend.estimate_fee()
    }

    fn get_chain_info(&self) -> RpcResult<ChainInfoJson> {
        self.backend.chain_info()
    }

    fn get_account_ops(
        &self,
        account: String,
        before: Option<OpPosition>,
        limit: Option<u32>,
    ) -> RpcResult<OpListJson> {
        let account = decode_id(&account, "account")?;
        self.backend.account_ops(account, before, ops_limit(limit)?)
    }

    fn get_latest_ops(
        &self,
        before: Option<OpPosition>,
        limit: Option<u32>,
    ) -> RpcResult<OpListJson> {
        self.backend.latest_ops(before, ops_limit(limit)?)
    }

    fn get_contract(&self, contract: String) -> RpcResult<ContractJson> {
        self.backend.contract(decode_id(&contract, "contract")?)
    }

    fn get_account_assets(
        &self,
        account: String,
        after: Option<String>,
        limit: Option<u32>,
    ) -> RpcResult<AccountAssetsJson> {
        let account = decode_id(&account, "account")?;
        let after = decode_cursor(after, ASSET_CURSOR_BYTES)?;
        self.backend
            .account_assets(account, after, ops_limit(limit)?)
    }

    fn get_token(&self, contract: String) -> RpcResult<TokenJson> {
        self.backend.token(decode_id(&contract, "contract")?)
    }

    fn get_token_holders(
        &self,
        contract: String,
        after: Option<String>,
        limit: Option<u32>,
    ) -> RpcResult<HoldersJson> {
        let contract = decode_id(&contract, "contract")?;
        let after = decode_cursor(after, HOLDER_CURSOR_BYTES)?;
        self.backend
            .token_holders(contract, after, ops_limit(limit)?)
    }

    fn get_nfts(
        &self,
        account: String,
        contract: Option<String>,
        after: Option<String>,
        limit: Option<u32>,
    ) -> RpcResult<NftsJson> {
        let account = decode_id(&account, "account")?;
        let contract = contract.map(|c| decode_id(&c, "contract")).transpose()?;
        let after = decode_cursor(after, NFT_CURSOR_BYTES)?;
        if let (Some(contract), Some(after)) = (&contract, &after) {
            if !after.starts_with(contract) {
                return Err(error(
                    codes::INVALID_PARAMS,
                    "after must name the contract given, a page's next",
                ));
            }
        }
        self.backend
            .nfts(account, contract, after, ops_limit(limit)?)
    }

    fn get_approvals(
        &self,
        account: String,
        after: Option<String>,
        limit: Option<u32>,
    ) -> RpcResult<ApprovalsJson> {
        let account = decode_id(&account, "account")?;
        let after = decode_cursor(after, APPROVAL_CURSOR_BYTES)?;
        self.backend.approvals(account, after, ops_limit(limit)?)
    }

    async fn subscribe_blocks(&self, pending: PendingSubscriptionSink) -> SubscriptionResult {
        let mut blocks = self.backend.subscribe_blocks();
        let sink = pending.accept().await?;
        loop {
            match blocks.recv().await {
                Ok(summary) => {
                    let message =
                        SubscriptionMessage::from(serde_json::value::to_raw_value(&summary)?);
                    if sink.send(message).await.is_err() {
                        break;
                    }
                }
                // When the subscriber is too slow, skip the backlogged notifications and keep pushing later blocks
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        Ok(())
    }
}

/// Cross-origin access (CORS, D-055): when a browser page calls the RPC from another origin, the server must allow
/// that origin in the response.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Cors {
    /// Cross-origin calls disallowed (default; command-line tools and server programs are unaffected).
    #[default]
    Disabled,
    /// Allow any origin (`*`, for devnet / signet demos).
    Any,
    /// Allow only the listed origins (e.g. `https://wallet.example.org`).
    Origins(Vec<String>),
}

impl Cors {
    /// Parse the value of `--rpc-cors`: `*` means any origin, otherwise a comma-separated list of origins; an empty
    /// string means disallowed.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(Cors::Disabled);
        }
        if s == "*" {
            return Ok(Cors::Any);
        }
        let origins: Vec<String> = s
            .split(',')
            .map(|o| o.trim().to_string())
            .filter(|o| !o.is_empty())
            .collect();
        for origin in &origins {
            http::HeaderValue::from_str(origin)
                .map_err(|e| format!("origin {origin} is invalid: {e}"))?;
        }
        Ok(Cors::Origins(origins))
    }

    fn layer(&self) -> Result<tower_http::cors::CorsLayer, String> {
        use tower_http::cors::{AllowOrigin, CorsLayer};
        let allowed = || {
            CorsLayer::new()
                .allow_methods([http::Method::POST, http::Method::GET, http::Method::OPTIONS])
                .allow_headers([http::header::CONTENT_TYPE])
        };
        Ok(match self {
            Cors::Disabled => CorsLayer::new(),
            Cors::Any => allowed().allow_origin(AllowOrigin::any()),
            Cors::Origins(origins) => {
                let values = origins
                    .iter()
                    .map(|o| {
                        http::HeaderValue::from_str(o)
                            .map_err(|e| format!("origin {o} is invalid: {e}"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                allowed().allow_origin(AllowOrigin::list(values))
            }
        })
    }
}

/// The longest request body, and WebSocket message, the node's and the indexer's servers read (D-132): 1 MiB, eight
/// times the hex of the longest op (`MAX_OP_BYTES`, 64 KiB); a longer one is answered with `-32007` (HTTP 413).
pub const MAX_REQUEST_BODY_BYTES: u32 = 1024 * 1024;
/// The longest response body (D-132; jsonrpsee's default, stated): a longer answer to a call is `-32008`, to a batch
/// `-32011`.
pub const MAX_RESPONSE_BODY_BYTES: u32 = 10 * 1024 * 1024;
/// The most calls in one batch (D-132; unlimited before): a longer batch is refused as a whole with jsonrpsee's
/// `-32010`, `id` null. The web wallet sends at most as many (`MAX_BATCH`) and halves a batch refused this way.
pub const MAX_BATCH_LEN: u32 = 64;
/// The most connections a server holds at once, HTTP and WebSocket together (D-132; jsonrpsee's default, stated).
pub const MAX_CONNECTIONS: u32 = 100;
/// The most subscriptions one WebSocket connection may hold (D-132; jsonrpsee's default is 1 024): a further one is
/// refused with `-32006`.
pub const MAX_SUBSCRIPTIONS_PER_CONNECTION: u32 = 16;

/// The limits of the node's and the indexer's servers (D-132, local policy).
pub fn server_config() -> ServerConfig {
    ServerConfig::builder()
        .max_request_body_size(MAX_REQUEST_BODY_BYTES)
        .max_response_body_size(MAX_RESPONSE_BODY_BYTES)
        .set_batch_request_config(BatchRequestConfig::Limit(MAX_BATCH_LEN))
        .max_connections(MAX_CONNECTIONS)
        .max_subscriptions_per_connection(MAX_SUBSCRIPTIONS_PER_CONNECTION)
        .build()
}

/// Start the HTTP + WebSocket server on `addr` (must be called inside a tokio runtime) with the limits of
/// [`server_config`]; returns the actual listen address and the handle.
/// `cors` decides whether browser pages may call cross-origin (D-055).
pub async fn start_server(
    addr: SocketAddr,
    backend: Arc<dyn Backend>,
    cors: &Cors,
) -> Result<(SocketAddr, ServerHandle), String> {
    let middleware = tower::ServiceBuilder::new().layer(cors.layer()?);
    let server = Server::builder()
        .set_config(server_config())
        .set_http_middleware(middleware)
        .build(addr)
        .await
        .map_err(|e| format!("cannot listen on {addr}: {e}"))?;
    let local = server.local_addr().map_err(|e| e.to_string())?;
    let handle = server.start(RpcImpl { backend }.into_rpc());
    Ok((local, handle))
}

/// Hex representation of `Option<[u8; 32]>`.
pub mod hex_array_opt {
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serialize.
    pub fn serialize<S: Serializer>(v: &Option<[u8; 32]>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(bytes) => s.serialize_some(&hex::encode(bytes)),
            None => s.serialize_none(),
        }
    }

    /// Deserialize.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<[u8; 32]>, D::Error> {
        match Option::<String>::deserialize(d)? {
            Some(s) => {
                let mut out = [0u8; 32];
                hex::decode_to_slice(&s, &mut out).map_err(D::Error::custom)?;
                Ok(Some(out))
            }
            None => Ok(None),
        }
    }
}

/// Decimal string representation of `Option<u64>`.
pub mod dec_u64_opt {
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serialize.
    pub fn serialize<S: Serializer>(v: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(n) => s.serialize_some(&n.to_string()),
            None => s.serialize_none(),
        }
    }

    /// Deserialize.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        match Option::<String>::deserialize(d)? {
            Some(s) => s.parse().map(Some).map_err(D::Error::custom),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_read_and_write_height_slot_entry() {
        let position = OpPosition {
            height: 81,
            slot: 2,
            entry: 0,
        };
        assert_eq!(position.to_string(), "81:2:0");
        assert_eq!(OpPosition::parse("81:2:0"), Ok(position));
        assert_eq!(
            serde_json::to_string(&position).unwrap(),
            "\"81:2:0\"".to_string()
        );
        assert_eq!(
            serde_json::from_str::<OpPosition>("\"81:2:0\"").unwrap(),
            position
        );
        let max = OpPosition {
            height: u64::MAX,
            slot: u32::MAX,
            entry: u32::MAX,
        };
        assert_eq!(OpPosition::parse(&max.to_string()), Ok(max));
        // Positions sort in the order the main chain executes entries
        assert!(OpPosition::parse("81:2:0").unwrap() < OpPosition::parse("81:10:0").unwrap());
        assert!(OpPosition::parse("80:9:9").unwrap() < OpPosition::parse("81:0:0").unwrap());
        for bad in [
            "",
            "81",
            "81:2",
            "81:2:0:1",
            "081:2:0",
            "81:-2:0",
            "81:2:x",
            "81:2:",
            " 81:2:0",
            "81:4294967296:0",
        ] {
            assert!(OpPosition::parse(bad).is_err(), "{bad:?}");
            assert!(
                serde_json::from_str::<OpPosition>(&format!("{bad:?}")).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn cursors_of_the_standards_index_have_their_exact_lengths() {
        assert_eq!(decode_cursor(None, ASSET_CURSOR_BYTES).unwrap(), None);
        for bytes in [
            ASSET_CURSOR_BYTES,
            HOLDER_CURSOR_BYTES,
            NFT_CURSOR_BYTES,
            APPROVAL_CURSOR_BYTES,
        ] {
            assert_eq!(
                decode_cursor(Some("a1".repeat(bytes)), bytes).unwrap(),
                Some(vec![0xa1; bytes])
            );
            for bad in [
                "a1".repeat(bytes - 1),
                "a1".repeat(bytes + 1),
                "zz".repeat(bytes),
                "a".repeat(2 * bytes - 1),
            ] {
                assert_eq!(
                    decode_cursor(Some(bad.clone()), bytes).unwrap_err().code(),
                    codes::INVALID_PARAMS,
                    "{bad}"
                );
            }
        }
    }

    #[test]
    fn assets_and_approvals_are_tagged_by_kind() {
        let assets = [
            (
                AccountAssetJson::Asset {
                    asset: [0; 32],
                    balance: 5,
                },
                serde_json::json!({ "kind": "asset", "asset": "00".repeat(32), "balance": "5" }),
            ),
            (
                AccountAssetJson::Token {
                    contract: [0xab; 32],
                    balance: u128::MAX,
                },
                serde_json::json!({
                    "kind": "token",
                    "contract": "ab".repeat(32),
                    "balance": u128::MAX.to_string(),
                }),
            ),
            (
                AccountAssetJson::Nft {
                    contract: [0xab; 32],
                    count: 3,
                },
                serde_json::json!({ "kind": "nft", "contract": "ab".repeat(32), "count": "3" }),
            ),
        ];
        for (asset, json) in assets {
            assert_eq!(serde_json::to_value(&asset).unwrap(), json);
            assert_eq!(
                serde_json::from_value::<AccountAssetJson>(json.clone()).unwrap(),
                asset
            );
            assert!(serde_json::from_value::<AccountAssetJson>(with_unknown_field(json)).is_err());
        }
        let approvals = [
            (
                ApprovalJson::Allowance {
                    contract: [1; 32],
                    spender: [2; 32],
                    amount: 50,
                    expires_at: Some(1_900_000_000),
                },
                serde_json::json!({
                    "kind": "allowance",
                    "contract": "01".repeat(32),
                    "spender": "02".repeat(32),
                    "amount": "50",
                    "expires_at": "1900000000",
                }),
            ),
            (
                ApprovalJson::Operator {
                    contract: [1; 32],
                    operator: [3; 32],
                    expires_at: None,
                },
                serde_json::json!({
                    "kind": "operator",
                    "contract": "01".repeat(32),
                    "operator": "03".repeat(32),
                    "expires_at": null,
                }),
            ),
            (
                ApprovalJson::Token {
                    contract: [1; 32],
                    token_id: [4; 32],
                    spender: [2; 32],
                },
                serde_json::json!({
                    "kind": "token",
                    "contract": "01".repeat(32),
                    "token_id": "04".repeat(32),
                    "spender": "02".repeat(32),
                }),
            ),
        ];
        for (approval, json) in approvals {
            assert_eq!(serde_json::to_value(&approval).unwrap(), json);
            assert_eq!(
                serde_json::from_value::<ApprovalJson>(json.clone()).unwrap(),
                approval
            );
            assert!(serde_json::from_value::<ApprovalJson>(with_unknown_field(json)).is_err());
        }
    }

    /// `json` with a field that no kind has: a client refuses it (D-007).
    fn with_unknown_field(mut json: serde_json::Value) -> serde_json::Value {
        json["note"] = serde_json::json!("x");
        json
    }

    #[test]
    fn asset_changes_are_tagged_by_kind_and_refuse_unknown_fields() {
        let (asset, a, b) = ([7; 32], [1; 32], [2; 32]);
        let (hex_asset, hex_a, hex_b) = ("07".repeat(32), "01".repeat(32), "02".repeat(32));
        let changes = [
            (
                AssetChangeJson::Transfer {
                    asset,
                    from: a,
                    to: b,
                    amount: 30,
                    events: 0,
                },
                serde_json::json!({
                    "kind": "transfer", "asset": hex_asset, "from": hex_a, "to": hex_b, "amount": "30", "events": 0,
                }),
            ),
            (
                AssetChangeJson::Funds {
                    asset,
                    from: a,
                    to: b,
                    amount: u128::MAX,
                    events: 1,
                },
                serde_json::json!({
                    "kind": "funds", "asset": hex_asset, "from": hex_a, "to": hex_b,
                    "amount": u128::MAX.to_string(), "events": 1,
                }),
            ),
            (
                AssetChangeJson::Payout {
                    asset,
                    from: b,
                    to: a,
                    amount: 7,
                    events: 2,
                },
                serde_json::json!({
                    "kind": "payout", "asset": hex_asset, "from": hex_b, "to": hex_a, "amount": "7", "events": 2,
                }),
            ),
            (
                AssetChangeJson::Issue {
                    asset,
                    to: a,
                    amount: 100,
                    events: 0,
                },
                serde_json::json!({ "kind": "issue", "asset": hex_asset, "to": hex_a, "amount": "100", "events": 0 }),
            ),
            (
                AssetChangeJson::Burn {
                    asset,
                    from: b,
                    amount: 3,
                    events: 0,
                },
                serde_json::json!({ "kind": "burn", "asset": hex_asset, "from": hex_b, "amount": "3", "events": 0 }),
            ),
            (
                AssetChangeJson::Create {
                    asset,
                    capabilities: vec!["freeze".into(), "pause".into()],
                    events: 0,
                },
                serde_json::json!({ "kind": "create", "asset": hex_asset, "capabilities": ["freeze", "pause"], "events": 0 }),
            ),
            (
                AssetChangeJson::Freeze {
                    asset,
                    account: b,
                    frozen: true,
                    events: 0,
                },
                serde_json::json!({ "kind": "freeze", "asset": hex_asset, "account": hex_b, "frozen": true, "events": 0 }),
            ),
            (
                AssetChangeJson::Allow {
                    asset,
                    account: b,
                    allowed: false,
                    events: 0,
                },
                serde_json::json!({ "kind": "allow", "asset": hex_asset, "account": hex_b, "allowed": false, "events": 0 }),
            ),
            (
                AssetChangeJson::Pause {
                    asset,
                    paused: true,
                    events: 0,
                },
                serde_json::json!({ "kind": "pause", "asset": hex_asset, "paused": true, "events": 0 }),
            ),
            (
                AssetChangeJson::Renounce {
                    asset,
                    capabilities: Vec::new(),
                    events: 0,
                },
                serde_json::json!({ "kind": "renounce", "asset": hex_asset, "capabilities": [], "events": 0 }),
            ),
        ];
        for (change, json) in changes {
            assert_eq!(serde_json::to_value(&change).unwrap(), json);
            assert_eq!(
                serde_json::from_value::<AssetChangeJson>(json.clone()).unwrap(),
                change
            );
            assert!(serde_json::from_value::<AssetChangeJson>(with_unknown_field(json)).is_err());
        }
        // a field of another kind is unknown too
        let pause = serde_json::json!({ "kind": "pause", "asset": hex_asset, "paused": true, "events": 0, "frozen": true });
        let refused = serde_json::from_value::<AssetChangeJson>(pause).unwrap_err();
        assert!(
            refused.to_string().contains("unknown field `frozen`"),
            "{refused}"
        );
    }

    #[test]
    fn a_page_holds_one_to_max_ops() {
        assert_eq!(ops_limit(None).unwrap(), DEFAULT_OPS_LIMIT as usize);
        assert_eq!(ops_limit(Some(1)).unwrap(), 1);
        assert_eq!(
            ops_limit(Some(MAX_OPS_LIMIT)).unwrap(),
            MAX_OPS_LIMIT as usize
        );
        for bad in [0, MAX_OPS_LIMIT + 1, u32::MAX] {
            assert_eq!(
                ops_limit(Some(bad)).unwrap_err().code(),
                codes::INVALID_PARAMS
            );
        }
    }
}
