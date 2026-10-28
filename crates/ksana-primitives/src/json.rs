//! The JSON representation of the types (serde), used for the shared test vectors and RPC; not part of consensus.
//!
//! - Byte strings (`[u8; N]`, `Vec<u8>`): lowercase hex strings;
//! - `u64` / `u128`: decimal strings (JavaScript numbers only have 53 bits of precision);
//! - `u8` / `u32`: JSON numbers;
//! - Enums: serde's default externally tagged form, e.g. `{"Token":{"amount":"5"}}`, `"BtcNative"`.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serializer};

fn parse_dec<T: std::str::FromStr>(s: &str) -> Result<T, String> {
    // Only the canonical spelling is accepted: digits only, no sign, no redundant leading zeros.
    let canonical =
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && (s == "0" || !s.starts_with('0'));
    if !canonical {
        return Err(format!("non-canonical decimal integer: {s:?}"));
    }
    s.parse::<T>()
        .map_err(|_| format!("integer out of range: {s:?}"))
}

pub mod hex_array {
    use super::*;

    pub fn serialize<S: Serializer, const N: usize>(v: &[u8; N], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
        d: D,
    ) -> Result<[u8; N], D::Error> {
        let s = String::deserialize(d)?;
        let mut out = [0u8; N];
        hex::decode_to_slice(&s, &mut out).map_err(D::Error::custom)?;
        Ok(out)
    }
}

pub mod hex_array_opt {
    use super::*;

    pub fn serialize<S: Serializer, const N: usize>(
        v: &Option<[u8; N]>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match v {
            Some(bytes) => s.serialize_some(&hex::encode(bytes)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
        d: D,
    ) -> Result<Option<[u8; N]>, D::Error> {
        match Option::<String>::deserialize(d)? {
            Some(s) => {
                let mut out = [0u8; N];
                hex::decode_to_slice(&s, &mut out).map_err(D::Error::custom)?;
                Ok(Some(out))
            }
            None => Ok(None),
        }
    }
}

pub mod hex_array_vec {
    use super::*;

    pub fn serialize<S: Serializer>(v: &[[u8; 32]], s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(v.iter().map(hex::encode))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<[u8; 32]>, D::Error> {
        let items = Vec::<String>::deserialize(d)?;
        items
            .iter()
            .map(|s| {
                let mut out = [0u8; 32];
                hex::decode_to_slice(s, &mut out).map_err(D::Error::custom)?;
                Ok(out)
            })
            .collect()
    }
}

pub mod hex_bytes {
    use super::*;

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        hex::decode(&s).map_err(D::Error::custom)
    }
}

pub mod hex_bytes_vec {
    use super::*;

    pub fn serialize<S: Serializer>(v: &[Vec<u8>], s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(v.iter().map(hex::encode))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Vec<u8>>, D::Error> {
        let items = Vec::<String>::deserialize(d)?;
        items
            .iter()
            .map(|s| hex::decode(s).map_err(D::Error::custom))
            .collect()
    }
}

pub mod hex_bytes_opt {
    use super::*;

    pub fn serialize<S: Serializer>(v: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(bytes) => s.serialize_some(&hex::encode(bytes)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        match Option::<String>::deserialize(d)? {
            Some(s) => hex::decode(&s).map(Some).map_err(D::Error::custom),
            None => Ok(None),
        }
    }
}

pub mod dec_u64 {
    use super::*;

    pub fn serialize<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        let s = String::deserialize(d)?;
        parse_dec(&s).map_err(D::Error::custom)
    }
}

pub mod dec_u128 {
    use super::*;

    pub fn serialize<S: Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        let s = String::deserialize(d)?;
        parse_dec(&s).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::parse_dec;

    #[test]
    fn decimal_strings_must_be_canonical() {
        assert_eq!(parse_dec::<u64>("0"), Ok(0));
        assert_eq!(parse_dec::<u64>("18446744073709551615"), Ok(u64::MAX));
        assert_eq!(
            parse_dec::<u128>("340282366920938463463374607431768211455"),
            Ok(u128::MAX)
        );
        for bad in ["", "+1", "-1", "01", "00", " 1", "1 ", "1e3", "0x10", "1.0"] {
            assert!(parse_dec::<u64>(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(parse_dec::<u64>("18446744073709551616").is_err());
    }
}
