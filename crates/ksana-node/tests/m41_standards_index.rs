//! M41: the node's index of the token standards' events (D-120, `docs/STANDARDS.md`): the declared contracts, the
//! balances and holders of KS-20 tokens, the owners of KS-721 tokens and the approvals, read from the receipts of the
//! main chain's blocks without running the VM.
//!
//! - The reference token, a collection and a receiving contract: the index answers what the contracts' own views
//!   answer, through the backend and over JSON-RPC (the kinds of assets, the cursors, paging and parameter checks).
//! - The operation listings name the accounts the standard events name and the contracts that emitted them.
//! - A reorg takes the events of the disconnected blocks back out in the same write as the main chain: the index then
//!   equals the one of a node that never saw them, and one built from scratch.
//! - A data directory without the standards index (from a version of D-111, or one such a version wrote to after a
//!   newer one) gets it built at startup, the same as the index kept as the blocks came.

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use borsh::{BorshDeserialize, BorshSerialize};
use common::side::{setup, Side};
use common::*;
use jsonrpsee::core::client::ClientT;
use jsonrpsee::core::params::ArrayParams;
use jsonrpsee::core::RpcResult;
use jsonrpsee::http_client::HttpClientBuilder;
use jsonrpsee::rpc_params;
use ksana_btc::mock::MockBtc;
use ksana_crypto::sign_vmop;
use ksana_mempool::{Mempool, Policy};
use ksana_node::chain::Chain;
use ksana_node::genesis::GenesisFile;
use ksana_node::producer::FeePolicy;
use ksana_node::rpc::NodeRpc;
use ksana_node::store::ChainStore;
use ksana_primitives::hash::sha256;
use ksana_primitives::{
    Action, Block, FeeMode, SigScheme, VmOp, DEVNET, KSA_ASSET_ID, VMOP_VERSION,
};
use ksana_rpc::{
    codes, AccountAssetJson, ApprovalJson, Backend, CallRequestJson, Cors, HolderJson,
    KsanaApiClient, OpPosition,
};
use ksana_state::rocks::DbMemory;
use ksana_test_contracts::{COLLECTION, RECEIVER, TOKEN};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;

/// An expiry no block of the tests reaches.
const FAR: u64 = 1 << 62;

fn signed(u: &User, nonce: u64, max_gas: u64, action: Action) -> VmOp {
    let mut op = VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account: u.account,
        nonce,
        fee: FeeMode::Token { amount: 10 },
        max_gas,
        action,
        sig: SigScheme::Bip340([0; 64]),
    };
    op.sig = sign_vmop(&op, &u.kp, &[7; 32]).unwrap();
    op
}

/// The op deploying `code` with `init_args`, and the contract's ID.
fn deploy(u: &User, nonce: u64, code: &[u8], init_args: Vec<u8>) -> (VmOp, [u8; 32]) {
    let contract = ksana_execution::builtin::contract_id(&u.account, nonce, &sha256(code));
    let action = Action::Deploy {
        code: code.to_vec(),
        init_args,
        funds: Vec::new(),
    };
    (signed(u, nonce, 3_000_000, action), contract)
}

fn call(u: &User, nonce: u64, contract: [u8; 32], method: &str, args: impl BorshSerialize) -> VmOp {
    let action = Action::Call {
        contract,
        method: method.into(),
        args: borsh::to_vec(&args).unwrap(),
        funds: Vec::new(),
    };
    signed(u, nonce, 1_000_000, action)
}

/// The arguments of the reference token's `init`: `(name, symbol, decimals, supply, minter, cap)`.
fn token_init(supply: u128, minter: Option<[u8; 32]>) -> Vec<u8> {
    borsh::to_vec(&(
        "Ksana Coin".to_string(),
        "KCN".to_string(),
        8u8,
        supply,
        minter,
        Some(2_000_000u128),
    ))
    .unwrap()
}

fn collection_init() -> Vec<u8> {
    borsh::to_vec(&("Ksana Art".to_string(), "KART".to_string())).unwrap()
}

/// The token ID of serial number `n` (`ks721::id`).
fn id(n: u128) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[16..].copy_from_slice(&n.to_be_bytes());
    out
}

/// This instance's RPC backend (no server started, called directly).
fn rpc(side: &Side) -> NodeRpc {
    rpc_for(&side.chain)
}

/// An RPC backend of `chain` with an entry pool of its own: the queries here read the chain alone.
fn rpc_for(chain: &Arc<Chain>) -> NodeRpc {
    NodeRpc {
        chain: Arc::clone(chain),
        mempool: Arc::new(Mutex::new(Mempool::new(DEVNET, Policy::default()))),
        sync: None,
        paused: Arc::new(AtomicBool::new(false)),
        events: broadcast::channel(16).0,
        relay: None,
        fees: FeePolicy::new(1),
        calls: AtomicUsize::new(0),
    }
}

/// Submits `ops` and produces blocks until none of them waits in the entry pool; each must have succeeded. Returns the
/// blocks produced.
fn include(side: &Side, ops: &[VmOp]) -> Vec<Block> {
    for op in ops {
        side.submit(op.clone());
    }
    let mut blocks = Vec::new();
    while ops.iter().any(|op| side.pending(op)) {
        blocks.push(side.mine());
        assert!(blocks.len() < 20, "the ops were not included");
    }
    let rpc = rpc(side);
    for op in ops {
        let status = rpc.op_status(op.op_id().unwrap()).unwrap();
        let receipt = status.receipt.expect("included");
        assert!(receipt.success, "{:?}", receipt.error);
    }
    blocks
}

/// What a view of `contract` answers.
fn view<T: BorshDeserialize>(
    rpc: &NodeRpc,
    contract: [u8; 32],
    method: &str,
    args: impl BorshSerialize,
) -> T {
    let receipt = rpc
        .call(CallRequestJson {
            contract,
            method: method.into(),
            args: borsh::to_vec(&args).unwrap(),
            funds: Vec::new(),
            caller: None,
            max_gas: None,
        })
        .unwrap();
    assert!(receipt.success, "{method}: {:?}", receipt.error);
    borsh::from_slice(&receipt.output).unwrap()
}

/// A listing in one page, also read one item at a time: the pages must join up into the same listing.
fn pages<T: PartialEq + std::fmt::Debug>(
    mut page: impl FnMut(Option<Vec<u8>>, usize) -> (Vec<T>, Option<String>),
) -> Vec<T> {
    let (all, next) = page(None, 500);
    assert_eq!(next, None);
    let mut joined = Vec::new();
    let mut after = None;
    loop {
        let (items, next) = page(after, 1);
        assert!(items.len() <= 1);
        joined.extend(items);
        match next {
            Some(next) => after = Some(hex::decode(next).unwrap()),
            None => break,
        }
    }
    assert_eq!(joined, all);
    all
}

fn assets_of(rpc: &NodeRpc, account: [u8; 32]) -> Vec<AccountAssetJson> {
    pages(|after, limit| {
        let page = rpc.account_assets(account, after, limit).unwrap();
        (page.assets, page.next)
    })
}

fn holders_of(rpc: &NodeRpc, contract: [u8; 32]) -> Vec<([u8; 32], u128)> {
    pages(|after, limit| {
        let page = rpc.token_holders(contract, after, limit).unwrap();
        (page.holders, page.next)
    })
    .into_iter()
    .map(|HolderJson { account, balance }| (account, balance))
    .collect()
}

fn nfts_of(
    rpc: &NodeRpc,
    account: [u8; 32],
    contract: Option<[u8; 32]>,
) -> Vec<([u8; 32], [u8; 32])> {
    pages(|after, limit| {
        let page = rpc.nfts(account, contract, after, limit).unwrap();
        (page.nfts, page.next)
    })
    .into_iter()
    .map(|nft| (nft.contract, nft.token_id))
    .collect()
}

fn approvals_of(rpc: &NodeRpc, account: [u8; 32]) -> Vec<ApprovalJson> {
    pages(|after, limit| {
        let page = rpc.approvals(account, after, limit).unwrap();
        (page.approvals, page.next)
    })
}

/// The ops listed under `account`, by ID.
fn listed(rpc: &NodeRpc, account: [u8; 32]) -> Vec<[u8; 32]> {
    rpc.account_ops(account, None, 500)
        .unwrap()
        .ops
        .into_iter()
        .map(|op| op.op_id)
        .collect()
}

/// A result as JSON, or its error code.
fn answer<T: Serialize>(result: RpcResult<T>) -> Value {
    match result {
        Ok(value) => serde_json::to_value(value).unwrap(),
        Err(e) => json!({ "error": e.code() }),
    }
}

/// Everything the index answers about `accounts` and `contracts`, and the ops listed under them (without their
/// statuses, which follow BTC).
fn answers(rpc: &NodeRpc, accounts: &[[u8; 32]], contracts: &[[u8; 32]]) -> Value {
    let mut out = serde_json::Map::new();
    for contract in contracts {
        let key = hex::encode(contract);
        out.insert(format!("contract {key}"), answer(rpc.contract(*contract)));
        out.insert(format!("token {key}"), answer(rpc.token(*contract)));
        out.insert(
            format!("holders {key}"),
            answer(rpc.token_holders(*contract, None, 500)),
        );
    }
    for account in accounts.iter().chain(contracts) {
        let key = hex::encode(account);
        out.insert(
            format!("assets {key}"),
            answer(rpc.account_assets(*account, None, 500)),
        );
        out.insert(
            format!("nfts {key}"),
            answer(rpc.nfts(*account, None, None, 500)),
        );
        out.insert(
            format!("approvals {key}"),
            answer(rpc.approvals(*account, None, 500)),
        );
        let ops = rpc.account_ops(*account, None, 500).unwrap();
        out.insert(
            format!("ops {key}"),
            ops.ops
                .iter()
                .map(|op| format!("{} {}", op.position, hex::encode(op.op_id)))
                .collect(),
        );
    }
    Value::Object(out)
}

/// The users and contracts of [`scenario`].
struct World {
    alice: User,
    bob: User,
    carol: User,
    token: [u8; 32],
    collection: [u8; 32],
    receiver: [u8; 32],
    /// The ops deploying the token, and alice's transfers to bob and of the receiver's tokens.
    deploy_token: VmOp,
    to_bob: VmOp,
    token_to_receiver: VmOp,
    nft_to_receiver: VmOp,
    approve_carol: VmOp,
}

impl World {
    fn accounts(&self) -> [[u8; 32]; 3] {
        [self.alice.account, self.bob.account, self.carol.account]
    }

    fn contracts(&self) -> [[u8; 32]; 3] {
        [self.token, self.collection, self.receiver]
    }
}

/// The reference token, a collection and a receiving contract, and what their users do with them, in five rounds of
/// blocks (each round needs the one before). In the end:
///
/// - the token: alice 999 580, bob 290, carol 45, the receiver 100 (supply 1 000 015); alice allowed carol 50 (carol
///   took 20 of it) and gave bob an allowance she revoked;
/// - the collection: #1 carol's, #2 burned, #3 the receiver's, #4 to #6 bob's; bob made carol his operator and
///   approved her for #4 (and for #1, which she then moved).
fn scenario(name: &str) -> (Side, World, Arc<MockBtc>, GenesisFile) {
    let (alice, bob, carol) = (
        user(&format!("{name}/alice")),
        user(&format!("{name}/bob")),
        user(&format!("{name}/carol")),
    );
    let (mock, genesis) = setup(&[(&alice, 1_000_000), (&bob, 1_000_000), (&carol, 1_000_000)]);
    let side = Side::new(&mock, &genesis, name);
    let (deploy_token, token) =
        deploy(&alice, 0, TOKEN, token_init(1_000_000, Some(alice.account)));
    let (deploy_collection, collection) = deploy(&alice, 1, COLLECTION, collection_init());
    let (deploy_receiver, receiver) = deploy(&alice, 2, RECEIVER, Vec::new());
    include(
        &side,
        &[deploy_token.clone(), deploy_collection, deploy_receiver],
    );

    let to_bob = call(&alice, 3, token, "transfer", (bob.account, 300u128));
    let approve_carol = call(
        &alice,
        4,
        token,
        "approve",
        (carol.account, 50u128, Some(FAR)),
    );
    let token_to_receiver = call(
        &alice,
        7,
        token,
        "transfer_and_call",
        (receiver, 100u128, b"hi".to_vec()),
    );
    let mut round = vec![
        to_bob.clone(),
        approve_carol.clone(),
        call(&alice, 5, token, "approve", (bob.account, 5u128, Some(FAR))),
        call(
            &alice,
            6,
            token,
            "approve",
            (bob.account, 0u128, None::<u64>),
        ),
        token_to_receiver.clone(),
        call(&alice, 8, token, "mint", (carol.account, 25u128)),
    ];
    for (nonce, (to, serial, uri)) in (9u64..).zip([
        (bob.account, 1u128, None),
        (alice.account, 2, None),
        (bob.account, 3, Some("ipfs://three".to_string())),
        (bob.account, 4, None),
        (bob.account, 5, None),
        (bob.account, 6, None),
    ]) {
        round.push(call(&alice, nonce, collection, "mint", (to, serial, uri)));
    }
    include(&side, &round);

    include(
        &side,
        &[
            call(&bob, 0, token, "burn", 10u128),
            call(&bob, 1, collection, "approve", (Some(carol.account), id(1))),
            call(
                &bob,
                2,
                collection,
                "set_operator",
                (carol.account, true, None::<u64>),
            ),
            call(&bob, 3, collection, "approve", (Some(carol.account), id(4))),
            call(
                &carol,
                0,
                token,
                "transfer_from",
                (alice.account, carol.account, 20u128),
            ),
        ],
    );
    include(
        &side,
        &[
            call(
                &carol,
                1,
                collection,
                "transfer_from",
                (bob.account, alice.account, id(3)),
            ),
            call(
                &carol,
                2,
                collection,
                "transfer_from",
                (bob.account, carol.account, id(1)),
            ),
        ],
    );
    let nft_to_receiver = call(
        &alice,
        16,
        collection,
        "transfer_and_call",
        (receiver, id(3), Vec::<u8>::new()),
    );
    include(
        &side,
        &[
            call(&alice, 15, collection, "burn", id(2)),
            nft_to_receiver.clone(),
        ],
    );
    let world = World {
        alice,
        bob,
        carol,
        token,
        collection,
        receiver,
        deploy_token,
        to_bob,
        token_to_receiver,
        nft_to_receiver,
        approve_carol,
    };
    (side, world, mock, genesis)
}

/// The built-in assets an account holds, as `ksana_getAccount` lists them, followed by the tokens and collections in
/// contract order.
fn expected_assets(
    rpc: &NodeRpc,
    account: [u8; 32],
    mut contracts: Vec<AccountAssetJson>,
) -> Vec<AccountAssetJson> {
    let mut assets: Vec<AccountAssetJson> = rpc
        .account(account)
        .unwrap()
        .balances
        .into_iter()
        .map(|b| AccountAssetJson::Asset {
            asset: b.asset,
            balance: b.amount,
        })
        .collect();
    contracts.sort_by_key(|asset| match asset {
        AccountAssetJson::Token { contract, .. } | AccountAssetJson::Nft { contract, .. } => {
            *contract
        }
        AccountAssetJson::Asset { asset, .. } => *asset,
    });
    assets.extend(contracts);
    assets
}

/// Holders, the largest amount first, the same amount by account from the highest.
fn by_amount(mut holders: Vec<([u8; 32], u128)>) -> Vec<([u8; 32], u128)> {
    holders.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
    holders
}

#[test]
fn the_index_answers_what_the_contracts_views_answer() {
    let (side, w, _mock, _genesis) = scenario("m41");
    let rpc = rpc(&side);
    let (alice, bob, carol) = (w.alice.account, w.bob.account, w.carol.account);
    let (token, collection, receiver) = (w.token, w.collection, w.receiver);

    // The contracts: code hash, deployer and declaration
    let contract = rpc.contract(token).unwrap();
    assert_eq!(contract.code_hash, sha256(TOKEN));
    assert_eq!(contract.deployer, alice);
    assert_eq!(
        contract.standards,
        ["KS-20", "KS-20-burnable", "KS-20-mintable"]
    );
    assert_eq!(
        contract.standards,
        view::<Vec<String>>(&rpc, token, "standards", ())
    );
    let deployed_at = rpc
        .account_ops(alice, None, 500)
        .unwrap()
        .ops
        .into_iter()
        .find(|op| op.op_id == w.deploy_token.op_id().unwrap())
        .unwrap()
        .position;
    assert_eq!(contract.declared, Some(deployed_at));
    assert_eq!(
        rpc.contract(collection).unwrap().standards,
        ["KS-721", "KS-721-burnable", "KS-721-enumerable"]
    );
    let plain = rpc.contract(receiver).unwrap();
    assert_eq!(plain.code_hash, sha256(RECEIVER));
    assert!(plain.standards.is_empty());
    assert_eq!(plain.declared, None);
    for (account, what) in [(bob, "a user"), ([0x77; 32], "nothing")] {
        let refused = rpc.contract(account).expect_err(what);
        assert_eq!(refused.code(), codes::NOT_FOUND, "{what}");
    }

    // The token: its metadata and supply as its views give them
    let t = rpc.token(token).unwrap();
    assert_eq!(t.standard, "KS-20");
    assert_eq!(t.extensions, ["KS-20-burnable", "KS-20-mintable"]);
    let (name, symbol, decimals) = view::<(String, String, u8)>(&rpc, token, "metadata", ());
    assert_eq!(
        (t.name.as_deref(), t.symbol.as_deref(), t.decimals),
        (Some(name.as_str()), Some(symbol.as_str()), Some(decimals))
    );
    assert_eq!(t.supply, 1_000_015);
    assert_eq!(t.supply, view::<u128>(&rpc, token, "total_supply", ()));
    assert_eq!(t.holders, 4);
    assert_eq!(t.declared, deployed_at);
    assert!(!t.irregular);
    let c = rpc.token(collection).unwrap();
    assert_eq!(c.standard, "KS-721");
    assert_eq!(
        (c.name.as_deref(), c.symbol.as_deref(), c.decimals),
        (Some("Ksana Art"), Some("KART"), None)
    );
    assert_eq!(c.supply, 5);
    assert_eq!(
        c.supply,
        u128::from(view::<u64>(&rpc, collection, "total_supply", ()))
    );
    assert_eq!(c.holders, 3);
    assert!(!c.irregular);
    for (account, what) in [(receiver, "no declaration"), ([0x77; 32], "nothing")] {
        let refused = rpc.token(account).expect_err(what);
        assert_eq!(refused.code(), codes::NOT_FOUND, "{what}");
        let refused = rpc.token_holders(account, None, 10).expect_err(what);
        assert_eq!(refused.code(), codes::NOT_FOUND, "{what}");
    }

    // Holders, as the views give the balances
    let token_holders = holders_of(&rpc, token);
    assert_eq!(
        token_holders,
        by_amount(vec![
            (alice, 999_580),
            (bob, 290),
            (carol, 45),
            (receiver, 100)
        ])
    );
    for &(account, balance) in &token_holders {
        assert_eq!(view::<u128>(&rpc, token, "balance_of", account), balance);
    }
    let nft_holders = holders_of(&rpc, collection);
    assert_eq!(
        nft_holders,
        by_amount(vec![(bob, 3), (carol, 1), (receiver, 1)])
    );
    for &(account, count) in &nft_holders {
        let held = view::<u64>(&rpc, collection, "balance_of", account);
        assert_eq!(u128::from(held), count);
    }
    assert_eq!(view::<u64>(&rpc, collection, "balance_of", alice), 0);

    // NFTs, as the owners the views give
    assert_eq!(nfts_of(&rpc, carol, None), [(collection, id(1))]);
    assert_eq!(
        nfts_of(&rpc, receiver, Some(collection)),
        [(collection, id(3))]
    );
    assert_eq!(
        nfts_of(&rpc, bob, Some(collection)),
        [
            (collection, id(4)),
            (collection, id(5)),
            (collection, id(6))
        ]
    );
    assert!(nfts_of(&rpc, alice, None).is_empty());
    assert!(nfts_of(&rpc, bob, Some(token)).is_empty());
    for (serial, owner) in [
        (1, Some(carol)),
        (2, None),
        (3, Some(receiver)),
        (4, Some(bob)),
        (5, Some(bob)),
        (6, Some(bob)),
    ] {
        let owns = view::<Option<[u8; 32]>>(&rpc, collection, "owner_of", id(serial));
        assert_eq!(owns, owner, "#{serial}");
    }

    // Approvals as set: the allowance keeps the amount set (what is left is the view's), the revoked one is gone, and
    // the move of #1 cleared its approval
    assert_eq!(
        approvals_of(&rpc, alice),
        [ApprovalJson::Allowance {
            contract: token,
            spender: carol,
            amount: 50,
            expires_at: Some(FAR),
        }]
    );
    assert_eq!(view::<u128>(&rpc, token, "allowance", (alice, carol)), 30);
    assert_eq!(view::<u128>(&rpc, token, "allowance", (alice, bob)), 0);
    assert_eq!(
        approvals_of(&rpc, bob),
        [
            ApprovalJson::Operator {
                contract: collection,
                operator: carol,
                expires_at: None,
            },
            ApprovalJson::Token {
                contract: collection,
                token_id: id(4),
                spender: carol,
            },
        ]
    );
    assert!(view::<bool>(&rpc, collection, "is_operator", (bob, carol)));
    assert_eq!(
        view::<Option<[u8; 32]>>(&rpc, collection, "approved", id(4)),
        Some(carol)
    );
    assert_eq!(
        view::<Option<[u8; 32]>>(&rpc, collection, "approved", id(1)),
        None
    );
    assert!(approvals_of(&rpc, carol).is_empty());

    // Every kind of asset in one listing: the built-in ones first, then the contracts by ID
    assert_eq!(
        assets_of(&rpc, carol),
        expected_assets(
            &rpc,
            carol,
            vec![
                AccountAssetJson::Token {
                    contract: token,
                    balance: 45
                },
                AccountAssetJson::Nft {
                    contract: collection,
                    count: 1
                },
            ]
        )
    );
    assert_eq!(
        assets_of(&rpc, bob),
        expected_assets(
            &rpc,
            bob,
            vec![
                AccountAssetJson::Token {
                    contract: token,
                    balance: 290
                },
                AccountAssetJson::Nft {
                    contract: collection,
                    count: 3
                },
            ]
        )
    );
    assert_eq!(
        assets_of(&rpc, alice),
        expected_assets(
            &rpc,
            alice,
            vec![AccountAssetJson::Token {
                contract: token,
                balance: 999_580
            }]
        )
    );
    let receiver_assets = assets_of(&rpc, receiver);
    assert_eq!(
        receiver_assets,
        expected_assets(
            &rpc,
            receiver,
            vec![
                AccountAssetJson::Token {
                    contract: token,
                    balance: 100
                },
                AccountAssetJson::Nft {
                    contract: collection,
                    count: 1
                },
            ]
        )
    );
    assert!(!receiver_assets
        .iter()
        .any(|a| matches!(a, AccountAssetJson::Asset { .. })));
    assert!(matches!(
        assets_of(&rpc, alice).first(),
        Some(AccountAssetJson::Asset { asset, .. }) if *asset == KSA_ASSET_ID
    ));

    // The listings: an op is listed under the accounts its standard events name and the contract that emitted them
    let id_of = |op: &VmOp| op.op_id().unwrap();
    assert!(listed(&rpc, bob).contains(&id_of(&w.to_bob)));
    assert!(listed(&rpc, carol).contains(&id_of(&w.approve_carol)));
    let receiver_ops = listed(&rpc, receiver);
    assert!(receiver_ops.contains(&id_of(&w.token_to_receiver)));
    assert!(receiver_ops.contains(&id_of(&w.nft_to_receiver)));
    assert!(listed(&rpc, collection).contains(&id_of(&w.nft_to_receiver)));
    // A deployment stays off the listing of the contract it deploys (D-111), though its `init` declared and minted
    let token_ops = listed(&rpc, token);
    assert!(!token_ops.contains(&id_of(&w.deploy_token)));
    assert!(token_ops.contains(&id_of(&w.to_bob)));
    assert!(listed(&rpc, w.alice.account).contains(&id_of(&w.deploy_token)));
}

/// Over JSON-RPC: the JSON forms, the cursors, and refusals of malformed parameters.
#[test]
fn the_index_over_json_rpc() {
    let (side, w, _mock, _genesis) = scenario("m41/rpc");
    let backend: Arc<dyn Backend> = Arc::new(rpc(&side));
    let rt = rt();
    let (addr, _server) = rt
        .block_on(ksana_rpc::start_server(
            "127.0.0.1:0".parse().unwrap(),
            backend,
            &Cors::Disabled,
        ))
        .unwrap();
    let c = HttpClientBuilder::default()
        .build(format!("http://{addr}"))
        .unwrap();
    let hex32 = |bytes: &[u8; 32]| hex::encode(bytes);
    let request = |method: &str, params: ArrayParams| -> Value {
        rt.block_on(c.request(method, params)).unwrap()
    };
    let refused = |method: &str, params: ArrayParams| -> i32 {
        let e = rt
            .block_on(c.request::<Value, _>(method, params))
            .unwrap_err();
        error_code(&e)
    };

    // The kinds of assets, amounts as decimal strings, the cursor as hex
    let first = request(
        "ksana_getAccountAssets",
        rpc_params![hex32(&w.carol.account), Value::Null, 1],
    );
    assert_eq!(first["assets"][0]["kind"], "asset");
    assert_eq!(first["assets"][0]["asset"], hex32(&KSA_ASSET_ID));
    assert!(first["assets"][0]["balance"].is_string());
    assert_eq!(
        first["next"],
        format!("00{}", hex32(&KSA_ASSET_ID)),
        "a built-in asset's cursor"
    );
    assert!(first["at"]["height"].is_string());
    let rest = request(
        "ksana_getAccountAssets",
        rpc_params![hex32(&w.carol.account), first["next"].clone()],
    );
    assert_eq!(rest["next"], Value::Null);
    let kinds: Vec<(Value, Value)> = rest["assets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| (a["kind"].clone(), a["contract"].clone()))
        .collect();
    let mut expected = vec![
        (json!("token"), json!(hex32(&w.token))),
        (json!("nft"), json!(hex32(&w.collection))),
    ];
    expected.sort_by(|a, b| a.1.as_str().cmp(&b.1.as_str()));
    assert_eq!(kinds, expected);
    for asset in rest["assets"].as_array().unwrap() {
        match asset["kind"].as_str() {
            Some("token") => assert_eq!(asset["balance"], "45"),
            Some("nft") => assert_eq!(asset["count"], "1"),
            other => panic!("{other:?}"),
        }
    }
    // A contract's cursor continues among the contracts
    let after_token = format!("01{}", hex32(&w.token));
    let after = request(
        "ksana_getAccountAssets",
        rpc_params![hex32(&w.carol.account), after_token],
    );
    assert!(after["assets"]
        .as_array()
        .unwrap()
        .iter()
        .all(|a| a["contract"].as_str() > Some(hex32(&w.token).as_str())));

    // The token and the collection
    let token = request("ksana_getToken", rpc_params![hex32(&w.token)]);
    assert_eq!(token["standard"], "KS-20");
    assert_eq!(token["decimals"], 8);
    assert_eq!(token["supply"], "1000015");
    assert_eq!(token["holders"], "4");
    assert_eq!(token["irregular"], false);
    assert!(OpPosition::parse(token["declared"].as_str().unwrap()).is_ok());
    let collection = request("ksana_getToken", rpc_params![hex32(&w.collection)]);
    assert_eq!(collection["standard"], "KS-721");
    assert_eq!(collection["decimals"], Value::Null);
    assert_eq!(collection["supply"], "5");
    let contract = request("ksana_getContract", rpc_params![hex32(&w.receiver)]);
    assert_eq!(contract["standards"], json!([]));
    assert_eq!(contract["declared"], Value::Null);
    assert_eq!(contract["code_hash"], hex32(&sha256(RECEIVER)));

    // Holders, the largest first, one page at a time
    let holders = request(
        "ksana_getTokenHolders",
        rpc_params![hex32(&w.token), Value::Null, 2],
    );
    assert_eq!(holders["holders"][0]["account"], hex32(&w.alice.account));
    assert_eq!(holders["holders"][0]["balance"], "999580");
    let cursor = holders["next"].as_str().unwrap();
    assert_eq!(cursor.len(), 2 * ksana_rpc::HOLDER_CURSOR_BYTES);
    let more = request(
        "ksana_getTokenHolders",
        rpc_params![hex32(&w.token), cursor, 2],
    );
    assert_eq!(more["holders"].as_array().unwrap().len(), 2);
    assert_eq!(more["next"], Value::Null);

    // NFTs and approvals
    let nfts = request(
        "ksana_getNfts",
        rpc_params![hex32(&w.bob.account), hex32(&w.collection), Value::Null, 2],
    );
    assert_eq!(nfts["nfts"][0]["token_id"], hex32(&id(4)));
    assert_eq!(
        nfts["next"],
        format!("{}{}", hex32(&w.collection), hex32(&id(5)))
    );
    let rest = request(
        "ksana_getNfts",
        rpc_params![hex32(&w.bob.account), Value::Null, nfts["next"].clone()],
    );
    assert_eq!(rest["nfts"][0]["token_id"], hex32(&id(6)));
    let approvals = request("ksana_getApprovals", rpc_params![hex32(&w.alice.account)]);
    assert_eq!(
        approvals["approvals"],
        json!([{
            "kind": "allowance",
            "contract": hex32(&w.token),
            "spender": hex32(&w.carol.account),
            "amount": "50",
            "expires_at": FAR.to_string(),
        }])
    );
    let approvals = request("ksana_getApprovals", rpc_params![hex32(&w.bob.account)]);
    assert_eq!(approvals["approvals"][0]["kind"], "operator");
    assert_eq!(approvals["approvals"][0]["expires_at"], Value::Null);
    assert_eq!(approvals["approvals"][1]["kind"], "token");
    assert_eq!(approvals["approvals"][1]["token_id"], hex32(&id(4)));

    // The typed client reads the same back
    let typed = rt
        .block_on(c.get_account_assets(hex32(&w.bob.account), None, None))
        .unwrap();
    assert_eq!(typed.assets, assets_of(&rpc(&side), w.bob.account));
    let typed = rt
        .block_on(c.get_approvals(hex32(&w.bob.account), None, None))
        .unwrap();
    assert_eq!(typed.approvals, approvals_of(&rpc(&side), w.bob.account));
    let typed = rt.block_on(c.get_token(hex32(&w.collection))).unwrap();
    assert_eq!(typed, rpc(&side).token(w.collection).unwrap());
    let typed = rt
        .block_on(c.get_token_holders(hex32(&w.collection), None, None))
        .unwrap();
    assert_eq!(typed.holders.len(), 3);
    let typed = rt
        .block_on(c.get_nfts(hex32(&w.carol.account), None, None, None))
        .unwrap();
    assert_eq!(typed.nfts.len(), 1);
    let typed = rt.block_on(c.get_contract(hex32(&w.token))).unwrap();
    assert_eq!(typed, rpc(&side).contract(w.token).unwrap());

    // Not a token, or not a contract
    assert_eq!(
        refused("ksana_getToken", rpc_params![hex32(&w.receiver)]),
        codes::NOT_FOUND
    );
    assert_eq!(
        refused("ksana_getContract", rpc_params![hex32(&w.bob.account)]),
        codes::NOT_FOUND
    );
    // Malformed parameters
    let account = hex32(&w.bob.account);
    for (method, params) in [
        ("ksana_getContract", rpc_params!["zz"]),
        ("ksana_getAccountAssets", rpc_params![&account, "00"]),
        (
            "ksana_getAccountAssets",
            rpc_params![&account, format!("02{}", hex32(&KSA_ASSET_ID))],
        ),
        (
            "ksana_getAccountAssets",
            rpc_params![&account, Value::Null, 0],
        ),
        (
            "ksana_getAccountAssets",
            rpc_params![&account, Value::Null, 501],
        ),
        (
            "ksana_getTokenHolders",
            rpc_params![hex32(&w.token), hex32(&w.bob.account)],
        ),
        (
            "ksana_getNfts",
            rpc_params![
                &account,
                hex32(&w.collection),
                format!("{}{}", hex32(&w.token), hex32(&id(4)))
            ],
        ),
        ("ksana_getNfts", rpc_params![&account, "zz"]),
        ("ksana_getApprovals", rpc_params![&account, "abcd"]),
    ] {
        assert_eq!(refused(method, params), codes::INVALID_PARAMS, "{method}");
    }
}

/// A reorg (the scenario of `m35`'s): the index of the node that switched branches equals the index of the node that
/// built the winning branch, and the one built from scratch; when the next block merges the old branch, its ops come
/// back at their new place.
#[test]
fn a_reorg_takes_the_events_of_the_disconnected_blocks_back() {
    let (alice, bob, carol) = (user("m41/r/alice"), user("m41/r/bob"), user("m41/r/carol"));
    let (mock, genesis) = setup(&[(&alice, 1_000_000), (&bob, 1_000_000)]);
    let a = Side::new(&mock, &genesis, "m41r/a");
    let b = Side::new(&mock, &genesis, "m41r/b");
    let (deploy_token, token) = deploy(&alice, 0, TOKEN, token_init(1_000, Some(alice.account)));
    let (deploy_collection, collection) = deploy(&alice, 1, COLLECTION, collection_init());
    for block in include(&a, &[deploy_token, deploy_collection]) {
        b.accept(&block).unwrap();
    }
    let accounts = [alice.account, bob.account, carol.account];
    let contracts = [token, collection];
    let before = answers(&rpc(&a), &accounts, &contracts);
    assert_eq!(answers(&rpc(&b), &accounts, &contracts), before);

    // A's branch: a transfer, an NFT and an allowance for bob; B's: a transfer to carol, with alice's same nonce
    let a_ops = [
        call(&alice, 2, token, "transfer", (bob.account, 300u128)),
        call(
            &alice,
            3,
            collection,
            "mint",
            (bob.account, 1u128, None::<String>),
        ),
        call(
            &alice,
            4,
            token,
            "approve",
            (bob.account, 7u128, None::<u64>),
        ),
        call(
            &bob,
            0,
            collection,
            "set_operator",
            (carol.account, true, Some(FAR)),
        ),
    ];
    let a_branch = include(&a, &a_ops);
    let on_a = rpc(&a);
    assert_eq!(holders_of(&on_a, collection), [(bob.account, 1)]);
    assert_eq!(approvals_of(&on_a, alice.account).len(), 1);
    assert_eq!(approvals_of(&on_a, bob.account).len(), 1);
    let mut b_branch = include(
        &b,
        &[call(&alice, 2, token, "transfer", (carol.account, 500u128))],
    );
    while b_branch.len() <= a_branch.len() + 1 {
        b_branch.push(b.mine());
    }
    for block in &b_branch {
        a.accept(block).unwrap();
    }
    assert_eq!(
        a.chain.tip().unwrap().hash,
        b_branch.last().unwrap().header.hash()
    );
    let (ra, rb) = (rpc(&a), rpc(&b));
    let switched = answers(&ra, &accounts, &contracts);
    assert_eq!(switched, answers(&rb, &accounts, &contracts));
    assert_ne!(switched, before);
    assert_eq!(
        holders_of(&ra, token),
        by_amount(vec![(alice.account, 500), (carol.account, 500)])
    );
    assert!(holders_of(&ra, collection).is_empty());
    assert!(nfts_of(&ra, bob.account, None).is_empty());
    assert!(approvals_of(&ra, alice.account).is_empty());
    assert!(approvals_of(&ra, bob.account).is_empty());
    assert_eq!(ra.token(collection).unwrap().supply, 0);
    // The same as built from scratch
    a.chain.store().rebuild_index().unwrap();
    assert_eq!(answers(&ra, &accounts, &contracts), switched);

    // The next block merges A's old branch: its ops come back where they executed now (alice's transfer with the
    // spent nonce is skipped)
    let merging = a.mine();
    for block in a_branch.iter().chain([&merging]) {
        b.accept(block).unwrap();
    }
    assert_eq!(b.chain.tip().unwrap().hash, merging.header.hash());
    let merged = answers(&ra, &accounts, &contracts);
    assert_eq!(merged, answers(&rb, &accounts, &contracts));
    for (account, balance) in holders_of(&ra, token) {
        assert_eq!(view::<u128>(&ra, token, "balance_of", account), balance);
    }
    for (account, count) in holders_of(&ra, collection) {
        let held = view::<u64>(&ra, collection, "balance_of", account);
        assert_eq!(u128::from(held), count);
    }
    let bobs = nfts_of(&ra, bob.account, None);
    assert_eq!(bobs, [(collection, id(1))]);
    for (contract, token_id) in bobs {
        let owner = view::<Option<[u8; 32]>>(&ra, contract, "owner_of", token_id);
        assert_eq!(owner, Some(bob.account));
    }
    a.chain.store().rebuild_index().unwrap();
    assert_eq!(answers(&ra, &accounts, &contracts), merged);
}

/// Opens the block store's RocksDB under `chain_dir` with all of its column families.
fn raw_db(chain_dir: &Path) -> rocksdb::DB {
    let names = rocksdb::DB::list_cf(&rocksdb::Options::default(), chain_dir).unwrap();
    rocksdb::DB::open_cf(&rocksdb::Options::default(), chain_dir, names).unwrap()
}

/// The value of a key of the block store's `meta` column family.
fn meta(chain_dir: &Path, key: &[u8]) -> Option<Vec<u8>> {
    let db = raw_db(chain_dir);
    let meta = db.cf_handle("meta").unwrap();
    db.get_cf(meta, key).unwrap()
}

/// A data directory of a version of D-111 (its marker `"op_index"`, no standards index), and one such a version
/// started on after this one (its marker beside this one's, naming the same tip): the node builds both indexes again
/// at startup, the same as it kept them.
#[test]
fn the_index_is_built_again_for_a_data_directory_without_it() {
    let (side, w, mock, genesis) = scenario("m41/b");
    let (accounts, contracts) = (w.accounts(), w.contracts());
    let kept = answers(&rpc(&side), &accounts, &contracts);
    let Side {
        chain,
        producer,
        dir,
        ..
    } = side;
    drop(producer);
    drop(chain);
    let chain_dir = dir.path().join("chain");
    let open = || {
        let chain = Arc::new(Chain::open(dir.path(), DEVNET, &genesis, mock.clone()).unwrap());
        assert!(chain.store().index_current().unwrap());
        answers(&rpc_for(&chain), &accounts, &contracts)
    };
    assert_eq!(open(), kept, "kept across a restart");

    // A version of D-111: its marker holds the tip and its height, as this one's does
    let marker = meta(&chain_dir, b"index").expect("this version's marker");
    {
        let mut db = raw_db(&chain_dir);
        for name in ["standards", "standards_undo"] {
            db.drop_cf(name).unwrap();
        }
        let meta = db.cf_handle("meta").unwrap();
        db.put_cf(meta, b"op_index", &marker).unwrap();
        db.delete_cf(meta, b"index").unwrap();
    }
    {
        let store = ChainStore::open(&chain_dir, &DbMemory::new()).unwrap();
        assert!(!store.index_current().unwrap());
        assert_eq!(store.declared(&w.token).unwrap(), None);
    }
    assert_eq!(open(), kept, "built for a data directory of D-111");
    assert_eq!(meta(&chain_dir, b"op_index"), None);
    assert_eq!(meta(&chain_dir, b"index"), Some(marker.clone()));

    // A version of D-111 started on it after this one, and stopped before it connected a block: this one's marker
    // still names the tip, but its index may miss what that version did (here: nothing)
    {
        let db = raw_db(&chain_dir);
        let meta = db.cf_handle("meta").unwrap();
        db.put_cf(meta, b"op_index", &marker).unwrap();
    }
    {
        let store = ChainStore::open(&chain_dir, &DbMemory::new()).unwrap();
        assert!(!store.index_current().unwrap());
        assert_eq!(
            store.account_amounts(&w.alice.account, None, 10).unwrap(),
            None
        );
    }
    assert_eq!(open(), kept, "built after a version of D-111 ran");
    assert_eq!(meta(&chain_dir, b"op_index"), None);
}
