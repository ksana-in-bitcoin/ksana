//! Genesis config file (D-027): defined in `ksana-execution::genesis_file` (shared by the node and the indexer),
//! re-exported here.

pub use ksana_execution::genesis_file::{
    Allocation, GenesisError, GenesisFile, BLOCK_SHARE_ACCOUNT_PLACEHOLDER,
};

use crate::NodeError;

impl From<GenesisError> for NodeError {
    fn from(e: GenesisError) -> Self {
        match e {
            GenesisError::Config(m) => NodeError::Config(m),
        }
    }
}
