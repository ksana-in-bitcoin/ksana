//! BIP340 tagged hash.

use sha2::{Digest, Sha256};

/// BIP340 tagged hash: `sha256(sha256(tag) ‖ sha256(tag) ‖ msg)`.
pub fn tagged_hash(tag: &str, msg: &[u8]) -> [u8; 32] {
    let tag_hash = Sha256::digest(tag.as_bytes());
    let mut hasher = Sha256::new();
    hasher.update(tag_hash);
    hasher.update(tag_hash);
    hasher.update(msg);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values computed by an independent implementation (Python hashlib).
    #[test]
    fn known_answers() {
        assert_eq!(
            hex::encode(tagged_hash("Ksana/VmOp/v1", b"")),
            "9a283d37c8d8c48783de8c1f23e529bfca082d61749ff2567656ef7601eb0625"
        );
        assert_eq!(
            hex::encode(tagged_hash("Ksana/VmOp/v1", b"abc")),
            "512ba59e1192612305608d3ec891ca39e6d13480becdb967fc427b377dee1b38"
        );
        assert_eq!(
            hex::encode(tagged_hash("BIP0340/challenge", &[0u8; 32])),
            "a50885aadef94ee57e5537e27ef82d4db7c756193539d3d8d0bb6ee5f3a7ad46"
        );
    }
}
