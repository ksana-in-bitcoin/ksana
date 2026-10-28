//! The shared vectors of the token standards (`tests/vectors/standards.json`, D-120), built with this crate's own types
//! and encoders, so that they are what a contract built on the reference implementation takes, answers and emits.
//! Shared by the generator (`main.rs`) and the test that keeps the committed file in step (`tests/standard_vectors.rs`).

use ksana_contract_sdk::standard::{self, AssetRef};
use ksana_contract_sdk::{encode, ks20, ks721, KSA_ASSET_ID};
use serde_json::{json, Value};

fn hexs(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

/// 32 bytes that all differ (`tag`, `tag + 1`, ...), so that a byte order mistake shows.
fn key(tag: u8) -> [u8; 32] {
    core::array::from_fn(|i| tag.wrapping_add(i as u8))
}

const ZERO: [u8; 32] = standard::ZERO_ACCOUNT;
const STAMP: u64 = 1_767_225_600;
const ODD_AMOUNT: u128 = 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10;

fn alice() -> [u8; 32] {
    key(0x10)
}

fn bob() -> [u8; 32] {
    key(0x40)
}

fn spender() -> [u8; 32] {
    key(0x70)
}

fn receiver() -> [u8; 32] {
    key(0xa0)
}

fn token() -> [u8; 32] {
    key(0xc0)
}

/// A token ID outside the serial numbers: a hash, say.
fn hashed_id() -> [u8; 32] {
    key(0x55)
}

fn opt_hex(value: Option<[u8; 32]>) -> Value {
    value.map_or(Value::Null, |v| json!(hexs(&v)))
}

fn opt_u64(value: Option<u64>) -> Value {
    value.map_or(Value::Null, |v| json!(v.to_string()))
}

fn opt_u128(value: Option<u128>) -> Value {
    value.map_or(Value::Null, |v| json!(v.to_string()))
}

fn vector(standard: &str, method: &str, name: &str, value: Value, borsh: &[u8]) -> Value {
    json!({ "standard": standard, "method": method, "name": name, "value": value, "borsh": hexs(borsh) })
}

/// The arguments of the standards' methods: `value` names each argument as `docs/STANDARDS.md` does.
fn calls() -> Vec<Value> {
    let c20 = |method: &str, name: &str, value: Value, borsh: Vec<u8>| {
        vector(ks20::ID, method, name, value, &borsh)
    };
    let c721 = |method: &str, name: &str, value: Value, borsh: Vec<u8>| {
        vector(ks721::ID, method, name, value, &borsh)
    };
    let (alice, bob, spender, receiver) = (alice(), bob(), spender(), receiver());
    vec![
        c20("standards", "no arguments", json!({}), Vec::new()),
        c20("metadata", "no arguments", json!({}), Vec::new()),
        c20("capabilities", "no arguments", json!({}), Vec::new()),
        c20("total_supply", "no arguments", json!({}), Vec::new()),
        c20(
            "balance_of",
            "an account",
            json!({ "account": hexs(&alice) }),
            encode(&alice),
        ),
        c20(
            "allowance",
            "owner and spender",
            json!({ "owner": hexs(&alice), "spender": hexs(&spender) }),
            encode(&(alice, spender)),
        ),
        c20(
            "allowance_info",
            "owner and spender",
            json!({ "owner": hexs(&alice), "spender": hexs(&spender) }),
            encode(&(alice, spender)),
        ),
        c20("minter", "no arguments", json!({}), Vec::new()),
        c20("cap", "no arguments", json!({}), Vec::new()),
        c20(
            "transfer",
            "a small amount",
            json!({ "to": hexs(&bob), "amount": "5" }),
            encode(&(bob, 5u128)),
        ),
        c20(
            "transfer",
            "every byte of the amount differs",
            json!({ "to": hexs(&bob), "amount": ODD_AMOUNT.to_string() }),
            encode(&(bob, ODD_AMOUNT)),
        ),
        c20(
            "transfer_from",
            "the largest amount",
            json!({ "from": hexs(&alice), "to": hexs(&bob), "amount": u128::MAX.to_string() }),
            encode(&(alice, bob, u128::MAX)),
        ),
        c20(
            "approve",
            "no expiry",
            json!({ "spender": hexs(&spender), "amount": "1000", "expires_at": null }),
            encode(&(spender, 1_000u128, None::<u64>)),
        ),
        c20(
            "approve",
            "unlimited, with an expiry",
            json!({ "spender": hexs(&spender), "amount": ks20::UNLIMITED.to_string(), "expires_at": STAMP.to_string() }),
            encode(&(spender, ks20::UNLIMITED, Some(STAMP))),
        ),
        c20(
            "approve",
            "a revocation",
            json!({ "spender": hexs(&spender), "amount": "0", "expires_at": null }),
            encode(&(spender, 0u128, None::<u64>)),
        ),
        c20(
            "transfer_and_call",
            "with data",
            json!({ "to": hexs(&receiver), "amount": "250", "data": hexs(b"swap:min=240") }),
            encode(&(receiver, 250u128, b"swap:min=240".to_vec())),
        ),
        c20(
            "transfer_and_call",
            "without data",
            json!({ "to": hexs(&receiver), "amount": "1", "data": "" }),
            encode(&(receiver, 1u128, Vec::<u8>::new())),
        ),
        c20(
            "burn",
            "an amount",
            json!({ "amount": "7" }),
            encode(&7u128),
        ),
        c20(
            "burn_from",
            "from an account",
            json!({ "from": hexs(&alice), "amount": "8" }),
            encode(&(alice, 8u128)),
        ),
        c20(
            "mint",
            "to an account",
            json!({ "to": hexs(&bob), "amount": "1000000" }),
            encode(&(bob, 1_000_000u128)),
        ),
        c20(
            "set_minter",
            "a new minter",
            json!({ "minter": hexs(&spender) }),
            encode(&Some(spender)),
        ),
        c20(
            "set_minter",
            "minting ended",
            json!({ "minter": null }),
            encode(&None::<[u8; 32]>),
        ),
        c721("standards", "no arguments", json!({}), Vec::new()),
        c721("metadata", "no arguments", json!({}), Vec::new()),
        c721("capabilities", "no arguments", json!({}), Vec::new()),
        c721(
            "owner_of",
            "a serial number",
            json!({ "token_id": hexs(&ks721::id(1)) }),
            encode(&ks721::id(1)),
        ),
        c721(
            "balance_of",
            "an owner",
            json!({ "owner": hexs(&alice) }),
            encode(&alice),
        ),
        c721(
            "token_uri",
            "a hashed ID",
            json!({ "token_id": hexs(&hashed_id()) }),
            encode(&hashed_id()),
        ),
        c721(
            "approved",
            "a serial number",
            json!({ "token_id": hexs(&ks721::id(2)) }),
            encode(&ks721::id(2)),
        ),
        c721(
            "is_operator",
            "owner and operator",
            json!({ "owner": hexs(&alice), "operator": hexs(&spender) }),
            encode(&(alice, spender)),
        ),
        c721(
            "operator_info",
            "owner and operator",
            json!({ "owner": hexs(&alice), "operator": hexs(&spender) }),
            encode(&(alice, spender)),
        ),
        c721("total_supply", "no arguments", json!({}), Vec::new()),
        c721(
            "tokens",
            "the first page",
            json!({ "start": "0", "limit": ks721::MAX_PAGE }),
            encode(&(0u64, ks721::MAX_PAGE)),
        ),
        c721(
            "tokens_of",
            "a later page",
            json!({ "owner": hexs(&alice), "start": "300", "limit": 25 }),
            encode(&(alice, 300u64, 25u32)),
        ),
        c721(
            "transfer",
            "a serial number",
            json!({ "to": hexs(&bob), "token_id": hexs(&ks721::id(1)) }),
            encode(&(bob, ks721::id(1))),
        ),
        c721(
            "transfer_from",
            "a hashed ID",
            json!({ "from": hexs(&alice), "to": hexs(&bob), "token_id": hexs(&hashed_id()) }),
            encode(&(alice, bob, hashed_id())),
        ),
        c721(
            "approve",
            "a spender",
            json!({ "spender": hexs(&spender), "token_id": hexs(&ks721::id(2)) }),
            encode(&(Some(spender), ks721::id(2))),
        ),
        c721(
            "approve",
            "cleared",
            json!({ "spender": null, "token_id": hexs(&ks721::id(2)) }),
            encode(&(None::<[u8; 32]>, ks721::id(2))),
        ),
        c721(
            "set_operator",
            "with an expiry",
            json!({ "operator": hexs(&spender), "approved": true, "expires_at": STAMP.to_string() }),
            encode(&(spender, true, Some(STAMP))),
        ),
        c721(
            "set_operator",
            "revoked",
            json!({ "operator": hexs(&spender), "approved": false, "expires_at": null }),
            encode(&(spender, false, None::<u64>)),
        ),
        c721(
            "transfer_and_call",
            "with data",
            json!({ "to": hexs(&receiver), "token_id": hexs(&ks721::id(3)), "data": hexs(b"list:price=100") }),
            encode(&(receiver, ks721::id(3), b"list:price=100".to_vec())),
        ),
        c721(
            "burn",
            "a serial number",
            json!({ "token_id": hexs(&ks721::id(4)) }),
            encode(&ks721::id(4)),
        ),
    ]
}

/// The answers of the standards' methods (the `Ok` bytes of the call's result).
fn answers() -> Vec<Value> {
    let c20 = |method: &str, name: &str, value: Value, borsh: Vec<u8>| {
        vector(ks20::ID, method, name, value, &borsh)
    };
    let c721 = |method: &str, name: &str, value: Value, borsh: Vec<u8>| {
        vector(ks721::ID, method, name, value, &borsh)
    };
    let (alice, spender) = (alice(), spender());
    let ks20_ids = [ks20::ID, ks20::BURNABLE, ks20::MINTABLE]
        .map(String::from)
        .to_vec();
    let ks721_ids = [ks721::ID, ks721::BURNABLE, ks721::ENUMERABLE]
        .map(String::from)
        .to_vec();
    let capabilities = [standard::CAP_MINT, standard::CAP_PAUSE, standard::CAP_FEE]
        .map(String::from)
        .to_vec();
    let reply = encode(&(token(), 250u128));
    let nft_reply = encode(&(token(), ks721::id(3)));
    let uri = "ipfs://bafkreigh2akiscaildc/1.json";
    let data_uri = "data:application/json,{\"name\":\"Ṇight ₿ ü\"}";
    let page = [ks721::id(1), hashed_id(), ks721::id(u128::MAX)];
    vec![
        c20(
            "standards",
            "the reference token",
            json!(ks20_ids),
            encode(&ks20_ids),
        ),
        c20(
            "standards",
            "no extensions",
            json!([ks20::ID]),
            encode(&[String::from(ks20::ID)].to_vec()),
        ),
        c20(
            "metadata",
            "non-ASCII text",
            json!({ "name": "Ksana Ñame ₿", "symbol": "KSÑ", "decimals": 8 }),
            encode(&("Ksana Ñame ₿", "KSÑ", 8u8)),
        ),
        c20(
            "capabilities",
            "none",
            json!([]),
            encode(&Vec::<String>::new()),
        ),
        c20(
            "capabilities",
            "in the standard's order",
            json!(capabilities),
            encode(&capabilities),
        ),
        c20(
            "total_supply",
            "every byte differs",
            json!(ODD_AMOUNT.to_string()),
            encode(&ODD_AMOUNT),
        ),
        c20("balance_of", "nothing", json!("0"), encode(&0u128)),
        c20(
            "allowance",
            "unlimited",
            json!(ks20::UNLIMITED.to_string()),
            encode(&ks20::UNLIMITED),
        ),
        c20(
            "allowance_info",
            "with an expiry",
            json!({ "amount": "1000", "expires_at": STAMP.to_string() }),
            encode(&(1_000u128, Some(STAMP))),
        ),
        c20(
            "allowance_info",
            "no expiry",
            json!({ "amount": "0", "expires_at": null }),
            encode(&(0u128, None::<u64>)),
        ),
        c20(
            "minter",
            "a minter",
            opt_hex(Some(alice)),
            encode(&Some(alice)),
        ),
        c20(
            "minter",
            "minting ended",
            opt_hex(None),
            encode(&None::<[u8; 32]>),
        ),
        c20(
            "cap",
            "a cap",
            opt_u128(Some(21_000_000)),
            encode(&Some(21_000_000u128)),
        ),
        c20("cap", "no cap", opt_u128(None), encode(&None::<u128>)),
        c20(
            "transfer_and_call",
            "the recipient's answer",
            json!(hexs(&reply)),
            encode(&reply),
        ),
        c20(
            "transfer_and_call",
            "an empty answer",
            json!(""),
            encode(&Vec::<u8>::new()),
        ),
        c721(
            "standards",
            "the test collection",
            json!(ks721_ids),
            encode(&ks721_ids),
        ),
        c721(
            "metadata",
            "non-ASCII text",
            json!({ "name": "Ksana Ärt 🎨", "symbol": "KÄRT" }),
            encode(&("Ksana Ärt 🎨", "KÄRT")),
        ),
        c721(
            "capabilities",
            "minting",
            json!([standard::CAP_MINT]),
            encode(&[String::from(standard::CAP_MINT)].to_vec()),
        ),
        c721(
            "owner_of",
            "an owner",
            opt_hex(Some(alice)),
            encode(&Some(alice)),
        ),
        c721(
            "owner_of",
            "no token",
            opt_hex(None),
            encode(&None::<[u8; 32]>),
        ),
        c721("balance_of", "a count", json!("3"), encode(&3u64)),
        c721("token_uri", "an IPFS URI", json!(uri), encode(&Some(uri))),
        c721(
            "token_uri",
            "a data URI",
            json!(data_uri),
            encode(&Some(data_uri)),
        ),
        c721("token_uri", "none", Value::Null, encode(&None::<String>)),
        c721(
            "approved",
            "a spender",
            opt_hex(Some(spender)),
            encode(&Some(spender)),
        ),
        c721("approved", "none", opt_hex(None), encode(&None::<[u8; 32]>)),
        c721("is_operator", "an operator", json!(true), encode(&true)),
        c721(
            "is_operator",
            "not an operator",
            json!(false),
            encode(&false),
        ),
        c721(
            "operator_info",
            "with an expiry",
            json!({ "approved": true, "expires_at": STAMP.to_string() }),
            encode(&(true, Some(STAMP))),
        ),
        c721(
            "operator_info",
            "revoked",
            json!({ "approved": false, "expires_at": null }),
            encode(&(false, None::<u64>)),
        ),
        c721(
            "total_supply",
            "a count",
            json!(u64::MAX.to_string()),
            encode(&u64::MAX),
        ),
        c721(
            "tokens",
            "a page",
            json!(page.iter().map(|id| hexs(id)).collect::<Vec<_>>()),
            encode(&page.to_vec()),
        ),
        c721(
            "tokens_of",
            "an empty page",
            json!([]),
            encode(&Vec::<[u8; 32]>::new()),
        ),
        c721(
            "transfer_and_call",
            "the recipient's answer",
            json!(hexs(&nft_reply)),
            encode(&nft_reply),
        ),
    ]
}

fn ks20_event(event: &ks20::Event) -> Value {
    match event {
        ks20::Event::Standards(ids) => json!({ "Standards": ids }),
        ks20::Event::Metadata {
            name,
            symbol,
            decimals,
        } => {
            json!({ "Metadata": { "name": name, "symbol": symbol, "decimals": decimals } })
        }
        ks20::Event::Transfer { from, to, amount } => {
            json!({ "Transfer": { "from": hexs(from), "to": hexs(to), "amount": amount.to_string() } })
        }
        ks20::Event::Approve {
            owner,
            spender,
            amount,
            expires_at,
        } => json!({ "Approve": {
            "owner": hexs(owner), "spender": hexs(spender), "amount": amount.to_string(), "expires_at": opt_u64(*expires_at),
        } }),
        ks20::Event::Minter { previous, minter } => {
            json!({ "Minter": { "previous": opt_hex(*previous), "minter": opt_hex(*minter) } })
        }
    }
}

fn ks721_event(event: &ks721::Event) -> Value {
    match event {
        ks721::Event::Standards(ids) => json!({ "Standards": ids }),
        ks721::Event::Metadata { name, symbol } => {
            json!({ "Metadata": { "name": name, "symbol": symbol } })
        }
        ks721::Event::Transfer { from, to, token_id } => {
            json!({ "Transfer": { "from": hexs(from), "to": hexs(to), "token_id": hexs(token_id) } })
        }
        ks721::Event::Approve {
            owner,
            spender,
            token_id,
        } => json!({ "Approve": {
            "owner": hexs(owner), "spender": opt_hex(*spender), "token_id": hexs(token_id),
        } }),
        ks721::Event::Operator {
            owner,
            operator,
            approved,
            expires_at,
        } => json!({ "Operator": {
            "owner": hexs(owner), "operator": hexs(operator), "approved": approved, "expires_at": opt_u64(*expires_at),
        } }),
        ks721::Event::Uri(token_id) => json!({ "Uri": opt_hex(*token_id) }),
    }
}

/// The standards' events, each checked to parse back to itself.
fn events() -> Vec<Value> {
    let (alice, bob, spender) = (alice(), bob(), spender());
    let ks20_events = [
        (
            "the declaration",
            ks20::Event::Standards(
                [ks20::ID, ks20::BURNABLE, ks20::MINTABLE]
                    .map(String::from)
                    .to_vec(),
            ),
        ),
        (
            "non-ASCII text",
            ks20::Event::Metadata {
                name: "Ksana Ñame ₿".into(),
                symbol: "KSÑ".into(),
                decimals: 18,
            },
        ),
        (
            "a mint",
            ks20::Event::Transfer {
                from: ZERO,
                to: alice,
                amount: ODD_AMOUNT,
            },
        ),
        (
            "a transfer",
            ks20::Event::Transfer {
                from: alice,
                to: bob,
                amount: 5,
            },
        ),
        (
            "a burn",
            ks20::Event::Transfer {
                from: bob,
                to: ZERO,
                amount: u128::MAX,
            },
        ),
        (
            "no expiry",
            ks20::Event::Approve {
                owner: alice,
                spender,
                amount: 1_000,
                expires_at: None,
            },
        ),
        (
            "unlimited, with an expiry",
            ks20::Event::Approve {
                owner: alice,
                spender,
                amount: ks20::UNLIMITED,
                expires_at: Some(STAMP),
            },
        ),
        (
            "the first minter",
            ks20::Event::Minter {
                previous: None,
                minter: Some(alice),
            },
        ),
        (
            "minting ended",
            ks20::Event::Minter {
                previous: Some(alice),
                minter: None,
            },
        ),
    ];
    let ks721_events = [
        (
            "the declaration",
            ks721::Event::Standards([ks721::ID, ks721::BURNABLE].map(String::from).to_vec()),
        ),
        (
            "non-ASCII text",
            ks721::Event::Metadata {
                name: "Ksana Ärt 🎨".into(),
                symbol: "KÄRT".into(),
            },
        ),
        (
            "a mint",
            ks721::Event::Transfer {
                from: ZERO,
                to: alice,
                token_id: ks721::id(1),
            },
        ),
        (
            "a transfer of a hashed ID",
            ks721::Event::Transfer {
                from: alice,
                to: bob,
                token_id: hashed_id(),
            },
        ),
        (
            "a burn",
            ks721::Event::Transfer {
                from: bob,
                to: ZERO,
                token_id: ks721::id(u128::MAX),
            },
        ),
        (
            "a spender",
            ks721::Event::Approve {
                owner: alice,
                spender: Some(spender),
                token_id: ks721::id(2),
            },
        ),
        (
            "cleared",
            ks721::Event::Approve {
                owner: alice,
                spender: None,
                token_id: ks721::id(2),
            },
        ),
        (
            "an operator with an expiry",
            ks721::Event::Operator {
                owner: alice,
                operator: spender,
                approved: true,
                expires_at: Some(STAMP),
            },
        ),
        (
            "an operator revoked",
            ks721::Event::Operator {
                owner: alice,
                operator: spender,
                approved: false,
                expires_at: None,
            },
        ),
        ("one token's URI", ks721::Event::Uri(Some(ks721::id(1)))),
        ("every token's URI", ks721::Event::Uri(None)),
    ];
    let mut out = Vec::new();
    for (name, event) in ks20_events {
        let borsh = event.encode();
        assert_eq!(ks20::Event::parse(&borsh), Some(event.clone()), "{name}");
        out.push(json!({ "standard": ks20::ID, "name": name, "value": ks20_event(&event), "borsh": hexs(&borsh) }));
    }
    for (name, event) in ks721_events {
        let borsh = event.encode();
        assert_eq!(ks721::Event::parse(&borsh), Some(event.clone()), "{name}");
        out.push(json!({ "standard": ks721::ID, "name": name, "value": ks721_event(&event), "borsh": hexs(&borsh) }));
    }
    out
}

/// Bytes that are no event of the standard: each is checked not to parse.
fn invalid_events() -> Vec<Value> {
    let (alice, bob) = (alice(), bob());
    let transfer20 = ks20::Event::Transfer {
        from: alice,
        to: bob,
        amount: 5,
    }
    .encode();
    let transfer721 = ks721::Event::Transfer {
        from: alice,
        to: bob,
        token_id: ks721::id(1),
    }
    .encode();
    let approve20 = ks20::Event::Approve {
        owner: alice,
        spender: bob,
        amount: 1,
        expires_at: None,
    }
    .encode();
    let operator = ks721::Event::Operator {
        owner: alice,
        operator: bob,
        approved: true,
        expires_at: None,
    }
    .encode();
    let with = |mut bytes: Vec<u8>, at: usize, value: u8| {
        bytes[at] = value;
        bytes
    };
    let trailing = |mut bytes: Vec<u8>| {
        bytes.push(0);
        bytes
    };
    let truncated = |mut bytes: Vec<u8>| {
        bytes.pop();
        bytes
    };
    let ks20_cases = vec![
        ("a trailing byte", trailing(transfer20.clone())),
        ("one byte short", truncated(transfer20.clone())),
        ("a KS-721 transfer", transfer721.clone()),
        (
            "an option tag of 2",
            with(approve20.clone(), approve20.len() - 1, 2),
        ),
        ("another event", encode(&("swap", alice, 5u128))),
        (
            "a name that is not UTF-8",
            with(transfer20.clone(), 4, 0xff),
        ),
        ("empty", Vec::new()),
    ];
    let ks721_cases = vec![
        ("a trailing byte", trailing(transfer721.clone())),
        ("a KS-20 transfer", transfer20),
        (
            "a boolean of 2",
            with(operator.clone(), operator.len() - 2, 2),
        ),
        ("a KS-20 approve", approve20),
        // the declaration's one ID starts at byte 21, after the name and the two lengths
        (
            "a declaration that is not UTF-8",
            with(
                encode(&("standards", vec![String::from(ks721::ID)])),
                21,
                0xc3,
            ),
        ),
    ];
    let mut out = Vec::new();
    for (name, borsh) in ks20_cases {
        assert_eq!(ks20::Event::parse(&borsh), None, "{name}");
        out.push(json!({ "standard": ks20::ID, "name": name, "borsh": hexs(&borsh) }));
    }
    for (name, borsh) in ks721_cases {
        assert_eq!(ks721::Event::parse(&borsh), None, "{name}");
        out.push(json!({ "standard": ks721::ID, "name": name, "borsh": hexs(&borsh) }));
    }
    out
}

fn token_ids() -> Value {
    let serials = [0u128, 1, 255, 1 << 64, (1 << 127) + 1, u128::MAX];
    let valid: Vec<Value> = serials
        .iter()
        .map(|serial| {
            let id = ks721::id(*serial);
            assert_eq!(ks721::serial(&id), Some(*serial));
            json!({ "serial": serial.to_string(), "token_id": hexs(&id) })
        })
        .collect();
    let mut above = [0u8; 32];
    above[15] = 1;
    let without: Vec<Value> = [above, hashed_id(), [0xff; 32]]
        .iter()
        .map(|id| {
            assert_eq!(ks721::serial(id), None);
            json!(hexs(id))
        })
        .collect();
    json!({ "serials": valid, "without_serial": without })
}

fn asset_refs() -> Value {
    let valid: Vec<Value> = [
        (
            "a token",
            AssetRef::Token(token()),
            json!({ "Token": hexs(&token()) }),
        ),
        (
            "KSA",
            AssetRef::Asset(KSA_ASSET_ID),
            json!({ "Asset": hexs(&KSA_ASSET_ID) }),
        ),
        (
            "a built-in asset",
            AssetRef::Asset(key(0xe0)),
            json!({ "Asset": hexs(&key(0xe0)) }),
        ),
    ]
    .into_iter()
    .map(|(name, value, json)| {
        let borsh = encode(&value);
        assert_eq!(ksana_contract_sdk::decode::<AssetRef>(&borsh), Ok(value));
        json!({ "name": name, "value": json, "borsh": hexs(&borsh) })
    })
    .collect();
    let mut tag2 = encode(&AssetRef::Token(token()));
    tag2[0] = 2;
    let mut short = encode(&AssetRef::Token(token()));
    short.pop();
    let mut long = encode(&AssetRef::Asset(KSA_ASSET_ID));
    long.push(0);
    let invalid: Vec<Value> = [
        ("a tag of 2", tag2),
        ("one byte short", short),
        ("a trailing byte", long),
        ("empty", Vec::new()),
    ]
    .into_iter()
    .map(|(name, borsh)| {
        assert!(
            ksana_contract_sdk::decode::<AssetRef>(&borsh).is_err(),
            "{name}"
        );
        json!({ "name": name, "borsh": hexs(&borsh) })
    })
    .collect();
    json!({ "valid": valid, "invalid": invalid })
}

/// The whole file.
pub fn vectors() -> Value {
    json!({
        "description": "The token standards KS-20 and KS-721 (docs/STANDARDS.md, D-120), as the contract SDK's reference implementation encodes them: `calls` are the Borsh arguments of each method (`value` names them as the standard does), `answers` the `Ok` bytes of each method's answer, `events` the events (externally tagged as D-007), `invalid_events` bytes that are no event of the standard, `token_ids` KS-721 IDs from serial numbers (big-endian in the last 16 bytes) and IDs without one, `asset_refs` the AssetRef of a KS-20 token or a built-in asset. Byte strings are hex, u64 / u128 decimal strings, u8 / u32 numbers (D-007).",
        "calls": calls(),
        "answers": answers(),
        "events": events(),
        "invalid_events": invalid_events(),
        "token_ids": token_ids(),
        "asset_refs": asset_refs(),
    })
}
