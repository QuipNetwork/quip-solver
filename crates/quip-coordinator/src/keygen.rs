//! `quip-coordinator keygen` — create a signer keystore.
//!
//! The keystore holds a 32-byte master seed as hex. `chain::extrinsic::
//! load_hybrid_pair` reads that seed and derives the hybrid sr25519 plus
//! FN-DSA-512 pair from it, so the seed is the only secret on disk.

use std::fmt;
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};

/// Why [`write_keystore`] could not produce a keystore.
#[derive(Debug)]
pub enum KeygenError {
    /// The target file already exists. Never overwrite a key.
    Exists(PathBuf),
    /// The keystore could not be created or written.
    Io(std::io::Error),
    /// The system randomness source failed.
    Random(getrandom::Error),
}

impl fmt::Display for KeygenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exists(p) => write!(
                f,
                "{} already exists; refusing to overwrite an existing keystore",
                p.display()
            ),
            Self::Io(e) => write!(f, "cannot write keystore: {e}"),
            Self::Random(e) => write!(f, "cannot read system randomness: {e}"),
        }
    }
}

impl std::error::Error for KeygenError {}

/// Lower-case hex, no `0x` prefix.
///
/// `chain::extrinsic::hex_encode` prefixes `0x` for RPC parameters, which is
/// the wrong shape for a keystore field. `hex_decode` accepts both spellings,
/// so the unprefixed form stays loadable and matches what other tooling
/// expects of a raw seed.
fn seed_to_hex(seed: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in seed {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Write a fresh keystore to `out`.
///
/// Fails if `out` exists. On unix the file is created with mode 0600, set at
/// creation time rather than afterwards so the seed is never briefly readable
/// by other users.
///
/// # Errors
///
/// Returns [`KeygenError::Exists`] if `out` is already present,
/// [`KeygenError::Random`] if the system randomness source fails, and
/// [`KeygenError::Io`] for any other filesystem failure.
pub fn write_keystore(out: &Path) -> Result<(), KeygenError> {
    if out.exists() {
        return Err(KeygenError::Exists(out.to_path_buf()));
    }
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(KeygenError::Io)?;
        }
    }

    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(KeygenError::Random)?;

    let mut opts = std::fs::OpenOptions::new();
    let _ = opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let _ = opts.mode(0o600);
    }

    let mut file = opts.open(out).map_err(|e| {
        if e.kind() == ErrorKind::AlreadyExists {
            KeygenError::Exists(out.to_path_buf())
        } else {
            KeygenError::Io(e)
        }
    })?;

    let body = format!("{{\"master_seed_hex\":\"{}\"}}\n", seed_to_hex(&seed));
    file.write_all(body.as_bytes()).map_err(KeygenError::Io)?;
    file.sync_all().map_err(KeygenError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("quip-keygen-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn read_seed_hex(path: &Path) -> String {
        let mut text = String::new();
        let mut f = std::fs::File::open(path).expect("keystore should exist");
        let _ = f
            .read_to_string(&mut text)
            .expect("keystore should be text");
        let v: serde_json::Value = serde_json::from_str(&text).expect("keystore should be json");
        v.get("master_seed_hex")
            .and_then(serde_json::Value::as_str)
            .expect("master_seed_hex should be a string")
            .to_string()
    }

    #[test]
    fn writes_a_random_owner_only_loadable_seed_and_never_overwrites() {
        let a = temp_path("a");
        let b = temp_path("b");
        write_keystore(&a).expect("first keygen should succeed");
        write_keystore(&b).expect("second keygen should succeed");

        let hex = read_seed_hex(&a);
        assert_eq!(hex.len(), 64, "32 bytes is 64 hex characters");
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!hex.starts_with("0x"), "keystore hex carries no 0x prefix");
        assert_ne!(hex, read_seed_hex(&b), "successive runs must differ");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&a)
                .expect("keystore should exist")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "keystore must not be group/world readable"
            );
        }

        let path = a.to_string_lossy().to_string();
        let _pair = crate::chain::extrinsic::load_hybrid_pair(&path)
            .expect("the coordinator must be able to load what keygen writes");

        let err = write_keystore(&a).expect_err("must refuse to overwrite");
        assert!(matches!(err, KeygenError::Exists(_)));
        assert_eq!(read_seed_hex(&a), hex, "refused write must not clobber");

        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }
}
