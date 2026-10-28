//! Tests of the NFT contract on the SDK's in-memory mock host: the KS-721 conformance kit, minting, the market's
//! fixed-price sales for built-in assets and the listings a change of owner drops, the paged views, the consistency
//! of the listings under random operations, and the ABI block of the module docs against `dispatch`.
#![cfg(not(target_arch = "wasm32"))]
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

use std::collections::{BTreeMap, BTreeSet};

use ksana_contract_sdk::ks721::{self, TokenId};
use ksana_contract_sdk::mock::{self, Failure};
use ksana_contract_sdk::{conformance, decode, encode, standard, Decode, Funds, KSA_ASSET_ID};
use nft::{Collection, Listing, Token};

/// The deployer of the collections, and so their first minter.
const ALICE: [u8; 32] = [0xa1; 32];
const BOB: [u8; 32] = [0xb0; 32];
const CAROL: [u8; 32] = [0xc4; 32];
const DAVE: [u8; 32] = [0xd0; 32];
const ACCOUNTS: [[u8; 32]; 4] = [ALICE, BOB, CAROL, DAVE];
const ZERO: [u8; 32] = [0; 32];
/// A second built-in asset besides KSA (its ID sorts after KSA's).
const GEM: [u8; 32] = [0x6e; 32];
/// What every account holds of KSA and of GEM at the start.
const FUNDS: u128 = 1_000_000;
/// A contract that takes tokens with `on_nft_received`.
const SAFE: [u8; 32] = [0x5a; 32];
/// Every account that can own tokens in these tests.
const OWNERS: [[u8; 32]; 5] = [ALICE, BOB, CAROL, DAVE, SAFE];

/// The ID of the `n`-th token minted.
fn id(n: u128) -> TokenId {
    ks721::id(n)
}

/// A deployed collection.
#[derive(Clone, Copy, Debug)]
struct Nft([u8; 32]);

fn deploy(nonce: u64, name: &str, symbol: &str, max_supply: Option<u64>) -> Result<Nft, Failure> {
    mock::deploy(
        ALICE,
        nonce,
        b"nft",
        nft::dispatch,
        &encode(&(name, symbol, max_supply)),
    )
    .map(Nft)
}

/// A contract that takes every token sent with `transfer_and_call` and answers with the collection that called.
fn safe(method: &str, args: &[u8]) -> ksana_contract_sdk::CallResult {
    match method {
        ks721::ON_RECEIVED => {
            let received = ks721::received(args)?;
            standard::reply(&received.collection)
        }
        _ => Err(standard::ERR_UNKNOWN_METHOD.to_vec()),
    }
}

/// A fresh mock where every account holds KSA and GEM, and the collection "Ksana Cats" deployed by ALICE.
fn setup(max_supply: Option<u64>) -> Nft {
    mock::reset();
    mock::register_asset(KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::register_asset(GEM, "GEM", 0, [0x99; 32]);
    for who in ACCOUNTS {
        mock::mint(KSA_ASSET_ID, who, FUNDS).unwrap();
        mock::mint(GEM, who, FUNDS).unwrap();
    }
    mock::register(SAFE, safe);
    let nft = deploy(0, "Ksana Cats", "KCAT", max_supply).unwrap();
    mock::clear_events();
    nft
}

fn ksa(amount: u128) -> Funds {
    Funds {
        asset: KSA_ASSET_ID,
        amount,
    }
}

fn gem(amount: u128) -> Funds {
    Funds { asset: GEM, amount }
}

/// The URI the tests mint the `n`-th token with.
fn uri_of(n: u128) -> String {
    format!("ipfs://cats/{n}.png")
}

fn unit(out: Vec<u8>) {
    decode::<()>(&out).unwrap()
}

fn ids(tokens: &[Token]) -> Vec<TokenId> {
    tokens.iter().map(|token| token.id).collect()
}

fn revert_reason<T: std::fmt::Debug>(result: Result<T, Failure>) -> Vec<u8> {
    match result {
        Err(Failure::Revert { reason, .. }) => reason,
        other => panic!("expected a revert, got {other:?}"),
    }
}

/// The data of the events recorded since the last call (or `mock::clear_events`), all emitted by the collection.
fn take_events(nft: Nft) -> Vec<Vec<u8>> {
    let events = mock::events();
    mock::clear_events();
    events
        .into_iter()
        .map(|event| {
            assert_eq!(event.contract, nft.0);
            event.data
        })
        .collect()
}

impl Nft {
    fn call(self, who: [u8; 32], method: &str, args: &[u8]) -> Result<Vec<u8>, Failure> {
        mock::call(who, self.0, method, args)
    }

    fn mint(self, who: [u8; 32], to: [u8; 32], uri: Option<&str>) -> Result<TokenId, Failure> {
        self.call(who, "mint", &encode(&(to, uri)))
            .map(|out| decode(&out).unwrap())
    }

    fn transfer(self, who: [u8; 32], to: [u8; 32], token_id: TokenId) -> Result<(), Failure> {
        self.call(who, "transfer", &encode(&(to, token_id)))
            .map(unit)
    }

    fn transfer_from(
        self,
        who: [u8; 32],
        from: [u8; 32],
        to: [u8; 32],
        token_id: TokenId,
    ) -> Result<(), Failure> {
        self.call(who, "transfer_from", &encode(&(from, to, token_id)))
            .map(unit)
    }

    fn approve(
        self,
        who: [u8; 32],
        spender: Option<[u8; 32]>,
        token_id: TokenId,
    ) -> Result<(), Failure> {
        self.call(who, "approve", &encode(&(spender, token_id)))
            .map(unit)
    }

    fn set_operator(
        self,
        who: [u8; 32],
        operator: [u8; 32],
        approved: bool,
        expires_at: Option<u64>,
    ) -> Result<(), Failure> {
        self.call(
            who,
            "set_operator",
            &encode(&(operator, approved, expires_at)),
        )
        .map(unit)
    }

    fn burn(self, who: [u8; 32], token_id: TokenId) -> Result<(), Failure> {
        self.call(who, "burn", &encode(&token_id)).map(unit)
    }

    fn list(
        self,
        who: [u8; 32],
        token_id: TokenId,
        asset: [u8; 32],
        price: u128,
    ) -> Result<(), Failure> {
        self.call(who, "list", &encode(&(token_id, asset, price)))
            .map(unit)
    }

    fn unlist(self, who: [u8; 32], token_id: TokenId) -> Result<(), Failure> {
        self.call(who, "unlist", &encode(&token_id)).map(unit)
    }

    fn buy(self, who: [u8; 32], token_id: TokenId, funds: &[Funds]) -> Result<(), Failure> {
        mock::call_with_funds(who, self.0, "buy", &encode(&token_id), funds).map(unit)
    }

    fn set_minter(self, who: [u8; 32], minter: Option<[u8; 32]>) -> Result<(), Failure> {
        self.call(who, "set_minter", &encode(&minter)).map(unit)
    }

    /// Calls a view as `ksana_call` does, read-only with nothing attached: in the mock straight in the contract's
    /// context, without the snapshot an operation takes.
    fn view<T: Decode>(self, method: &str, args: &[u8]) -> T {
        let out = mock::with_context(BOB, self.0, || nft::dispatch(method, args)).unwrap_or_else(
            |reason| panic!("{method} reverted: {}", String::from_utf8_lossy(&reason)),
        );
        assert!(
            out.len() <= mock::MAX_OUTPUT_BYTES,
            "{method} returns too much"
        );
        decode(&out).unwrap()
    }

    fn collection(self) -> Collection {
        self.view("collection", &[])
    }

    fn capabilities(self) -> Vec<String> {
        self.view("capabilities", &[])
    }

    fn token(self, token_id: TokenId) -> Option<Token> {
        self.view("token", &encode(&token_id))
    }

    fn owner_of(self, token_id: TokenId) -> Option<[u8; 32]> {
        self.view("owner_of", &encode(&token_id))
    }

    fn balance_of(self, owner: [u8; 32]) -> u64 {
        self.view("balance_of", &encode(&owner))
    }

    fn details(self, start: u64, limit: u32) -> Vec<Token> {
        self.view("details", &encode(&(start, limit)))
    }

    fn details_of(self, owner: [u8; 32], start: u64, limit: u32) -> Vec<Token> {
        self.view("details_of", &encode(&(owner, start, limit)))
    }

    fn tokens(self, start: u64, limit: u32) -> Vec<TokenId> {
        self.view("tokens", &encode(&(start, limit)))
    }

    fn tokens_of(self, owner: [u8; 32], start: u64, limit: u32) -> Vec<TokenId> {
        self.view("tokens_of", &encode(&(owner, start, limit)))
    }

    fn listings(self, start: u64, limit: u32) -> Vec<Token> {
        self.view("listings", &encode(&(start, limit)))
    }

    fn is_operator(self, owner: [u8; 32], operator: [u8; 32]) -> bool {
        self.view("is_operator", &encode(&(owner, operator)))
    }
}

/// A paged view read to the end, 50 at a time.
fn all(page: impl Fn(u64) -> Vec<Token>) -> Vec<Token> {
    let mut out = Vec::new();
    loop {
        let next = page(out.len() as u64);
        if next.is_empty() {
            return out;
        }
        out.extend(next);
    }
}

/// Checks the market's records against the collection's: every listed token is in the listing list exactly once,
/// at the position its index names, and every token of the list is listed and exists; the details views list
/// every token as `token` gives it; the collection holds none of the assets.
fn check_listings(nft: Nft) {
    let every = all(|start| nft.details(start, 50));
    let collection = nft.collection();
    assert_eq!(every.len() as u64, collection.supply);
    let mut listed = BTreeSet::new();
    for token in &every {
        assert_eq!(nft.token(token.id).as_ref(), Some(token));
        assert_ne!(token.approved, Some(token.owner));
        if token.listing.is_some() {
            listed.insert(token.id);
        }
    }
    let mut held = 0;
    for owner in OWNERS {
        let of = all(|start| nft.details_of(owner, start, 50));
        assert_eq!(nft.balance_of(owner), of.len() as u64);
        assert!(of.iter().all(|token| token.owner == owner));
        held += of.len();
    }
    assert_eq!(held, every.len());
    let list = all(|start| nft.listings(start, 50));
    for (position, token) in list.iter().enumerate() {
        assert!(token.listing.is_some_and(|listing| listing.price > 0));
        let stored = mock::storage_value(&nft.0, &[b"q".as_slice(), &token.id].concat());
        assert_eq!(stored, Some(encode(&(position as u64))));
    }
    assert_eq!(ids(&list).into_iter().collect::<BTreeSet<_>>(), listed);
    assert_eq!(list.len(), listed.len());
    assert_eq!(
        mock::storage_value(&nft.0, b"listings"),
        (!list.is_empty()).then(|| encode(&(list.len() as u64)))
    );
    assert_eq!(mock::balance(&KSA_ASSET_ID, &nft.0), 0);
    assert_eq!(mock::balance(&GEM, &nft.0), 0);
}

// ---------------------------------------------------------------------------
// KS-721

#[test]
fn the_collection_follows_ks721() {
    // Accounts of their own: the kit moves tokens between the holder and its own accounts
    const MINTER: [u8; 32] = [0x11; 32];
    const HOLDER: [u8; 32] = [0x12; 32];
    conformance::ks721(|| {
        let init = encode(&("Ksana Cats", "KCAT", None::<u64>));
        let nft = Nft(mock::deploy(MINTER, 0, b"nft", nft::dispatch, &init).unwrap());
        let tokens = (1..=3)
            .map(|n| nft.mint(MINTER, HOLDER, Some(&uri_of(n))).unwrap())
            .collect();
        conformance::Ks721Subject {
            collection: nft.0,
            holder: HOLDER,
            tokens,
        }
    })
    .unwrap();
}

#[test]
fn init_declares_the_collection() {
    mock::reset();
    let nft = deploy(0, "Ksana Cats", "KCAT", Some(3)).unwrap();
    let standards: Vec<String> = [ks721::ID, ks721::BURNABLE, ks721::ENUMERABLE]
        .map(String::from)
        .to_vec();
    assert_eq!(
        take_events(nft),
        [
            encode(&("standards", &standards)),
            encode(&("metadata", "Ksana Cats", "KCAT")),
            encode(&("minter", None::<[u8; 32]>, Some(ALICE))),
        ]
    );
    assert_eq!(nft.view::<Vec<String>>("standards", &[]), standards);
    assert_eq!(
        nft.view::<(String, String)>("metadata", &[]),
        ("Ksana Cats".to_string(), "KCAT".to_string())
    );
    assert_eq!(
        nft.collection(),
        Collection {
            name: "Ksana Cats".to_string(),
            symbol: "KCAT".to_string(),
            minter: Some(ALICE),
            minted: 0,
            supply: 0,
            max_supply: Some(3),
        }
    );
    assert_eq!(nft.capabilities(), ["mint"]);
}

#[test]
fn init_checks_its_arguments() {
    mock::reset();
    mock::register_asset(KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::mint(KSA_ASSET_ID, ALICE, 1).unwrap();
    let long_name = "ñ".repeat(32) + "x";
    for (nonce, (name, symbol, max_supply), reason) in [
        (1, ("", "S", None), standard::ERR_NAME),
        (2, (long_name.as_str(), "S", None), standard::ERR_NAME),
        (3, ("N", "", None), standard::ERR_SYMBOL),
        (4, ("N", "SEVENTEEN_BYTES_X", None), standard::ERR_SYMBOL),
        (
            5,
            ("N", "S", Some(0)),
            b"max_supply must be positive".as_slice(),
        ),
    ] {
        let result = deploy(nonce, name, symbol, max_supply);
        assert_eq!(
            revert_reason(result),
            reason,
            "{name} {symbol} {max_supply:?}"
        );
    }
    // 64 bytes of name and 16 of symbol are fine, and so is a cap of one
    let name = "ñ".repeat(32);
    let nft = deploy(6, &name, "SIXTEEN_BYTES_XX", Some(1)).unwrap();
    assert_eq!(nft.collection().name, name);
    // Funds attached at deployment are refused
    let result = mock::deploy_with_funds(
        ALICE,
        7,
        b"nft",
        nft::dispatch,
        &encode(&("N", "S", None::<u64>)),
        &[ksa(1)],
    );
    assert_eq!(revert_reason(result), standard::ERR_UNEXPECTED_FUNDS);
}

#[test]
fn mint_numbers_the_tokens_for_the_minter_only() {
    let nft = setup(None);
    assert_eq!(nft.mint(ALICE, BOB, Some(&uri_of(1))), Ok(id(1)));
    assert_eq!(nft.mint(ALICE, CAROL, None), Ok(id(2)));
    assert_eq!(
        take_events(nft),
        [
            encode(&("transfer", ZERO, BOB, id(1))),
            encode(&("transfer", ZERO, CAROL, id(2))),
        ]
    );
    assert_eq!(
        nft.token(id(1)),
        Some(Token {
            id: id(1),
            owner: BOB,
            uri: Some(uri_of(1)),
            approved: None,
            listing: None,
        })
    );
    assert_eq!(nft.token(id(2)).unwrap().uri, None);
    assert_eq!(
        nft.view::<Option<String>>("token_uri", &encode(&id(1))),
        Some(uri_of(1))
    );
    // Only the minter, to neither the zero account nor the collection, with a URI of at most 512 bytes
    let long = "x".repeat(513);
    for (who, to, uri, reason) in [
        (BOB, BOB, None, standard::ERR_NOT_THE_MINTER),
        (ALICE, ZERO, None, standard::ERR_ZERO_ACCOUNT),
        (ALICE, nft.0, None, standard::ERR_SELF_RECIPIENT),
        (
            ALICE,
            BOB,
            Some(long.as_str()),
            b"uri must be at most 512 bytes".as_slice(),
        ),
    ] {
        assert_eq!(revert_reason(nft.mint(who, to, uri)), reason);
    }
    assert_eq!(nft.mint(ALICE, BOB, Some(&"x".repeat(512))), Ok(id(3)));
    let collection = nft.collection();
    assert_eq!((collection.minted, collection.supply), (3, 3));
    check_listings(nft);
}

#[test]
fn max_supply_caps_every_mint_ever() {
    let nft = setup(Some(2));
    nft.mint(ALICE, BOB, None).unwrap();
    nft.mint(ALICE, BOB, None).unwrap();
    assert_eq!(nft.capabilities(), Vec::<String>::new());
    assert_eq!(
        revert_reason(nft.mint(ALICE, BOB, None)),
        b"max supply reached"
    );
    // A burned token still counts, and its ID is never minted again
    nft.burn(BOB, id(1)).unwrap();
    assert_eq!(
        revert_reason(nft.mint(ALICE, BOB, None)),
        b"max supply reached"
    );
    let collection = nft.collection();
    assert_eq!((collection.minted, collection.supply), (2, 1));
    assert_eq!(nft.token(id(1)), None);
}

#[test]
fn set_minter_hands_over_or_ends_minting() {
    let nft = setup(None);
    assert_eq!(
        revert_reason(nft.set_minter(BOB, Some(BOB))),
        standard::ERR_NOT_THE_MINTER
    );
    assert_eq!(
        revert_reason(nft.set_minter(ALICE, Some(ZERO))),
        standard::ERR_ZERO_ACCOUNT
    );
    nft.set_minter(ALICE, Some(BOB)).unwrap();
    assert_eq!(
        take_events(nft),
        [encode(&("minter", Some(ALICE), Some(BOB)))]
    );
    assert_eq!(
        revert_reason(nft.mint(ALICE, CAROL, None)),
        standard::ERR_NOT_THE_MINTER
    );
    assert_eq!(nft.mint(BOB, CAROL, None), Ok(id(1)));
    nft.set_minter(BOB, None).unwrap();
    assert_eq!(nft.collection().minter, None);
    assert!(nft.capabilities().is_empty());
    for who in [ALICE, BOB] {
        assert_eq!(
            revert_reason(nft.mint(who, CAROL, None)),
            standard::ERR_MINTING_ENDED
        );
        assert_eq!(
            revert_reason(nft.set_minter(who, Some(who))),
            standard::ERR_MINTING_ENDED
        );
    }
}

#[test]
fn owners_transfer_and_the_others_transfer_from() {
    let nft = setup(None);
    let token = nft.mint(ALICE, BOB, None).unwrap();
    // The approved account and an operator move it with transfer_from, naming the owner
    nft.approve(BOB, Some(CAROL), token).unwrap();
    assert_eq!(
        revert_reason(nft.transfer(CAROL, DAVE, token)),
        ks721::ERR_NOT_THE_OWNER
    );
    assert_eq!(
        revert_reason(nft.transfer_from(CAROL, DAVE, DAVE, token)),
        ks721::ERR_WRONG_OWNER
    );
    nft.transfer_from(CAROL, BOB, DAVE, token).unwrap();
    assert_eq!(nft.owner_of(token), Some(DAVE));
    // The change of owner cleared the approval
    assert_eq!(nft.token(token).unwrap().approved, None);
    assert_eq!(
        revert_reason(nft.transfer_from(CAROL, DAVE, CAROL, token)),
        ks721::ERR_NOT_AUTHORIZED
    );
    // An operator, until its expiry
    mock::set_block_timestamp(1_000);
    nft.set_operator(DAVE, CAROL, true, Some(1_100)).unwrap();
    assert_eq!(
        take_events(nft).last().unwrap(),
        &encode(&("operator", DAVE, CAROL, true, Some(1_100u64)))
    );
    assert!(nft.is_operator(DAVE, CAROL));
    nft.transfer_from(CAROL, DAVE, ALICE, token).unwrap();
    nft.transfer(ALICE, DAVE, token).unwrap();
    mock::set_block_timestamp(1_100);
    assert!(!nft.is_operator(DAVE, CAROL));
    assert_eq!(
        nft.view::<(bool, Option<u64>)>("operator_info", &encode(&(DAVE, CAROL))),
        (true, Some(1_100))
    );
    assert_eq!(
        revert_reason(nft.transfer_from(CAROL, DAVE, CAROL, token)),
        ks721::ERR_NOT_AUTHORIZED
    );
    assert_eq!(nft.balance_of(DAVE), 1);
    check_listings(nft);
}

// ---------------------------------------------------------------------------
// The market

#[test]
fn list_unlist_and_relist() {
    let nft = setup(None);
    let token = nft.mint(ALICE, BOB, None).unwrap();
    mock::clear_events();
    // Only the owner lists, at a price above 0
    nft.approve(BOB, Some(CAROL), token).unwrap();
    nft.set_operator(BOB, DAVE, true, None).unwrap();
    mock::clear_events();
    for who in [CAROL, DAVE, ALICE] {
        assert_eq!(
            revert_reason(nft.list(who, token, KSA_ASSET_ID, 10)),
            ks721::ERR_NOT_THE_OWNER
        );
    }
    assert_eq!(
        revert_reason(nft.list(BOB, token, KSA_ASSET_ID, 0)),
        b"price must be positive"
    );
    assert_eq!(
        revert_reason(nft.list(BOB, id(9), KSA_ASSET_ID, 10)),
        ks721::ERR_UNKNOWN_TOKEN
    );
    nft.list(BOB, token, KSA_ASSET_ID, 10).unwrap();
    // Listing again replaces the listing
    nft.list(BOB, token, GEM, 7).unwrap();
    assert_eq!(
        take_events(nft),
        [
            encode(&("list", BOB, token, KSA_ASSET_ID, 10u128)),
            encode(&("list", BOB, token, GEM, 7u128)),
        ]
    );
    assert_eq!(
        nft.token(token).unwrap().listing,
        Some(Listing {
            asset: GEM,
            price: 7
        })
    );
    assert_eq!(ids(&nft.listings(0, 50)), [token]);
    check_listings(nft);
    assert_eq!(
        revert_reason(nft.unlist(DAVE, token)),
        ks721::ERR_NOT_THE_OWNER
    );
    nft.unlist(BOB, token).unwrap();
    assert_eq!(take_events(nft), [encode(&("unlist", BOB, token))]);
    assert_eq!(
        revert_reason(nft.unlist(BOB, token)),
        b"token is not listed"
    );
    assert!(nft.listings(0, 50).is_empty());
    check_listings(nft);
}

#[test]
fn buy_pays_the_seller_and_keeps_nothing() {
    let nft = setup(None);
    let token = nft.mint(ALICE, BOB, Some(&uri_of(1))).unwrap();
    nft.approve(BOB, Some(CAROL), token).unwrap();
    nft.list(BOB, token, KSA_ASSET_ID, 250).unwrap();
    mock::clear_events();
    nft.buy(DAVE, token, &[ksa(250)]).unwrap();
    assert_eq!(
        nft.token(token),
        Some(Token {
            id: token,
            owner: DAVE,
            uri: Some(uri_of(1)),
            approved: None,
            listing: None,
        })
    );
    // The sale takes the listing without an unlist
    assert_eq!(
        take_events(nft),
        [
            encode(&("transfer", BOB, DAVE, token)),
            encode(&("sale", BOB, DAVE, token, KSA_ASSET_ID, 250u128)),
        ]
    );
    assert_eq!(mock::balance(&KSA_ASSET_ID, &DAVE), FUNDS - 250);
    assert_eq!(mock::balance(&KSA_ASSET_ID, &BOB), FUNDS + 250);
    assert!(nft.listings(0, 50).is_empty());
    assert_eq!(nft.tokens_of(DAVE, 0, 100), [token]);
    assert!(nft.tokens_of(BOB, 0, 100).is_empty());
    check_listings(nft);
    // Sold on in another asset, to an operator of the seller, who buys like anyone else
    nft.set_operator(DAVE, CAROL, true, None).unwrap();
    nft.list(DAVE, token, GEM, 3).unwrap();
    nft.buy(CAROL, token, &[gem(3)]).unwrap();
    assert_eq!(nft.owner_of(token), Some(CAROL));
    assert_eq!(mock::balance(&GEM, &DAVE), FUNDS + 3);
    assert_eq!(mock::balance(&GEM, &CAROL), FUNDS - 3);
    check_listings(nft);
}

#[test]
fn buy_takes_exactly_the_price_in_the_listed_asset() {
    let nft = setup(None);
    let token = nft.mint(ALICE, BOB, None).unwrap();
    nft.list(BOB, token, KSA_ASSET_ID, 100).unwrap();
    for funds in [
        vec![],
        vec![ksa(99)],
        vec![ksa(101)],
        vec![gem(100)],
        vec![ksa(100), gem(1)],
    ] {
        assert_eq!(
            revert_reason(nft.buy(CAROL, token, &funds)),
            b"attach exactly the price",
            "{funds:?}"
        );
    }
    assert_eq!(
        revert_reason(nft.buy(BOB, token, &[ksa(100)])),
        b"buyer is the seller"
    );
    let other = nft.mint(ALICE, BOB, None).unwrap();
    assert_eq!(
        revert_reason(nft.buy(CAROL, other, &[ksa(100)])),
        b"token is not listed"
    );
    assert_eq!(
        revert_reason(nft.buy(CAROL, id(9), &[ksa(100)])),
        ks721::ERR_UNKNOWN_TOKEN
    );
    assert_eq!(nft.owner_of(token), Some(BOB));
    assert_eq!(mock::balance(&KSA_ASSET_ID, &CAROL), FUNDS);
    // The largest price there is, in an asset of its own
    const BIG: [u8; 32] = [0x7b; 32];
    mock::register_asset(BIG, "BIG", 0, [0x99; 32]);
    mock::mint(BIG, CAROL, u128::MAX).unwrap();
    nft.list(BOB, other, BIG, u128::MAX).unwrap();
    let price = Funds {
        asset: BIG,
        amount: u128::MAX,
    };
    nft.buy(CAROL, other, &[price]).unwrap();
    assert_eq!(mock::balance(&BIG, &BOB), u128::MAX);
    assert_eq!(mock::balance(&BIG, &CAROL), 0);
    check_listings(nft);
}

#[test]
fn a_change_of_owner_drops_the_listing() {
    let nft = setup(None);
    let tokens: Vec<TokenId> = (1..=5)
        .map(|_| nft.mint(ALICE, BOB, None).unwrap())
        .collect();
    for token in &tokens {
        nft.list(BOB, *token, KSA_ASSET_ID, 10).unwrap();
    }
    nft.approve(BOB, Some(DAVE), tokens[1]).unwrap();
    mock::clear_events();
    let moves: [(&str, Result<Vec<u8>, Failure>); 5] = [
        (
            "transfer",
            nft.call(BOB, "transfer", &encode(&(CAROL, tokens[0]))),
        ),
        (
            "transfer_from",
            nft.call(DAVE, "transfer_from", &encode(&(BOB, DAVE, tokens[1]))),
        ),
        (
            "transfer_and_call",
            nft.call(
                BOB,
                "transfer_and_call",
                &encode(&(SAFE, tokens[2], b"hi".to_vec())),
            ),
        ),
        ("burn", nft.call(BOB, "burn", &encode(&tokens[3]))),
        // Back with the account that listed it, a token is still not listed
        (
            "transfer back",
            nft.call(CAROL, "transfer", &encode(&(BOB, tokens[0]))),
        ),
    ];
    for (what, result) in moves {
        result.unwrap_or_else(|failure| panic!("{what}: {failure:?}"));
    }
    assert_eq!(
        take_events(nft),
        [
            encode(&("unlist", BOB, tokens[0])),
            encode(&("transfer", BOB, CAROL, tokens[0])),
            encode(&("unlist", BOB, tokens[1])),
            encode(&("transfer", BOB, DAVE, tokens[1])),
            encode(&("unlist", BOB, tokens[2])),
            encode(&("transfer", BOB, SAFE, tokens[2])),
            encode(&("unlist", BOB, tokens[3])),
            encode(&("transfer", BOB, ZERO, tokens[3])),
            encode(&("transfer", CAROL, BOB, tokens[0])),
        ]
    );
    assert_eq!(ids(&nft.listings(0, 50)), [tokens[4]]);
    assert_eq!(nft.token(tokens[0]).unwrap().listing, None);
    assert_eq!(
        revert_reason(nft.buy(DAVE, tokens[0], &[ksa(10)])),
        b"token is not listed"
    );
    check_listings(nft);
}

#[test]
fn every_own_method_but_buy_refuses_attached_funds() {
    let nft = setup(None);
    let token = nft.mint(ALICE, BOB, None).unwrap();
    nft.list(BOB, token, KSA_ASSET_ID, 10).unwrap();
    for (method, who, args) in [
        ("mint", ALICE, encode(&(BOB, None::<String>))),
        ("list", BOB, encode(&(token, KSA_ASSET_ID, 10u128))),
        ("unlist", BOB, encode(&token)),
        ("set_minter", ALICE, encode(&Some(BOB))),
        ("collection", BOB, Vec::new()),
        ("token", BOB, encode(&token)),
        ("details", BOB, encode(&(0u64, 10u32))),
        ("details_of", BOB, encode(&(BOB, 0u64, 10u32))),
        ("listings", BOB, encode(&(0u64, 10u32))),
        ("transfer", BOB, encode(&(CAROL, token))),
    ] {
        let result = mock::call_with_funds(who, nft.0, method, &args, &[ksa(1)]);
        assert_eq!(
            revert_reason(result),
            standard::ERR_UNEXPECTED_FUNDS,
            "{method}"
        );
    }
}

// ---------------------------------------------------------------------------
// Views and paging

#[test]
fn views_page_by_position() {
    let nft = setup(None);
    let tokens: Vec<TokenId> = (0..120)
        .map(|n| {
            let to = if n % 2 == 0 { BOB } else { CAROL };
            nft.mint(ALICE, to, Some(&"u".repeat(512))).unwrap()
        })
        .collect();
    // The details of a page of tokens, at most 50 at a time and within a call's output
    assert_eq!(ids(&nft.details(0, 1_000)), tokens[..50]);
    assert_eq!(ids(&nft.details(100, 50)), tokens[100..]);
    assert!(nft.details(120, 50).is_empty());
    assert!(nft.details(0, 0).is_empty());
    assert_eq!(nft.tokens(0, 1_000), tokens[..100]);
    let bobs: Vec<TokenId> = tokens.iter().step_by(2).copied().collect();
    assert_eq!(ids(&nft.details_of(BOB, 10, 5)), bobs[10..15]);
    assert_eq!(nft.tokens_of(BOB, 0, 100), bobs);
    // Every token listed: the listings page the same way
    for token in &tokens {
        let owner = nft.owner_of(*token).unwrap();
        nft.list(owner, *token, GEM, 1).unwrap();
    }
    assert_eq!(nft.listings(0, 1_000).len(), 50);
    assert_eq!(all(|start| nft.listings(start, 50)).len(), 120);
    // A burn moves the last token into the burned one's place
    nft.burn(BOB, tokens[0]).unwrap();
    assert_eq!(nft.details(0, 1)[0].id, tokens[119]);
    assert_eq!(nft.listings(0, 1)[0].id, tokens[119]);
    check_listings(nft);
}

#[test]
fn views_of_unknown_tokens_and_accounts() {
    let nft = setup(None);
    assert_eq!(nft.token(id(1)), None);
    assert_eq!(nft.owner_of(id(1)), None);
    assert_eq!(nft.balance_of(BOB), 0);
    assert!(nft.details_of(BOB, 0, 50).is_empty());
    assert!(nft.listings(0, 50).is_empty());
    assert!(!nft.is_operator(BOB, CAROL));
    for (method, args) in [
        ("burn", encode(&id(1))),
        ("transfer", encode(&(BOB, id(1)))),
        ("unlist", encode(&id(1))),
    ] {
        assert_eq!(
            revert_reason(nft.call(BOB, method, &args)),
            ks721::ERR_UNKNOWN_TOKEN,
            "{method}"
        );
    }
}

// ---------------------------------------------------------------------------
// Random operations

/// What the random test expects the collection to hold.
#[derive(Default)]
struct Model {
    minted: u128,
    owners: BTreeMap<TokenId, [u8; 32]>,
    approved: BTreeMap<TokenId, [u8; 32]>,
    listings: BTreeMap<TokenId, Listing>,
    operators: BTreeSet<([u8; 32], [u8; 32])>,
}

impl Model {
    fn may_move(&self, token_id: &TokenId, who: [u8; 32]) -> bool {
        self.owners.get(token_id).is_some_and(|owner| {
            *owner == who
                || self.approved.get(token_id) == Some(&who)
                || self.operators.contains(&(*owner, who))
        })
    }

    fn may_approve(&self, token_id: &TokenId, who: [u8; 32]) -> bool {
        self.owners
            .get(token_id)
            .is_some_and(|owner| *owner == who || self.operators.contains(&(*owner, who)))
    }

    fn give(&mut self, token_id: TokenId, to: [u8; 32]) {
        self.owners.insert(token_id, to);
        self.approved.remove(&token_id);
        self.listings.remove(&token_id);
    }

    fn burn(&mut self, token_id: &TokenId) {
        self.owners.remove(token_id);
        self.approved.remove(token_id);
        self.listings.remove(token_id);
    }

    /// The collection agrees with the model on every token and every operator.
    fn check(&self, nft: Nft) {
        let collection = nft.collection();
        assert_eq!(
            (u128::from(collection.minted), collection.supply),
            (self.minted, self.owners.len() as u64)
        );
        for n in 0..=self.minted + 1 {
            let token_id = id(n);
            let expected = self.owners.get(&token_id).map(|owner| Token {
                id: token_id,
                owner: *owner,
                uri: Some(uri_of(n)),
                approved: self.approved.get(&token_id).copied(),
                listing: self.listings.get(&token_id).copied(),
            });
            assert_eq!(nft.token(token_id), expected, "token {n}");
        }
        for owner in ACCOUNTS {
            for operator in ACCOUNTS {
                assert_eq!(
                    nft.is_operator(owner, operator),
                    self.operators.contains(&(owner, operator))
                );
            }
        }
    }
}

#[test]
fn random_operations_keep_the_listings_consistent() {
    let nft = setup(None);
    let mut model = Model::default();
    let mut succeeded: BTreeMap<&str, u32> = BTreeMap::new();
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = |bound: u64| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) % bound
    };
    for _ in 0..1_000 {
        let op = next(17);
        // Mostly a live token (a listed one to unlist or buy), now and then any ID, burned and unknown ones too
        let pool: Vec<TokenId> = if op >= 14 {
            model.listings.keys().copied().collect()
        } else {
            model.owners.keys().copied().collect()
        };
        let token_id = if !pool.is_empty() && next(5) != 0 {
            pool[next(pool.len() as u64) as usize]
        } else {
            id(u128::from(next(model.minted as u64 + 2)))
        };
        let owner = model.owners.get(&token_id).copied();
        // Mostly the owner for what owners do, otherwise anyone
        let who = match owner {
            Some(owner) if op < 15 && next(2) == 0 => owner,
            _ => ACCOUNTS[next(4) as usize],
        };
        let other = ACCOUNTS[next(4) as usize];
        let (what, expected, result) = match op {
            0..=2 => {
                let minter = if next(8) == 0 { who } else { ALICE };
                let result = nft.mint(minter, other, Some(&uri_of(model.minted + 1)));
                let expected = minter == ALICE;
                if expected {
                    model.minted += 1;
                    assert_eq!(result, Ok(id(model.minted)));
                    model.owners.insert(id(model.minted), other);
                }
                ("mint", expected, result.map(|_| ()))
            }
            3 | 4 => {
                let expected = owner == Some(who) && owner != Some(other);
                let result = nft.transfer(who, other, token_id);
                if expected {
                    model.give(token_id, other);
                }
                ("transfer", expected, result)
            }
            5 | 6 => {
                // Mostly the owner as `from`, now and then another account
                let from = match owner {
                    Some(owner) if next(4) != 0 => owner,
                    _ => ACCOUNTS[next(4) as usize],
                };
                let expected =
                    owner == Some(from) && model.may_move(&token_id, who) && owner != Some(other);
                let result = nft.transfer_from(who, from, other, token_id);
                if expected {
                    model.give(token_id, other);
                }
                ("transfer_from", expected, result)
            }
            7 | 8 => {
                let expected = model.may_move(&token_id, who);
                let result = nft.burn(who, token_id);
                if expected {
                    model.burn(&token_id);
                }
                ("burn", expected, result)
            }
            9 | 10 => {
                let spender = if next(3) == 0 { None } else { Some(other) };
                let expected = model.may_approve(&token_id, who) && spender != owner;
                let result = nft.approve(who, spender, token_id);
                if expected {
                    match spender {
                        Some(spender) => model.approved.insert(token_id, spender),
                        None => model.approved.remove(&token_id),
                    };
                }
                ("approve", expected, result)
            }
            11 => {
                let approved = next(3) != 0;
                let expected = who != other;
                let result = nft.set_operator(who, other, approved, None);
                if expected && approved {
                    model.operators.insert((who, other));
                } else if expected {
                    model.operators.remove(&(who, other));
                }
                ("set_operator", expected, result)
            }
            12 | 13 => {
                let asset = if next(2) == 0 { KSA_ASSET_ID } else { GEM };
                let price = u128::from(next(5)) * 10;
                let expected = owner == Some(who) && price > 0;
                let result = nft.list(who, token_id, asset, price);
                if expected {
                    model.listings.insert(token_id, Listing { asset, price });
                }
                ("list", expected, result)
            }
            14 => {
                let expected = owner == Some(who) && model.listings.contains_key(&token_id);
                let result = nft.unlist(who, token_id);
                if expected {
                    model.listings.remove(&token_id);
                }
                ("unlist", expected, result)
            }
            _ => {
                let listing = model.listings.get(&token_id).copied();
                // The exact price, or now and then one unit more
                let extra = u128::from(next(4) == 0);
                let funds = listing.map_or_else(
                    || vec![ksa(10)],
                    |listing| {
                        vec![Funds {
                            asset: listing.asset,
                            amount: listing.price + extra,
                        }]
                    },
                );
                let expected = listing.is_some() && owner != Some(who) && extra == 0;
                let result = nft.buy(who, token_id, &funds);
                if expected {
                    model.give(token_id, who);
                }
                ("buy", expected, result)
            }
        };
        match &result {
            Ok(()) => assert!(expected, "{what} succeeded"),
            Err(Failure::Revert { reason, .. }) => assert!(
                !expected,
                "{what} reverted: {}",
                String::from_utf8_lossy(reason)
            ),
            Err(Failure::Trap(reason)) => panic!("{what} trapped: {reason}"),
        }
        if expected {
            *succeeded.entry(what).or_default() += 1;
        }
        model.check(nft);
        check_listings(nft);
        // Sales only move the assets between the accounts
        for asset in [KSA_ASSET_ID, GEM] {
            let total: u128 = ACCOUNTS.iter().map(|who| mock::balance(&asset, who)).sum();
            assert_eq!(total, 4 * FUNDS);
        }
    }
    // Every kind of operation succeeded often enough to matter
    for what in [
        "mint",
        "transfer",
        "transfer_from",
        "burn",
        "approve",
        "set_operator",
        "list",
        "unlist",
        "buy",
    ] {
        let count = succeeded.get(what).copied().unwrap_or(0);
        assert!(count >= 10, "{what}: {succeeded:?}");
    }
}

// ---------------------------------------------------------------------------
// The ABI block

const SOURCE: &str = include_str!("../src/lib.rs");

/// The lines of the ABI block of the module docs, without the `//!`.
fn abi_block() -> Vec<&'static str> {
    let mut lines = SOURCE
        .lines()
        .map(|line| line.trim_start().trim_start_matches("//!").trim());
    assert!(lines.any(|line| line == "```abi"), "no abi block");
    lines.take_while(|line| *line != "```").collect()
}

/// The method names the ABI block declares (its `init`, `call` and `view` lines) and its event names.
fn declarations() -> (BTreeSet<String>, BTreeSet<String>) {
    let name = |rest: &str| rest.split('(').next().unwrap().trim().to_string();
    let mut methods = BTreeSet::new();
    let mut events = BTreeSet::new();
    for line in abi_block() {
        let declaration = line.split("//").next().unwrap().trim();
        let declaration = declaration.strip_prefix("payable ").unwrap_or(declaration);
        if declaration.starts_with("init(") {
            methods.insert("init".to_string());
        } else if let Some(rest) = declaration
            .strip_prefix("call ")
            .or_else(|| declaration.strip_prefix("view "))
        {
            methods.insert(name(rest));
        } else if let Some(rest) = declaration.strip_prefix("event ") {
            events.insert(name(rest));
        }
    }
    (methods, events)
}

/// Every method a collection on `ks721` with both extensions answers: the standard's, besides the collection's own.
const STANDARD_METHODS: [&str; 18] = [
    "standards",
    "metadata",
    "capabilities",
    "owner_of",
    "balance_of",
    "token_uri",
    "approved",
    "is_operator",
    "operator_info",
    "transfer",
    "transfer_from",
    "approve",
    "set_operator",
    "transfer_and_call",
    "burn",
    "total_supply",
    "tokens",
    "tokens_of",
];

#[test]
fn abi_block_matches_dispatch_and_events() {
    let (methods, events) = declarations();
    // The collection's own methods, its init among them, and the standard's
    let own: BTreeSet<String> = [
        "init",
        "mint",
        "list",
        "unlist",
        "buy",
        "set_minter",
        "collection",
        "token",
        "details",
        "details_of",
        "listings",
    ]
    .map(String::from)
    .into();
    let expected: BTreeSet<String> = own
        .iter()
        .cloned()
        .chain(STANDARD_METHODS.map(String::from))
        .collect();
    assert_eq!(methods, expected);
    // Every declared method is answered, never with `unknown method` (here without arguments: a malformed-arguments
    // revert for most)
    let nft = setup(None);
    for method in &methods {
        let result = mock::with_context(ALICE, nft.0, || nft::dispatch(method, &[]));
        assert_ne!(result, Err(b"unknown method".to_vec()), "{method}");
    }
    for method in ["burn_from", "allowance", "on_nft_received"] {
        let result = mock::with_context(ALICE, nft.0, || nft::dispatch(method, &[]));
        assert_eq!(result, Err(b"unknown method".to_vec()), "{method}");
    }
    // Every event of a deployment, a mint, an approval, an operator, a listing, a sale, a transfer that drops a
    // listing and a burn is declared, and each declared event shows up
    mock::reset();
    mock::register_asset(KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::mint(KSA_ASSET_ID, CAROL, FUNDS).unwrap();
    let nft = deploy(0, "Ksana Cats", "KCAT", None).unwrap();
    let token = nft.mint(ALICE, BOB, None).unwrap();
    nft.approve(BOB, Some(CAROL), token).unwrap();
    nft.set_operator(BOB, DAVE, true, None).unwrap();
    nft.list(BOB, token, KSA_ASSET_ID, 5).unwrap();
    nft.buy(CAROL, token, &[ksa(5)]).unwrap();
    nft.list(CAROL, token, KSA_ASSET_ID, 5).unwrap();
    nft.transfer(CAROL, DAVE, token).unwrap();
    nft.burn(DAVE, token).unwrap();
    nft.set_minter(ALICE, None).unwrap();
    let seen: BTreeSet<String> = mock::events()
        .iter()
        .map(|event| decode_name(&event.data))
        .collect();
    assert_eq!(seen, events);
}

/// The name of an event: the first string of its tuple.
fn decode_name(data: &[u8]) -> String {
    let mut input = data;
    String::decode(&mut input).unwrap()
}
