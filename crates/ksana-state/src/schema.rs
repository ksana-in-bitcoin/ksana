//! RocksDB column families and key-value encoding. These are the local storage format and not part of consensus (see
//! the storage layout table in the crate docs).

use jmt::storage::NodeKey;

use crate::{StateError, Version};

/// Version range metadata.
pub(crate) const CF_META: &str = "meta";
/// JMT nodes: `version ‖ nibble count ‖ path` → Borsh(Node).
pub(crate) const CF_NODES: &str = "jmt_nodes";
/// Stale node index: `stale_since ‖ node key` → empty.
pub(crate) const CF_STALE: &str = "jmt_stale";
/// Value history: `key_hash ‖ !version` → stored value.
pub(crate) const CF_VALUES: &str = "values";
/// Key-values of the latest version: full key → value.
pub(crate) const CF_LATEST: &str = "latest";
/// Keys really changed in each version: `version ‖ full key` → empty.
pub(crate) const CF_CHANGES: &str = "changes";
/// Root of each version: `version` → 32-byte root.
pub(crate) const CF_ROOTS: &str = "roots";

/// All column families needed when opening the database (RocksDB's `default` column family is opened separately and
/// not used).
pub(crate) const COLUMN_FAMILIES: [&str; 7] = [
    CF_META, CF_NODES, CF_STALE, CF_VALUES, CF_LATEST, CF_CHANGES, CF_ROOTS,
];

/// The only key in the `meta` column family.
pub(crate) const META_KEY: &[u8] = b"meta";

/// Storage format version; incremented when the layout changes, and a database in an old format refuses to open.
pub(crate) const FORMAT_VERSION: u8 = 1;

/// A key greater than any valid version prefix (version numbers are < `u64::MAX`), used as the (exclusive) right end
/// of the range "all keys after some version".
pub(crate) const VERSIONS_END: [u8; 8] = u64::MAX.to_be_bytes();

const STORED_TOMBSTONE: u8 = 0x00;
const STORED_VALUE: u8 = 0x01;

/// Available version range `[oldest, latest]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Meta {
    pub(crate) oldest: Version,
    pub(crate) latest: Version,
}

impl Meta {
    /// `format version ‖ oldest ‖ latest`, 17 bytes in total.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(17);
        out.push(FORMAT_VERSION);
        out.extend_from_slice(&self.oldest.to_be_bytes());
        out.extend_from_slice(&self.latest.to_be_bytes());
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, StateError> {
        let corrupted = || StateError::Corrupted("metadata has the wrong length".to_owned());
        let (format, rest) = bytes.split_first().ok_or_else(corrupted)?;
        if *format != FORMAT_VERSION {
            return Err(StateError::Corrupted(format!(
                "unsupported storage format version {format} (this program uses {FORMAT_VERSION})"
            )));
        }
        let (oldest, rest) = rest.split_first_chunk::<8>().ok_or_else(corrupted)?;
        let (latest, rest) = rest.split_first_chunk::<8>().ok_or_else(corrupted)?;
        if !rest.is_empty() {
            return Err(corrupted());
        }
        let meta = Meta {
            oldest: u64::from_be_bytes(*oldest),
            latest: u64::from_be_bytes(*latest),
        };
        if meta.oldest > meta.latest {
            return Err(StateError::Corrupted(format!(
                "oldest {} is greater than latest {} in the metadata",
                meta.oldest, meta.latest
            )));
        }
        Ok(meta)
    }
}

/// Splits off the 8-byte big-endian version number at the start of a key.
pub(crate) fn split_version(bytes: &[u8]) -> Result<(Version, &[u8]), StateError> {
    let (head, rest) = bytes.split_first_chunk::<8>().ok_or_else(|| {
        StateError::Corrupted("key is shorter than the 8-byte version prefix".to_owned())
    })?;
    Ok((u64::from_be_bytes(*head), rest))
}

/// Storage encoding of a JMT node key: `version ‖ nibble count ‖ path`, two nibbles per path byte (high nibble first,
/// padded with 0 at the end for an odd count).
/// The nibble count and the path correspond one to one, so the encoding is injective; the version comes first, so the
/// nodes of one version are contiguous in the key space.
pub(crate) fn node_key(key: &NodeKey) -> Result<Vec<u8>, StateError> {
    let path = key.nibble_path();
    let num_nibbles = u8::try_from(path.num_nibbles()).map_err(|_| {
        StateError::Jmt(format!(
            "node path too long: {} nibbles",
            path.num_nibbles()
        ))
    })?;
    let mut out = Vec::with_capacity(8 + 1 + 32);
    out.extend_from_slice(&key.version().to_be_bytes());
    out.push(num_nibbles);
    let mut high = true;
    for nibble in path.nibbles() {
        let nibble = u8::from(nibble);
        if high {
            out.push(nibble.wrapping_shl(4));
        } else if let Some(last) = out.last_mut() {
            *last |= nibble;
        }
        high = !high;
    }
    Ok(out)
}

/// Storage key of the root node (empty path) of version `version`.
pub(crate) fn root_node_key(version: Version) -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    out.extend_from_slice(&version.to_be_bytes());
    out.push(0);
    out
}

/// Key of the stale index: `stale_since ‖ node storage key`.
pub(crate) fn stale_key(stale_since: Version, node_db_key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(node_db_key.len().saturating_add(8));
    out.extend_from_slice(&stale_since.to_be_bytes());
    out.extend_from_slice(node_db_key);
    out
}

/// Key of the value history: `key_hash ‖ !version` (big-endian). With the version inverted, the history of one key is
/// ordered from newest to oldest version, and the first item found by seeking to `key_hash ‖ !max_version` is the
/// latest value at a version `<= max_version`.
pub(crate) fn value_key(key_hash: &[u8; 32], version: Version) -> Vec<u8> {
    let mut out = Vec::with_capacity(40);
    out.extend_from_slice(key_hash);
    out.extend_from_slice(&(!version).to_be_bytes());
    out
}

/// Stored value in the value history: `0x00` means deleted, `0x01 ‖ value` means written (the value may be empty).
pub(crate) fn encode_stored_value(value: Option<&[u8]>) -> Vec<u8> {
    match value {
        None => vec![STORED_TOMBSTONE],
        Some(value) => {
            let mut out = Vec::with_capacity(value.len().saturating_add(1));
            out.push(STORED_VALUE);
            out.extend_from_slice(value);
            out
        }
    }
}

pub(crate) fn decode_stored_value(bytes: &[u8]) -> Result<Option<Vec<u8>>, StateError> {
    match bytes.split_first() {
        Some((&STORED_TOMBSTONE, [])) => Ok(None),
        Some((&STORED_VALUE, value)) => Ok(Some(value.to_vec())),
        _ => Err(StateError::Corrupted(
            "stored value in the value history has the wrong format".to_owned(),
        )),
    }
}

/// Key of a change record: `version ‖ full key`.
pub(crate) fn change_key(version: Version, key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len().saturating_add(8));
    out.extend_from_slice(&version.to_be_bytes());
    out.extend_from_slice(key);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use jmt::storage::NibblePath;

    /// jmt's `NibblePath::new` is not public; construct it through its only `FromIterator<Nibble>` impl
    /// (`Nibble: From<u8>`).
    pub(crate) fn path(nibbles: &[u8]) -> NibblePath {
        nibbles.iter().map(|&n| n.into()).collect()
    }

    #[test]
    fn meta_roundtrip_and_rejects_garbage() {
        let meta = Meta {
            oldest: 3,
            latest: 0x0102_0304_0506_0708,
        };
        let bytes = meta.encode();
        assert_eq!(bytes.len(), 17);
        assert_eq!(Meta::decode(&bytes).unwrap(), meta);

        let mut wrong_format = bytes.clone();
        wrong_format[0] = FORMAT_VERSION + 1;
        assert!(Meta::decode(&wrong_format).is_err());
        assert!(Meta::decode(&bytes[..16]).is_err());
        let mut too_long = bytes.clone();
        too_long.push(0);
        assert!(Meta::decode(&too_long).is_err());
        let inverted = Meta {
            oldest: 5,
            latest: 4,
        }
        .encode();
        assert!(Meta::decode(&inverted).is_err());
    }

    #[test]
    fn stored_value_roundtrip() {
        for value in [None, Some(&b""[..]), Some(&b"\x00"[..]), Some(&b"abc"[..])] {
            let encoded = encode_stored_value(value);
            assert_eq!(decode_stored_value(&encoded).unwrap().as_deref(), value);
        }
        assert!(decode_stored_value(b"").is_err());
        assert!(decode_stored_value(b"\x00x").is_err());
        assert!(decode_stored_value(b"\x02").is_err());
    }

    #[test]
    fn value_keys_sort_newest_first() {
        let kh = [7u8; 32];
        let v1 = value_key(&kh, 1);
        let v2 = value_key(&kh, 2);
        let v_max = value_key(&kh, u64::MAX - 1);
        assert!(v_max < v2 && v2 < v1);
        assert_eq!(&v1[..32], &kh);
        assert_eq!(v1.len(), 40);
    }

    #[test]
    fn version_prefixes() {
        let key = change_key(0x0a0b, b"xyz");
        assert_eq!(split_version(&key).unwrap(), (0x0a0b, &b"xyz"[..]));
        assert!(split_version(b"short").is_err());
        assert_eq!(root_node_key(9), [0, 0, 0, 0, 0, 0, 0, 9, 0]);
        assert_eq!(stale_key(1, &[0xaa]), [0, 0, 0, 0, 0, 0, 0, 1, 0xaa]);
        assert!(VERSIONS_END.as_slice() > root_node_key(u64::MAX - 1).as_slice());
    }

    #[test]
    fn node_key_encoding() {
        let root = NodeKey::new(5, path(&[]));
        assert_eq!(node_key(&root).unwrap(), root_node_key(5));

        let odd = NodeKey::new(0x0102, path(&[0xa, 0xb, 0xc]));
        assert_eq!(
            node_key(&odd).unwrap(),
            [0, 0, 0, 0, 0, 0, 1, 2, 3, 0xab, 0xc0]
        );
        let even = NodeKey::new(1, path(&[0xa, 0xb]));
        assert_eq!(node_key(&even).unwrap(), [0, 0, 0, 0, 0, 0, 0, 1, 2, 0xab]);
        // Paths [a, 0] and [a] pack to the same bytes; they are told apart by the nibble count.
        let trailing_zero = NodeKey::new(1, path(&[0xa, 0]));
        let single = NodeKey::new(1, path(&[0xa]));
        assert_ne!(
            node_key(&trailing_zero).unwrap(),
            node_key(&single).unwrap()
        );

        let full = NodeKey::new(u64::MAX, path(&[0xf; 64]));
        let encoded = node_key(&full).unwrap();
        assert_eq!(encoded.len(), 8 + 1 + 32);
        assert_eq!(encoded[8], 64);
        assert!(encoded[9..].iter().all(|&b| b == 0xff));
    }
}
