//! PoW consensus: ASERT difficulty adjustment (D-026) + Bitcoin's target and work conventions (D-025) + the mining
//! algorithm selected by the network params (D-062; Argon2id, D-094) + two-speed difficulty (D-084, D-090: the
//! effective target of an empty block is the `bits` target divided by its weight, and the weight doubles from N up to
//! the cap according to the number of consecutive empty blocks on the selected chain).

use std::sync::Arc;

use ksana_primitives::{BlockHeader, Params};

use crate::algorithm::{pow_algorithm, PowAlgorithm};
use crate::asert::Asert;
use crate::pow::{check_pow, effective_target, work_from_target};
use crate::{Consensus, ConsensusError, U256};

/// PoW consensus (SPEC §6).
#[derive(Clone, Debug)]
pub struct PowConsensus {
    params: Params,
    asert: Asert,
    algorithm: Arc<dyn PowAlgorithm>,
}

/// Equal when the params, the difficulty adjustment and the algorithm (by name) are all the same.
impl PartialEq for PowConsensus {
    fn eq(&self, other: &Self) -> bool {
        self.params == other.params
            && self.asert == other.asert
            && self.algorithm.name() == other.algorithm.name()
    }
}

impl Eq for PowConsensus {}

impl PowConsensus {
    /// Uses `genesis` as the ASERT anchor (D-026) and `params.pow_algorithm` as the mining algorithm (D-062, with
    /// `params.chain_id` as the salt, D-094). The genesis block height must be 0 and its `bits` must be a valid target
    /// not above the PoW limit; the block interval and half-life in the params must be greater than 0, and the PoW
    /// limit and the algorithm parameters must be valid.
    pub fn new(params: &Params, genesis: &BlockHeader) -> Result<Self, ConsensusError> {
        Self::with_algorithm(params, genesis, pow_algorithm(params)?)
    }

    /// Same as [`PowConsensus::new`], but uses the given mining algorithm (for testing other algorithms; the selection
    /// in the network params is ignored).
    pub fn with_algorithm(
        params: &Params,
        genesis: &BlockHeader,
        algorithm: Arc<dyn PowAlgorithm>,
    ) -> Result<Self, ConsensusError> {
        Ok(Self {
            params: params.clone(),
            asert: Asert::from_genesis(params, genesis)?,
            algorithm,
        })
    }

    /// The mining algorithm.
    pub fn algorithm(&self) -> &dyn PowAlgorithm {
        self.algorithm.as_ref()
    }

    /// The protocol params.
    pub fn params(&self) -> &Params {
        &self.params
    }

    /// The difficulty adjustment.
    pub fn asert(&self) -> &Asert {
        &self.asert
    }

    /// The header's version is not above the highest this node knows (D-076).
    fn check_known_version(&self, header: &BlockHeader) -> Result<(), ConsensusError> {
        let max_known = self.params.max_known_version();
        if header.version > max_known {
            return Err(ConsensusError::UnknownVersion {
                got: header.version,
                max_known,
            });
        }
        Ok(())
    }
}

impl Consensus for PowConsensus {
    fn streak(&self, header: &BlockHeader, selected_parent_streak: u64) -> u64 {
        self.params.block_streak(header, selected_parent_streak)
    }

    fn weight(&self, header: &BlockHeader, selected_parent_streak: u64) -> u64 {
        self.params.block_weight(header, selected_parent_streak)
    }

    fn next_bits(
        &self,
        selected_parent: &BlockHeader,
        daa_score: u64,
        weight: u64,
    ) -> Result<u32, ConsensusError> {
        // two-speed difficulty (D-084): remove this block's own weight, so empty and non-empty blocks have
        // the same bits
        let height = daa_score
            .checked_sub(weight)
            .ok_or(ConsensusError::Overflow)?;
        self.asert.next_bits(height, selected_parent.timestamp)
    }

    fn check_header_pow(&self, header: &BlockHeader, weight: u64) -> Result<(), ConsensusError> {
        self.check_known_version(header)?;
        check_pow(self.algorithm(), header, self.params.pow_limit_bits, weight)
    }

    fn check_header_rules(
        &self,
        selected_parent: &BlockHeader,
        daa_score: u64,
        weight: u64,
        header: &BlockHeader,
    ) -> Result<(), ConsensusError> {
        self.check_known_version(header)?;
        let expected = self.params.protocol_version(daa_score);
        if header.version != expected {
            return Err(ConsensusError::BadVersion {
                expected,
                got: header.version,
            });
        }
        let (first, rest) = header
            .parents
            .split_first()
            .ok_or(ConsensusError::BadParent)?;
        if *first != selected_parent.hash() {
            return Err(ConsensusError::BadParent);
        }
        let ascending = rest.windows(2).all(|w| matches!(w, [a, b] if a < b));
        if !ascending || rest.contains(first) {
            return Err(ConsensusError::BadParentOrder);
        }
        let expected_height = selected_parent
            .height
            .checked_add(1)
            .ok_or(ConsensusError::Overflow)?;
        if header.height != expected_height {
            return Err(ConsensusError::BadHeight {
                expected: expected_height,
                got: header.height,
            });
        }
        let expected_bits = self.next_bits(selected_parent, daa_score, weight)?;
        if header.bits != expected_bits {
            return Err(ConsensusError::BitsMismatch {
                expected: expected_bits,
                got: header.bits,
            });
        }
        Ok(())
    }

    fn work(&self, header: &BlockHeader, weight: u64) -> Result<U256, ConsensusError> {
        work_from_target(&effective_target(
            header.bits,
            self.params.pow_limit_bits,
            weight,
        )?)
    }

    fn solve_chunk(&self) -> u64 {
        self.algorithm.solve_chunk()
    }

    fn solve(
        &self,
        header: &mut BlockHeader,
        weight: u64,
        start: u64,
        iterations: u64,
    ) -> Result<bool, ConsensusError> {
        match effective_target(header.bits, self.params.pow_limit_bits, weight) {
            Ok(target) => self.algorithm.solve(header, &target, start, iterations),
            Err(_) => Ok(false),
        }
    }
}
