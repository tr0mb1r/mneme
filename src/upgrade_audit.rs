//! First-boot upgrade audit (release-planning v2.1 §5.3, Invariant 7
//! per pin `01KR5ZB7ED01HADZXZKKBV882Z`).
//!
//! On v1.1's first boot against an existing data directory, scan L4
//! once for memories exceeding `[budgets].max_remember_chars`, then
//! write a passive summary to `~/.mneme/diagnostics.log` so the user
//! can find oversized entries without spelunking. Gated by a marker
//! file at `~/.mneme/run/upgrade-audit.done` — runs at most ONCE per
//! data directory.
//!
//! PASSIVE NOTIFICATION ONLY. The audit never auto-modifies existing
//! memories. v1.0 accepted arbitrary content; some users will arrive
//! at v1.1 with L4 memories above the new ceiling. Those memories
//! remain readable + `recall`-able — only NEW writes/updates above
//! the limit are gated by C.M2. The verbatim principle (don't mutate
//! what the user told you to remember) is binding.
//!
//! The marker file lives in the v1.1-managed `~/.mneme/run/`
//! subdirectory. v1.0 binaries tolerate that directory cleanly per
//! Invariant 2 (verified by task #1 + the v1.0.1 backup-run-exclusion
//! patch in task #26).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;

use crate::Result;
use crate::mcp::tools::size_tier::{self, CorpusSizeStats};
use crate::storage::Storage;

/// Filename of the marker that pins "audit already ran for this data
/// directory". Lives under `~/.mneme/run/`.
pub const AUDIT_MARKER_FILE: &str = "upgrade-audit.done";

/// Filename of the diagnostics log the audit appends to.
pub const DIAGNOSTICS_LOG_FILE: &str = "diagnostics.log";

/// Outcome of [`run_if_needed`]. Lets callers log/observe what
/// happened without the audit module needing tracing context.
#[derive(Debug, Clone)]
pub enum AuditOutcome {
    /// Marker present from a prior boot; audit was a no-op.
    AlreadyDone,
    /// Audit ran. The `CorpusSizeStats` is what was written to the
    /// log; surface it to telemetry / boot logs as the caller sees fit.
    Ran(CorpusSizeStats),
}

/// Run the first-boot audit if the marker is absent. Best-effort:
/// every IO error becomes a `Result::Err` so the caller can decide
/// to log + continue (the audit is informational, not load-bearing).
///
/// `root` is the data directory (`~/.mneme`). `storage` is the
/// already-open redb handle (so the audit doesn't reopen + take a
/// second lock). `max_chars` is the configured ceiling against which
/// the tier classifier runs.
pub async fn run_if_needed(
    root: &Path,
    storage: &Arc<dyn Storage>,
    max_chars: usize,
) -> Result<AuditOutcome> {
    let marker = marker_path(root);
    if marker.exists() {
        return Ok(AuditOutcome::AlreadyDone);
    }

    // Make sure the run/ dir exists with restrictive perms before
    // the daemon ever spawns. v1.1's daemon mode will rely on this
    // dir existing too (sockets, auth.token); creating it here
    // costs nothing.
    let run_dir = run_dir(root);
    std::fs::create_dir_all(&run_dir)?;
    set_run_dir_perms(&run_dir)?;

    let stats = size_tier::count_corpus(storage, max_chars).await?;
    write_log_entry(root, max_chars, &stats)?;
    write_marker(&marker)?;
    Ok(AuditOutcome::Ran(stats))
}

/// Path to the audit-done marker. Exposed so tests + admin tooling
/// can stat / unlink it deliberately.
pub fn marker_path(root: &Path) -> PathBuf {
    run_dir(root).join(AUDIT_MARKER_FILE)
}

/// Path to the diagnostics log the audit appends to. Exposed for
/// the same reasons as [`marker_path`].
pub fn diagnostics_log_path(root: &Path) -> PathBuf {
    root.join(DIAGNOSTICS_LOG_FILE)
}

fn run_dir(root: &Path) -> PathBuf {
    root.join("run")
}

#[cfg(unix)]
fn set_run_dir_perms(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(dir, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_run_dir_perms(_dir: &Path) -> Result<()> {
    // Windows ACLs are managed when the daemon binds the named pipe;
    // the directory itself inherits the user's profile permissions
    // (which are already user-owned), so no explicit chmod is needed
    // here.
    Ok(())
}

fn write_marker(marker: &Path) -> Result<()> {
    let timestamp = Utc::now().to_rfc3339();
    std::fs::write(
        marker,
        format!("v1.1-upgrade-audit completed {timestamp}\n"),
    )?;
    Ok(())
}

fn write_log_entry(root: &Path, max_chars: usize, stats: &CorpusSizeStats) -> Result<()> {
    let path = diagnostics_log_path(root);
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(
        f,
        "{ts} v1.1 upgrade audit | max_remember_chars={limit} | total={total} \
         normal={n} advisory={a} warning={w} over_limit={o}",
        ts = Utc::now().to_rfc3339(),
        limit = max_chars,
        total = stats.total(),
        n = stats.normal,
        a = stats.advisory,
        w = stats.warning,
        o = stats.over_limit,
    )?;
    if !stats.over_limit_ids.is_empty() {
        writeln!(
            f,
            "  over-limit memory IDs (use `mneme inspect <id>` to view; \
             `mneme.forget` to remove if desired):"
        )?;
        for id in &stats.over_limit_ids {
            writeln!(f, "    {id}")?;
        }
        writeln!(
            f,
            "  Existing oversized memories remain readable + recall-able \
             (verbatim principle preserved). Only NEW writes/updates above \
             {max_chars} chars are rejected by the `remember`/`update` tools."
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::Embedder;
    use crate::embed::stub::StubEmbedder;
    use crate::memory::semantic::{MemoryKind, SemanticStore};
    use crate::storage::memory_impl::MemoryStorage;
    use tempfile::TempDir;

    async fn fresh_storage_with(tmp: &TempDir, seeds: &[&str]) -> Arc<dyn Storage> {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&storage), embedder).unwrap();
        for s in seeds {
            semantic
                .remember(s, MemoryKind::Fact, vec![], "test".into())
                .await
                .unwrap();
        }
        storage
    }

    #[tokio::test]
    async fn marker_absent_runs_audit_and_writes_log_plus_marker() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let storage =
            fresh_storage_with(&tmp, &["normal", &"a".repeat(700), &"b".repeat(15_000)]).await;

        let outcome = run_if_needed(root, &storage, size_tier::DEFAULT_MAX_CHARS)
            .await
            .unwrap();
        match outcome {
            AuditOutcome::Ran(stats) => {
                assert_eq!(stats.normal, 1);
                assert_eq!(stats.advisory, 1);
                assert_eq!(stats.warning, 0);
                assert_eq!(stats.over_limit, 1);
            }
            AuditOutcome::AlreadyDone => {
                panic!("first run should have executed the audit")
            }
        }

        // Marker created.
        assert!(marker_path(root).exists(), "marker file must be created");

        // Log file populated with summary + the over-limit ID.
        let log = std::fs::read_to_string(diagnostics_log_path(root)).unwrap();
        assert!(log.contains("v1.1 upgrade audit"));
        assert!(log.contains("max_remember_chars=10000"));
        assert!(log.contains("over_limit=1"));
        assert!(log.contains("over-limit memory IDs"));
    }

    #[tokio::test]
    async fn marker_present_skips_audit() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let storage = fresh_storage_with(&tmp, &["alpha", "bravo"]).await;

        // Pre-create the marker as if a previous boot already ran.
        std::fs::create_dir_all(root.join("run")).unwrap();
        std::fs::write(marker_path(root), "fake prior run").unwrap();

        let outcome = run_if_needed(root, &storage, size_tier::DEFAULT_MAX_CHARS)
            .await
            .unwrap();
        assert!(matches!(outcome, AuditOutcome::AlreadyDone));

        // Diagnostics log NOT created — second boot is a no-op.
        assert!(
            !diagnostics_log_path(root).exists(),
            "no log entry should land on a no-op audit"
        );
        // Marker text preserved (we did NOT rewrite it).
        let marker_contents = std::fs::read_to_string(marker_path(root)).unwrap();
        assert_eq!(marker_contents, "fake prior run");
    }

    #[tokio::test]
    async fn empty_corpus_audit_emits_zero_summary() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let storage = fresh_storage_with(&tmp, &[]).await;

        let outcome = run_if_needed(root, &storage, size_tier::DEFAULT_MAX_CHARS)
            .await
            .unwrap();
        assert!(matches!(outcome, AuditOutcome::Ran(_)));

        let log = std::fs::read_to_string(diagnostics_log_path(root)).unwrap();
        assert!(log.contains("total=0"));
        assert!(log.contains("normal=0"));
        // No over_limit_ids section when the count is zero.
        assert!(!log.contains("over-limit memory IDs"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_dir_gets_0700_perms_after_audit() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let storage = fresh_storage_with(&tmp, &["x"]).await;

        run_if_needed(root, &storage, size_tier::DEFAULT_MAX_CHARS)
            .await
            .unwrap();
        let mode = std::fs::metadata(root.join("run"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "run/ must be 0o700, got {mode:o}");
    }

    #[tokio::test]
    async fn audit_log_appends_across_data_dirs() {
        // The marker is per-data-dir; running the audit in two
        // different data dirs each writes its own log. This is
        // mostly a sanity check that nothing reaches outside `root`.
        let tmp_a = TempDir::new().unwrap();
        let tmp_b = TempDir::new().unwrap();
        let storage_a = fresh_storage_with(&tmp_a, &["a"]).await;
        let storage_b = fresh_storage_with(&tmp_b, &["b1", "b2"]).await;

        run_if_needed(tmp_a.path(), &storage_a, size_tier::DEFAULT_MAX_CHARS)
            .await
            .unwrap();
        run_if_needed(tmp_b.path(), &storage_b, size_tier::DEFAULT_MAX_CHARS)
            .await
            .unwrap();

        let log_a = std::fs::read_to_string(diagnostics_log_path(tmp_a.path())).unwrap();
        let log_b = std::fs::read_to_string(diagnostics_log_path(tmp_b.path())).unwrap();
        assert!(log_a.contains("total=1"));
        assert!(log_b.contains("total=2"));
    }
}
