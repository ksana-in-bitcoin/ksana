//! Compiles the example contracts of the `contracts/` workspace to `wasm32-unknown-unknown` (release, `--locked`), then
//! copies the `.wasm` files to `OUT_DIR`, from where `src/lib.rs` exports them with `include_bytes!`.
//!
//! - Builds with the same cargo (`CARGO`), with the target directory under `OUT_DIR`, separate from the outer build's
//!   target directory, so the nested cargo does not wait for the directory lock held by the outer one.
//! - Removes inherited environment variables that would change or break the nested build (`RUSTFLAGS`,
//!   `RUSTC_WORKSPACE_WRAPPER` (set by cargo clippy), `CARGO_TARGET_DIR`, `CARGO_BUILD_*`, `CARGO_PROFILE_*`, the
//!   build-script-only variables, etc.); the build is controlled only by `contracts/Cargo.toml`,
//!   `contracts/.cargo/config.toml` and `rust-toolchain.toml`.
//! - Each contract is compiled separately with `cargo rustc --crate-type cdylib`: if the `rlib` in the manifest (for
//!   the native tests) took part in the same compilation, LTO could not internalize symbols and the module would grow
//!   noticeably (D-021).

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

const TARGET: &str = "wasm32-unknown-unknown";

/// (cargo package name, produced wasm file name).
const CONTRACTS: [(&str, &str); 10] = [
    ("token", "token.wasm"),
    ("asset-token", "asset_token.wasm"),
    ("amm", "amm.wasm"),
    ("amm-factory", "amm_factory.wasm"),
    ("btc-payment-trigger", "btc_payment_trigger.wasm"),
    ("ksa-exchange", "ksa_exchange.wasm"),
    ("poll", "poll.wasm"),
    ("probe", "probe.wasm"),
    ("receiver", "receiver.wasm"),
    ("collection", "collection.wasm"),
];

/// Environment variables not passed to the nested build.
const REMOVED_VARS: &[&str] = &[
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_ENCODED_RUSTDOCFLAGS",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_PRIMARY_PACKAGE",
    "CARGO_RUSTC_CURRENT_DIR",
    // Build-script-only variables.
    "OUT_DIR",
    "TARGET",
    "HOST",
    "NUM_JOBS",
    "OPT_LEVEL",
    "DEBUG",
    "PROFILE",
    "RUSTC_LINKER",
];

/// Environment variables with these prefixes are not passed either: cargo's build configuration (target, target
/// directory, profile, linker, rustflags) and the outer build script's package information.
const REMOVED_PREFIXES: &[&str] = &[
    "CARGO_BUILD_",
    "CARGO_TARGET_",
    "CARGO_PROFILE_",
    "CARGO_CFG_",
    "CARGO_FEATURE_",
    "CARGO_PKG_",
    "CARGO_MANIFEST_",
    "DEP_",
    "__CARGO",
];

fn should_remove(key: &OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    REMOVED_VARS.contains(&key) || REMOVED_PREFIXES.iter().any(|p| key.starts_with(p))
}

fn required_env(key: &str) -> Result<OsString, String> {
    env::var_os(key)
        .ok_or_else(|| format!("{key} is not set (this build script must run under cargo)"))
}

/// Whether the standard library for the wasm32 target is installed in rustc's sysroot.
fn wasm_target_installed(rustc: &OsStr) -> Result<bool, String> {
    let output = Command::new(rustc)
        .args(["--print", "sysroot"])
        .output()
        .map_err(|e| {
            format!(
                "failed to run `{} --print sysroot`: {e}",
                rustc.to_string_lossy()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "`{} --print sysroot` failed: {}",
            rustc.to_string_lossy(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let sysroot = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(Path::new(&sysroot)
        .join("lib")
        .join("rustlib")
        .join(TARGET)
        .join("lib")
        .is_dir())
}

fn rerun_if_changed(path: &Path) {
    // A nonexistent path makes cargo rerun the build script every time; only register paths that exist.
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn run() -> Result<(), String> {
    let manifest_dir = PathBuf::from(required_env("CARGO_MANIFEST_DIR")?);
    let root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .ok_or("cannot locate the repository root")?
        .to_path_buf();
    let contracts = root.join("contracts");
    let sdk = root.join("crates").join("ksana-contract-sdk");
    let out_dir = PathBuf::from(required_env("OUT_DIR")?);
    let cargo = required_env("CARGO")?;
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));

    // Contract sources, manifest, lock file, link configuration; SDK sources and manifest (it inherits dependencies and
    // fields from the root workspace); toolchain.
    for path in [
        contracts.join("Cargo.toml"),
        contracts.join("Cargo.lock"),
        contracts.join(".cargo").join("config.toml"),
        contracts.join("examples"),
        contracts.join("testing"),
        sdk.join("Cargo.toml"),
        sdk.join("src"),
        root.join("Cargo.toml"),
        root.join("rust-toolchain.toml"),
    ] {
        rerun_if_changed(&path);
    }

    if !wasm_target_installed(&rustc)? {
        return Err(format!(
            "the `{TARGET}` target is not installed for {}.\n\
             It is listed in rust-toolchain.toml; install it with `rustup target add {TARGET}` \
             (or `rustup toolchain install` in the repository root).",
            rustc.to_string_lossy()
        ));
    }

    let target_dir = out_dir.join("contracts-target");
    for (package, _) in CONTRACTS {
        let mut command = Command::new(&cargo);
        // Run under contracts/ so that contracts/.cargo/config.toml (link arguments such as the linear memory limit) is
        // read.
        command.current_dir(&contracts).args([
            "rustc",
            "--release",
            "--locked",
            "--target",
            TARGET,
            "--package",
            package,
            "--crate-type",
            "cdylib",
        ]);
        command
            .arg("--manifest-path")
            .arg(contracts.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(&target_dir);
        for (key, _) in env::vars_os() {
            if should_remove(&key) {
                command.env_remove(&key);
            }
        }
        // All output of the nested cargo goes to stderr: the build script's stdout is parsed as directives by the outer
        // cargo.
        command.stdout(Stdio::from(io::stderr()));
        let status = command
            .status()
            .map_err(|e| format!("failed to run {}: {e}", cargo.to_string_lossy()))?;
        if !status.success() {
            return Err(format!(
                "building contract `{package}` for {TARGET} failed ({status}); see the output above.\n\
                 Reproduce with: cd contracts && cargo rustc --release --locked --target {TARGET} \
                 --package {package} --crate-type cdylib"
            ));
        }
    }

    let wasm_dir = target_dir.join(TARGET).join("release");
    for (_, file) in CONTRACTS {
        let from = wasm_dir.join(file);
        fs::copy(&from, out_dir.join(file))
            .map_err(|e| format!("failed to copy {}: {e}", from.display()))?;
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}
