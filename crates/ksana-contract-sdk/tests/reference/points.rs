use ksana_contract_sdk::{self as sdk, AssetMetaInput, CAP_FREEZE, CAP_MINT};

/// The sub-ID of this contract's asset; the asset's ID is `sdk::asset_id(&sdk::self_id(), &POINTS)`.
const POINTS: [u8; 32] = [0; 32];

/// The asset's metadata, which every later issue passes again.
fn points() -> AssetMetaInput {
    AssetMetaInput {
        name: "Points".into(),
        symbol: "PTS".into(),
        decimals: 2,
    }
}

/// In `init`: creates the asset, able to mint more and to freeze accounts, with `supply` for the deployer.
pub fn create_points(supply: u128) -> [u8; 32] {
    sdk::asset_create(
        &POINTS,
        &points(),
        CAP_MINT | CAP_FREEZE,
        &sdk::caller(),
        supply,
    )
}

/// Issues `amount` more to `to`; the method that calls it checks who may.
pub fn mint_points(to: &[u8; 32], amount: u128) -> [u8; 32] {
    sdk::asset_issue(&POINTS, &points(), to, amount)
}

/// Freezes `account`, or lifts its freeze: a frozen account can neither send, receive nor burn the points.
pub fn freeze_points(account: &[u8; 32], frozen: bool) {
    sdk::asset_freeze(&POINTS, account, frozen);
}
