//! The rules of built-in assets shared by the execution layer, the VM and the RPC (D-121): the limits of an asset's
//! metadata, the capabilities its issuing contract declares when it creates the asset, and the status flags the
//! issuing contract sets on an account.

/// The shortest asset name (bytes of UTF-8).
pub const MIN_ASSET_NAME_BYTES: usize = 1;
/// The longest asset name (bytes of UTF-8).
pub const MAX_ASSET_NAME_BYTES: usize = 64;
/// The shortest asset symbol (bytes of UTF-8).
pub const MIN_ASSET_SYMBOL_BYTES: usize = 1;
/// The longest asset symbol (bytes of UTF-8).
pub const MAX_ASSET_SYMBOL_BYTES: usize = 16;
/// The most decimals an asset can have (the token standard's limit too).
pub const MAX_ASSET_DECIMALS: u8 = 18;

/// Capability: the issuing contract can issue more of the asset (`asset_issue`).
pub const CAP_MINT: u8 = 1;
/// Capability: the issuing contract can freeze an account, which can then neither send, receive nor burn the asset.
pub const CAP_FREEZE: u8 = 2;
/// Capability: only the accounts the issuing contract allows can send, receive and burn the asset.
pub const CAP_ALLOWLIST: u8 = 4;
/// Capability: the issuing contract can pause the asset, which then moves out of no account but its own.
pub const CAP_PAUSE: u8 = 8;
/// The capabilities defined so far; the other bits are reserved and refused.
pub const CAPABILITIES: u8 = CAP_MINT | CAP_FREEZE | CAP_ALLOWLIST | CAP_PAUSE;
/// The capabilities of an asset first issued by `asset_issue` rather than created by `asset_create`.
pub const DEFAULT_CAPABILITIES: u8 = CAP_MINT;

/// The capabilities by name, in bit order: the names the RPC reports, the same as the token standard's
/// `capabilities()` view.
pub const CAPABILITY_NAMES: [(u8, &str); 4] = [
    (CAP_MINT, "mint"),
    (CAP_FREEZE, "freeze"),
    (CAP_ALLOWLIST, "allowlist"),
    (CAP_PAUSE, "pause"),
];

/// Status flag: the account is frozen; it counts only while the asset has [`CAP_FREEZE`].
pub const STATUS_FROZEN: u8 = 1;
/// Status flag: the account is allowed; it counts only while the asset has [`CAP_ALLOWLIST`].
pub const STATUS_ALLOWED: u8 = 2;

/// The status flags by name, in bit order.
pub const STATUS_NAMES: [(u8, &str); 2] = [(STATUS_FROZEN, "frozen"), (STATUS_ALLOWED, "allowed")];

/// Checks an asset's metadata against the limits above; `Err` names the field that breaks them. A Borsh `String` is
/// valid UTF-8 already; Unicode character classes are not checked, as their tables change with the Rust version.
pub fn check_meta(name: &str, symbol: &str, decimals: u8) -> Result<(), &'static str> {
    if !(MIN_ASSET_NAME_BYTES..=MAX_ASSET_NAME_BYTES).contains(&name.len()) {
        return Err("asset name must be 1 to 64 bytes");
    }
    if !(MIN_ASSET_SYMBOL_BYTES..=MAX_ASSET_SYMBOL_BYTES).contains(&symbol.len()) {
        return Err("asset symbol must be 1 to 16 bytes");
    }
    if decimals > MAX_ASSET_DECIMALS {
        return Err("asset decimals must be at most 18");
    }
    Ok(())
}

/// The names of the capabilities in `capabilities`, in bit order (reserved bits have no name).
pub fn capability_names(capabilities: u8) -> Vec<String> {
    names(&CAPABILITY_NAMES, capabilities)
}

/// The names of the status flags in `status`, in bit order.
pub fn status_names(status: u8) -> Vec<String> {
    names(&STATUS_NAMES, status)
}

fn names(table: &[(u8, &str)], bits: u8) -> Vec<String> {
    table
        .iter()
        .filter(|(bit, _)| bits & bit != 0)
        .map(|(_, name)| (*name).to_string())
        .collect()
}
