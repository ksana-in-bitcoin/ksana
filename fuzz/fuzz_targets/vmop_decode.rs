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
    op.op_id()
        .expect("a successfully decoded VmOp must have an op_id");
    // Any signature (including a BIP322 placeholder, wrong length, invalid public key) can only fail verification,
    // never crash; a path-C op is verified against the first input of the carrying transaction (D-086).
    let _ = ksana_crypto::verify_vmop(&op);
    let first_input = ksana_primitives::OutPoint {
        txid: [0xc1; 32],
        vout: 1,
    };
    let _ = ksana_crypto::verify_native_vmop(&op, &first_input);
});
