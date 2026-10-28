//! Verify this crate's Schnorr signing / verification with the official BIP340 test vectors.
//!
//! `tests/data/bip340-test-vectors.csv` is taken verbatim from
//! <https://github.com/bitcoin/bips/blob/master/bip-0340/test-vectors.csv>
//! (sha256 34c9d1d9c3a88d524bc80778540dc43f8306ec249a7485293063c376db851c2d).
//! Rows 15–18 are the non-32-byte messages added in 2022; Ksana only signs 32-byte digests (VmOp signature hash,
//! host function `verify_schnorr`), so those rows do not apply, and the test asserts that exactly 15 rows apply.

use ksana_crypto::{account_of, keypair_from_secret, schnorr_sign, schnorr_verify};

struct Row {
    index: String,
    secret_key: Option<[u8; 32]>,
    public_key: Vec<u8>,
    aux_rand: Option<[u8; 32]>,
    message: Vec<u8>,
    signature: [u8; 64],
    valid: bool,
}

fn rows() -> Vec<Row> {
    let csv = include_str!("data/bip340-test-vectors.csv");
    csv.lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let cols: Vec<&str> = line.split(',').collect();
            let opt32 = |s: &str| -> Option<[u8; 32]> {
                (!s.is_empty()).then(|| hex::decode(s).unwrap().try_into().unwrap())
            };
            Row {
                index: cols[0].to_string(),
                secret_key: opt32(cols[1]),
                public_key: hex::decode(cols[2]).unwrap(),
                aux_rand: opt32(cols[3]),
                message: hex::decode(cols[4]).unwrap(),
                signature: hex::decode(cols[5]).unwrap().try_into().unwrap(),
                valid: match cols[6] {
                    "TRUE" => true,
                    "FALSE" => false,
                    other => panic!("unknown verification result {other}"),
                },
            }
        })
        .collect()
}

#[test]
fn official_bip340_vectors() {
    let all = rows();
    assert_eq!(all.len(), 19);
    let applicable: Vec<&Row> = all.iter().filter(|r| r.message.len() == 32).collect();
    assert_eq!(applicable.len(), 15);

    for row in applicable {
        let digest: [u8; 32] = row.message.clone().try_into().unwrap();
        if let (Some(sk), Some(aux)) = (row.secret_key, row.aux_rand) {
            let keypair = keypair_from_secret(&sk).unwrap();
            // The public key of the official vectors is the untweaked x-only public key; the account is its
            // BIP86-tweaked output public key (D-092), and the two differ
            assert_eq!(
                keypair.x_only_public_key().0.serialize().to_vec(),
                row.public_key,
                "vector {}: public key",
                row.index
            );
            assert_ne!(account_of(&keypair).to_vec(), row.public_key);
            assert_eq!(
                schnorr_sign(&digest, &keypair, &aux),
                row.signature,
                "vector {}: signature",
                row.index
            );
        }
        let verified = match <[u8; 32]>::try_from(row.public_key.as_slice()) {
            Ok(pk) => schnorr_verify(&pk, &digest, &row.signature).is_ok(),
            Err(_) => false,
        };
        assert_eq!(
            verified, row.valid,
            "vector {}: verification result",
            row.index
        );
    }
}

#[test]
fn invalid_secret_keys_are_rejected() {
    assert!(keypair_from_secret(&[0u8; 32]).is_err());
    // The curve order n itself is not a valid secret key.
    let n =
        hex::decode("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141").unwrap();
    assert!(keypair_from_secret(&n.try_into().unwrap()).is_err());
}
