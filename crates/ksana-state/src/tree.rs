//! Glue layer between jmt and RocksDB.
//!
//! `anyhow` appears only in this module: the `TreeReader` methods of jmt 0.12 return `anyhow::Result`, and jmt does
//! not re-export anyhow. Errors produced by this module are first built as [`StateError`] before being handed to jmt,
//! and errors returned by jmt are converted back into [`StateError`], so the public API only sees [`StateError`].

use jmt::storage::{LeafNode, Node, NodeKey, TreeReader, TreeUpdateBatch};
use jmt::{JellyfishMerkleTree, KeyHash, OwnedValue};
use rocksdb::DB;
use sha2::Sha256;

use crate::schema::{self, CF_NODES};
use crate::store::{cf_handle, read_value_at};
use crate::{StateError, Version};

/// The JMT used by the state tree: the hash function is SHA-256.
type Tree<'a> = JellyfishMerkleTree<'a, TreeStore<'a>, Sha256>;

/// jmt's read-only storage interface, reading the committed data in RocksDB directly.
struct TreeStore<'a> {
    db: &'a DB,
    /// When computing version `v`, jmt always looks for the previous version's root at `v - 1` (`TreeCache::new`).
    /// When an empty database starts at genesis `g > 0`, the non-existent root at `g - 1` is treated as the empty
    /// tree (`Node::Null`); otherwise a genesis with an empty write set or only deletions would write the empty root
    /// at version `g - 1`, and version `g` would have no root. For `g = 0` jmt handles the empty tree itself.
    virtual_empty_root: Option<Version>,
}

impl TreeReader for TreeStore<'_> {
    fn get_node_option(&self, node_key: &NodeKey) -> anyhow::Result<Option<Node>> {
        if self.virtual_empty_root == Some(node_key.version()) && node_key.nibble_path().is_empty()
        {
            return Ok(Some(Node::Null));
        }
        Ok(read_node(self.db, node_key)?)
    }

    fn get_value_option(
        &self,
        max_version: Version,
        key_hash: KeyHash,
    ) -> anyhow::Result<Option<OwnedValue>> {
        Ok(read_value_at(self.db, None, &key_hash.0, max_version)?)
    }

    fn get_rightmost_leaf(&self) -> anyhow::Result<Option<(NodeKey, LeafNode)>> {
        // Only jmt's restore (rebuilding the tree from a snapshot) calls this; this crate does not use restore.
        Err(anyhow::anyhow!(
            "ksana-state does not support get_rightmost_leaf (only used by jmt restore)"
        ))
    }
}

/// Reads one JMT node.
pub(crate) fn read_node(db: &DB, node_key: &NodeKey) -> Result<Option<Node>, StateError> {
    let cf = cf_handle(db, CF_NODES)?;
    let Some(bytes) = db.get_pinned_cf(cf, schema::node_key(node_key)?)? else {
        return Ok(None);
    };
    let node = borsh::from_slice::<Node>(&bytes)
        .map_err(|e| StateError::Corrupted(format!("JMT node cannot be decoded: {e}")))?;
    Ok(Some(node))
}

/// Applies `value_set` as version `version` on top of `base` (the previous version; `None` for an empty database).
/// Returns the new root and the update batch produced by jmt, without writing to the database. The caller guarantees
/// that `version` is `base + 1` (for an empty database any value `< u64::MAX`).
pub(crate) fn apply(
    db: &DB,
    base: Option<Version>,
    version: Version,
    value_set: Vec<(KeyHash, Option<OwnedValue>)>,
) -> Result<([u8; 32], TreeUpdateBatch), StateError> {
    let store = TreeStore {
        db,
        virtual_empty_root: match base {
            Some(_) => None,
            None => version.checked_sub(1),
        },
    };
    let tree: Tree<'_> = JellyfishMerkleTree::new(&store);
    let (root, update) = tree
        .put_value_set(value_set, version)
        .map_err(from_anyhow)?;
    Ok((root.0, update))
}

/// Recomputes the root of version `version` from the root node (tests check the `roots` column family with it).
#[cfg(test)]
pub(crate) fn root_from_nodes(db: &DB, version: Version) -> Result<[u8; 32], StateError> {
    let store = TreeStore {
        db,
        virtual_empty_root: None,
    };
    let tree: Tree<'_> = JellyfishMerkleTree::new(&store);
    Ok(tree.get_root_hash(version).map_err(from_anyhow)?.0)
}

/// Converts a jmt error back into [`StateError`]: errors this module handed to jmt are taken back as they are, the
/// rest (inconsistencies reported by jmt itself) are recorded as [`StateError::Jmt`].
fn from_anyhow(err: anyhow::Error) -> StateError {
    match err.downcast::<StateError>() {
        Ok(err) => err,
        Err(err) => StateError::Jmt(format!("{err:#}")),
    }
}
