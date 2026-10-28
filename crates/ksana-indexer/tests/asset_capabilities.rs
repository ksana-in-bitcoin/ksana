//! The indexer and the node agree on the issuer capabilities of built-in assets (D-121): a contract creates an asset
//! with every capability, allows, freezes, pauses and renounces, and the movements the asset's rules refuse fail their
//! ops; the indexer, reading only the BTC chain, reaches the node's `state_root` at every height, so the asset's record,
//! the accounts' status flags and the balances the refusals left alone are the same on both.

mod common;

use common::*;
use ksana_crypto::sign_vmop;
use ksana_execution::builtin::{asset_id, contract_id};
use ksana_execution::OpStatus;
use ksana_primitives::asset::{CAPABILITIES, CAP_MINT, CAP_PAUSE};
use ksana_primitives::hash::sha256;
use ksana_primitives::{Action, Entry, SigScheme, VmOp, VMOP_VERSION};
use ksana_test_contracts::PROBE;

const GAS: u64 = 5_000_000;
const TRANSFER_GAS: u64 = 100_000;

fn enc<T: borsh::BorshSerialize>(v: &T) -> Vec<u8> {
    borsh::to_vec(v).unwrap()
}

/// A path-B op of `u` with a sponsored fee of 2.
fn op(u: &User, nonce: u64, max_gas: u64, action: Action) -> VmOp {
    let mut op = VmOp {
        version: VMOP_VERSION,
        chain_id: PARAMS.chain_id,
        account: u.account,
        nonce,
        fee: token(2),
        max_gas,
        action,
        sig: SigScheme::Bip340([0; 64]),
    };
    op.sig = sign_vmop(&op, &u.kp, &[3; 32]).unwrap();
    op
}

fn call(contract: [u8; 32], method: &str, args: Vec<u8>) -> Action {
    Action::Call {
        contract,
        method: method.into(),
        args,
        funds: vec![],
    }
}

/// The status of `op`'s receipt in the block at `height` on the node.
fn status_of(side: &Side, height: u64, op: &VmOp) -> OpStatus {
    let hash = side.block(height).header.hash();
    let id = op.op_id().unwrap();
    let receipts = side.chain.store().receipts(&hash).unwrap().unwrap();
    receipts
        .receipts
        .into_iter()
        .find(|r| r.op_id == Some(id))
        .map(|r| r.status)
        .unwrap()
}

fn refused(message: &str) -> OpStatus {
    OpStatus::Failed(message.into())
}

#[test]
fn the_indexer_follows_an_asset_with_issuer_capabilities() {
    let (alice, bob) = (user("caps/alice"), user("caps/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000), (&bob, 1_000)]);
    let a = Side::new(&mock, &genesis, "caps");

    // 1: Alice deploys probe, which creates the asset with every capability (probe's methods take anyone's call), and
    // drives it through the rules
    let probe = contract_id(&alice.account, 0, &sha256(PROBE));
    let sub_id = [7u8; 32];
    let asset = asset_id(&probe, &sub_id);
    // (name, symbol, decimals): the Borsh encoding of `AssetMetaInput`
    let meta = ("Dollar".to_string(), "USDX".to_string(), 6u8);
    let send_asset = |to: &User, amount: u128| Action::AssetTransfer {
        asset,
        to: to.account,
        amount,
    };
    let steps = [
        (
            GAS,
            Action::Deploy {
                code: PROBE.to_vec(),
                init_args: enc(&b"ok".to_vec()),
                funds: vec![],
            },
            OpStatus::Success,
        ),
        (
            GAS,
            call(
                probe,
                "create_asset",
                enc(&(sub_id, meta.clone(), CAPABILITIES, probe, 1_000u128)),
            ),
            OpStatus::Success,
        ),
        (
            GAS,
            call(probe, "allow", enc(&(sub_id, alice.account, true))),
            OpStatus::Success,
        ),
        (
            GAS,
            call(probe, "send", enc(&(asset, alice.account, 300u128))),
            OpStatus::Success,
        ),
        (
            TRANSFER_GAS,
            send_asset(&bob, 10),
            refused("rejected: recipient is not on this asset's allowlist"),
        ),
        (
            GAS,
            call(probe, "allow", enc(&(sub_id, bob.account, true))),
            OpStatus::Success,
        ),
        (TRANSFER_GAS, send_asset(&bob, 50), OpStatus::Success),
        (
            GAS,
            call(probe, "freeze", enc(&(sub_id, bob.account, true))),
            OpStatus::Success,
        ),
        (
            GAS,
            call(probe, "pause", enc(&(sub_id, true))),
            OpStatus::Success,
        ),
        (
            TRANSFER_GAS,
            send_asset(&bob, 1),
            refused("rejected: asset is paused"),
        ),
        // the issuing contract still sends while paused, but not to a frozen account
        (
            GAS,
            call(probe, "send", enc(&(asset, bob.account, 5u128))),
            refused("rejected: recipient is frozen for this asset"),
        ),
        (
            GAS,
            call(probe, "send", enc(&(asset, alice.account, 5u128))),
            OpStatus::Success,
        ),
        // giving up the pause resumes the asset, giving up minting fixes its supply
        (
            GAS,
            call(probe, "renounce", enc(&(sub_id, CAP_PAUSE | CAP_MINT))),
            OpStatus::Success,
        ),
        (
            GAS,
            call(
                probe,
                "issue_meta",
                enc(&(sub_id, meta, alice.account, 1u128)),
            ),
            refused("rejected: asset supply is fixed"),
        ),
        (
            GAS,
            call(probe, "freeze", enc(&(sub_id, bob.account, false))),
            OpStatus::Success,
        ),
    ];
    let ops: Vec<(VmOp, OpStatus)> = steps
        .into_iter()
        .enumerate()
        .map(|(nonce, (max_gas, action, status))| {
            (op(&alice, nonce as u64, max_gas, action), status)
        })
        .collect();
    for (op, _) in &ops {
        a.submit(op.clone());
    }
    let b1 = a.mine();
    assert_eq!(b1.entries.len(), ops.len(), "every step in one block");
    for (step, (op, status)) in ops.iter().enumerate() {
        assert_eq!(status_of(&a, 1, op), *status, "step {step}");
    }
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);

    // 4: Bob, unfrozen, sends; 5: Alice has him frozen again; 6: his transfer is refused
    let unfrozen = op(&bob, 0, TRANSFER_GAS, send_asset(&alice, 20));
    a.submit(unfrozen.clone());
    let b4 = a.mine();
    assert_eq!(
        status_of(&a, b4.header.height, &unfrozen),
        OpStatus::Success
    );
    let freeze = op(
        &alice,
        ops.len() as u64,
        GAS,
        call(probe, "freeze", enc(&(sub_id, bob.account, true))),
    );
    a.submit(freeze.clone());
    let b5 = a.mine();
    assert_eq!(status_of(&a, b5.header.height, &freeze), OpStatus::Success);
    let frozen = op(&bob, 1, TRANSFER_GAS, send_asset(&alice, 1));
    a.submit(frozen.clone());
    let b6 = a.mine();
    assert_eq!(
        status_of(&a, b6.header.height, &frozen),
        refused("rejected: sender is frozen for this asset")
    );
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);
    // 9: unlocks the sponsored fees up to block 6 and carries the headers of 7 and 8, after which block 6 has
    // SETTLEMENT_DEPTH confirmations on the indexer's main chain too
    let b9 = a.mine();
    assert!(
        matches!(b9.entries.as_slice(), [Entry::Unlock { height: 6, .. }]),
        "{:?}",
        b9.entries
    );
    a.mine_empty();
    a.mine_empty();
    a.broadcast_settlements();
    mock.mine(1);

    let dir = tempfile::tempdir().unwrap();
    let ix = open_indexer(dir.path(), &genesis, &mock);
    catch_up(&ix);
    assert!(ix.status().height >= b6.header.height, "{:?}", ix.status());
    assert_matches_node(&ix, &a);
}
