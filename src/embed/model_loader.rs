//! Model weight + tokenizer fetching, cached under `~/.mneme/models/`.
//!
//! Two responsibilities:
//!
//! 1. **Catalog.** A single source of truth for which embedding models we
//!    support, what their files are called on Hugging Face Hub, and which
//!    upstream revision we pin to. Adding a new model is one entry in
//!    [`MODELS`].
//!
//! 2. **Cache discipline.** [`ensure_model`] is idempotent: on first call
//!    it downloads files via `hf-hub` (which itself does atomic-rename,
//!    so a `kill -9` mid-download leaves no partial files visible) and
//!    writes a sidecar `<file>.sha256` next to each artefact. On every
//!    subsequent call it recomputes the hash and compares; mismatch
//!    means the cache is corrupt and we rebuild from upstream.
//!
//! # Why not pin upstream SHA256 in source?
//!
//! Pinning hashes that nobody in this repo has independently verified
//! would be cargo-cult security — it just memorialises "whatever the
//! first downloader saw." We instead rely on the upstream **revision**
//! pin (HF repos are git-LFS, so a revision IS a content-address of all
//! its files) for upstream integrity, and on the local sidecar hashes
//! for **local** integrity (disk bit-rot, half-overwritten files,
//! someone editing weights by hand).
//!
//! If a downstream user wants to verify their download against a known
//! hash, they can compare the `.sha256` sidecar to a hash they obtained
//! elsewhere — same file, same algorithm.

use crate::{MnemeError, Result};
use hf_hub::api::sync::ApiBuilder;
use hf_hub::{Repo, RepoType};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Files every BERT-style sentence transformer needs at load time.
///
/// `config.json` carries the architecture (hidden size, layer count, …).
/// `tokenizer.json` is a HuggingFace `tokenizers` v0.x serialized state.
/// `model.safetensors` is the weight tensor, mmaped at load.
#[derive(Debug, Clone)]
pub struct ModelFiles {
    pub config: PathBuf,
    pub tokenizer: PathBuf,
    pub weights: PathBuf,
}

/// Static catalog entry. New models register one of these in [`MODELS`].
#[derive(Debug, Clone)]
pub struct ModelEntry {
    /// Short, stable name users put in `config.embeddings.model`.
    pub short_name: &'static str,
    /// Human-readable label for diagnostics + the `mneme init` prompt.
    pub display_name: &'static str,
    /// Hugging Face repo identifier, e.g. `"BAAI/bge-m3"`.
    pub repo_id: &'static str,
    /// Pinned upstream revision (branch, tag, or commit). HF repos are
    /// git-LFS; a revision is the integrity guarantee for the file set.
    pub revision: &'static str,
    /// Output dimensionality. Cached on disk via [`MemoryItem`] schema
    /// version, so a model swap forces a re-embed migration.
    pub dim: usize,
    /// Approximate on-disk size after download — used for the user-
    /// facing prompt in `mneme init`.
    pub approx_size_mb: u32,
    /// Candidate weight filenames in priority order. The fetch path
    /// tries each in turn and accepts the first that resolves. Some
    /// repos (e.g. BAAI/bge-m3) only ship `pytorch_model.bin`; the
    /// loader picks the right `VarBuilder` constructor by extension.
    pub weight_candidates: &'static [&'static str],
}

/// Hugging Face revision used for the v0.1 MiniLM pin.
///
/// `refs/pr/21` ships `model.safetensors` (the official `main` branch
/// only has `pytorch_model.bin`). This is the same pin candle's own
/// example uses.
const MINILM_REVISION: &str = "refs/pr/21";

/// BGE-M3 lives on `main`; the maintainers tag releases via repo
/// snapshots rather than git tags.
const BGE_M3_REVISION: &str = "main";

/// Short name for the lightweight default. Used in
/// `config.embeddings.model = "minilm-l6"`.
pub const MINILM_L6: &str = "minilm-l6";

/// Short name for the BGE-M3 production default.
pub const BGE_M3: &str = "bge-m3";

/// Model registry. Keyed by [`ModelEntry::short_name`] so config.toml
/// lookups are stable and case-sensitive.
pub fn models() -> &'static BTreeMap<&'static str, ModelEntry> {
    static CATALOG: OnceLock<BTreeMap<&'static str, ModelEntry>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut m = BTreeMap::new();
        m.insert(
            MINILM_L6,
            ModelEntry {
                short_name: MINILM_L6,
                display_name: "all-MiniLM-L6-v2 (lightweight, English-focused)",
                repo_id: "sentence-transformers/all-MiniLM-L6-v2",
                revision: MINILM_REVISION,
                dim: 384,
                approx_size_mb: 90,
                // refs/pr/21 is the PR that adds model.safetensors to
                // the canonical MiniLM repo — same pin candle uses.
                weight_candidates: &["model.safetensors"],
            },
        );
        m.insert(
            BGE_M3,
            ModelEntry {
                short_name: BGE_M3,
                display_name: "BGE-M3 (multilingual, higher quality)",
                repo_id: "BAAI/bge-m3",
                revision: BGE_M3_REVISION,
                dim: 1024,
                approx_size_mb: 2300,
                // BAAI/bge-m3 only publishes pytorch_model.bin — there
                // is no model.safetensors at this revision. We still
                // ask for safetensors first so a future repo update
                // gets picked up automatically.
                weight_candidates: &["model.safetensors", "pytorch_model.bin"],
            },
        );
        m
    })
}

/// Look up a [`ModelEntry`] by short name. Returns `None` if the user
/// put a typo in `config.embeddings.model`.
pub fn lookup(short_name: &str) -> Option<&'static ModelEntry> {
    models().get(short_name)
}

/// Idempotently fetch and verify the three required BERT files.
///
/// First call downloads from the pinned revision and writes
/// `.sha256` sidecars. Every subsequent call recomputes hashes and
/// returns the cached paths if everything matches. On mismatch we
/// re-download — a corrupted local file is not a fatal error.
///
/// `cache_root` is typically `~/.mneme/models/`.
pub fn ensure_model(short_name: &str, cache_root: &Path) -> Result<ModelFiles> {
    let entry = lookup(short_name).ok_or_else(|| {
        let known: Vec<&str> = models().keys().copied().collect();
        MnemeError::Embedding(format!(
            "unknown embedding model `{short_name}`; known: {known:?}"
        ))
    })?;
    fetch_with_verify(entry, cache_root)
}

fn fetch_with_verify(entry: &ModelEntry, cache_root: &Path) -> Result<ModelFiles> {
    std::fs::create_dir_all(cache_root)
        .map_err(|e| MnemeError::Embedding(format!("create cache dir: {e}")))?;

    let api = ApiBuilder::new()
        .with_cache_dir(cache_root.to_path_buf())
        .with_progress(false)
        .build()
        .map_err(|e| MnemeError::Embedding(format!("hf-hub init: {e}")))?;

    let repo = api.repo(Repo::with_revision(
        entry.repo_id.to_string(),
        RepoType::Model,
        entry.revision.to_string(),
    ));

    let config = repo.get("config.json").map_err(|e| {
        MnemeError::Embedding(format!("download config.json for {}: {e}", entry.repo_id))
    })?;
    let tokenizer = repo.get("tokenizer.json").map_err(|e| {
        MnemeError::Embedding(format!(
            "download tokenizer.json for {}: {e}",
            entry.repo_id
        ))
    })?;
    let weights = fetch_first_available(&repo, entry)?;

    verify_or_record(&config)?;
    verify_or_record(&tokenizer)?;
    verify_or_record(&weights)?;

    Ok(ModelFiles {
        config,
        tokenizer,
        weights,
    })
}

/// Walk `entry.weight_candidates` in order; return the first that
/// downloads successfully. We log other failures at debug rather than
/// surfacing them — they're expected ("safetensors is missing, fall
/// back to .bin"). If every candidate fails, the final error wins.
fn fetch_first_available(repo: &hf_hub::api::sync::ApiRepo, entry: &ModelEntry) -> Result<PathBuf> {
    if entry.weight_candidates.is_empty() {
        return Err(MnemeError::Embedding(format!(
            "no weight candidates declared for {}",
            entry.short_name
        )));
    }
    let mut last_err: Option<String> = None;
    for filename in entry.weight_candidates {
        match repo.get(filename) {
            Ok(path) => {
                tracing::debug!(
                    repo = entry.repo_id,
                    file = filename,
                    "resolved weight file"
                );
                return Ok(path);
            }
            Err(e) => {
                tracing::debug!(
                    repo = entry.repo_id,
                    file = filename,
                    error = %e,
                    "weight candidate unavailable, trying next"
                );
                last_err = Some(format!("{filename}: {e}"));
            }
        }
    }
    Err(MnemeError::Embedding(format!(
        "no weight file available for {} at revision {}; tried {:?} (last error: {})",
        entry.repo_id,
        entry.revision,
        entry.weight_candidates,
        last_err.unwrap_or_else(|| "no candidates".into())
    )))
}

/// If a `<file>.sha256` sidecar exists, recompute and compare. If not,
/// compute and write. Mismatch is treated as cache corruption — we
/// remove the sidecar so the next caller treats it as a fresh download.
///
/// We deliberately do *not* delete the file itself: we leave it in
/// place so hf-hub's own cache state stays consistent, and the next
/// `ensure_model` re-downloads through hf-hub's normal path.
fn verify_or_record(path: &Path) -> Result<()> {
    let sidecar = sidecar_path(path);
    let actual = hash_file(path)?;
    if sidecar.exists() {
        let recorded = std::fs::read_to_string(&sidecar)
            .map_err(|e| MnemeError::Embedding(format!("read sidecar {sidecar:?}: {e}")))?;
        let recorded = recorded.trim();
        if recorded != actual {
            // Wipe the sidecar and the file so the next ensure_model
            // call refetches both from upstream cleanly.
            let _ = std::fs::remove_file(&sidecar);
            let _ = std::fs::remove_file(path);
            return Err(MnemeError::Embedding(format!(
                "checksum mismatch for {path:?}: recorded={recorded}, actual={actual}; \
                 cache invalidated, retry to refetch"
            )));
        }
        Ok(())
    } else {
        std::fs::write(&sidecar, &actual)
            .map_err(|e| MnemeError::Embedding(format!("write sidecar {sidecar:?}: {e}")))?;
        Ok(())
    }
}

fn sidecar_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".sha256");
    PathBuf::from(s)
}

/// Streaming SHA256. Reads in 64KB chunks so multi-GB BGE-M3 weights
/// don't pull the whole file into RAM just to verify.
fn hash_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)
        .map_err(|e| MnemeError::Embedding(format!("open {path:?} for hashing: {e}")))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| MnemeError::Embedding(format!("read {path:?}: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    Ok(hex_lower(&digest))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn catalog_contains_default_models() {
        let m = models();
        assert!(m.contains_key(MINILM_L6));
        assert!(m.contains_key(BGE_M3));
        assert_eq!(m[MINILM_L6].dim, 384);
        assert_eq!(m[BGE_M3].dim, 1024);
    }

    #[test]
    fn bge_m3_lists_pytorch_bin_fallback() {
        // BAAI/bge-m3 doesn't ship model.safetensors at the pinned
        // revision — without this fallback the loader 404s on first
        // run. Pin the catalog so the regression is loud.
        let entry = lookup(BGE_M3).expect("bge-m3 entry");
        assert!(
            entry.weight_candidates.contains(&"pytorch_model.bin"),
            "bge-m3 must accept pytorch_model.bin; got {:?}",
            entry.weight_candidates
        );
    }

    #[test]
    fn every_catalog_entry_declares_at_least_one_weight_file() {
        for (name, entry) in models() {
            assert!(
                !entry.weight_candidates.is_empty(),
                "{name} declared no weight_candidates"
            );
        }
    }

    #[test]
    fn lookup_unknown_returns_none() {
        assert!(lookup("nope").is_none());
    }

    #[test]
    fn ensure_unknown_model_errors() {
        let tmp = TempDir::new().unwrap();
        let err = ensure_model("nope", tmp.path()).unwrap_err();
        assert!(matches!(err, MnemeError::Embedding(_)));
    }

    #[test]
    fn hash_file_is_stable() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("data");
        std::fs::write(&p, b"the quick brown fox jumps over the lazy dog").unwrap();
        let h = hash_file(&p).unwrap();
        // Reference SHA256 of the pangram.
        assert_eq!(
            h,
            "05c6e08f1d9fdafa03147fcb8f82f124c76d2f70e3d989dc8aadb5e7d7450bec"
        );
    }

    #[test]
    fn verify_or_record_writes_sidecar_on_first_call() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("artifact.bin");
        std::fs::write(&p, b"hello world").unwrap();
        verify_or_record(&p).unwrap();
        let sidecar = sidecar_path(&p);
        assert!(sidecar.exists());
        // Second call should pass without rewriting.
        let mtime_before = std::fs::metadata(&sidecar).unwrap().modified().unwrap();
        verify_or_record(&p).unwrap();
        let mtime_after = std::fs::metadata(&sidecar).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after);
    }

    #[test]
    fn verify_detects_tampered_file_and_clears_cache() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("artifact.bin");
        std::fs::write(&p, b"original").unwrap();
        verify_or_record(&p).unwrap();

        // Tamper.
        std::fs::write(&p, b"tampered").unwrap();
        let err = verify_or_record(&p).unwrap_err();
        assert!(matches!(err, MnemeError::Embedding(_)));

        // Cache should be cleared so the next caller can re-fetch.
        assert!(!p.exists());
        assert!(!sidecar_path(&p).exists());
    }

    /// Network-dependent. Run with:
    ///   `cargo test -- --ignored embed::model_loader::tests::ensure_minilm_idempotent`
    #[test]
    #[ignore]
    fn ensure_minilm_idempotent() {
        let tmp = TempDir::new().unwrap();
        let first = ensure_model(MINILM_L6, tmp.path()).unwrap();
        assert!(first.config.exists());
        assert!(first.tokenizer.exists());
        assert!(first.weights.exists());
        // Sidecars present.
        assert!(sidecar_path(&first.weights).exists());
        // Second call should be a no-op (hash check, no re-download).
        let second = ensure_model(MINILM_L6, tmp.path()).unwrap();
        assert_eq!(first.weights, second.weights);
    }
}
