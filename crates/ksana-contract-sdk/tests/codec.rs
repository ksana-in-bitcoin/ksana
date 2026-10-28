//! `Decode` is consistent with the Borsh wire format: encoding round-trips, and for arbitrary bytes the acceptance /
//! rejection is exactly the same as `borsh::from_slice`.
#![cfg(not(target_arch = "wasm32"))]

use borsh::{BorshDeserialize, BorshSerialize};
use ksana_contract_sdk::{decode, encode, AssetMetaInput, Decode, ERR_MALFORMED_ARGS};
use proptest::prelude::*;

/// For the same bytes, `decode` and `borsh::from_slice` either both succeed with the same result or both fail.
fn agrees<T>(bytes: &[u8]) -> Result<(), TestCaseError>
where
    T: Decode + BorshDeserialize + PartialEq + std::fmt::Debug,
{
    let ours = decode::<T>(bytes).ok();
    let theirs = borsh::from_slice::<T>(bytes).ok();
    prop_assert_eq!(ours, theirs);
    Ok(())
}

fn round_trips<T>(value: &T) -> Result<(), TestCaseError>
where
    T: Decode + BorshSerialize + PartialEq + std::fmt::Debug,
{
    let bytes = borsh::to_vec(value).unwrap();
    prop_assert_eq!(&encode(value), &bytes);
    prop_assert_eq!(&decode::<T>(&bytes).unwrap(), value);
    Ok(())
}

proptest! {
    #[test]
    fn round_trip_primitives(
        a in any::<u8>(), b in any::<u16>(), c in any::<u32>(), d in any::<u64>(), e in any::<u128>(),
        f in any::<i8>(), g in any::<i64>(), h in any::<i128>(), flag in any::<bool>(),
    ) {
        round_trips(&a)?;
        round_trips(&b)?;
        round_trips(&c)?;
        round_trips(&d)?;
        round_trips(&e)?;
        round_trips(&f)?;
        round_trips(&g)?;
        round_trips(&h)?;
        round_trips(&flag)?;
        round_trips(&(a, b, c, d, e, f, g, h))?;
    }

    #[test]
    fn round_trip_collections(
        bytes in proptest::collection::vec(any::<u8>(), 0..300),
        hashes in proptest::collection::vec(any::<[u8; 32]>(), 0..10),
        text in ".{0,40}",
        texts in proptest::collection::vec(".{0,8}", 0..5),
        maybe in proptest::option::of(any::<u64>()),
        nested in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..5), 0..5),
    ) {
        round_trips(&bytes)?;
        round_trips(&hashes)?;
        round_trips(&text)?;
        round_trips(&texts)?;
        round_trips(&maybe)?;
        round_trips(&nested)?;
        round_trips(&(text.clone(), bytes.clone()))?;
        round_trips(&(text.clone(), text.clone(), 7u8, u128::MAX))?;
        let ok: Result<Vec<u8>, String> = Ok(bytes.clone());
        let err: Result<Vec<u8>, String> = Err(text.clone());
        round_trips(&ok)?;
        round_trips(&err)?;
    }

    #[test]
    fn asset_meta_round_trip(name in ".{0,40}", symbol in ".{0,10}", decimals in any::<u8>()) {
        let meta = AssetMetaInput { name: name.clone(), symbol: symbol.clone(), decimals };
        let bytes = encode(&meta);
        prop_assert_eq!(&bytes, &borsh::to_vec(&(name.clone(), symbol.clone(), decimals)).unwrap());
        prop_assert_eq!(decode::<AssetMetaInput>(&bytes).unwrap(), meta);
    }

    #[test]
    fn same_acceptance_as_borsh(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        agrees::<u8>(&bytes)?;
        agrees::<u32>(&bytes)?;
        agrees::<u128>(&bytes)?;
        agrees::<bool>(&bytes)?;
        agrees::<[u8; 4]>(&bytes)?;
        agrees::<Vec<u8>>(&bytes)?;
        agrees::<String>(&bytes)?;
        agrees::<Option<u16>>(&bytes)?;
        agrees::<Vec<[u8; 2]>>(&bytes)?;
        agrees::<Vec<String>>(&bytes)?;
        agrees::<(u8, bool, Option<u8>)>(&bytes)?;
        agrees::<Result<u8, u16>>(&bytes)?;
        agrees::<()>(&bytes)?;
    }

    /// Byte strings with a valid length prefix and random content (mostly non-UTF-8): String acceptance rules match
    /// Borsh.
    #[test]
    fn same_utf8_rules_as_borsh(content in proptest::collection::vec(any::<u8>(), 0..16)) {
        let bytes = borsh::to_vec(&content).unwrap();
        agrees::<String>(&bytes)?;
    }
}

#[test]
fn decode_error_is_revert_reason() {
    assert_eq!(decode::<u8>(&[]), Err(ERR_MALFORMED_ARGS.to_vec()));
}
