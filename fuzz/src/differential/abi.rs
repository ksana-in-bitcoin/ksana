//! The Rust side of the playground's ABI codec (`playground/src/abi/codec.ts`, D-119): the types an `abi` block can
//! declare, each as a concrete Rust type decoded with the contract SDK's [`Decode`] (what a contract decodes its
//! arguments with), and the value in the form the oracle writes the codec's `Value` in: integers as decimal strings,
//! byte strings in hex, `Option` as `null` or the value, tuples and `Vec` as arrays, a struct as an object by field
//! name, an enum as `{"variant", "values"}`.
//!
//! A struct or an enum of an `abi` block is a type of the contract's own, decoded the way contracts write it by hand:
//! a struct field by field, an enum by a `u8` tag in declaration order. The table includes the types that take no
//! bytes in Rust (`()`, an empty struct, an enum of one variant without fields): Borsh, and the contract SDK, refuse a
//! `Vec` of them whatever its length.

use ksana_contract_sdk::Decode;
use serde_json::{json, Value};

/// How a decoded value is written for the comparison.
trait Render {
    fn render(&self) -> Value;
}

macro_rules! render_int {
    ($($ty:ty),*) => {$(
        impl Render for $ty {
            fn render(&self) -> Value {
                Value::from(self.to_string())
            }
        }
    )*};
}

render_int!(u8, u16, u32, u64, u128, i8, i16, i32, i64, i128);

impl Render for bool {
    fn render(&self) -> Value {
        Value::from(*self)
    }
}

impl Render for String {
    fn render(&self) -> Value {
        Value::from(self.as_str())
    }
}

impl<const N: usize> Render for [u8; N] {
    fn render(&self) -> Value {
        Value::from(hex::encode(self))
    }
}

impl<T: Render> Render for Option<T> {
    fn render(&self) -> Value {
        self.as_ref().map_or(Value::Null, Render::render)
    }
}

impl<T: Render> Render for Vec<T> {
    fn render(&self) -> Value {
        Value::Array(self.iter().map(Render::render).collect())
    }
}

impl Render for () {
    fn render(&self) -> Value {
        json!([])
    }
}

macro_rules! render_tuple {
    ($($name:ident),+) => {
        impl<$($name: Render),+> Render for ($($name,)+) {
            #[allow(non_snake_case)]
            fn render(&self) -> Value {
                let ($($name,)+) = self;
                json!([$($name.render()),+])
            }
        }
    };
}

render_tuple!(A);
render_tuple!(A, B);
render_tuple!(A, B, C);

/// `Bytes` (`Vec<u8>`): a byte string, written in hex.
struct Bytes(Vec<u8>);

impl Decode for Bytes {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Vec::<u8>::decode(input).map(Bytes)
    }
}

impl Render for Bytes {
    fn render(&self) -> Value {
        Value::from(hex::encode(&self.0))
    }
}

/// `struct Pair { a: u8, b: String }`.
struct Pair {
    a: u8,
    b: String,
}

impl Decode for Pair {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        Some(Pair {
            a: Decode::decode(input)?,
            b: Decode::decode(input)?,
        })
    }
}

impl Render for Pair {
    fn render(&self) -> Value {
        json!({ "a": self.a.render(), "b": self.b.render() })
    }
}

/// `struct Unit {}`: no fields, no bytes.
struct Unit;

impl Decode for Unit {
    fn decode(_input: &mut &[u8]) -> Option<Self> {
        Some(Unit)
    }
}

impl Render for Unit {
    fn render(&self) -> Value {
        json!({})
    }
}

fn variant(name: &str, values: Vec<Value>) -> Value {
    json!({ "variant": name, "values": values })
}

/// `enum Side { Token(ContractId), Asset(AssetId) }` (the amm example's side).
enum Side {
    Token([u8; 32]),
    Asset([u8; 32]),
}

impl Decode for Side {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        match u8::decode(input)? {
            0 => Some(Side::Token(Decode::decode(input)?)),
            1 => Some(Side::Asset(Decode::decode(input)?)),
            _ => None,
        }
    }
}

impl Render for Side {
    fn render(&self) -> Value {
        match self {
            Side::Token(id) => variant("Token", vec![id.render()]),
            Side::Asset(id) => variant("Asset", vec![id.render()]),
        }
    }
}

/// `enum One { A }`: one variant without fields, no bytes in Rust (its Borsh tag is still a byte).
enum One {
    A,
}

impl Decode for One {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        match u8::decode(input)? {
            0 => Some(One::A),
            _ => None,
        }
    }
}

impl Render for One {
    fn render(&self) -> Value {
        match self {
            One::A => variant("A", vec![]),
        }
    }
}

/// `enum Mixed { A, B(u32, String), C(Pair) }`.
enum Mixed {
    A,
    B(u32, String),
    C(Pair),
}

impl Decode for Mixed {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        match u8::decode(input)? {
            0 => Some(Mixed::A),
            1 => Some(Mixed::B(Decode::decode(input)?, Decode::decode(input)?)),
            2 => Some(Mixed::C(Decode::decode(input)?)),
            _ => None,
        }
    }
}

impl Render for Mixed {
    fn render(&self) -> Value {
        match self {
            Mixed::A => variant("A", vec![]),
            Mixed::B(n, s) => variant("B", vec![n.render(), s.render()]),
            Mixed::C(p) => variant("C", vec![p.render()]),
        }
    }
}

/// The structs and enums the table's types use, as the playground's `Abi` holds them.
pub fn abi_defs() -> Value {
    let id = |name: &str| json!({ "kind": "id", "name": name });
    json!({
        "structs": {
            "Pair": { "name": "Pair", "fields": [
                { "name": "a", "type": { "kind": "int", "name": "u8" } },
                { "name": "b", "type": { "kind": "string" } },
            ] },
            "Unit": { "name": "Unit", "fields": [] },
        },
        "enums": {
            "Side": { "name": "Side", "variants": [
                { "name": "Token", "fields": [id("ContractId")] },
                { "name": "Asset", "fields": [id("AssetId")] },
            ] },
            "One": { "name": "One", "variants": [{ "name": "A", "fields": [] }] },
            "Mixed": { "name": "Mixed", "variants": [
                { "name": "A", "fields": [] },
                { "name": "B", "fields": [{ "kind": "int", "name": "u32" }, { "kind": "string" }] },
                { "name": "C", "fields": [{ "kind": "struct", "name": "Pair" }] },
            ] },
        },
    })
}

/// One type of the table.
pub struct AbiType {
    /// The type as an `abi` block writes it.
    pub text: &'static str,
    /// The type as the playground's parser makes it (`AbiType` in `playground/src/abi/types.ts`), as JSON.
    abi: &'static str,
    /// Decodes all of the bytes as the type with the contract SDK; `None` when it refuses them.
    decode: fn(&[u8]) -> Option<Value>,
}

impl AbiType {
    /// The playground's `AbiType` of this type.
    pub fn abi_json(&self) -> Value {
        serde_json::from_str(self.abi).unwrap_or(Value::Null)
    }

    /// The contract SDK's reading of `bytes` as this type.
    pub fn decode(&self, bytes: &[u8]) -> Option<Value> {
        (self.decode)(bytes)
    }
}

fn decode<T: Decode + Render>(bytes: &[u8]) -> Option<Value> {
    ksana_contract_sdk::decode::<T>(bytes)
        .ok()
        .map(|v| v.render())
}

macro_rules! abi_types {
    ($(($text:literal, $abi:literal, $ty:ty)),* $(,)?) => {
        &[$(AbiType { text: $text, abi: $abi, decode: decode::<$ty> }),*]
    };
}

/// The types of an `abi` block the test decodes, with the Rust type each stands for.
pub const ABI_TYPES: &[AbiType] = abi_types![
    ("u8", r#"{"kind":"int","name":"u8"}"#, u8),
    ("u16", r#"{"kind":"int","name":"u16"}"#, u16),
    ("u32", r#"{"kind":"int","name":"u32"}"#, u32),
    ("u64", r#"{"kind":"int","name":"u64"}"#, u64),
    ("u128", r#"{"kind":"int","name":"u128"}"#, u128),
    ("i8", r#"{"kind":"int","name":"i8"}"#, i8),
    ("i16", r#"{"kind":"int","name":"i16"}"#, i16),
    ("i32", r#"{"kind":"int","name":"i32"}"#, i32),
    ("i64", r#"{"kind":"int","name":"i64"}"#, i64),
    ("i128", r#"{"kind":"int","name":"i128"}"#, i128),
    ("bool", r#"{"kind":"bool"}"#, bool),
    ("String", r#"{"kind":"string"}"#, String),
    ("Bytes", r#"{"kind":"bytes"}"#, Bytes),
    ("[u8; 1]", r#"{"kind":"fixed","len":1}"#, [u8; 1]),
    ("[u8; 33]", r#"{"kind":"fixed","len":33}"#, [u8; 33]),
    ("Account", r#"{"kind":"id","name":"Account"}"#, [u8; 32]),
    ("TokenId", r#"{"kind":"id","name":"TokenId"}"#, [u8; 32]),
    ("Option<u64>", r#"{"kind":"option","inner":{"kind":"int","name":"u64"}}"#, Option<u64>),
    ("Option<String>", r#"{"kind":"option","inner":{"kind":"string"}}"#, Option<String>),
    ("Option<Account>", r#"{"kind":"option","inner":{"kind":"id","name":"Account"}}"#, Option<[u8; 32]>),
    // An option inside an option, through a tuple: the parser refuses `Option<Option<T>>` (D-133)
    (
        "Option<(Option<bool>,)>",
        r#"{"kind":"option","inner":{"kind":"tuple","items":[{"kind":"option","inner":{"kind":"bool"}}]}}"#,
        Option<(Option<bool>,)>
    ),
    ("Vec<String>", r#"{"kind":"vec","inner":{"kind":"string"}}"#, Vec<String>),
    ("Vec<Account>", r#"{"kind":"vec","inner":{"kind":"id","name":"Account"}}"#, Vec<[u8; 32]>),
    ("Vec<Bytes>", r#"{"kind":"vec","inner":{"kind":"bytes"}}"#, Vec<Bytes>),
    ("Vec<Option<u8>>", r#"{"kind":"vec","inner":{"kind":"option","inner":{"kind":"int","name":"u8"}}}"#, Vec<Option<u8>>),
    ("Vec<bool>", r#"{"kind":"vec","inner":{"kind":"bool"}}"#, Vec<bool>),
    ("Vec<u128>", r#"{"kind":"vec","inner":{"kind":"int","name":"u128"}}"#, Vec<u128>),
    ("Vec<i8>", r#"{"kind":"vec","inner":{"kind":"int","name":"i8"}}"#, Vec<i8>),
    ("Vec<Vec<String>>", r#"{"kind":"vec","inner":{"kind":"vec","inner":{"kind":"string"}}}"#, Vec<Vec<String>>),
    ("(u8, String)", r#"{"kind":"tuple","items":[{"kind":"int","name":"u8"},{"kind":"string"}]}"#, (u8, String)),
    (
        "(bool, Option<Account>, u128)",
        r#"{"kind":"tuple","items":[{"kind":"bool"},{"kind":"option","inner":{"kind":"id","name":"Account"}},{"kind":"int","name":"u128"}]}"#,
        (bool, Option<[u8; 32]>, u128)
    ),
    ("()", r#"{"kind":"tuple","items":[]}"#, ()),
    ("Vec<()>", r#"{"kind":"vec","inner":{"kind":"tuple","items":[]}}"#, Vec<()>),
    ("Vec<((),)>", r#"{"kind":"vec","inner":{"kind":"tuple","items":[{"kind":"tuple","items":[]}]}}"#, Vec<((),)>),
    ("Pair", r#"{"kind":"struct","name":"Pair"}"#, Pair),
    ("Vec<Pair>", r#"{"kind":"vec","inner":{"kind":"struct","name":"Pair"}}"#, Vec<Pair>),
    ("Unit", r#"{"kind":"struct","name":"Unit"}"#, Unit),
    ("Vec<Unit>", r#"{"kind":"vec","inner":{"kind":"struct","name":"Unit"}}"#, Vec<Unit>),
    ("Side", r#"{"kind":"enum","name":"Side"}"#, Side),
    ("Option<Side>", r#"{"kind":"option","inner":{"kind":"enum","name":"Side"}}"#, Option<Side>),
    ("Vec<Side>", r#"{"kind":"vec","inner":{"kind":"enum","name":"Side"}}"#, Vec<Side>),
    ("One", r#"{"kind":"enum","name":"One"}"#, One),
    ("Vec<One>", r#"{"kind":"vec","inner":{"kind":"enum","name":"One"}}"#, Vec<One>),
    ("Mixed", r#"{"kind":"enum","name":"Mixed"}"#, Mixed),
    ("Vec<Mixed>", r#"{"kind":"vec","inner":{"kind":"enum","name":"Mixed"}}"#, Vec<Mixed>),
];

/// The declarations of the test's `abi` block: the structs and enums of [`abi_defs`] and the events of [`ABI_EVENTS`].
const ABI_DECLARATIONS: &str = "struct Pair { a: u8, b: String }
struct Unit { }
enum Side { Token(ContractId), Asset(AssetId) }
enum One { A }
enum Mixed { A, B(u32, String), C(Pair) }
event transfer(from: Account, to: Account, amount: u128)
event note(text: String, tags: Vec<String>)
event flag(on: bool, at: Option<u64>)
event side(which: Side)
event empty()";

/// The test's types as a contract's source declares them: an `abi` block with [`ABI_DECLARATIONS`] and a method
/// `t<i>(a: <type>)` for each of [`ABI_TYPES`] in order. The oracle checks that the playground's parser makes of it
/// exactly the JSON of [`abi_defs`], [`ABI_EVENTS`] and [`ABI_TYPES`].
pub fn abi_block() -> String {
    let mut out = String::from("//! ```abi\n");
    for line in ABI_DECLARATIONS.lines() {
        out.push_str(&format!("//! {line}\n"));
    }
    for (i, ty) in ABI_TYPES.iter().enumerate() {
        out.push_str(&format!("//! call t{i}(a: {})\n", ty.text));
    }
    out.push_str("//! ```\n");
    out
}

/// The events of the `abi` block [`Request::AbiEvent`](super::Request::AbiEvent) decodes with: name and fields.
pub const ABI_EVENTS: &str = r#"[
    { "name": "transfer", "fields": [
        { "name": "from", "type": { "kind": "id", "name": "Account" } },
        { "name": "to", "type": { "kind": "id", "name": "Account" } },
        { "name": "amount", "type": { "kind": "int", "name": "u128" } } ] },
    { "name": "note", "fields": [
        { "name": "text", "type": { "kind": "string" } },
        { "name": "tags", "type": { "kind": "vec", "inner": { "kind": "string" } } } ] },
    { "name": "flag", "fields": [
        { "name": "on", "type": { "kind": "bool" } },
        { "name": "at", "type": { "kind": "option", "inner": { "kind": "int", "name": "u64" } } } ] },
    { "name": "side", "fields": [ { "name": "which", "type": { "kind": "enum", "name": "Side" } } ] },
    { "name": "empty", "fields": [] }
]"#;

fn fields(names: &[&str], values: Value) -> Value {
    let Value::Array(values) = values else {
        return Value::Null;
    };
    Value::Array(
        names
            .iter()
            .zip(values)
            .map(|(name, value)| json!([name, value]))
            .collect(),
    )
}

/// An event decoded the way a contract emits it, a Borsh tuple with the name first, against [`ABI_EVENTS`]:
/// `{"name", "fields": [[name, value], …]}`, or `None` when the bytes are no event of the block.
pub fn decode_abi_event(data: &[u8]) -> Option<Value> {
    let mut rest = data;
    let name = String::decode(&mut rest)?;
    let fields = match name.as_str() {
        "transfer" => fields(
            &["from", "to", "amount"],
            decode::<([u8; 32], [u8; 32], u128)>(rest)?,
        ),
        "note" => fields(&["text", "tags"], decode::<(String, Vec<String>)>(rest)?),
        "flag" => fields(&["on", "at"], decode::<(bool, Option<u64>)>(rest)?),
        "side" => fields(&["which"], decode::<(Side,)>(rest)?),
        "empty" => fields(&[], decode::<()>(rest)?),
        _ => return None,
    };
    Some(json!({ "name": name, "fields": fields }))
}
