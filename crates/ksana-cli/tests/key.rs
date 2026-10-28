//! SPEC §16 "secret keys exist only in wallets and on the producer's machine; logs must not output any key material":
//! `ksana-cli key` prints only the account and the address, never the secret key.

use std::process::Command;

#[test]
fn key_command_never_prints_the_secret_key() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("alice.key");
    // The first run generates the secret key file, the second reads it
    for _ in 0..2 {
        let out = Command::new(env!("CARGO_BIN_EXE_ksana-cli"))
            .arg("key")
            .arg(&key)
            .output()
            .unwrap();
        assert!(out.status.success());
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
        .to_lowercase();
        let secret = std::fs::read_to_string(&key).unwrap().trim().to_lowercase();
        assert_eq!(secret.len(), 64);
        assert!(text.contains("account"), "{text}");
        assert!(
            !text.contains(&secret),
            "output contains the secret key:\n{text}"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the secret key file must be readable and writable by the owner only"
        );
    }
}
