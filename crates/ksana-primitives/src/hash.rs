//! Hash functions.

use sha2::{Digest, Sha256};

/// SHA-256.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// Double SHA-256: `sha256(sha256(data))`.
pub fn sha256d(data: &[u8]) -> [u8; 32] {
    sha256(&sha256(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values computed by an independent implementation (Python hashlib).
    #[test]
    fn sha256_known_answers() {
        assert_eq!(
            hex::encode(sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex::encode(sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256d_known_answers() {
        assert_eq!(
            hex::encode(sha256d(b"")),
            "5df6e0e2761359d30a8275058e299fcc0381534545f55cf43e41983f5d4c9456"
        );
        assert_eq!(
            hex::encode(sha256d(b"abc")),
            "4f8b42c22dd3729b519ba6f68d2da7cc5b2d606d05daed5ad5128cc03e6c6358"
        );
    }
}
