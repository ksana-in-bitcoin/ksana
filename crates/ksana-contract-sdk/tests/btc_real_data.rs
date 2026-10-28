//! Tests of the `btc` module against real Bitcoin data.
//!
//! All expected values have been recomputed and checked from the raw bytes with an independent implementation (Python
//! hashlib):
//! - Mainnet block 100000: the header is assembled from the public fields, its hash equals the well-known
//!   `000000000003ba27…e506` and satisfies the proof of work; the Merkle root computed from the 4 well-known txids
//!   equals the root in the header.
//! - Mainnet block `00000000b0c5a240…70e7` (2009, 2 transactions; raw bytes taken from the unit tests of rust-bitcoin
//!   0.32): the header hash satisfies the proof of work, and the Merkle root formed from the txids computed from the
//!   raw transactions equals the root in the header.
//! - Several real transactions (likewise taken from the rust-bitcoin unit tests, including segwit transactions): the
//!   sha256d of the serialization with the witness stripped equals the public txid, and the sha256d of the full
//!   serialization equals the public wtxid (proving the witness-stripped serialization is correct).
//!
//! A transaction ID / hash in hex marked "display order" is the reverse of the internal byte order.
#![cfg(not(target_arch = "wasm32"))]

use ksana_contract_sdk::btc::{
    self, merkle_root_from_branch, parse_outputs, verify_merkle_branch, verify_tx_inclusion,
    BlockHeader, TxOut,
};

fn h32(hex_str: &str) -> [u8; 32] {
    hex::decode(hex_str).unwrap().try_into().unwrap()
}

/// Display order (RPC / block explorers) to internal byte order.
fn display(hex_str: &str) -> [u8; 32] {
    let mut bytes = h32(hex_str);
    bytes.reverse();
    bytes
}

// ---------------------------------------------------------------------------
// Mainnet block 100000

const BLOCK_100000_HEADER: &str = "0100000050120119172a610421a6c3011dd330d9df07b63616c2cc1f1cd00200000000006657a9252aacd5c0b2940996ecff952228c3067cc38d4885efb5a4ac4247e9f337221b4d4c86041b0f2b5710";

const BLOCK_100000_TXIDS: [&str; 4] = [
    "8c14f0db3df150123e6f3dbbf30f8b955a8249b62ac1d1ff16284aefa3d06d87",
    "fff2525b8931402dd09222c50775608f75787bd2b87e56995a7bdd30f79702c4",
    "6359f0868171b1d194cbee1af2f16ea598ae8fad666d9b012c8ed2b79a236ec4",
    "e9a66845e05d5abc0ad04ec80f774a7e585c6e8db975962d069a522137b80c1d",
];

/// Merkle branches of each transaction computed in Python per the Bitcoin rules (internal byte order).
const BLOCK_100000_BRANCHES: [[&str; 2]; 4] = [
    [
        "c40297f730dd7b5a99567eb8d27b78758f607507c52292d02d4031895b52f2ff",
        "49aef42d78e3e9999c9e6ec9e1dddd6cb880bf3b076a03be1318ca789089308e",
    ],
    [
        "876dd0a3ef4a2816ffd1c12ab649825a958b0ff3bb3d6f3e1250f13ddbf0148c",
        "49aef42d78e3e9999c9e6ec9e1dddd6cb880bf3b076a03be1318ca789089308e",
    ],
    [
        "1d0cb83721529a062d9675b98d6e5c587e4a770fc84ed00abc5a5de04568a6e9",
        "15b88c5107195bf09eb9da89b83d95b3d070079a3c5c5d3d17d0dcd873fbdacc",
    ],
    [
        "c46e239ab7d28e2c019b6d66ad8fae98a56ef1f21aeecb94d1b1718186f05963",
        "15b88c5107195bf09eb9da89b83d95b3d070079a3c5c5d3d17d0dcd873fbdacc",
    ],
];

fn header_100000() -> BlockHeader {
    BlockHeader::parse(&hex::decode(BLOCK_100000_HEADER).unwrap()).unwrap()
}

#[test]
fn block_100000_header_fields_and_hash() {
    let header = header_100000();
    assert_eq!(header.version, 1);
    assert_eq!(
        header.prev_blockhash,
        display("000000000002d01c1fccc21636b607dfd930d31d01c3a62104612a1719011250")
    );
    assert_eq!(
        header.merkle_root,
        display("f3e94742aca4b5ef85488dc37c06c3282295ffec960994b2c0d5ac2a25a95766")
    );
    assert_eq!(header.time, 1_293_623_863);
    assert_eq!(header.bits, 0x1b04_864c);
    assert_eq!(header.nonce, 274_148_111);
    assert_eq!(
        header.block_hash(),
        display("000000000003ba27aa200b1cecaad478d2b00432346c3f1f3986da1afd33e506")
    );
    assert_eq!(
        header.to_bytes().to_vec(),
        hex::decode(BLOCK_100000_HEADER).unwrap()
    );
}

#[test]
fn block_100000_merkle_branches() {
    let header = header_100000();
    for (index, (txid, branch)) in BLOCK_100000_TXIDS
        .iter()
        .zip(BLOCK_100000_BRANCHES.iter())
        .enumerate()
    {
        let leaf = display(txid);
        let branch: Vec<[u8; 32]> = branch.iter().map(|h| h32(h)).collect();
        let index = u32::try_from(index).unwrap();
        assert_eq!(
            merkle_root_from_branch(&leaf, index, &branch),
            Some(header.merkle_root),
            "tx {index}"
        );
        assert!(verify_merkle_branch(
            &leaf,
            index,
            &branch,
            &header.merkle_root
        ));
        // A wrong position, a tampered branch and a tampered leaf all fail.
        assert!(!verify_merkle_branch(
            &leaf,
            index ^ 1,
            &branch,
            &header.merkle_root
        ));
        let mut tampered = branch.clone();
        tampered[1][0] ^= 1;
        assert!(!verify_merkle_branch(
            &leaf,
            index,
            &tampered,
            &header.merkle_root
        ));
        let mut wrong_leaf = leaf;
        wrong_leaf[31] ^= 0x80;
        assert!(!verify_merkle_branch(
            &wrong_leaf,
            index,
            &branch,
            &header.merkle_root
        ));
        // Position bits beyond the branch length must be 0.
        assert!(!verify_merkle_branch(
            &leaf,
            index | 4,
            &branch,
            &header.merkle_root
        ));
        // Byte order: a display-order txid as the leaf fails.
        assert!(!verify_merkle_branch(
            &h32(txid),
            index,
            &branch,
            &header.merkle_root
        ));
    }
}

// ---------------------------------------------------------------------------
// Mainnet block 00000000b0c5a240b2a61d2e75692224efd4cbecdf6eaf4cc2cf477ca7c270e7 (2 transactions)

const SMALL_BLOCK_HEADER: &str = "010000004ddccd549d28f385ab457e98d1b11ce80bfea2c5ab93015ade4973e400000000bf4473e53794beae34e64fccc471dace6ae544180816f89591894e0f417a914cd74d6e49ffff001d323b3a7b";
const SMALL_BLOCK_COINBASE: &str = "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff0804ffff001d026e04ffffffff0100f2052a0100000043410446ef0102d1ec5240f0d061a4246c1bdef63fc3dbab7733052fbbf0ecd8f41fc26bf049ebb4f9527f374280259e7cfa99c48b0e3f39c51347a19a5819651503a5ac00000000";
const SMALL_BLOCK_TX1: &str = "010000000321f75f3139a013f50f315b23b0c9a2b6eac31e2bec98e5891c924664889942260000000049483045022100cb2c6b346a978ab8c61b18b5e9397755cbd17d6eb2fe0083ef32e067fa6c785a02206ce44e613f31d9a6b0517e46f3db1576e9812cc98d159bfdaf759a5014081b5c01ffffffff79cda0945903627c3da1f85fc95d0b8ee3e76ae0cfdc9a65d09744b1f8fc85430000000049483045022047957cdd957cfd0becd642f6b84d82f49b6cb4c51a91f49246908af7c3cfdf4a022100e96b46621f1bffcf5ea5982f88cef651e9354f5791602369bf5a82a6cd61a62501fffffffffe09f5fe3ffbf5ee97a54eb5e5069e9da6b4856ee86fc52938c2f979b0f38e82000000004847304402204165be9a4cbab8049e1af9723b96199bfd3e85f44c6b4c0177e3962686b26073022028f638da23fc003760861ad481ead4099312c60030d4cb57820ce4d33812a5ce01ffffffff01009d966b01000000434104ea1feff861b51fe3f5f8a3b12d0f4712db80e919548a80839fc47c6a21e66d957e9c5d8cd108c7a2d2324bad71f9904ac0ae7336507d785b17a2c115e427a32fac00000000";

#[test]
fn small_block_txids_outputs_and_inclusion() {
    let header = BlockHeader::parse(&hex::decode(SMALL_BLOCK_HEADER).unwrap()).unwrap();
    assert_eq!(
        header.block_hash(),
        display("00000000b0c5a240b2a61d2e75692224efd4cbecdf6eaf4cc2cf477ca7c270e7")
    );
    assert_eq!(header.time, 1_231_965_655);
    assert_eq!(header.bits, 0x1d00_ffff);
    assert_eq!(header.nonce, 2_067_413_810);

    let coinbase = hex::decode(SMALL_BLOCK_COINBASE).unwrap();
    let tx1 = hex::decode(SMALL_BLOCK_TX1).unwrap();
    let coinbase_id = h32("21da2ae8cc773b020b4873f597369416cf961a1896c24106b0198459fec2df77");
    let tx1_id = h32("339d9a371e2b5a26147ddfd87228b900ff75762a18a40f2778bedbcde7e9b0a3");
    assert_eq!(btc::txid(&coinbase), coinbase_id);
    assert_eq!(btc::txid(&tx1), tx1_id);

    assert_eq!(
        parse_outputs(&coinbase).unwrap(),
        vec![TxOut {
            value: 5_000_000_000,
            script_pubkey: &hex::decode("410446ef0102d1ec5240f0d061a4246c1bdef63fc3dbab7733052fbbf0ecd8f41fc26bf049ebb4f9527f374280259e7cfa99c48b0e3f39c51347a19a5819651503a5ac").unwrap(),
        }]
    );
    assert_eq!(
        parse_outputs(&tx1).unwrap(),
        vec![TxOut {
            value: 6_100_000_000,
            script_pubkey: &hex::decode("4104ea1feff861b51fe3f5f8a3b12d0f4712db80e919548a80839fc47c6a21e66d957e9c5d8cd108c7a2d2324bad71f9904ac0ae7336507d785b17a2c115e427a32fac").unwrap(),
        }]
    );

    // The real coinbase is recognized; an ordinary transaction is not a coinbase.
    assert!(btc::is_coinbase(&coinbase));
    assert!(!btc::is_coinbase(&tx1));

    // A block of two transactions: each one's branch is the other's txid; the coinbase proof pins the tree depth
    // (D-072).
    let cb_branch = [tx1_id];
    assert_eq!(
        verify_tx_inclusion(&header, &tx1, 1, &[coinbase_id], &coinbase, &cb_branch),
        Some(tx1_id)
    );
    assert_eq!(
        verify_tx_inclusion(&header, &coinbase, 0, &[tx1_id], &coinbase, &cb_branch),
        Some(coinbase_id)
    );
    assert_eq!(
        verify_tx_inclusion(&header, &tx1, 0, &[coinbase_id], &coinbase, &cb_branch),
        None
    );
    assert_eq!(
        verify_tx_inclusion(&header, &tx1, 1, &[], &coinbase, &cb_branch),
        None
    );
    // What is given at position 0 is not a coinbase
    assert_eq!(
        verify_tx_inclusion(&header, &tx1, 1, &[coinbase_id], &tx1, &[coinbase_id]),
        None
    );
    let mut altered = tx1.clone();
    let last = altered.len() - 5;
    altered[last] ^= 1; // flip one byte of the output script
    assert_eq!(
        verify_tx_inclusion(&header, &altered, 1, &[coinbase_id], &coinbase, &cb_branch),
        None
    );
}

// ---------------------------------------------------------------------------
// Individual real transactions

#[test]
fn legacy_transaction() {
    let raw = hex::decode("0100000001a15d57094aa7a21a28cb20b59aab8fc7d1149a3bdbcddba9c622e4f5f6a99ece010000006c493046022100f93bb0e7d8db7bd46e40132d1f8242026e045f03a0efe71bbb8e3f475e970d790221009337cd7f1f929f00cc6ff01f03729b069a7c21b59b1736ddfee5db5946c5da8c0121033b9b137ee87d5a812d6f506efdd37f0affa7ffc310711c06c7f3e097c9447c52ffffffff0100e1f505000000001976a9140389035a9225b3839e2bbf32d826a1e222031fd888ac00000000").unwrap();
    assert_eq!(
        btc::txid(&raw),
        display("a6eab3c14ab5272a58a5ba91505ba1a4b6d7a3a9fcbd187b6cd99a7b6d548cb7")
    );
    let script = hex::decode("76a9140389035a9225b3839e2bbf32d826a1e222031fd888ac").unwrap();
    assert_eq!(
        parse_outputs(&raw).unwrap(),
        vec![TxOut {
            value: 100_000_000,
            script_pubkey: &script,
        }]
    );
}

#[test]
fn legacy_transaction_with_op_return_output() {
    let raw = hex::decode("01000000010c7196428403d8b0c88fcb3ee8d64f56f55c8973c9ab7dd106bb4f3527f5888d000000006a4730440220503a696f55f2c00eee2ac5e65b17767cd88ed04866b5637d3c1d5d996a70656d02202c9aff698f343abb6d176704beda63fcdec503133ea4f6a5216b7f925fa9910c0121024d89b5a13d6521388969209df27a8469bd565aff10e8d42cef931fad5121bfb8ffffffff02b825b404000000001976a914ef79e7ee9fff98bcfd08473d2b76b02a48f8c69088ac0000000000000000296a2732363030393438363937313732333132373633313032313332353630353838373931323132373000000000").unwrap();
    assert_eq!(
        btc::txid(&raw),
        display("971ed48a62c143bbd9c87f4bafa2ef213cfa106c6e140f111931d0be307468dd")
    );
    let outputs = parse_outputs(&raw).unwrap();
    assert_eq!(outputs.len(), 2);
    assert_eq!(outputs[0].value, 78_915_000);
    assert_eq!(
        outputs[0].script_pubkey,
        hex::decode("76a914ef79e7ee9fff98bcfd08473d2b76b02a48f8c69088ac").unwrap()
    );
    assert_eq!(outputs[1].value, 0);
    assert_eq!(
        outputs[1].script_pubkey,
        hex::decode(
            "6a27323630303934383639373137323331323736333130323133323536303538383739313231323730"
        )
        .unwrap()
    );
}

/// Segwit transaction f5864806…: the full serialization (with witness) is rejected; the witness-stripped serialization
/// yields the public txid.
#[test]
fn segwit_transaction_requires_stripped_serialization() {
    let with_witness = hex::decode("02000000000101595895ea20179de87052b4046dfe6fd515860505d6511a9004cf12a1f93cac7c0100000000ffffffff01deb807000000000017a9140f3444e271620c736808aa7b33e370bd87cb5a078702483045022100fb60dad8df4af2841adc0346638c16d0b8035f5e3f3753b88db122e70c79f9370220756e6633b17fd2710e626347d28d60b0a2d6cbb41de51740644b9fb3ba7751040121028fa937ca8cba2197a37c007176ed8941055d3bcb8627d085e94553e62f057dcc00000000").unwrap();
    let stripped = hex::decode("0200000001595895ea20179de87052b4046dfe6fd515860505d6511a9004cf12a1f93cac7c0100000000ffffffff01deb807000000000017a9140f3444e271620c736808aa7b33e370bd87cb5a078700000000").unwrap();
    assert_eq!(parse_outputs(&with_witness), None);
    // Computed over the serialization with witness it gives the wtxid.
    assert_eq!(
        btc::txid(&with_witness),
        display("80b7d8a82d5d5bf92905b06f2014dd699e03837ca172e3a59d51426ebbe3e7f5")
    );
    assert_eq!(
        btc::txid(&stripped),
        display("f5864806e3565c34d1b41e716f72609d00b55ea5eac5b924c9719a842ef42206")
    );
    let script = hex::decode("a9140f3444e271620c736808aa7b33e370bd87cb5a0787").unwrap();
    assert_eq!(
        parse_outputs(&stripped).unwrap(),
        vec![TxOut {
            value: 506_078,
            script_pubkey: &script,
        }]
    );
}

/// Segwit transaction 9652aa62…: 2 P2SH-P2WSH inputs, 11 outputs.
#[test]
fn segwit_transaction_with_many_outputs() {
    let stripped = hex::decode("0100000002ff34f95a672bb6a4f6ff4a7e90fa8c7b3be7e70ffc39bc99be3bda67942e836c0000000023220020cde476664d3fa347b8d54ef3aee33dcb686a65ced2b5207cbf4ec5eda6b9b46e4f414d4c934ad81d330314e888888e3bd22c7dde8aac2ca9227b30d7c40093248af7812201000000232200200af6f6a071a69d5417e592ed99d256ddfd8b3b2238ac73f5da1b06fc0b2e79d54f414d4c0ba0c8f505000000001976a914dcb5898d9036afad9209e6ff0086772795b1441088ac033c0f000000000017a914889f8c10ff2bd4bb9dabb68c5c0d700a46925e6c87033c0f000000000017a914889f8c10ff2bd4bb9dabb68c5c0d700a46925e6c87033c0f000000000017a914889f8c10ff2bd4bb9dabb68c5c0d700a46925e6c87033c0f000000000017a914889f8c10ff2bd4bb9dabb68c5c0d700a46925e6c87033c0f000000000017a914889f8c10ff2bd4bb9dabb68c5c0d700a46925e6c87033c0f000000000017a914889f8c10ff2bd4bb9dabb68c5c0d700a46925e6c87033c0f000000000017a914889f8c10ff2bd4bb9dabb68c5c0d700a46925e6c87033c0f000000000017a914889f8c10ff2bd4bb9dabb68c5c0d700a46925e6c87033c0f000000000017a914889f8c10ff2bd4bb9dabb68c5c0d700a46925e6c87033c0f000000000017a914889f8c10ff2bd4bb9dabb68c5c0d700a46925e6c8700000000").unwrap();
    assert_eq!(
        btc::txid(&stripped),
        display("9652aa62b0e748caeec40c4cb7bc17c6792435cc3dfe447dd1ca24f912a1c6ec")
    );
    let outputs = parse_outputs(&stripped).unwrap();
    assert_eq!(outputs.len(), 11);
    assert_eq!(outputs[0].value, 99_993_760);
    assert_eq!(
        outputs[0].script_pubkey,
        hex::decode("76a914dcb5898d9036afad9209e6ff0086772795b1441088ac").unwrap()
    );
    let p2sh = hex::decode("a914889f8c10ff2bd4bb9dabb68c5c0d700a46925e6c87").unwrap();
    for out in &outputs[1..] {
        assert_eq!(out.value, 998_403);
        assert_eq!(out.script_pubkey, p2sh);
    }
}

// ---------------------------------------------------------------------------
// Robustness: no truncation, extension or mutation panics; structural damage is rejected.

fn real_transactions() -> Vec<Vec<u8>> {
    [SMALL_BLOCK_COINBASE, SMALL_BLOCK_TX1]
        .iter()
        .map(|h| hex::decode(h).unwrap())
        .collect()
}

#[test]
fn every_truncation_and_extension_is_rejected() {
    for raw in real_transactions() {
        assert!(parse_outputs(&raw).is_some());
        for len in 0..raw.len() {
            assert_eq!(parse_outputs(&raw[..len]), None, "prefix of length {len}");
        }
        let mut extended = raw.clone();
        extended.push(0);
        assert_eq!(parse_outputs(&extended), None);
    }
}

#[test]
fn single_byte_mutations_never_panic() {
    for raw in real_transactions() {
        for i in 0..raw.len() {
            for bit in 0..8 {
                let mut mutated = raw.clone();
                mutated[i] ^= 1 << bit;
                // Only required not to panic; the result may be None or (when the script content changes) another
                // set of outputs.
                let _ = parse_outputs(&mutated);
            }
        }
    }
}

#[test]
fn structural_rules() {
    let raw = hex::decode(SMALL_BLOCK_COINBASE).unwrap();
    // An input count of 0 (i.e. the segwit marker) is rejected.
    let mut no_inputs = raw[..4].to_vec();
    no_inputs.push(0);
    no_inputs.extend_from_slice(&raw[4 + 1 + 41 + 8..]);
    assert_eq!(parse_outputs(&no_inputs), None);
    // An output count of 0 is rejected: version + 1 input + 0 outputs + lock time.
    let input_end = 4 + 1 + 32 + 4 + 1 + 8 + 4;
    let mut no_outputs = raw[..input_end].to_vec();
    no_outputs.push(0);
    no_outputs.extend_from_slice(&[0, 0, 0, 0]);
    assert_eq!(parse_outputs(&no_outputs), None);
    // A value exceeding MAX_MONEY is rejected.
    let mut too_much = raw.clone();
    too_much[input_end + 1..input_end + 9].copy_from_slice(&(btc::MAX_MONEY + 1).to_le_bytes());
    assert_eq!(parse_outputs(&too_much), None);
    let mut max_money = raw.clone();
    max_money[input_end + 1..input_end + 9].copy_from_slice(&btc::MAX_MONEY.to_le_bytes());
    assert_eq!(parse_outputs(&max_money).unwrap()[0].value, btc::MAX_MONEY);
    // A non-minimal CompactSize (0xfd 0x01 0x00 for 1 output) is rejected.
    let mut non_canonical = raw[..input_end].to_vec();
    non_canonical.extend_from_slice(&[0xfd, 0x01, 0x00]);
    non_canonical.extend_from_slice(&raw[input_end + 1..]);
    assert_eq!(parse_outputs(&non_canonical), None);
}

#[test]
fn sixty_four_byte_transactions_are_rejected() {
    // A 64-byte "transaction" is indistinguishable from a Merkle internal node (CVE-2017-12842), even though its
    // inclusion can indeed be proven.
    let left = [0x11u8; 32];
    let right = [0x22u8; 32];
    let fake_tx: Vec<u8> = left.iter().chain(right.iter()).copied().collect();
    let fake_id = ksana_contract_sdk::sha256d(&fake_tx);
    // A block of two transactions: [real coinbase, 64-byte transaction]
    let coinbase = hex::decode(SMALL_BLOCK_COINBASE).unwrap();
    let coinbase_id = btc::txid(&coinbase);
    let root = ksana_contract_sdk::sha256d(&[coinbase_id, fake_id].concat());
    let header = BlockHeader {
        version: 1,
        prev_blockhash: [0; 32],
        merkle_root: root,
        time: 0,
        bits: 0,
        nonce: 0,
    };
    assert!(verify_merkle_branch(&fake_id, 1, &[coinbase_id], &root));
    assert_eq!(
        verify_tx_inclusion(&header, &fake_tx, 1, &[coinbase_id], &coinbase, &[fake_id]),
        None
    );
}

#[test]
fn header_parse_requires_exactly_80_bytes() {
    let bytes = hex::decode(BLOCK_100000_HEADER).unwrap();
    assert!(BlockHeader::parse(&bytes[..79]).is_none());
    let mut long = bytes.clone();
    long.push(0);
    assert!(BlockHeader::parse(&long).is_none());
}

mod properties {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn parse_outputs_never_panics(data in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = parse_outputs(&data);
        }

        #[test]
        fn merkle_never_panics(
            leaf in any::<[u8; 32]>(),
            index in any::<u32>(),
            branch in proptest::collection::vec(any::<[u8; 32]>(), 0..40),
        ) {
            let root = merkle_root_from_branch(&leaf, index, &branch);
            prop_assert_eq!(root.is_some(), branch.len() <= 32 && (branch.len() == 32 || index >> branch.len() == 0));
        }
    }
}
