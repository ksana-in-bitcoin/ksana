//! Native tests using the contract SDK's transactional mock host.
#![cfg(not(target_arch = "wasm32"))]
#![allow(missing_docs, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeSet;

use ksana_contract_sdk::mock::{self, Failure};
use ksana_contract_sdk::{decode, encode, Funds, KSA_ASSET_ID};
use poll::{dispatch, MAX_OPTION, MAX_OPTIONS, MAX_QUESTION};

const CREATOR: [u8; 32] = [1; 32];

fn options(labels: &[&str]) -> Vec<String> {
    labels.iter().map(|l| l.to_string()).collect()
}

fn deploy(question: &str, labels: &[&str]) -> Result<[u8; 32], Failure> {
    mock::deploy(
        CREATOR,
        0,
        b"poll",
        dispatch,
        &encode(&(question, options(labels))),
    )
}

/// The reason a call reverted with.
fn reason(result: Result<Vec<u8>, Failure>) -> String {
    match result {
        Err(Failure::Revert { reason, .. }) => String::from_utf8(reason).unwrap(),
        other => panic!("expected a revert, got {other:?}"),
    }
}

fn results(contract: [u8; 32]) -> Vec<u64> {
    decode(&mock::call([9; 32], contract, "results", &[]).unwrap()).unwrap()
}

#[test]
fn vote_close_and_read_the_poll() {
    mock::reset();
    let contract = deploy("Which fruit?", &["apple", "pear", "plum"]).unwrap();
    let poll: (String, Vec<String>, bool) =
        decode(&mock::call([9; 32], contract, "poll", &[]).unwrap()).unwrap();
    assert_eq!(
        poll,
        (
            "Which fruit?".to_string(),
            options(&["apple", "pear", "plum"]),
            true
        )
    );
    assert_eq!(results(contract), vec![0, 0, 0]);

    let votes: u64 =
        decode(&mock::call([2; 32], contract, "vote", &encode(&1u32)).unwrap()).unwrap();
    assert_eq!(votes, 1);
    let votes: u64 =
        decode(&mock::call([3; 32], contract, "vote", &encode(&1u32)).unwrap()).unwrap();
    assert_eq!(votes, 2);
    mock::call(CREATOR, contract, "vote", &encode(&2u32)).unwrap();
    assert_eq!(results(contract), vec![0, 2, 1]);
    let voted = |account: [u8; 32]| -> Option<u32> {
        decode(&mock::call([9; 32], contract, "voted", &encode(&account)).unwrap()).unwrap()
    };
    assert_eq!(voted([2; 32]), Some(1));
    assert_eq!(voted(CREATOR), Some(2));
    assert_eq!(voted([4; 32]), None);

    mock::call(CREATOR, contract, "close", &[]).unwrap();
    let (_, _, open): (String, Vec<String>, bool) =
        decode(&mock::call([9; 32], contract, "poll", &[]).unwrap()).unwrap();
    assert!(!open);
    let events: Vec<(_, _)> = mock::events()
        .into_iter()
        .map(|e| (e.contract, e.data))
        .collect();
    assert_eq!(
        events,
        vec![
            (contract, encode(&("voted", [2u8; 32], 1u32))),
            (contract, encode(&("voted", [3u8; 32], 1u32))),
            (contract, encode(&("voted", CREATOR, 2u32))),
            (contract, encode(&("closed", 3u64))),
        ]
    );
}

#[test]
fn what_the_poll_refuses_changes_nothing() {
    mock::reset();
    let contract = deploy("Which fruit?", &["apple", "pear"]).unwrap();
    mock::call([2; 32], contract, "vote", &encode(&0u32)).unwrap();
    mock::clear_events();

    let vote = |account: [u8; 32], option: u32| {
        reason(mock::call(account, contract, "vote", &encode(&option)))
    };
    assert_eq!(vote([2; 32], 1), "already voted");
    assert_eq!(vote([3; 32], 2), "unknown option");
    assert_eq!(vote([3; 32], u32::MAX), "unknown option");
    assert_eq!(
        reason(mock::call([2; 32], contract, "close", &[])),
        "only the creator"
    );
    assert_eq!(
        reason(mock::call([3; 32], contract, "vote", &[1])),
        "malformed arguments"
    );
    assert_eq!(
        reason(mock::call([3; 32], contract, "close", &[0])),
        "malformed arguments"
    );
    assert_eq!(
        reason(mock::call([3; 32], contract, "missing", &[])),
        "unknown method"
    );
    assert!(mock::call(CREATOR, contract, "init", &[]).is_err());
    assert_eq!(results(contract), vec![1, 0]);

    mock::call(CREATOR, contract, "close", &[]).unwrap();
    assert_eq!(vote([3; 32], 0), "voting is closed");
    assert_eq!(
        reason(mock::call(CREATOR, contract, "close", &[])),
        "voting is closed"
    );
    assert_eq!(results(contract), vec![1, 0]);
    assert_eq!(
        mock::events()
            .into_iter()
            .map(|e| e.data)
            .collect::<Vec<_>>(),
        vec![encode(&("closed", 1u64))]
    );
}

#[test]
fn init_checks_the_question_and_the_options() {
    mock::reset();
    let long_question = "q".repeat(MAX_QUESTION + 1);
    let long_option = "o".repeat(MAX_OPTION + 1);
    let many: Vec<String> = (0..=MAX_OPTIONS).map(|i| i.to_string()).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    let refused = |question: &str, labels: &[&str]| match deploy(question, labels) {
        Err(Failure::Revert { reason, .. }) => String::from_utf8(reason).unwrap(),
        other => panic!("expected a revert, got {other:?}"),
    };
    assert_eq!(
        refused("", &["a", "b"]),
        "the question must be 1 to 200 bytes"
    );
    assert_eq!(
        refused(&long_question, &["a", "b"]),
        "the question must be 1 to 200 bytes"
    );
    assert_eq!(refused("Q?", &["a"]), "a poll has 2 to 8 options");
    assert_eq!(refused("Q?", &many), "a poll has 2 to 8 options");
    assert_eq!(refused("Q?", &["a", ""]), "an option must be 1 to 64 bytes");
    assert_eq!(
        refused("Q?", &["a", &long_option]),
        "an option must be 1 to 64 bytes"
    );
    // the longest of everything it takes, in a language other than English
    let question = "ü".repeat(MAX_QUESTION / 2);
    let label = "ñ".repeat(MAX_OPTION / 2);
    let labels = vec![label.as_str(); MAX_OPTIONS];
    let contract = deploy(&question, &labels).unwrap();
    assert_eq!(results(contract), vec![0; MAX_OPTIONS]);
}

#[test]
fn attached_funds_are_refused() {
    mock::reset();
    let contract = deploy("Which fruit?", &["apple", "pear"]).unwrap();
    mock::register_asset(KSA_ASSET_ID, "KSA", 8, [0; 32]);
    mock::mint(KSA_ASSET_ID, [2; 32], 10).unwrap();
    let funds = [Funds {
        asset: KSA_ASSET_ID,
        amount: 1,
    }];
    let result = mock::call_with_funds([2; 32], contract, "vote", &encode(&0u32), &funds);
    assert_eq!(reason(result), "unexpected attached funds");
    assert_eq!(mock::balance(&KSA_ASSET_ID, &[2; 32]), 10);
    assert_eq!(results(contract), vec![0, 0]);
    let deployed = mock::deploy_with_funds(
        [2; 32],
        0,
        b"poll",
        dispatch,
        &encode(&("Q?", options(&["a", "b"]))),
        &funds,
    );
    assert!(matches!(deployed, Err(Failure::Revert { .. })));
}

/// The methods the abi block declares are the ones `dispatch` answers.
#[test]
fn the_abi_block_matches_dispatch() {
    let source = include_str!("../src/lib.rs");
    let block: Vec<&str> = source
        .lines()
        .map(str::trim_start)
        .skip_while(|l| *l != "//! ```abi")
        .skip(1)
        .take_while(|l| *l != "//! ```")
        .map(|l| l.trim_start_matches("//!").trim())
        .collect();
    let declared: BTreeSet<&str> = block
        .iter()
        .filter_map(|l| {
            let l = l
                .strip_prefix("call ")
                .or_else(|| l.strip_prefix("view "))?;
            l.split('(').next()
        })
        .collect();
    assert_eq!(
        declared,
        BTreeSet::from(["vote", "close", "poll", "results", "voted"])
    );
    mock::reset();
    let contract = deploy("Which fruit?", &["apple", "pear"]).unwrap();
    for method in ["poll", "results", "vote", "voted", "close"] {
        let args = match method {
            "vote" => encode(&0u32),
            "voted" => encode(&CREATOR),
            _ => Vec::new(),
        };
        assert!(
            mock::call(CREATOR, contract, method, &args).is_ok(),
            "{method}"
        );
    }
    assert!(block
        .iter()
        .any(|l| l.starts_with("init(question: String, options: Vec<String>)")));
    assert!(block.contains(&"event voted(voter: Account, option: u32)"));
    assert!(block.contains(&"event closed(total: u64)"));
}
