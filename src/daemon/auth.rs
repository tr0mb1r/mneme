//! Daemon auth-token storage + rotation (ADR-0012 D3 / D4 — A.M4 commit 1).
//!
//! The token value lives in **exactly one file**:
//! `<root>/run/auth.token`, mode `0600` on Unix. Per-agent
//! configurations reference the path, never the value (per
//! Invariant 3 of pin `01KR5ZB7ED01HADZXZKKBV882Z`). The daemon
//! reads + verifies from disk on every client connection's
//! handshake — no in-memory caching that would defeat rotation.
//!
//! This commit lands the token-management primitive: generation,
//! reading, rotation. Daemon-side verification on the
//! `DaemonServeMany` accept loop is a follow-up A.M4 commit (it
//! touches the MCP protocol surface — needs a defined client
//! handshake step before the standard MCP `initialize`).
//!
//! Token shape:
//!
//! - 32 bytes from the OS RNG (`rand::rng().fill_bytes`).
//! - Encoded as 43-char URL-safe base64 (no padding) for human-
//!   readable copy/paste during debugging — any user accidentally
//!   pastes the token into chat / docs once and rotates it; we
//!   want the rotation to be one command, not "regenerate the file
//!   manually."
//! - Verification uses constant-time comparison (`subtle`) so
//!   timing-side-channel leakage doesn't reveal a prefix match.
//!
//! Rotation is atomic per ADR-0012 D4: write to
//! `auth.token.tmp`, fsync, rename over `auth.token`, fsync the
//! parent dir. POSIX rename is atomic on the same filesystem
//! (and `<root>/run/` is by construction one filesystem with
//! `<root>/`).

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use rand::RngCore;
use thiserror::Error;

/// Filename of the daemon auth token under `<root>/run/`.
pub const AUTH_TOKEN_FILENAME: &str = "auth.token";

/// Mode bits applied to the auth token file (Unix only). `0600` so
/// only the owning user can read it.
#[cfg(unix)]
pub const AUTH_TOKEN_MODE: u32 = 0o600;

/// Number of random bytes generated per token. 32 bytes = 256
/// bits of entropy, well past brute-force feasibility; 43-char
/// base64 fits on one line of output.
const TOKEN_BYTES: usize = 32;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("auth token IO error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Path to the auth-token file given the data-dir root.
pub fn token_path(root: &Path) -> PathBuf {
    root.join("run").join(AUTH_TOKEN_FILENAME)
}

/// Generate the auth token file if it doesn't already exist. No-op
/// if present (preserves the existing token across daemon restarts —
/// rotation is an explicit user action via [`rotate_token`], not a
/// startup side effect).
///
/// Creates `<root>/run/` with mode `0700` if absent (mirrors
/// `upgrade_audit::run_if_needed` and `daemon::listener::bind_listener`).
pub fn ensure_token(root: &Path) -> Result<PathBuf, AuthError> {
    let path = token_path(root);
    if path.exists() {
        return Ok(path);
    }
    write_atomic(&path, &generate_token())?;
    Ok(path)
}

/// Read the auth token from disk. Returns the raw token bytes
/// (the base64-encoded string the daemon will compare against
/// what the client presented). Errors if the file is missing —
/// the daemon expects [`ensure_token`] to have run by then.
pub fn read_token(root: &Path) -> Result<Vec<u8>, AuthError> {
    let path = token_path(root);
    std::fs::read(&path).map_err(|source| AuthError::Io {
        path: path.clone(),
        source,
    })
}

/// Generate a fresh token and atomically replace the existing
/// file. Idempotent in the sense of "always produces a valid
/// post-state"; the actual token value is fresh on every call.
/// Existing daemon connections stay valid (the token check
/// fires only at handshake per ADR-0012 D3) — the new value
/// takes effect on the next connect.
pub fn rotate_token(root: &Path) -> Result<PathBuf, AuthError> {
    let path = token_path(root);
    write_atomic(&path, &generate_token())?;
    Ok(path)
}

/// Constant-time comparison of two tokens. Returns `true` if
/// they're byte-identical, `false` otherwise. Used by the daemon's
/// verification path (next A.M4 commit) to avoid leaking
/// prefix-match timing information.
pub fn tokens_match(presented: &[u8], expected: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    presented.ct_eq(expected).into()
}

fn generate_token() -> Vec<u8> {
    let mut raw = [0u8; TOKEN_BYTES];
    rand::rng().fill_bytes(&mut raw);
    // URL-safe base64, no padding. 32 bytes → 43 chars.
    base64_url_no_pad(&raw).into_bytes()
}

/// Minimal URL-safe base64 (no padding). Inlined rather than
/// pulling a `base64` dep — the token-encoding path has exactly
/// one caller and the implementation fits in a screen.
fn base64_url_no_pad(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let mut buf = [0u8; 3];
        buf[..chunk.len()].copy_from_slice(chunk);
        let b0 = buf[0];
        let b1 = buf[1];
        let b2 = buf[2];
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[((b0 & 0b11) << 4 | b1 >> 4) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((b1 & 0b1111) << 2 | b2 >> 6) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(b2 & 0b111111) as usize] as char);
        }
    }
    out
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), AuthError> {
    let parent = path.parent().ok_or_else(|| AuthError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "auth token path has no parent"),
    })?;
    std::fs::create_dir_all(parent).map_err(|source| AuthError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    set_run_dir_perms(parent)?;

    let tmp = path.with_extension("token.tmp");
    {
        let mut f = std::fs::File::create(&tmp).map_err(|source| AuthError::Io {
            path: tmp.clone(),
            source,
        })?;
        f.write_all(contents).map_err(|source| AuthError::Io {
            path: tmp.clone(),
            source,
        })?;
        f.sync_all().map_err(|source| AuthError::Io {
            path: tmp.clone(),
            source,
        })?;
    }
    set_token_perms(&tmp)?;
    std::fs::rename(&tmp, path).map_err(|source| AuthError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    // fsync the parent dir so the rename is crash-durable per D4.
    if let Ok(dir) = std::fs::File::open(parent)
        && let Err(source) = dir.sync_all()
    {
        // Non-fatal — most filesystems durable-rename without an
        // explicit dir fsync, and the next boot's atomic-rename
        // semantics still hold. Log for visibility.
        tracing::warn!(error = %source, parent = %parent.display(), "auth token parent dir fsync failed");
    }
    Ok(())
}

#[cfg(unix)]
fn set_token_perms(path: &Path) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(AUTH_TOKEN_MODE)).map_err(
        |source| AuthError::Io {
            path: path.to_path_buf(),
            source,
        },
    )
}

#[cfg(not(unix))]
fn set_token_perms(_path: &Path) -> Result<(), AuthError> {
    // Windows ACLs are managed when the daemon's named-pipe
    // listener is bound (M4 sub-commit). The token file inherits
    // the user-profile ACL — already user-owned + non-world-
    // readable on a default install.
    Ok(())
}

#[cfg(unix)]
fn set_run_dir_perms(dir: &Path) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|source| {
        AuthError::Io {
            path: dir.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn set_run_dir_perms(_dir: &Path) -> Result<(), AuthError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn ensure_creates_file_with_owner_only_mode() {
        let tmp = TempDir::new().unwrap();
        let path = ensure_token(tmp.path()).unwrap();
        assert!(path.exists());
        assert_eq!(path, token_path(tmp.path()));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, AUTH_TOKEN_MODE, "token must be 0o600, got {mode:o}");
        }
    }

    #[test]
    fn ensure_creates_run_dir_with_owner_only_mode() {
        let tmp = TempDir::new().unwrap();
        ensure_token(tmp.path()).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(tmp.path().join("run"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "run/ must be 0o700, got {mode:o}");
        }
    }

    #[test]
    fn ensure_is_no_op_if_token_already_exists() {
        let tmp = TempDir::new().unwrap();
        ensure_token(tmp.path()).unwrap();
        let first = read_token(tmp.path()).unwrap();
        ensure_token(tmp.path()).unwrap();
        let second = read_token(tmp.path()).unwrap();
        assert_eq!(first, second, "ensure must not overwrite an existing token");
    }

    #[test]
    fn rotate_replaces_token_with_a_fresh_value() {
        let tmp = TempDir::new().unwrap();
        ensure_token(tmp.path()).unwrap();
        let original = read_token(tmp.path()).unwrap();
        rotate_token(tmp.path()).unwrap();
        let rotated = read_token(tmp.path()).unwrap();
        assert_ne!(original, rotated, "rotate must change the token value");
        assert_eq!(rotated.len(), original.len(), "token shape unchanged");
    }

    #[test]
    fn token_is_43_char_base64_url() {
        let tmp = TempDir::new().unwrap();
        ensure_token(tmp.path()).unwrap();
        let raw = read_token(tmp.path()).unwrap();
        assert_eq!(
            raw.len(),
            43,
            "token must be 43 chars (32 bytes base64-no-pad)"
        );
        let s = std::str::from_utf8(&raw).expect("token must be valid utf-8");
        assert!(
            s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "token must be url-safe-base64 chars only, got {s:?}"
        );
    }

    #[test]
    fn tokens_match_constant_time_returns_true_on_equal() {
        let a = b"identical-token";
        let b = b"identical-token";
        assert!(tokens_match(a, b));
    }

    #[test]
    fn tokens_match_returns_false_on_different_length() {
        let a = b"short";
        let b = b"a-much-longer-token";
        assert!(!tokens_match(a, b));
    }

    #[test]
    fn tokens_match_returns_false_on_byte_diff() {
        let a = b"identical-token";
        let b = b"identical-tokeN"; // last byte differs
        assert!(!tokens_match(a, b));
    }

    #[test]
    fn rotate_is_atomic_no_tmp_left_behind() {
        let tmp = TempDir::new().unwrap();
        ensure_token(tmp.path()).unwrap();
        rotate_token(tmp.path()).unwrap();
        let tmp_path = token_path(tmp.path()).with_extension("token.tmp");
        assert!(
            !tmp_path.exists(),
            ".token.tmp must be renamed away post-rotate"
        );
    }

    #[test]
    fn token_path_is_under_run_subdir() {
        let tmp = TempDir::new().unwrap();
        let path = token_path(tmp.path());
        assert_eq!(path.parent().unwrap(), tmp.path().join("run"));
        assert_eq!(path.file_name().unwrap(), AUTH_TOKEN_FILENAME);
    }

    #[test]
    fn base64_encoding_matches_known_vectors() {
        // Standard URL-safe base64 (no padding) test vectors.
        // Confirms the inline encoder matches `base64::URL_SAFE_NO_PAD`
        // bit-for-bit so a future swap to a real base64 dep is a
        // pure refactor.
        assert_eq!(base64_url_no_pad(b""), "");
        assert_eq!(base64_url_no_pad(b"f"), "Zg");
        assert_eq!(base64_url_no_pad(b"fo"), "Zm8");
        assert_eq!(base64_url_no_pad(b"foo"), "Zm9v");
        assert_eq!(base64_url_no_pad(b"foob"), "Zm9vYg");
        assert_eq!(base64_url_no_pad(b"fooba"), "Zm9vYmE");
        assert_eq!(base64_url_no_pad(b"foobar"), "Zm9vYmFy");
        // URL-safe alphabet (- and _) — bytes that produce 62/63 in
        // standard base64. 0xfb (1111 1011) → 6 bits = 62 + 11 → "+" / "-"
        assert_eq!(base64_url_no_pad(&[0xfb, 0xff]), "-_8");
    }

    /// Rotate while the daemon is "running" (simulated): the token
    /// file is replaced atomically. Any client that connects with
    /// the OLD token will fail verification (when verification
    /// lands in the next A.M4 commit); existing connections stay
    /// valid because the token check fires only at handshake.
    /// Today the only invariant we can pin is that the file is
    /// updated in place atomically.
    #[test]
    fn rotate_under_concurrent_reads_never_yields_partial_token() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        use std::time::Duration;

        let tmp = TempDir::new().unwrap();
        ensure_token(tmp.path()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_reader = Arc::clone(&stop);
        let root = tmp.path().to_path_buf();

        let reader = thread::spawn(move || {
            while !stop_reader.load(Ordering::Relaxed) {
                let token = read_token(&root).expect("read");
                assert_eq!(
                    token.len(),
                    43,
                    "concurrent read got partial token of length {}: {:?}",
                    token.len(),
                    token
                );
                thread::yield_now();
            }
        });

        for _ in 0..50 {
            rotate_token(tmp.path()).unwrap();
            thread::sleep(Duration::from_micros(100));
        }
        stop.store(true, Ordering::Relaxed);
        reader.join().expect("reader did not panic");
    }
}
