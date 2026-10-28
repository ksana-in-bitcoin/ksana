//! Mining algorithm (D-062): PoW value computation and solving decoupled from consensus; the network's algorithm is
//! Argon2id (D-094).
//!
//! Consensus uses the algorithm only through [`PowAlgorithm`]: the encoding of `bits`, the target, the work and the
//! difficulty adjustment are all defined in terms of the "target" (D-025, D-026), independent of the algorithm; the
//! block identifier (block hash) is always `sha256d(borsh(header))`. The algorithm selected by the network is given by
//! [`Params::pow_algorithm`] ([`pow_algorithm`]); changing the algorithm or its parameters is a consensus change.
//!
//! - [`Argon2id`]: the network's algorithm (RFC 9106). The message is the Borsh encoding of the block header (including
//!   the nonce), the salt is `"ksana-pow" ‖ chain_id`, memory, passes and parallelism follow the network params; the
//!   32-byte output interpreted as little-endian is the PoW value. One computation per block header takes about 1 ms
//!   (2 MiB); verification and solving cost the same, and there is no intermediate state to reuse.
//! - [`Sha256d`]: the block hash interpreted as little-endian — only for tests and benchmarks (about 1 µs per
//!   attempt), not the algorithm of any network.

use std::fmt::Debug;
use std::sync::Arc;

use argon2::{Algorithm, Argon2, Block, Version};
use ksana_primitives::{Argon2Params, BlockHeader, Params, PowAlgorithmId};

use crate::pow::pow_value;
use crate::{ConsensusError, U256};

/// Mining algorithm. Implementations must be deterministic: the PoW value of the same block header is the same on any
/// machine.
///
/// A computation failure (e.g. out of memory) is a local fault, not an invalid block: it returns
/// [`ConsensusError::PowUnavailable`], and the caller must not judge the block invalid because of it.
pub trait PowAlgorithm: Send + Sync + Debug {
    /// The algorithm's name (for logs).
    fn name(&self) -> &'static str;

    /// The PoW value of a block header: the PoW is met when it is not greater than the (effective) target
    /// represented by `bits`.
    fn pow_value(&self, header: &BlockHeader) -> Result<U256, ConsensusError>;

    /// The suggested number of nonces to try in one [`PowAlgorithm::solve`] call: about 10-20 milliseconds of work.
    /// The block production loop checks between two calls whether the template has expired, so this value determines
    /// how fast the producer reacts to new entries and new blocks. Default 2^14 (suitable for a hash of about 1 µs per
    /// attempt); slow algorithms must override it.
    fn solve_chunk(&self) -> u64 {
        1 << 14
    }

    /// Looks within `[start, start + iterations)` (no wrap-around, up to `u64::MAX`) in increasing nonce order for the
    /// first nonce with `pow_value ≤ target`. When found, writes it to `header.nonce` and returns `Ok(true)`; otherwise
    /// returns `Ok(false)` and `header` is unchanged.
    ///
    /// The default implementation calls [`PowAlgorithm::pow_value`] nonce by nonce; an algorithm may provide a faster
    /// implementation, and the result must be the same.
    fn solve(
        &self,
        header: &mut BlockHeader,
        target: &U256,
        start: u64,
        iterations: u64,
    ) -> Result<bool, ConsensusError> {
        let Some(span) = iterations.checked_sub(1) else {
            return Ok(false);
        };
        let original = header.nonce;
        for nonce in start..=start.saturating_add(span) {
            header.nonce = nonce;
            if self.pow_value(header)? <= *target {
                return Ok(true);
            }
        }
        header.nonce = original;
        Ok(false)
    }
}

/// The Argon2id (RFC 9106, version 0x13) PoW (D-094):
/// `pow_value = LE(Argon2id(message = borsh(header), salt = "ksana-pow" ‖ chain_id, memory m KiB, passes t,
/// parallelism p, output 32 bytes))`.
///
/// The parameters come from the network's [`Params::pow_algorithm`] ([`Argon2id::new`] checks them), the salt from
/// `chain_id`; there is no secret key and no associated data.
#[derive(Clone, Debug)]
pub struct Argon2id {
    argon2: Argon2<'static>,
    params: Argon2Params,
    salt: [u8; 13],
}

impl PartialEq for Argon2id {
    fn eq(&self, other: &Self) -> bool {
        self.params == other.params && self.salt == other.salt
    }
}

impl Eq for Argon2id {}

impl Argon2id {
    /// The number of nonces tried per `solve` call (about 10-20 milliseconds).
    const SOLVE_CHUNK: u64 = 16;

    /// The algorithm with parameters `params` on network `chain_id`. Returns [`ConsensusError::InvalidParams`] when the
    /// parameters are outside Argon2's range (memory less than `8 × lanes` KiB, 0 passes, parallelism 0 or more than
    /// 2^24 − 1).
    pub fn new(chain_id: u32, params: Argon2Params) -> Result<Self, ConsensusError> {
        let argon2_params = argon2::Params::new(
            params.memory_kib,
            params.iterations,
            params.lanes,
            Some(Argon2Params::OUTPUT_LEN),
        )
        .map_err(|e| {
            ConsensusError::InvalidParams(match e {
                argon2::Error::MemoryTooLittle | argon2::Error::MemoryTooMuch => {
                    "Argon2id memory parameter out of range (at least 8 KiB × parallelism)"
                }
                argon2::Error::ThreadsTooFew | argon2::Error::ThreadsTooMany => {
                    "Argon2id parallelism out of range (1-2^24 − 1)"
                }
                argon2::Error::TimeTooSmall => "Argon2id passes must be at least 1",
                _ => "Argon2id parameters invalid",
            })
        })?;
        Ok(Self {
            argon2: Argon2::new(Algorithm::Argon2id, Version::V0x13, argon2_params),
            params,
            salt: Argon2Params::salt(chain_id),
        })
    }

    /// The parameters.
    pub fn params(&self) -> Argon2Params {
        self.params
    }

    /// The salt: `"ksana-pow" ‖ chain_id` (4 little-endian bytes).
    pub fn salt(&self) -> &[u8; 13] {
        &self.salt
    }

    /// The Argon2id output of a block header (32 bytes, the raw bytes of the PoW value).
    pub fn pow_hash(&self, header: &BlockHeader) -> Result<[u8; 32], ConsensusError> {
        let mut out = [0u8; 32];
        self.argon2
            .hash_password_into(&header.to_bytes(), &self.salt, &mut out)
            .map_err(unavailable)?;
        Ok(out)
    }

    /// Computes with the given memory blocks (solving reuses the same memory, avoiding allocating and zeroing m KiB
    /// on every attempt).
    fn pow_hash_with_memory(
        &self,
        header: &BlockHeader,
        memory: &mut [Block],
    ) -> Result<[u8; 32], ConsensusError> {
        let mut out = [0u8; 32];
        self.argon2
            .hash_password_into_with_memory(&header.to_bytes(), &self.salt, &mut out, memory)
            .map_err(unavailable)?;
        Ok(out)
    }
}

/// Computation failure (local fault).
fn unavailable(e: argon2::Error) -> ConsensusError {
    ConsensusError::PowUnavailable(e.to_string())
}

impl PowAlgorithm for Argon2id {
    fn name(&self) -> &'static str {
        "argon2id"
    }

    fn pow_value(&self, header: &BlockHeader) -> Result<U256, ConsensusError> {
        Ok(pow_value(&self.pow_hash(header)?))
    }

    fn solve_chunk(&self) -> u64 {
        Self::SOLVE_CHUNK
    }

    fn solve(
        &self,
        header: &mut BlockHeader,
        target: &U256,
        start: u64,
        iterations: u64,
    ) -> Result<bool, ConsensusError> {
        let Some(span) = iterations.checked_sub(1) else {
            return Ok(false);
        };
        let block_count = self.argon2.params().block_count();
        let mut memory: Vec<Block> = Vec::new();
        memory.try_reserve_exact(block_count).map_err(|_| {
            ConsensusError::PowUnavailable("allocating Argon2id memory failed".into())
        })?;
        memory.resize(block_count, Block::new());
        let original = header.nonce;
        for nonce in start..=start.saturating_add(span) {
            header.nonce = nonce;
            if pow_value(&self.pow_hash_with_memory(header, &mut memory)?) <= *target {
                return Ok(true);
            }
        }
        header.nonce = original;
        Ok(false)
    }
}

/// SHA256d: PoW value = the block hash interpreted as little-endian (D-025). Only for tests and benchmarks (about 1 µs
/// per attempt, about 1 000 times faster than Argon2id), not the algorithm of any network (D-094); nodes and indexers
/// can inject it via `NodeConfig::pow_algorithm` / `IndexerConfig::pow_algorithm`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sha256d;

impl PowAlgorithm for Sha256d {
    fn name(&self) -> &'static str {
        "sha256d"
    }

    fn pow_value(&self, header: &BlockHeader) -> Result<U256, ConsensusError> {
        Ok(pow_value(&header.hash()))
    }
}

/// The algorithm selected by the network params ([`Params::pow_algorithm`], with `params.chain_id` as the salt);
/// returns [`ConsensusError::InvalidParams`] when the algorithm parameters are invalid.
pub fn pow_algorithm(params: &Params) -> Result<Arc<dyn PowAlgorithm>, ConsensusError> {
    match params.pow_algorithm {
        PowAlgorithmId::Argon2id(argon2) => Ok(Arc::new(Argon2id::new(params.chain_id, argon2)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ksana_primitives::{BtcAnchor, DEVNET};

    fn header() -> BlockHeader {
        BlockHeader {
            version: 2,
            height: 7,
            parents: vec![[0x11; 32], [0x22; 32]],
            btc_anchor: BtcAnchor {
                height: 900_000,
                hash: [0x33; 32],
            },
            entries_root: [0x44; 32],
            state_root: [0x55; 32],
            producer: [0x66; 32],
            timestamp: 1_700_000_000,
            bits: 0x207f_ffff,
            nonce: 0x0123_4567_89ab_cdef,
        }
    }

    /// The Argon2id test vector of RFC 9106 §5.3 (with secret key and associated data, through the same
    /// implementation): checks the dependency's implementation itself.
    #[test]
    fn argon2_crate_matches_rfc_9106_vector() {
        let params = argon2::ParamsBuilder::new()
            .m_cost(32)
            .t_cost(3)
            .p_cost(4)
            .data(argon2::AssociatedData::new(&[0x04; 12]).unwrap())
            .output_len(32)
            .build()
            .unwrap();
        let argon2 =
            Argon2::new_with_secret(&[0x03; 8], Algorithm::Argon2id, Version::V0x13, params)
                .unwrap();
        let mut out = [0u8; 32];
        argon2
            .hash_password_into(&[0x01; 32], &[0x02; 16], &mut out)
            .unwrap();
        assert_eq!(
            out,
            [
                0x0d, 0x64, 0x0d, 0xf5, 0x8d, 0x78, 0x76, 0x6c, 0x08, 0xc0, 0x37, 0xa3, 0x4a, 0x8b,
                0x53, 0xc9, 0xd0, 0x1e, 0xf0, 0x45, 0x2d, 0x75, 0xb6, 0x5e, 0xb5, 0x25, 0x20, 0xe9,
                0x6b, 0x01, 0xe6, 0x59,
            ]
        );
    }

    /// The network params yield Argon2id: the salt contains the chain ID, the PoW value is the output interpreted as
    /// little-endian; the memory-reusing path of `solve` agrees with `pow_value` one by one.
    #[test]
    fn network_algorithm_is_argon2id_with_the_chain_salt() {
        let algorithm = pow_algorithm(&DEVNET).unwrap();
        assert_eq!(algorithm.name(), "argon2id");
        let PowAlgorithmId::Argon2id(params) = DEVNET.pow_algorithm;
        let argon2id = Argon2id::new(DEVNET.chain_id, params).unwrap();
        assert_eq!(argon2id.params(), params);
        assert_eq!(
            argon2id.salt().to_vec(),
            [b"ksana-pow".as_slice(), &DEVNET.chain_id.to_le_bytes()].concat()
        );
        let h = header();
        let hash = argon2id.pow_hash(&h).unwrap();
        assert_eq!(algorithm.pow_value(&h).unwrap(), U256::from_le_bytes(hash));
        assert_eq!(argon2id.pow_hash(&h).unwrap(), hash, "determinism");
        // another chain ID has a different salt and a different value
        let other = Argon2id::new(DEVNET.chain_id + 1, params).unwrap();
        assert_ne!(other.pow_hash(&h).unwrap(), hash);
        assert_ne!(argon2id, other);
        // a different nonce gives a different value
        let mut h2 = h.clone();
        h2.nonce += 1;
        assert_ne!(argon2id.pow_hash(&h2).unwrap(), hash);

        // solving (reusing memory) and the default one-by-one attempts find the same nonce; with target 2^255 about
        // half of the nonces qualify
        let target = U256::MAX.shr(1);
        let mut by_default = h.clone();
        let mut by_memory = h.clone();
        let found = argon2id.solve(&mut by_memory, &target, 1000, 64).unwrap();
        assert!(found);
        let mut nonce = 1000;
        loop {
            by_default.nonce = nonce;
            if argon2id.pow_value(&by_default).unwrap() <= target {
                break;
            }
            nonce += 1;
        }
        assert_eq!(by_memory.nonce, nonce);
        assert_eq!(by_memory, by_default);
        // the block header is unchanged when nothing is found
        let mut unsolved = h.clone();
        assert!(!argon2id.solve(&mut unsolved, &U256::ZERO, 0, 3).unwrap());
        assert_eq!(unsolved, h);
        assert!(!argon2id.solve(&mut unsolved, &U256::MAX, 0, 0).unwrap());
        assert_eq!(unsolved, h);
        assert_eq!(algorithm.solve_chunk(), 16);
    }

    /// Parameter checks: memory less than 8 × parallelism, 0 passes and parallelism 0 are all parameter errors (local
    /// configuration), not an invalid block.
    #[test]
    fn invalid_argon2_parameters_are_configuration_errors() {
        let ok = Argon2Params {
            memory_kib: 8,
            iterations: 1,
            lanes: 1,
        };
        assert!(Argon2id::new(1, ok).is_ok());
        for bad in [
            Argon2Params {
                memory_kib: 7,
                ..ok
            },
            Argon2Params {
                memory_kib: 15,
                lanes: 2,
                ..ok
            },
            Argon2Params {
                iterations: 0,
                ..ok
            },
            Argon2Params { lanes: 0, ..ok },
            Argon2Params {
                lanes: 1 << 24,
                memory_kib: u32::MAX,
                ..ok
            },
        ] {
            assert!(
                matches!(Argon2id::new(1, bad), Err(ConsensusError::InvalidParams(_))),
                "{bad:?}"
            );
        }
        let params = Params {
            pow_algorithm: PowAlgorithmId::Argon2id(Argon2Params {
                iterations: 0,
                ..ok
            }),
            ..DEVNET
        };
        assert!(matches!(
            pow_algorithm(&params),
            Err(ConsensusError::InvalidParams(_))
        ));
    }

    /// The test-only SHA256d: the PoW value is just the block hash interpreted as little-endian.
    #[test]
    fn sha256d_is_the_block_hash_little_endian() {
        let h = header();
        assert_eq!(
            Sha256d.pow_value(&h).unwrap(),
            U256::from_le_bytes(h.hash())
        );
        assert_eq!(Sha256d.name(), "sha256d");
        assert_eq!(Sha256d.solve_chunk(), 1 << 14);
    }
}
