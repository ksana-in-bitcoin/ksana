//! The indexer's JSON-RPC (D-048): based on jsonrpsee, HTTP and WebSocket on the same port, with the same conventions
//! as the node RPC (`docs/RPC.md`): byte strings are lowercase hex, `u64` is a decimal string, `u32` is a JSON number.
//!
//! | Method | Parameters | Result |
//! | --- | --- | --- |
//! | `indexer_status` | none | `{ "height", "hash", "state_root", "btc_height", "finalized", "halted" }` |
//! | `indexer_stateRoot` | block ID: decimal height, `"latest"` or 64-digit hex block hash | `{ "height", "hash", "state_root" }` |
//!
//! `indexer_stateRoot` only answers for blocks already executed and verified; other blocks return error `-32004`.

use std::net::SocketAddr;
use std::sync::Arc;

use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::server::{Server, ServerHandle};
use ksana_primitives::json::{dec_u64, hex_array};
use ksana_rpc::{codes, error, BlockId, BlockRef};
use serde::{Deserialize, Serialize};

use crate::{Indexer, IndexerStatus};

/// Result of `indexer_status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusJson {
    /// Height of the highest block executed with its `state_root` verified.
    #[serde(with = "dec_u64")]
    pub height: u64,
    /// Hash of that block (may be all zeros after a restart, before verification against the BTC data).
    #[serde(with = "hex_array")]
    pub hash: [u8; 32],
    /// State root of that block.
    #[serde(with = "hex_array")]
    pub state_root: [u8; 32],
    /// Height of the scanned BTC tip.
    pub btc_height: Option<u32>,
    /// The BTC-final tip (D-082, D-114): the highest finalized block decided at a BTC block that has
    /// `BTC_FINALITY_CONFS` confirmations, by the same rule as the node's `ksana_getChainInfo`.
    pub finalized: BlockRef,
    /// Reason processing halted after an invalid block was found.
    pub halted: Option<String>,
}

impl From<IndexerStatus> for StatusJson {
    fn from(s: IndexerStatus) -> Self {
        StatusJson {
            height: s.height,
            hash: s.hash,
            state_root: s.state_root,
            btc_height: s.btc_height,
            finalized: BlockRef {
                height: s.btc_final_height,
                hash: s.btc_final_hash,
            },
            halted: s.halted,
        }
    }
}

/// Result of `indexer_stateRoot`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateRootJson {
    /// Height.
    #[serde(with = "dec_u64")]
    pub height: u64,
    /// Block hash.
    #[serde(with = "hex_array")]
    pub hash: [u8; 32],
    /// State root after execution (matches the block header).
    #[serde(with = "hex_array")]
    pub state_root: [u8; 32],
}

/// The indexer's JSON-RPC interface (method names carry the `indexer_` prefix).
#[rpc(server, client, namespace = "indexer")]
pub trait IndexerApi {
    /// Indexer status: the highest verified block, the BTC scan height, whether processing has halted.
    #[method(name = "status", blocking)]
    fn status(&self) -> RpcResult<StatusJson>;

    /// Hash and state root of a block already executed and verified (block ID: decimal height, `"latest"` or block
    /// hash).
    #[method(name = "stateRoot", blocking)]
    fn state_root(&self, id: String) -> RpcResult<StateRootJson>;
}

struct RpcImpl {
    indexer: Arc<Indexer>,
}

impl IndexerApiServer for RpcImpl {
    fn status(&self) -> RpcResult<StatusJson> {
        Ok(self.indexer.status().into())
    }

    fn state_root(&self, id: String) -> RpcResult<StateRootJson> {
        let found = match BlockId::parse(&id)? {
            BlockId::Height(height) => self
                .indexer
                .block_at(height)
                .map(|(hash, state_root)| (height, hash, state_root)),
            BlockId::Latest => {
                let status = self.indexer.status();
                self.indexer
                    .block_at(status.height)
                    .map(|(hash, state_root)| (status.height, hash, state_root))
            }
            BlockId::Hash(hash) => self
                .indexer
                .find_block(&hash)
                .map(|(height, state_root)| (height, hash, state_root)),
        };
        let (height, hash, state_root) = found.ok_or_else(|| {
            error(
                codes::NOT_FOUND,
                "the block has not yet been executed and verified by the indexer",
            )
        })?;
        Ok(StateRootJson {
            height,
            hash,
            state_root,
        })
    }
}

/// Starts the HTTP + WebSocket server on `addr` (must be called inside a tokio runtime) with the node's limits
/// ([`ksana_rpc::server_config`], D-132); returns the actual listening address and the handle.
pub async fn start_server(
    addr: SocketAddr,
    indexer: Arc<Indexer>,
) -> Result<(SocketAddr, ServerHandle), String> {
    let server = Server::builder()
        .set_config(ksana_rpc::server_config())
        .build(addr)
        .await
        .map_err(|e| format!("cannot listen on {addr}: {e}"))?;
    let local = server.local_addr().map_err(|e| e.to_string())?;
    let handle = server.start(RpcImpl { indexer }.into_rpc());
    Ok((local, handle))
}
