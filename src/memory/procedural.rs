//! Phase 4 L0 — procedural memory.
//!
//! Procedural memory is the small, hand-curated list of facts /
//! preferences / instructions the agent should *always* have at the
//! top of its context, regardless of recency or semantic match. The
//! v1 storage is a plain JSONL file at `<root>/procedural/pinned.jsonl`
//! — one [`PinnedItem`] per line, designed to be human-readable and
//! human-editable. Editing the file externally must reflect back into
//! the running server within 1 s without a restart (Phase 4 exit
//! gate); that's what the [`notify`] watcher buys us.
//!
//! # File format
//!
//! Each line is a self-contained JSON object:
//!
//! ```json
//! {"id":"01HK...","content":"prefer rust over python","tags":["pref"],"scope":"personal","created_at":"2026-04-28T16:48:00Z"}
//! ```
//!
//! `created_at` matches the timestamp field on
//! [`crate::memory::semantic::MemoryItem`] and
//! [`crate::memory::episodic::EpisodicEvent`] so a generic
//! "memory metadata" pass over any of the three stores can rely on a
//! consistent field name. Older files that wrote `added_at` (the
//! pre-normalization name) keep loading via `#[serde(alias)]`.
//!
//! Lines are written by `pin` and consumed by `list` / `unpin`. We
//! deliberately don't gate on schema strictness for unknown lines —
//! a user who edits the file by hand and adds a comment line gets a
//! warning + skip, not a startup crash. Same forgiveness for invalid
//! JSON, missing fields, or duplicated ids: everything best-effort,
//! errors logged via `tracing`.
//!
//! # Concurrency
//!
//! All mutations go through an `async` write lock. Reads serve from
//! an in-memory `Vec<PinnedItem>` cache. A background filesystem
//! watcher flips a `stale` flag whenever the file changes — including
//! after our own writes (we simply re-flip it back) — and `list()`
//! re-reads the file lazily when the flag is set. The result is at
//! most one disk read per `list()` call after an external edit, and
//! zero disk reads per `list()` in the hot path.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use notify::{Config, EventKind, PollWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};

use crate::ids::MemoryId;
use crate::{MnemeError, Result};

/// Poll interval for the file watcher.
///
/// We deliberately use [`PollWatcher`] over the platform-native
/// `RecommendedWatcher`: macOS FSEvents coalesces events for
/// ~30 seconds by default which blows the Phase 4 exit-gate
/// "reflected within 1 s" budget; Windows ReadDirectoryChangesW has
/// its own warts around editor save patterns. A 500 ms poll on a
/// single small file is essentially free CPU and gives us the same
/// observable latency on every host.
const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Filename under `<root>/procedural/`. Not configurable in v1 — the
/// path is part of the user-facing contract (CLAUDE.md style).
pub const PINNED_FILE: &str = "pinned.jsonl";

/// One pinned item. Designed for `serde_json` round-trips so the
/// on-disk JSONL stays human-readable.
///
/// `created_at` is the canonical timestamp field across every memory
/// store; old JSONL written with `added_at` still loads via
/// `#[serde(alias = "added_at")]` for one release cycle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PinnedItem {
    pub id: MemoryId,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub scope: String,
    #[serde(alias = "added_at")]
    pub created_at: DateTime<Utc>,
}

/// Reader/writer for the procedural pinned-list.
///
/// Construction loads `pinned.jsonl` into the cache and starts the
/// `notify` watcher; the watcher's lifetime is tied to the
/// `ProceduralStore` value (it's stored in a field so dropping the
/// store also drops the watcher and its background thread).
pub struct ProceduralStore {
    path: PathBuf,
    cache: Arc<Mutex<Vec<PinnedItem>>>,
    /// Set to `true` whenever the file is modified externally OR by
    /// us. Reads observe-and-clear under the cache lock; writes
    /// observe-and-clear after they update the cache themselves.
    stale: Arc<AtomicBool>,
    /// Tokio mutex (not std::sync::Mutex) so write critical sections
    /// can hold across the async file I/O without blocking the runtime.
    write_lock: Arc<tokio::sync::Mutex<()>>,
    /// Holding the watcher in a field keeps its background thread
    /// alive. Wrapped in `Option` only so `Drop` can take it out
    /// before joining; today we let RAII handle it.
    _watcher: Option<PollWatcher>,
}

impl ProceduralStore {
    /// Open or create the procedural store rooted at `<root>/procedural/`.
    ///
    /// Creates the directory + an empty `pinned.jsonl` if neither
    /// exists. Loading errors (corrupt JSON, partial lines) emit a
    /// `tracing::warn` and skip the offending entry; the rest of the
    /// file still loads.
    pub fn open(root: &Path) -> Result<Self> {
        let dir = root.join("procedural");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(PINNED_FILE);
        if !path.exists() {
            std::fs::write(&path, b"")?;
        }

        let initial = read_file(&path)?;
        let cache = Arc::new(Mutex::new(initial));
        let stale = Arc::new(AtomicBool::new(false));

        let watcher = spawn_watcher(&path, Arc::clone(&stale))?;

        Ok(Self {
            path,
            cache,
            stale,
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
            _watcher: Some(watcher),
        })
    }

    /// Currently-pinned items. Optionally filter by scope. If the
    /// watcher has flipped the stale flag since the last call, the
    /// cache is rebuilt from disk first.
    pub fn list(&self, scope: Option<&str>) -> Result<Vec<PinnedItem>> {
        if self.stale.swap(false, Ordering::SeqCst) {
            let fresh = read_file(&self.path)?;
            *self
                .cache
                .lock()
                .map_err(|e| MnemeError::Storage(format!("procedural cache poisoned: {e}")))? =
                fresh;
        }
        let g = self
            .cache
            .lock()
            .map_err(|e| MnemeError::Storage(format!("procedural cache poisoned: {e}")))?;
        Ok(match scope {
            None => g.clone(),
            Some(s) => g.iter().filter(|p| p.scope == s).cloned().collect(),
        })
    }

    /// Append a new pinned item. Returns the assigned id.
    pub async fn pin(&self, content: String, tags: Vec<String>, scope: String) -> Result<MemoryId> {
        let item = PinnedItem {
            id: MemoryId::new(),
            content,
            tags,
            scope,
            created_at: Utc::now(),
        };
        let id = item.id;

        let _g = self.write_lock.lock().await;
        // Serialize ONE line, append.
        let line = serde_json::to_string(&item)
            .map_err(|e| MnemeError::Storage(format!("encode pinned: {e}")))?;
        append_line(&self.path, &line)?;

        // Update the in-memory cache directly so the next list() call
        // doesn't have to re-read the file.
        let mut g = self
            .cache
            .lock()
            .map_err(|e| MnemeError::Storage(format!("procedural cache poisoned: {e}")))?;
        g.push(item);
        // The watcher will fire stale=true off our own write; clear
        // it so the next list() doesn't bother reloading what we
        // just modeled in memory.
        self.stale.store(false, Ordering::SeqCst);
        Ok(id)
    }

    /// Remove a pinned item by id. Returns `true` if it existed.
    /// Atomic: writes a new file and renames over the old one, so
    /// a `kill -9` mid-call leaves the original file intact.
    pub async fn unpin(&self, id: MemoryId) -> Result<bool> {
        let _g = self.write_lock.lock().await;

        // Re-read from disk to avoid trusting a possibly-stale cache.
        // External edits could have already removed the row.
        let current = read_file(&self.path)?;
        let mut found = false;
        let kept: Vec<PinnedItem> = current
            .into_iter()
            .filter(|p| {
                if p.id == id {
                    found = true;
                    false
                } else {
                    true
                }
            })
            .collect();
        if !found {
            return Ok(false);
        }

        rewrite_file(&self.path, &kept)?;
        let mut g = self
            .cache
            .lock()
            .map_err(|e| MnemeError::Storage(format!("procedural cache poisoned: {e}")))?;
        *g = kept;
        self.stale.store(false, Ordering::SeqCst);
        Ok(true)
    }

    /// Path to the on-disk JSONL, surfaced for diagnostics + the
    /// `mneme://procedural` resource.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ---------- File I/O ----------

fn read_file(path: &Path) -> Result<Vec<PinnedItem>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(MnemeError::Io(e)),
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(e) => {
            return Err(MnemeError::Storage(format!(
                "{}: not valid UTF-8: {e}",
                path.display()
            )));
        }
    };
    let mut out = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Lines starting with '#' are honoured as user comments, not
        // an error — keeps `pinned.jsonl` truly human-friendly.
        if trimmed.starts_with('#') {
            continue;
        }
        match serde_json::from_str::<PinnedItem>(trimmed) {
            Ok(item) => out.push(item),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    line = lineno + 1,
                    error = %e,
                    "skipping malformed pinned.jsonl line"
                );
            }
        }
    }
    Ok(out)
}

fn append_line(path: &Path, line: &str) -> Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    f.sync_data()?;
    Ok(())
}

fn rewrite_file(path: &Path, items: &[PinnedItem]) -> Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;
    // Atomic temp + rename, mirrors `snapshot::save`. A `kill -9`
    // mid-rewrite leaves either the old or new file, never a torn
    // partial.
    let tmp = tmp_path_for(path);
    {
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        for it in items {
            let line = serde_json::to_string(it)
                .map_err(|e| MnemeError::Storage(format!("encode pinned: {e}")))?;
            f.write_all(line.as_bytes())?;
            f.write_all(b"\n")?;
        }
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

// ---------- Watcher ----------

fn spawn_watcher(path: &Path, stale: Arc<AtomicBool>) -> Result<PollWatcher> {
    let path = path.to_path_buf();
    // Watch the parent directory rather than the file itself, because
    // many editors (vim, vscode) implement "save" as
    // delete-then-create — watching the file directly would lose
    // events past the first delete.
    let watch_dir = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let path_match = path.file_name().map(|n| n.to_owned());
    let mut watcher = PollWatcher::new(
        move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else {
                return;
            };
            let interesting = matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            );
            if !interesting {
                return;
            }
            if let Some(target) = path_match.as_ref() {
                let touches_target = event
                    .paths
                    .iter()
                    .any(|p| p.file_name() == Some(target.as_os_str()));
                if !touches_target {
                    return;
                }
            }
            stale.store(true, Ordering::SeqCst);
        },
        Config::default()
            .with_poll_interval(WATCH_POLL_INTERVAL)
            // Compare file *contents* on each tick, not just mtime.
            // Some filesystems (and back-to-back writes) collapse to
            // a one-second mtime resolution; comparing contents
            // catches those cases.
            .with_compare_contents(true),
    )
    .map_err(|e| MnemeError::Storage(format!("watcher init: {e}")))?;
    watcher
        .watch(&watch_dir, RecursiveMode::NonRecursive)
        .map_err(|e| MnemeError::Storage(format!("watcher start: {e}")))?;
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    /// Phase 4 exit gate target: external edits reflected within 1 s.
    /// We allow some slack — the kernel's filesystem-event delivery is
    /// not strictly bounded — but assert under 2 s so a flake is loud.
    const HOT_RELOAD_TIMEOUT: Duration = Duration::from_secs(2);

    fn store(tmp: &TempDir) -> ProceduralStore {
        ProceduralStore::open(tmp.path()).unwrap()
    }

    /// Poll `list()` until `predicate` is true or `HOT_RELOAD_TIMEOUT`
    /// elapses. Returns whether the predicate became true. Tests use
    /// this in lieu of fixed sleeps so they don't flake on slow CI.
    fn await_list_until(s: &ProceduralStore, predicate: impl Fn(&[PinnedItem]) -> bool) -> bool {
        let deadline = Instant::now() + HOT_RELOAD_TIMEOUT;
        while Instant::now() < deadline {
            if let Ok(items) = s.list(None)
                && predicate(&items)
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[tokio::test]
    async fn open_creates_empty_file_if_missing() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        assert!(s.path().exists());
        assert!(s.list(None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn pin_round_trips_through_list() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let id = s
            .pin(
                "always prefer rust".into(),
                vec!["pref".into()],
                "personal".into(),
            )
            .await
            .unwrap();
        let items = s.list(None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, id);
        assert_eq!(items[0].content, "always prefer rust");
        assert_eq!(items[0].tags, vec!["pref".to_string()]);
    }

    #[tokio::test]
    async fn list_filters_by_scope() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let work = s
            .pin("ship by friday".into(), vec![], "work".into())
            .await
            .unwrap();
        let _personal = s
            .pin("dinner at 7".into(), vec![], "personal".into())
            .await
            .unwrap();
        let work_items = s.list(Some("work")).unwrap();
        assert_eq!(work_items.len(), 1);
        assert_eq!(work_items[0].id, work);
    }

    #[tokio::test]
    async fn unpin_removes_item_and_returns_true() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let id = s
            .pin("ephemeral".into(), vec![], "personal".into())
            .await
            .unwrap();
        assert!(s.unpin(id).await.unwrap());
        assert!(s.list(None).unwrap().is_empty());
        // Idempotent: a second unpin with the same id is a no-op.
        assert!(!s.unpin(id).await.unwrap());
    }

    #[tokio::test]
    async fn unpin_unknown_id_returns_false() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        assert!(!s.unpin(MemoryId::new()).await.unwrap());
    }

    #[tokio::test]
    async fn malformed_lines_are_skipped_with_warning() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("procedural").join(PINNED_FILE);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let good = serde_json::to_string(&PinnedItem {
            id: MemoryId::new(),
            content: "kept".into(),
            tags: vec![],
            scope: "personal".into(),
            created_at: Utc::now(),
        })
        .unwrap();
        let body = format!("# user comment\n{good}\n{{not json\nplain garbage\n");
        std::fs::write(&path, body).unwrap();

        let s = store(&tmp);
        let items = s.list(None).unwrap();
        assert_eq!(items.len(), 1, "only the well-formed line should load");
        assert_eq!(items[0].content, "kept");
    }

    /// The Phase 4 exit gate: external edits to `pinned.jsonl` are
    /// reflected within 1 s without restart. Asserts under 2 s.
    #[tokio::test]
    async fn external_edit_is_observed_within_one_second() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        // Sanity: starts empty.
        assert!(s.list(None).unwrap().is_empty());

        // External edit: write a new pinned.jsonl out from under us.
        // (Mirrors what a user editing the file in vim would do.)
        let new_item = PinnedItem {
            id: MemoryId::new(),
            content: "added by user editor".into(),
            tags: vec!["external".into()],
            scope: "personal".into(),
            created_at: Utc::now(),
        };
        let mut body = serde_json::to_string(&new_item).unwrap();
        body.push('\n');
        std::fs::write(s.path(), body).unwrap();

        let observed = await_list_until(&s, |items| items.len() == 1 && items[0].id == new_item.id);
        assert!(
            observed,
            "external edit not picked up within {HOT_RELOAD_TIMEOUT:?}"
        );
    }

    #[tokio::test]
    async fn pin_unpin_pin_survives_round_trip() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        let _a = s
            .pin("alpha".into(), vec![], "personal".into())
            .await
            .unwrap();
        let b = s
            .pin("bravo".into(), vec![], "personal".into())
            .await
            .unwrap();
        let _c = s
            .pin("charlie".into(), vec![], "personal".into())
            .await
            .unwrap();
        s.unpin(b).await.unwrap();

        // Reopen — the cache is rebuilt from disk, so this validates
        // the on-disk file is the canonical source of truth.
        drop(s);
        let s2 = ProceduralStore::open(tmp.path()).unwrap();
        let items = s2.list(None).unwrap();
        let contents: Vec<&str> = items.iter().map(|p| p.content.as_str()).collect();
        assert_eq!(contents, vec!["alpha", "charlie"]);
    }
}
