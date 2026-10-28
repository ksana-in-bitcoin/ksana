//! Encoding and decoding.
//!
//! - Encoding always uses Borsh ([`encode`]): Borsh serialization does not involve `core::fmt`.
//! - Decoding uses this module's [`Decode`]: the wire format is byte-for-byte the same as Borsh, but on error it only
//!   returns `None` and formats no error message. Borsh's own deserializers for `String` / `bool` / `Option` /
//!   `Result` / enums build error messages with `format!`, linking `core::fmt` into the contract; since D-073 this no
//!   longer gets the deployment rejected, but it makes the contract noticeably larger.

use alloc::string::String;
use alloc::vec::Vec;

use borsh::BorshSerialize;

use crate::read::{take, take_array};

/// Return value of a contract method: `Ok(data)` is the result, `Err(reason)` an explicit revert (ABI §2).
pub type CallResult = Result<Vec<u8>, Vec<u8>>;

/// Revert reason when [`decode`] fails.
pub const ERR_MALFORMED_ARGS: &[u8] = b"malformed arguments";

/// Decoding consistent with the Borsh wire format (without using `core::fmt`).
///
/// Implemented for: integers (little-endian), `bool` (only 0 / 1 accepted), `[u8; N]`, `Vec<T>` (u32 length prefix),
/// `String` (UTF-8), `Option<T>`, `Result<T, E>`, `()`, tuples of up to 8 elements, and [`crate::AssetMetaInput`].
/// Structs can implement it by hand in field order, e.g. `Some(Self { a: A::decode(input)?, b: B::decode(input)? })`.
pub trait Decode: Sized {
    /// Decodes one value from the head of `input` and advances `input`; returns `None` on a format error.
    fn decode(input: &mut &[u8]) -> Option<Self>;

    /// Decodes a `Vec` of `len` elements (the `Vec<T>` implementation calls it; `u8` overrides it with a whole-slice
    /// copy).
    ///
    /// Like Borsh, rejects collections of zero-sized types. Every element of a non-zero-sized type takes at least
    /// 1 byte, so it fails right away when `len` exceeds the remaining bytes, and the pre-allocation is capped too, so
    /// a malicious length prefix cannot cause a large allocation.
    #[doc(hidden)]
    fn decode_vec(len: usize, input: &mut &[u8]) -> Option<Vec<Self>> {
        let size = core::mem::size_of::<Self>();
        if size == 0 || len > input.len() {
            return None;
        }
        // Same as borsh's `hint::cautious`: pre-allocate at most 4 KiB.
        let cap = len.min(4096usize.checked_div(size).unwrap_or(0));
        let mut out = Vec::with_capacity(cap);
        for _ in 0..len {
            out.push(Self::decode(input)?);
        }
        Some(out)
    }
}

/// Fully decodes `bytes` as `T` (trailing bytes also count as failure); returns `Err(ERR_MALFORMED_ARGS)` on failure,
/// which can be propagated directly as a revert with `?`.
pub fn decode<T: Decode>(bytes: &[u8]) -> Result<T, Vec<u8>> {
    let mut input = bytes;
    match T::decode(&mut input) {
        Some(value) if input.is_empty() => Ok(value),
        _ => Err(ERR_MALFORMED_ARGS.to_vec()),
    }
}

/// Borsh encoding. Only fails when a collection length exceeds u32 (impossible with linear memory capped at 16 MiB),
/// in which case it traps.
pub fn encode<T: BorshSerialize + ?Sized>(value: &T) -> Vec<u8> {
    let mut out = Vec::new();
    if value.serialize(&mut out).is_err() {
        crate::trap();
    }
    out
}

/// `Ok(encode(value))`: the method succeeds and returns the Borsh-encoded `value`.
pub fn ok<T: BorshSerialize + ?Sized>(value: &T) -> CallResult {
    Ok(encode(value))
}

/// `Err(reason)`: an explicit revert; all state changes of this op are rolled back (ABI §2).
pub fn revert<T>(reason: &str) -> Result<T, Vec<u8>> {
    Err(reason.as_bytes().to_vec())
}

/// Decodes the entry input: Borsh-encoded `(method: String, args: Vec<u8>)`, without copying or allocating.
pub(crate) fn decode_call_input(input: &[u8]) -> Option<(&str, &[u8])> {
    let mut rest = input;
    let method_len = usize::try_from(u32::from_le_bytes(take_array(&mut rest)?)).ok()?;
    let method = core::str::from_utf8(take(&mut rest, method_len)?).ok()?;
    let args_len = usize::try_from(u32::from_le_bytes(take_array(&mut rest)?)).ok()?;
    let args = take(&mut rest, args_len)?;
    rest.is_empty().then_some((method, args))
}

macro_rules! impl_decode_int {
    ($($ty:ty),*) => {$(
        impl Decode for $ty {
            fn decode(input: &mut &[u8]) -> Option<Self> {
                Some(<$ty>::from_le_bytes(take_array(input)?))
            }
        }
    )*};
}

impl_decode_int!(u16, u32, u64, u128, i8, i16, i32, i64, i128);

impl Decode for u8 {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        let [b] = take_array::<1>(input)?;
        Some(b)
    }

    fn decode_vec(len: usize, input: &mut &[u8]) -> Option<Vec<Self>> {
        Some(take(input, len)?.to_vec())
    }
}

impl Decode for bool {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        match take_array::<1>(input)? {
            [0] => Some(false),
            [1] => Some(true),
            _ => None,
        }
    }
}

impl<const N: usize> Decode for [u8; N] {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        take_array(input)
    }
}

impl<T: Decode> Decode for Vec<T> {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        let len = usize::try_from(u32::decode(input)?).ok()?;
        T::decode_vec(len, input)
    }
}

impl Decode for String {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        let len = usize::try_from(u32::decode(input)?).ok()?;
        let bytes = take(input, len)?;
        core::str::from_utf8(bytes).ok().map(String::from)
    }
}

impl<T: Decode> Decode for Option<T> {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        match u8::decode(input)? {
            0 => Some(None),
            1 => Some(Some(T::decode(input)?)),
            _ => None,
        }
    }
}

impl<T: Decode, E: Decode> Decode for Result<T, E> {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        // Borsh: 0 = Err, 1 = Ok.
        match u8::decode(input)? {
            0 => Some(Err(E::decode(input)?)),
            1 => Some(Ok(T::decode(input)?)),
            _ => None,
        }
    }
}

impl Decode for () {
    fn decode(_input: &mut &[u8]) -> Option<Self> {
        Some(())
    }
}

macro_rules! impl_decode_tuple {
    ($($name:ident),+) => {
        impl<$($name: Decode),+> Decode for ($($name,)+) {
            fn decode(input: &mut &[u8]) -> Option<Self> {
                Some(($($name::decode(input)?,)+))
            }
        }
    };
}

impl_decode_tuple!(A);
impl_decode_tuple!(A, B);
impl_decode_tuple!(A, B, C);
impl_decode_tuple!(A, B, C, D);
impl_decode_tuple!(A, B, C, D, E);
impl_decode_tuple!(A, B, C, D, E, F);
impl_decode_tuple!(A, B, C, D, E, F, G);
impl_decode_tuple!(A, B, C, D, E, F, G, H);

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    #[test]
    fn call_input_matches_borsh() {
        let input = borsh::to_vec(&("transfer".to_string(), vec![1u8, 2, 3])).unwrap();
        assert_eq!(
            decode_call_input(&input),
            Some(("transfer", &[1u8, 2, 3][..]))
        );
        // The way the SDK itself builds call input (&str, &[u8]) encodes the same as (String, Vec<u8>).
        assert_eq!(encode(&("transfer", &[1u8, 2, 3][..])), input);
        // Trailing bytes, truncation and a non-UTF-8 method name are all rejected.
        let mut long = input.clone();
        long.push(0);
        assert_eq!(decode_call_input(&long), None);
        assert_eq!(decode_call_input(&input[..input.len() - 1]), None);
        let bad = borsh::to_vec(&(vec![0xffu8], Vec::<u8>::new())).unwrap();
        assert_eq!(decode_call_input(&bad), None);
    }

    #[test]
    fn decode_requires_full_consumption() {
        assert_eq!(decode::<u32>(&[1, 0, 0, 0]), Ok(1));
        assert_eq!(
            decode::<u32>(&[1, 0, 0, 0, 0]),
            Err(ERR_MALFORMED_ARGS.to_vec())
        );
        assert_eq!(decode::<u32>(&[1, 0, 0]), Err(ERR_MALFORMED_ARGS.to_vec()));
        assert_eq!(decode::<()>(&[]), Ok(()));
    }

    #[test]
    fn rejects_non_canonical_tags() {
        assert_eq!(decode::<bool>(&[2]), Err(ERR_MALFORMED_ARGS.to_vec()));
        assert_eq!(
            decode::<Option<u8>>(&[2, 0]),
            Err(ERR_MALFORMED_ARGS.to_vec())
        );
        assert_eq!(
            decode::<Result<u8, u8>>(&[2, 0]),
            Err(ERR_MALFORMED_ARGS.to_vec())
        );
    }

    #[test]
    fn rejects_zero_sized_collections_like_borsh() {
        let empty = borsh::to_vec(&0u32).unwrap();
        assert!(decode::<Vec<()>>(&empty).is_err());
        assert!(borsh::from_slice::<Vec<()>>(&empty).is_err());
    }

    #[test]
    fn huge_length_prefix_fails_without_allocating() {
        // Declares u32::MAX elements of [u8; 32] but actually has only 32 bytes.
        let mut data = u32::MAX.to_le_bytes().to_vec();
        data.extend_from_slice(&[7u8; 32]);
        assert!(decode::<Vec<[u8; 32]>>(&data).is_err());
        assert!(decode::<Vec<u8>>(&data).is_err());
        assert!(decode::<String>(&data).is_err());
    }
}
