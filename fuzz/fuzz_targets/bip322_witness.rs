//! BIP322 simple witness (D-052, D-092): arbitrary bytes as an account's signature can only fail verification, never
//! crash. The first byte selects the account (0 is a valid account, 1 is 32 bytes not on the curve, anything else is
//! all zeros); the remaining bytes serve both as the witness encoding for a fixed message and as the `Bip322Simple`
//! signature of a fixed VmOp.

#![no_main]

use std::sync::OnceLock;

use ksana_crypto::bip322::verify_for_account;
use ksana_primitives::{Action, FeeMode, SigScheme, VmOp, DEVNET};
use libfuzzer_sys::fuzz_target;

fn fixed_op(account: [u8; 32]) -> VmOp {
    VmOp {
        version: 1,
        chain_id: DEVNET.chain_id,
        account,
        nonce: 3,
        fee: FeeMode::Token { amount: 2 },
        max_gas: 100_000,
        action: Action::AssetTransfer {
            asset: [0; 32],
            to: [9; 32],
            amount: 5,
        },
        sig: SigScheme::Bip340([0; 64]),
    }
}

fn valid_account() -> [u8; 32] {
    static ACCOUNT: OnceLock<[u8; 32]> = OnceLock::new();
    *ACCOUNT.get_or_init(|| {
        ksana_crypto::account_of(
            &ksana_crypto::keypair_from_secret(&[0x33; 32]).expect("the fixed secret key is valid"),
        )
    })
}

fuzz_target!(|data: &[u8]| {
    let Some((selector, witness)) = data.split_first() else {
        return;
    };
    let account = match selector % 3 {
        0 => valid_account(),
        1 => [0xff; 32],
        _ => [0; 32],
    };
    let _ = verify_for_account(&account, b"fuzz", witness);
    let op = VmOp {
        sig: SigScheme::Bip322Simple(witness.to_vec()),
        ..fixed_op(account)
    };
    let _ = ksana_crypto::verify_vmop(&op);
});
