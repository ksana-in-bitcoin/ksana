//! The contract reference for AI assistants, `playground/src/ai/ksana-contracts.md` (D-124), gives the VM's gas
//! prices, its limits and the networks' parameters as tables and sentences. This test builds every row of those tables
//! from the constants and checks that the reference has exactly these rows, and the sentences that carry a parameter,
//! so that a change of a price, a limit or a parameter fails here until the reference shows it. The contract SDK's
//! test of the same file (`crates/ksana-contract-sdk/tests/reference.rs`) checks its signatures and its examples.

use ksana_execution::GenesisConfig;
use ksana_primitives::asset::{MAX_ASSET_DECIMALS, MAX_ASSET_NAME_BYTES, MAX_ASSET_SYMBOL_BYTES};
use ksana_primitives::{Params, DEVNET, MAINNET, MAX_CALL_FUNDS, SIGNET};
use ksana_vm::gas::*;
use ksana_vm::limits::*;
use ksana_vm::MAX_CALL_INPUT_BYTES;

/// The reference, as the playground ships it and the site publishes it.
const REFERENCE: &str = include_str!("../../../playground/src/ai/ksana-contracts.md");

const MIB: u64 = 1024 * 1024;

/// The reference with every run of whitespace, line breaks included, as one space: a sentence of it, however wrapped.
fn flat() -> String {
    REFERENCE.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A number as the reference writes it: `1234567` as `1,234,567`.
fn n(value: impl Into<u128>) -> String {
    let digits = value.into().to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A `usize` as the reference writes it.
fn z(value: usize) -> String {
    n(u64::try_from(value).unwrap())
}

/// The table rows of the section under `## {title}`, without each table's header row and separator.
fn table_rows(title: &str) -> Vec<String> {
    let start = REFERENCE
        .find(&format!("\n## {title}\n"))
        .unwrap_or_else(|| panic!("no section {title}"));
    let section = &REFERENCE[start + 1..];
    let end = section[3..].find("\n## ").map_or(section.len(), |i| i + 3);
    let mut out = Vec::new();
    let mut in_table = false;
    for line in section[..end].lines() {
        if !line.starts_with('|') {
            in_table = false;
            continue;
        }
        if in_table && !line.starts_with("| ---") {
            out.push(line.to_string());
        }
        in_table = true;
    }
    out
}

fn same(title: &str, mut expected: Vec<String>) {
    let mut actual = table_rows(title);
    actual.sort();
    expected.sort();
    assert_eq!(
        actual, expected,
        "the rows of the reference's section {title} differ from the constants"
    );
}

#[test]
fn the_gas_tables_are_the_vm_prices() {
    // rows that name several functions at one price
    assert_eq!(ASSET_BALANCE, ASSET_POLICY);
    assert_eq!(ASSET_BALANCE, ASSET_STATUS);
    assert_eq!(ASSET_TRANSFER, ASSET_BURN);
    assert_eq!(
        (ASSET_ISSUE_BASE, ASSET_ISSUE_PER_BYTE),
        (ASSET_CREATE_BASE, ASSET_CREATE_PER_BYTE)
    );
    assert_eq!(ASSET_SET_STATUS, ASSET_PAUSE);
    assert_eq!(ASSET_SET_STATUS, ASSET_RENOUNCE);
    // the SDK's sha256d is two sha256, the second of 32 bytes; asset_id, contract_id and created_contract_id hash 64,
    // 72 and 96 bytes
    let sha = |bytes: u64| SHA256_BASE + SHA256_PER_BYTE * bytes;
    let rows = vec![
        format!(
            "| `storage::get`, `storage::read` | {} + {} per byte of key and value |",
            n(STORAGE_GET_BASE),
            n(STORAGE_GET_PER_BYTE)
        ),
        format!(
            "| `storage::set`, `storage::write` | {} + {} per byte of key and value |",
            n(STORAGE_SET_BASE),
            n(STORAGE_SET_PER_BYTE)
        ),
        format!(
            "| `storage::remove` | {} + {} per byte of key |",
            n(STORAGE_REMOVE_BASE),
            n(STORAGE_REMOVE_PER_BYTE)
        ),
        format!(
            "| `caller`, `self_id`, `block_height`, `block_timestamp`, `chain_id`, `gas_left`, `btc_anchor_height` | {} each |",
            n(ENV_QUERY)
        ),
        format!("| `btc_header` | {} |", n(BTC_HEADER)),
        format!(
            "| `native_tx` | {} + {} per byte returned |",
            n(NATIVE_TX_BASE),
            n(NATIVE_TX_PER_BYTE)
        ),
        format!(
            "| `asset_balance`, `asset_policy`, `asset_status` | {} |",
            n(ASSET_BALANCE)
        ),
        format!("| `asset_transfer`, `asset_burn` | {} |", n(ASSET_TRANSFER)),
        format!(
            "| `asset_issue`, `asset_create` | {} + {} per byte of the Borsh `AssetMetaInput` |",
            n(ASSET_ISSUE_BASE),
            n(ASSET_ISSUE_PER_BYTE)
        ),
        format!(
            "| `asset_freeze`, `asset_allow`, `asset_pause`, `asset_renounce` | {} |",
            n(ASSET_SET_STATUS)
        ),
        format!(
            "| `call_funds`, `received` | {} + {} per byte of the Borsh `Vec<Funds>` |",
            n(CALL_FUNDS_BASE),
            n(CALL_FUNDS_PER_BYTE)
        ),
        format!(
            "| `call`, `call_with_funds`, `try_call`, `try_call_with`, `delegate_call` | {} + {} per byte of input + {} per \
             byte of the callee's code + {} per attached asset, and what the callee uses |",
            n(CALL_CONTRACT_BASE),
            n(CALL_CONTRACT_PER_INPUT_BYTE),
            n(CALL_PER_CODE_BYTE),
            n(FUNDS_PER_ASSET)
        ),
        format!(
            "| `create_contract` | {} + {} per byte of `init_args` + {} per byte of code + {} per attached asset, and what \
             `init` uses |",
            n(CREATE_CONTRACT_BASE),
            n(CALL_CONTRACT_PER_INPUT_BYTE),
            n(CALL_PER_CODE_BYTE),
            n(FUNDS_PER_ASSET)
        ),
        format!(
            "| `emit`, `emit_event` | {} + {} per byte |",
            n(EMIT_EVENT_BASE),
            n(EMIT_EVENT_PER_BYTE)
        ),
        format!(
            "| `sha256` | {} + {} per byte |",
            n(SHA256_BASE),
            n(SHA256_PER_BYTE)
        ),
        format!(
            "| `sha256d` | {} + {} per byte (two `sha256`) |",
            n(sha(32) + SHA256_BASE),
            n(SHA256_PER_BYTE)
        ),
        format!(
            "| `asset_id`, `contract_id`, `created_contract_id` | {}, {} and {} (one `sha256` each) |",
            n(sha(64)),
            n(sha(72)),
            n(sha(96))
        ),
        format!("| `verify_schnorr` | {} |", n(VERIFY_SCHNORR)),
        format!(
            "| Deploy | {} + {} per byte of code + {} per byte of code to load it + {} per attached asset, and what `init` \
             uses |",
            n(DEPLOY_BASE),
            n(DEPLOY_PER_CODE_BYTE),
            n(CALL_PER_CODE_BYTE),
            n(FUNDS_PER_ASSET)
        ),
        format!(
            "| Call | {} + {} per byte of the contract's code + {} per attached asset, and what the method uses |",
            n(CALL_BASE),
            n(CALL_PER_CODE_BYTE),
            n(FUNDS_PER_ASSET)
        ),
        format!("| Asset transfer | {} |", n(ASSET_TRANSFER_ACTION)),
    ];
    same("Gas", rows);
    // what the command line section says a deployment costs
    let deploy = format!(
        "a deployment costs about {} plus {} per byte of the module plus its `init`",
        n(DEPLOY_BASE),
        n(DEPLOY_PER_CODE_BYTE + CALL_PER_CODE_BYTE)
    );
    assert!(flat().contains(&deploy), "{deploy}");
}

/// The parameters the reference gives, of one network.
fn limits_of(params: &Params) -> (u32, u64, u32, usize, u64, u64) {
    let rules = params.rules(0);
    (
        params.max_call_depth,
        params.max_wasm_memory_bytes,
        params.btc_query_depth,
        params.max_op_bytes,
        rules.block_gas_limit,
        rules.native_max_gas,
    )
}

#[test]
fn the_limits_table_is_the_vm_limits_and_the_networks_parameters() {
    // "These limits are the same on devnet, the signet and mainnet"
    assert_eq!(limits_of(&DEVNET), limits_of(&SIGNET));
    assert_eq!(limits_of(&DEVNET), limits_of(&MAINNET));
    assert!(flat().contains("These limits are the same on devnet, the signet and mainnet."));
    let p = &DEVNET;
    assert_eq!(p.max_wasm_memory_bytes % MIB, 0);
    assert_eq!(MAX_TOTAL_MEMORY_PER_OP % MIB, 0);
    let rows = vec![
        format!("| Storage key | {} bytes |", z(MAX_STORAGE_KEY_BYTES)),
        format!("| Storage value | {} bytes |", z(MAX_STORAGE_VALUE_BYTES)),
        format!("| Event | {} bytes |", z(MAX_EVENT_BYTES)),
        format!(
            "| Call input (method name and arguments) | {} bytes |",
            z(MAX_CALL_INPUT_BYTES)
        ),
        format!("| Return value | {} bytes |", z(MAX_OUTPUT_BYTES)),
        format!("| Assets attached to one call | {} |", z(MAX_CALL_FUNDS)),
        format!(
            "| Nested calls, the operation's own call the first | {} levels |",
            p.max_call_depth
        ),
        format!(
            "| Memory of one instance | {} MiB |",
            p.max_wasm_memory_bytes / MIB
        ),
        format!(
            "| Stack of one instance, by the VM's estimate | {} bytes |",
            n(STACK_BUDGET)
        ),
        format!(
            "| Instances in one operation | {} |",
            z(MAX_INSTANCES_PER_OP)
        ),
        format!(
            "| Memory of all instances in one operation | {} MiB |",
            MAX_TOTAL_MEMORY_PER_OP / MIB
        ),
        format!("| Table entries of a module | {} |", z(MAX_TABLE_ELEMENTS)),
        format!("| Operation, code included | {} bytes |", z(p.max_op_bytes)),
        format!(
            "| Gas of one operation | {}; {} on path C |",
            n(p.rules(0).block_gas_limit),
            n(p.rules(0).native_max_gas)
        ),
        format!(
            "| Bitcoin headers a contract reads | up to `btc_anchor_height() - {}` |",
            p.btc_query_depth
        ),
        format!(
            "| Asset name, symbol, decimals | 1 to {} bytes, 1 to {} bytes, at most {} |",
            z(MAX_ASSET_NAME_BYTES),
            z(MAX_ASSET_SYMBOL_BYTES),
            MAX_ASSET_DECIMALS
        ),
        format!(
            "| Asset metadata, the Borsh `AssetMetaInput` | {} bytes |",
            z(MAX_ASSET_META_BYTES)
        ),
    ];
    same("Limits", rows);
}

#[test]
fn the_networks_and_ksa_are_as_the_reference_says() {
    let chains = format!(
        "Devnet has the chain ID {}, the signet {} and mainnet {}",
        DEVNET.chain_id, SIGNET.chain_id, MAINNET.chain_id
    );
    assert!(flat().contains(&chains), "{chains}");
    let stub = format!(
        "/// {} devnet, {} signet, {} mainnet.",
        DEVNET.chain_id, SIGNET.chain_id, MAINNET.chain_id
    );
    assert!(REFERENCE.contains(&stub), "{stub}");
    let decimals = GenesisConfig::devnet(Vec::new()).ksa_decimals;
    let ksa = format!(
        "with {decimals} decimals: 1 KSA is {} base units",
        n(10u128.pow(u32::from(decimals)))
    );
    assert!(flat().contains(&ksa), "{ksa}");
}

/// The number format itself.
#[test]
fn numbers_are_written_with_thousands_separators() {
    assert_eq!(n(0u64), "0");
    assert_eq!(n(100u64), "100");
    assert_eq!(n(1_000u64), "1,000");
    assert_eq!(n(65_536u64), "65,536");
    assert_eq!(n(100_000_000u64), "100,000,000");
}
