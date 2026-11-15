//! VmOp decoding (SPEC §15): no input bytes may make decoding, encoding, op_id or signature verification crash; when
//! decoding succeeds, the encoding is canonical.

#![no_main]

use ksana_primitives::{VmOp, DEVNET};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(op) = VmOp::decode(data, &DEVNET) else {
        return;
    };
    // Borsh decoding is canonical: re-encoding yields the original input (the contract of codec.rs).
    let encoded = op
        .encode()
        .expect("a successfully decoded VmOp must encode");
    assert_eq!(encoded, data, "VmOp encoding is not canonical");
    // sig is the last field: the content of the signature message is a prefix of the full encoding.
    let unsigned = op
        .encode_unsigned()
        .expect("a successfully decoded VmOp must encode");
    assert!(
        data.starts_with(&unsigned),
        "signature message is not a prefix of the full encoding"
    );
    let op_id = op
        .op_id()
        .expect("a successfully decoded VmOp must have an op_id");
    // Half a signature (D-154): a signature that splits is the half it gives joined with the rest again, and with a
    // token fee (path B) the op has the same ID by either; half a signature gives the aggregate its term or an
    // error, never a crash.
    if let Some((half, s)) = op.split() {
        assert!(half.sig.is_half() && half.split().is_none());
        assert_eq!(half.join(&s).as_ref(), Some(&op), "split and join differ");
        if matches!(op.fee, ksana_primitives::FeeMode::Token { .. }) {
            assert_eq!(
                half.op_id().expect("half the op must have an op_id"),
                op_id,
                "one op, two IDs"
            );
        }
    }
    let _ = ksana_crypto::half_signed(&op);
    // Any signature (including a BIP322 placeholder, wrong length, invalid public key) can only fail verification,
    // never crash; a path-C op is verified against the first input of the carrying transaction (D-086).
    let _ = ksana_crypto::verify_vmop(&op);
    let first_input = ksana_primitives::OutPoint {
        txid: [0xc1; 32],
        vout: 1,
    };
    let _ = ksana_crypto::verify_native_vmop(&op, &first_input);
});
