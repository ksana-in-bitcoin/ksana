# Shared test vectors

Fixed test vectors shared by Rust and the TypeScript SDK (SPEC §11.3, §14 M0). All hex is lowercase.
Once committed, the vectors are the reference: they are only regenerated with
`cargo run -p ksana-crypto --example gen_vectors` when the encoding is changed on purpose (a consensus change)
(`pow.json` with `cargo run -p ksana-consensus --example gen_pow_vectors`), and the change is recorded in
`docs/DECISIONS.md`. `standards.json` holds the token standards' encodings (D-120), conventions between contracts,
wallets and indexers rather than consensus, fixed all the same once contracts use them: it is regenerated with
`cargo run -p ksana-contract-sdk --example gen_standard_vectors` only when a standard changes on purpose, recorded in
DECISIONS the same way.

The JSON representation rules are in DECISIONS D-007: byte strings are hex strings, `u64` / `u128` are decimal
strings, `u8` / `u32` are numbers, enums are in externally tagged form (`{"Token":{"amount":"5"}}`, `"BtcNative"`).
BTC transaction IDs and block hashes are in Bitcoin internal byte order (D-001).

| File | Contents | Rust check |
| --- | --- | --- |
| `codec.json` | `valid`: the JSON value and Borsh encoding of every consensus type and every enum variant; VmOps come with `op_id`, BlockHeaders with `block_hash`; the header sequence in `Payload` is the compressed encoding (D-075, including parallel blocks and merges, references, the empty entries root, wrapped differences). `invalid`: bytes that must fail to decode; for those with `error` (each canonicality rule of the compressed headers) the error message must contain it, and the Rust and TS SDK error messages are the same | `crates/ksana-primitives/tests/vectors.rs` |
| `signing.json` | `tagged_hash` samples; `cases`: secret key, `aux_rand`, account (the Taproot output key of the secret key tweaked per BIP86, D-092), signing message content `unsigned_borsh`, `sighash`, the signed VmOp with its encoding and `op_id`; `tampered`: VmOps tampered field by field that must fail verification; `bip322`: BIP322 simple signatures of the account's Taproot address (P2TR key-path; message, `message_hash`, `to_spend` / `to_sign` transaction IDs, witness, D-052, D-092) | `crates/ksana-crypto/tests/vectors.rs` |
| `merkle.json` | RFC 6962 reference roots; `entries_root` samples (D-004) | `crates/ksana-crypto/tests/vectors.rs` |
| `pow.json` | PoW (D-094): for each case the header (JSON and Borsh), chain ID and Argon2id parameters, salt, block hash, the 32-byte Argon2id output `pow_hash`, `pow_value` interpreted as little-endian, weight, effective target and whether it is met | `crates/ksana-consensus/tests/argon2id.rs` |
| `standards.json` | The token standards KS-20 and KS-721 (`docs/STANDARDS.md`, D-120) as the contract SDK's reference implementation encodes them: `calls`, the Borsh arguments of every method (`value` names them as the standard does); `answers`, the `Ok` bytes of every view's answer and of `transfer_and_call`'s; `events`, the events of both standards (externally tagged), each parsed back to itself; `invalid_events`, bytes that are no event of the standard (a byte more or less, the other standard's event, an option tag or a boolean of 2, a name or a declaration that is not UTF-8); `token_ids`, KS-721 IDs from serial numbers (big-endian in the last 16 bytes) and IDs without one; `asset_refs`, the `AssetRef` of a token or a built-in asset and bytes that are neither | `crates/ksana-contract-sdk/tests/standard_vectors.rs` (the committed file is what the generator writes) |

A BIP340 signature is determined by `(secret_key, sighash, aux_rand)` (the signing key is `secret_key` tweaked per
BIP86, the verifying key is the account), so a correct SDK implementation must produce byte-for-byte the same
signature; the witness of the BIP322 cases is likewise determined by `aux_rand`.
