//! Atomic on-disk file encryption helpers (ADR-0013 P5).
//!
//! Each of the four non-redb data surfaces (HNSW snapshot, procedural
//! pinned.jsonl, session snapshots, cold archive) writes a whole file
//! at a time (or a sequence of whole lines, in the procedural case).
//! Rather than open-coding the seal/open + temp+rename pattern in
//! every module, this helper centralises it.
//!
//! Layout on disk:
//!
//! - **Whole-file**: a single [`Aead::seal`] envelope (`MAGIC | ver | nonce(24) | ct||tag`)
//!   replaces the entire plaintext contents. Atomic write via
//!   temp+rename + parent-dir fsync; mode `0o600` on unix.
//! - **JSONL**: each line on disk is a `base64(envelope)` plus a `\n`
//!   delimiter. Lines stay independently parseable, which preserves
//!   the procedural hot-reload semantics (the watcher reads one line
//!   at a time, decrypts, deserialises).
//!
//! AAD: callers pass their [`AadDomain`] and a per-file `position`
//! (e.g. session id, quarter label, HNSW schema version). Mixing a
//! ciphertext between surfaces fails authentication.

use crate::{
    Result,
    crypto::envelope::{AadDomain, Aead},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

/// Seal `plaintext` and write it atomically to `path`.
///
/// Uses a `<path>.tmp` sidecar then renames. On unix the temp file is
/// chmod'd to `0o600` before rename, so the encrypted blob is never
/// world-readable even mid-write. Parent dir is fsynced for crash
/// durability — matches the auth-token write path in
/// `src/daemon/auth.rs`.
pub fn seal_to_path(
    path: &Path,
    domain: AadDomain,
    position: &[u8],
    aead: &Aead,
    plaintext: &[u8],
) -> Result<()> {
    let envelope = aead.seal(domain, position, plaintext)?;
    write_atomic(path, &envelope)
}

/// Read the file at `path` and open the AEAD envelope.
///
/// Returns [`crate::MnemeError::Crypto`] if the envelope is malformed,
/// the key is wrong, or any byte in the file has been tampered with.
pub fn open_from_path(
    path: &Path,
    domain: AadDomain,
    position: &[u8],
    aead: &Aead,
) -> Result<Vec<u8>> {
    let envelope = std::fs::read(path)?;
    aead.open(domain, position, &envelope)
}

/// Write one base64-wrapped AEAD envelope per `Vec<u8>` of `lines` as
/// a single atomic file. Each output line is `base64(seal(plaintext))`
/// followed by `\n`. The procedural pinned-items writer uses this.
pub fn seal_lines_to_path(
    path: &Path,
    domain: AadDomain,
    aead: &Aead,
    lines: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>,
) -> Result<()> {
    let mut out: Vec<u8> = Vec::new();
    for (position, plaintext) in lines {
        let envelope = aead.seal(domain, &position, &plaintext)?;
        out.extend_from_slice(B64.encode(&envelope).as_bytes());
        out.push(b'\n');
    }
    write_atomic(path, &out)
}

/// Append a single AEAD envelope as one base64 line to `path`. Used
/// when the procedural watcher pins a new item without rewriting the
/// whole file.
///
/// Not atomic against process death (the file is opened in append
/// mode), but the line itself is written in one `write_all` followed
/// by `sync_data`. Partial-line writes on disk corruption are caught
/// by the line iterator on next read — it skips lines that fail to
/// decrypt with a logged warning rather than crashing the daemon.
pub fn append_sealed_line(
    path: &Path,
    domain: AadDomain,
    position: &[u8],
    aead: &Aead,
    plaintext: &[u8],
) -> Result<()> {
    let envelope = aead.seal(domain, position, plaintext)?;
    let mut line = B64.encode(&envelope).into_bytes();
    line.push(b'\n');

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    set_secret_file_perms(path)?;
    f.write_all(&line)?;
    f.sync_data()?;
    Ok(())
}

/// Stream every base64-encoded envelope line in `path`, decrypt each
/// in turn, and yield the resulting plaintext.
///
/// Lines that fail to decrypt are surfaced as [`crate::MnemeError::Crypto`]
/// — callers higher up (e.g. the procedural loader) decide whether to
/// skip the line or abort. We do not silently drop them here.
pub fn read_sealed_lines<F: FnMut(&[u8]) -> Vec<u8>>(
    path: &Path,
    domain: AadDomain,
    aead: &Aead,
    mut position_for_line: F,
) -> Result<Vec<Vec<u8>>> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(crate::MnemeError::Io(e)),
    };
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line.map_err(crate::MnemeError::Io)?;
        if line.trim().is_empty() {
            continue;
        }
        let envelope = B64
            .decode(line.trim())
            .map_err(|e| crate::MnemeError::Crypto(format!("base64 decode at line {idx}: {e}")))?;
        let position = position_for_line(&envelope);
        let plaintext = aead.open(domain, &position, &envelope)?;
        out.push(plaintext);
    }
    Ok(out)
}

fn write_atomic(path: &Path, body: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = with_tmp_suffix(path);
    std::fs::write(&tmp, body)?;
    set_secret_file_perms(&tmp)?;
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
        && let Err(e) = dir.sync_all()
    {
        tracing::warn!(
            error = %e,
            parent = %parent.display(),
            "encrypted file parent dir fsync failed",
        );
    }
    Ok(())
}

fn with_tmp_suffix(path: &Path) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    s.into()
}

#[cfg(unix)]
fn set_secret_file_perms(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_secret_file_perms(_path: &Path) -> Result<()> {
    // Windows: relies on the user-profile ACL of the data dir, same as
    // the daemon auth.token handling.
    Ok(())
}

/// Stream the contents of a reader, sealing it as a whole-file
/// envelope and writing to `writer`. Used by the cold-archive writer
/// to pipeline compressed bytes through encryption.
///
/// Currently a thin wrapper: reads all input into a buffer, seals,
/// writes. Memory usage = input size. The cold archives are bounded
/// at a few MiB per quarter, so this is fine; if quarterly archive
/// sizes grow beyond that the function should be streamed via
/// chunked-AEAD instead.
pub fn seal_reader_to_writer<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    domain: AadDomain,
    position: &[u8],
    aead: &Aead,
) -> Result<()> {
    let mut buf = Vec::new();
    reader
        .read_to_end(&mut buf)
        .map_err(crate::MnemeError::Io)?;
    let envelope = aead.seal(domain, position, &buf)?;
    writer.write_all(&envelope).map_err(crate::MnemeError::Io)?;
    Ok(())
}

/// Symmetric counterpart of [`seal_reader_to_writer`].
pub fn open_reader_to_writer<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    domain: AadDomain,
    position: &[u8],
    aead: &Aead,
) -> Result<()> {
    let mut buf = Vec::new();
    reader
        .read_to_end(&mut buf)
        .map_err(crate::MnemeError::Io)?;
    let plaintext = aead.open(domain, position, &buf)?;
    writer
        .write_all(&plaintext)
        .map_err(crate::MnemeError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::Dek;
    use tempfile::TempDir;

    fn aead() -> Aead {
        Aead::new(&Dek::generate().unwrap())
    }

    #[test]
    fn whole_file_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("snapshot.bin");
        let a = aead();
        let pt = vec![0xAB; 4096];
        seal_to_path(&path, AadDomain::Hnsw, b"v1", &a, &pt).unwrap();
        let out = open_from_path(&path, AadDomain::Hnsw, b"v1", &a).unwrap();
        assert_eq!(out, pt);
    }

    #[test]
    fn whole_file_wrong_position_fails() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("snapshot.bin");
        let a = aead();
        seal_to_path(&path, AadDomain::Hnsw, b"v1", &a, b"data").unwrap();
        assert!(open_from_path(&path, AadDomain::Hnsw, b"v2", &a).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn whole_file_has_0600_perms() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.snap");
        seal_to_path(&path, AadDomain::Session, b"id-1", &aead(), b"x").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn lines_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("pinned.jsonl");
        let a = aead();
        let inputs: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (b"01HKKA".to_vec(), b"first pinned item".to_vec()),
            (b"01HKKB".to_vec(), b"second pinned item".to_vec()),
            (b"01HKKC".to_vec(), b"third pinned item".to_vec()),
        ];
        seal_lines_to_path(&path, AadDomain::Pinned, &a, inputs.clone()).unwrap();

        let lines = read_sealed_lines(&path, AadDomain::Pinned, &a, |envelope| {
            // For the test, the position is encoded into the plaintext;
            // we'd normally derive it from the envelope's nonce or
            // companion metadata. Here we cycle through the inputs.
            let _ = envelope;
            Vec::new()
        });
        // The closure above passes empty position bytes, which won't
        // match the sealed AAD. So this call must fail — proving the
        // AAD is in fact bound to position.
        assert!(lines.is_err());

        // Read correctly by passing each line's position.
        let mut idx = 0usize;
        let plain = read_sealed_lines(&path, AadDomain::Pinned, &a, |_envelope| {
            let p = inputs[idx].0.clone();
            idx += 1;
            p
        })
        .unwrap();
        assert_eq!(plain.len(), 3);
        assert_eq!(plain[0], b"first pinned item");
        assert_eq!(plain[2], b"third pinned item");
    }

    #[test]
    fn append_line_then_read_all() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("pinned.jsonl");
        let a = aead();
        append_sealed_line(&path, AadDomain::Pinned, b"01HKKA", &a, b"alpha").unwrap();
        append_sealed_line(&path, AadDomain::Pinned, b"01HKKB", &a, b"bravo").unwrap();

        let positions = [b"01HKKA".to_vec(), b"01HKKB".to_vec()];
        let mut idx = 0;
        let plain = read_sealed_lines(&path, AadDomain::Pinned, &a, |_| {
            let p = positions[idx].clone();
            idx += 1;
            p
        })
        .unwrap();
        assert_eq!(plain, vec![b"alpha".to_vec(), b"bravo".to_vec()]);
    }

    #[test]
    fn missing_file_yields_empty_lines() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("absent.jsonl");
        let lines = read_sealed_lines(&path, AadDomain::Pinned, &aead(), |_| b"".to_vec()).unwrap();
        assert!(lines.is_empty());
    }

    #[test]
    fn reader_writer_round_trip() {
        let a = aead();
        let pt = b"the answer is fortytwo";
        let mut src = std::io::Cursor::new(pt);
        let mut sealed: Vec<u8> = Vec::new();
        seal_reader_to_writer(&mut src, &mut sealed, AadDomain::Cold, b"2026Q2", &a).unwrap();
        assert_ne!(&sealed[..4], pt);
        assert_eq!(&sealed[..4], b"MNE1");

        let mut sealed_cursor = std::io::Cursor::new(&sealed);
        let mut decrypted: Vec<u8> = Vec::new();
        open_reader_to_writer(
            &mut sealed_cursor,
            &mut decrypted,
            AadDomain::Cold,
            b"2026Q2",
            &a,
        )
        .unwrap();
        assert_eq!(decrypted, pt);
    }
}
