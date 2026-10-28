//! The genesis config file (D-027). All nodes and indexers of the same network must use the same config.
//!
//! ```json
//! {
//!   "timestamp": "1790000000",
//!   "btc_anchor": { "height": 0, "hash": "<64 hex digits, internal byte order>" },
//!   "bits": 545259519,
//!   "allocations": [ { "account": "<64 hex digits>", "amount": "1000000000" } ],
//!   "block_share_account": "<64 hex digits>"
//! }
//! ```
//!
//! `bits` may be omitted and defaults to `pow_limit_bits`; it can only be harder than the PoW limit (D-027).
//! `block_share_account` may be omitted: when omitted the whole block reward goes to the producer, when given the
//! `BLOCK_SHARE_BPS` share of every reward slot is credited to it (D-089).
//!
//! `timestamp` should be the time the network starts: ASERT uses the genesis block as its anchor (D-026); if the
//! genesis timestamp is far earlier than the actual block production time, the chain looks badly behind schedule to
//! ASERT and the difficulty stays at the floor until it catches up with the schedule.
//!
//! The mainnet template is `mainnet/genesis.json` in the repository (D-096): `block_share_account` is the placeholder
//! [`BLOCK_SHARE_ACCOUNT_PLACEHOLDER`], and `timestamp` and `btc_anchor` are zero values.
//! [`GenesisFile::check_network`] rejects a config carrying the placeholder on every network; a mainnet config must
//! also give the block share account, have no allocations, and have the anchor and the timestamp filled in. The node
//! and the indexer check this when they build the genesis block, so the template cannot start as it is. Mainnet does
//! not start at all yet: its block share is not published (D-134), and a mainnet config that passes every other check
//! is refused as well.

use borsh::BorshSerialize;
use ksana_primitives::json::{dec_u128, dec_u64, hex_array, hex_array_opt};
use ksana_primitives::{BlockHeader, BtcAnchor, Network, Params};
use serde::{Deserialize, Serialize};

use crate::{GenesisConfig, WriteSet};

/// Placeholder of the block share account in the mainnet genesis config template (`mainnet/genesis.json`): the ASCII
/// bytes `KSANA_BLOCK_SHARE_ACCOUNT_TBD___` (32 bytes). It is nobody's account (D-096): a genesis config carrying it
/// cannot start on any network.
pub const BLOCK_SHARE_ACCOUNT_PLACEHOLDER: [u8; 32] = *b"KSANA_BLOCK_SHARE_ACCOUNT_TBD___";

/// Errors of the genesis config file.
#[derive(Debug, thiserror::Error)]
pub enum GenesisError {
    /// The file cannot be read or parsed, the pre-allocations are invalid, or `bits` is invalid.
    #[error("genesis config error: {0}")]
    Config(String),
}

/// One pre-allocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize)]
#[serde(deny_unknown_fields)]
pub struct Allocation {
    /// Account (hex of the 32-byte output public key of a Taproot address, D-092; any 32 bytes, such as a contract ID,
    /// also work).
    #[serde(with = "hex_array")]
    pub account: [u8; 32],
    /// KSA amount.
    #[serde(with = "dec_u128")]
    pub amount: u128,
}

/// Contents of the genesis config file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize)]
#[serde(deny_unknown_fields)]
pub struct GenesisFile {
    /// Genesis block timestamp (seconds).
    #[serde(with = "dec_u64")]
    pub timestamp: u64,
    /// BTC anchor of the genesis block.
    pub btc_anchor: BtcAnchor,
    /// Difficulty target of the genesis block; defaults to the PoW limit.
    #[serde(default)]
    pub bits: Option<u32>,
    /// KSA pre-allocations.
    pub allocations: Vec<Allocation>,
    /// Block share account (D-089, x-only public key): the `BLOCK_SHARE_BPS` share of every reward slot is credited to
    /// it; when omitted the whole block reward goes to the producer. Like the allocations it is written into the
    /// genesis state and enters the config fingerprint.
    #[serde(default, with = "hex_array_opt")]
    pub block_share_account: Option<[u8; 32]>,
}

impl GenesisFile {
    /// Reads from a JSON file.
    pub fn load(path: &std::path::Path) -> Result<Self, GenesisError> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            GenesisError::Config(format!("reading genesis config {}: {e}", path.display()))
        })?;
        serde_json::from_str(&text).map_err(|e| {
            GenesisError::Config(format!("parsing genesis config {}: {e}", path.display()))
        })
    }

    fn config(&self) -> GenesisConfig {
        GenesisConfig::devnet(
            self.allocations
                .iter()
                .map(|a| (a.account, a.amount))
                .collect(),
        )
        .with_block_share_account(self.block_share_account)
    }

    /// The write set of the genesis state.
    pub fn state_writes(&self) -> Result<WriteSet, GenesisError> {
        self.config()
            .writes()
            .map_err(|e| GenesisError::Config(format!("genesis pre-allocations invalid: {e}")))
    }

    /// Fingerprint of the config: the data directory records it to prevent opening the same directory with a different
    /// genesis config.
    pub fn fingerprint(&self) -> Result<[u8; 32], GenesisError> {
        let bytes = borsh::to_vec(self).map_err(|e| GenesisError::Config(e.to_string()))?;
        Ok(ksana_primitives::hash::sha256(&bytes))
    }

    /// Whether the config fits the network (D-096): the block share account must not be the placeholder
    /// [`BLOCK_SHARE_ACCOUNT_PLACEHOLDER`] (on any network); mainnet (by `params.chain_id`) must also give the block
    /// share account (D-089), have no genesis allocations (the upper bound on total rewards is the max supply), and
    /// have a BTC anchor and a timestamp that are not the template's zero values; and since mainnet's block share is
    /// not published yet (D-134), a mainnet config that passes all of that is refused too. [`GenesisFile::header`] runs
    /// this check first, so the node and the indexer already refuse when building the genesis block; the node calls it
    /// once more on its own before writing any data.
    pub fn check_network(&self, params: &Params) -> Result<(), GenesisError> {
        if self.block_share_account == Some(BLOCK_SHARE_ACCOUNT_PLACEHOLDER) {
            return Err(GenesisError::Config(
                "the block share account is still the placeholder KSANA_BLOCK_SHARE_ACCOUNT_TBD___: replace it with a real account (the 64 hex characters of a Taproot address) before starting (D-096)"
                    .into(),
            ));
        }
        if Network::from_chain_id(params.chain_id) != Some(Network::Mainnet) {
            return Ok(());
        }
        if self.block_share_account.is_none() {
            return Err(GenesisError::Config(
                "a mainnet genesis config must give the block share account block_share_account (D-089)".into(),
            ));
        }
        if !self.allocations.is_empty() {
            return Err(GenesisError::Config(
                "mainnet has no genesis allocations (D-089: the upper bound on total rewards is the max supply); allocations must be empty"
                    .into(),
            ));
        }
        if self.btc_anchor.height == 0 || self.btc_anchor.hash == [0; 32] {
            return Err(GenesisError::Config(
                "the btc_anchor of the mainnet genesis config is still the template value: fill in the height and hash (internal byte order) of a BTC block that already has ANCHOR_DEPTH confirmations at genesis"
                    .into(),
            ));
        }
        if self.timestamp == 0 {
            return Err(GenesisError::Config(
                "the timestamp of the mainnet genesis config is still the template value: fill in the genesis time (Unix seconds)".into(),
            ));
        }
        Err(GenesisError::Config(
            "mainnet cannot start yet: its block share (D-089) is not published, and MAINNET holds placeholders for it (D-134)"
                .into(),
        ))
    }

    /// Genesis block header (D-027); `state_root` is the root of the genesis state. It first checks that the config
    /// fits the network ([`GenesisFile::check_network`]) and the total of the genesis allocations.
    pub fn header(
        &self,
        params: &Params,
        state_root: [u8; 32],
    ) -> Result<BlockHeader, GenesisError> {
        self.check_network(params)?;
        self.config()
            .check_supply(params)
            .map_err(|e| GenesisError::Config(format!("genesis pre-allocations invalid: {e}")))?;
        let config = |e: ksana_consensus::ConsensusError| GenesisError::Config(e.to_string());
        let mut header = ksana_consensus::genesis_header(
            params,
            self.timestamp,
            self.btc_anchor.clone(),
            state_root,
        )
        .map_err(config)?;
        if let Some(bits) = self.bits {
            // must be a valid compact target and must not exceed the PoW limit
            ksana_consensus::target_from_bits(bits, params.pow_limit_bits).map_err(config)?;
            header.bits = bits;
        }
        Ok(header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ksana_primitives::{DEVNET, MAINNET, SIGNET};

    /// A filled-in mainnet config.
    fn mainnet_file() -> GenesisFile {
        GenesisFile {
            timestamp: 1_800_000_000,
            btc_anchor: BtcAnchor {
                height: 900_000,
                hash: [7; 32],
            },
            bits: None,
            allocations: Vec::new(),
            block_share_account: Some([1; 32]),
        }
    }

    fn message(result: Result<(), GenesisError>) -> String {
        match result {
            Err(GenesisError::Config(m)) => m,
            Ok(()) => String::new(),
        }
    }

    /// The placeholder cannot start on any network; it is exactly those 32 ASCII bytes.
    #[test]
    fn placeholder_is_rejected_on_every_network() {
        assert_eq!(
            BLOCK_SHARE_ACCOUNT_PLACEHOLDER.as_slice(),
            b"KSANA_BLOCK_SHARE_ACCOUNT_TBD___".as_slice()
        );
        let file = GenesisFile {
            block_share_account: Some(BLOCK_SHARE_ACCOUNT_PLACEHOLDER),
            ..mainnet_file()
        };
        for network in Network::ALL {
            let params = network.params();
            assert!(
                message(file.check_network(params)).contains("placeholder"),
                "{network:?}"
            );
            assert!(file.header(params, [0; 32]).is_err(), "{network:?}");
        }
    }

    /// Mainnet: must have a block share account, no allocations, and a non-zero anchor and timestamp; and while its
    /// block share is not published (D-134), a config that has all of them cannot build the genesis block either.
    #[test]
    fn mainnet_requires_a_share_account_no_allocations_and_a_real_anchor() {
        let filled = mainnet_file();
        assert!(message(filled.check_network(&MAINNET)).contains("not published"));
        assert!(filled.header(&MAINNET, [9; 32]).is_err());

        let missing = GenesisFile {
            block_share_account: None,
            ..mainnet_file()
        };
        assert!(message(missing.check_network(&MAINNET)).contains("block_share_account"));
        let allocated = GenesisFile {
            allocations: vec![Allocation {
                account: [2; 32],
                amount: 0,
            }],
            ..mainnet_file()
        };
        assert!(message(allocated.check_network(&MAINNET)).contains("allocations"));
        let zero_height = GenesisFile {
            btc_anchor: BtcAnchor {
                height: 0,
                hash: [7; 32],
            },
            ..mainnet_file()
        };
        assert!(message(zero_height.check_network(&MAINNET)).contains("btc_anchor"));
        let zero_hash = GenesisFile {
            btc_anchor: BtcAnchor {
                height: 900_000,
                hash: [0; 32],
            },
            ..mainnet_file()
        };
        assert!(message(zero_hash.check_network(&MAINNET)).contains("btc_anchor"));
        let no_time = GenesisFile {
            timestamp: 0,
            ..mainnet_file()
        };
        assert!(message(no_time.check_network(&MAINNET)).contains("timestamp"));
        for bad in [missing, allocated, zero_height, zero_hash, no_time] {
            assert!(bad.header(&MAINNET, [0; 32]).is_err());
        }
    }

    /// devnet and signet are not bound by the mainnet requirements: no share account, allocations, anchor height 0 and
    /// timestamp 0 are all fine.
    #[test]
    fn devnet_and_signet_accept_what_mainnet_rejects() {
        let file = GenesisFile {
            timestamp: 0,
            btc_anchor: BtcAnchor {
                height: 0,
                hash: [0; 32],
            },
            bits: None,
            allocations: vec![Allocation {
                account: [2; 32],
                amount: 1_000,
            }],
            block_share_account: None,
        };
        for params in [&DEVNET, &SIGNET] {
            assert!(file.check_network(params).is_ok());
            assert!(file.header(params, [0; 32]).is_ok());
        }
        assert!(file.header(&MAINNET, [0; 32]).is_err());
    }

    /// The mainnet template `mainnet/genesis.json` in the repository: it cannot start as it is (placeholder), and filled
    /// in with a real account, anchor and timestamp it waits for mainnet's block share to be published (D-134); the
    /// template has no `bits`, so the genesis difficulty defaults to the PoW limit.
    #[test]
    fn mainnet_template_refuses_to_start_until_filled_in() {
        let text = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../mainnet/genesis.json"
        ));
        let file: GenesisFile = serde_json::from_str(text).unwrap();
        assert_eq!(
            file.block_share_account,
            Some(BLOCK_SHARE_ACCOUNT_PLACEHOLDER)
        );
        assert!(file.allocations.is_empty());
        assert_eq!(file.bits, None);
        assert_eq!(file.timestamp, 0);
        assert_eq!(file.btc_anchor.height, 0);
        assert!(message(file.check_network(&MAINNET)).contains("placeholder"));
        assert!(file.header(&MAINNET, [0; 32]).is_err());
        assert!(
            file.header(&DEVNET, [0; 32]).is_err(),
            "the placeholder is refused on devnet too"
        );

        let filled = GenesisFile {
            timestamp: 1_800_000_000,
            btc_anchor: BtcAnchor {
                height: 900_000,
                hash: [7; 32],
            },
            block_share_account: Some([1; 32]),
            ..file
        };
        assert!(message(filled.check_network(&MAINNET)).contains("not published"));
        assert_ne!(filled.fingerprint().unwrap(), [0; 32]);
    }
}
