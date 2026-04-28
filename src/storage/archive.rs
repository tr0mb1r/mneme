//! Phase 4 §4 — cold-tier archive.
//!
//! Memories that have aged past the warm-tier window are migrated
//! into per-quarter zstd-compressed bundles under `<root>/cold/`. The
//! bundle file name encodes the calendar quarter:
//!
//! ```text
//! <root>/cold/2026-Q2.zst
//! ```
//!
//! Each bundle is a single postcard-encoded `Vec<EpisodicEvent>`,
//! zstd-compressed at level 3 (good speed/ratio tradeoff for
//! mostly-text JSON payloads). Bundles are append-rewrite: a new
//! event in 2026-Q2 forces a decompress → push → re-sort by
//! `created_at` → recompress → atomic temp+rename.
//!
//! That's expensive at scale, but the consolidation cadence is
//! quarterly-or-slower for warm→cold transitions, so each rewrite
//! happens ~once per N hundred days of new cold inflow. Read by id
//! is a linear scan over a single decompressed bundle — well under
//! the spec §13 < 500ms target for our v0.1 cardinalities (≤10K
//! events per quarter).
//!
//! Quarterly bucketing matches calendar quarters as observed by
//! `chrono::DateTime<Utc>::month`:
//!
//! ```text
//! Jan/Feb/Mar  → Q1
//! Apr/May/Jun  → Q2
//! Jul/Aug/Sep  → Q3
//! Oct/Nov/Dec  → Q4
//! ```

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::EventId;
use crate::memory::episodic::EpisodicEvent;
use crate::{MnemeError, Result};

/// zstd compression level used for cold bundles. Level 3 is the
/// upstream default — best speed/ratio balance for JSON-ish
/// payloads. Level 19 would shave ~30% but costs orders of
/// magnitude more CPU on write, which doesn't pay back on
/// quarterly inflow rates.
const ZSTD_LEVEL: i32 = 3;

/// Subdirectory under `<root>/`. Matches `storage::layout::SUBDIRS`.
const COLD_SUBDIR: &str = "cold";

/// Per-bundle on-disk wrapper. Carries a schema version so we can
/// evolve the payload shape without breaking older snapshots.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ColdBundle {
    schema: u32,
    /// Sorted ascending by `created_at` so binary-search lookups stay
    /// predictable as the file grows.
    events: Vec<EpisodicEvent>,
}

const COLD_BUNDLE_SCHEMA: u32 = 1;

/// Calendar quarter `(year, quarter ∈ 1..=4)` — used as the bundle
/// key.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct Quarter {
    pub year: i32,
    /// 1, 2, 3, or 4.
    pub quarter: u8,
}

impl Quarter {
    /// Quarter that contains `ts`.
    pub fn containing(ts: DateTime<Utc>) -> Self {
        let q = ((ts.month() - 1) / 3 + 1) as u8;
        Self {
            year: ts.year(),
            quarter: q,
        }
    }

    fn filename(&self) -> String {
        format!("{:04}-Q{}.zst", self.year, self.quarter)
    }
}

/// Cold-tier archive rooted at `<root>/cold/`.
///
/// Construction is cheap (no I/O); the directory is created on first
/// `append` if it doesn't already exist, matching the lazy-init
/// pattern the rest of `storage::` uses.
#[derive(Clone)]
pub struct ColdArchive {
    dir: PathBuf,
}

impl ColdArchive {
    pub fn new(root: &Path) -> Self {
        Self {
            dir: root.join(COLD_SUBDIR),
        }
    }

    /// Append `events` into their corresponding quarterly bundles.
    ///
    /// Events are grouped by `Quarter::containing(created_at)`; one
    /// rewrite per affected quarter. Existing bundles are read,
    /// merged with the new events (preserving id-uniqueness — duplicate
    /// ids prefer the *new* event so re-running a partial migration
    /// is idempotent), sorted by `created_at`, and rewritten via
    /// atomic temp+rename so a `kill -9` mid-call leaves the previous
    /// bundle intact.
    pub fn append(&self, events: &[EpisodicEvent]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| MnemeError::Storage(format!("create cold dir: {e}")))?;

        let mut by_quarter: std::collections::BTreeMap<Quarter, Vec<EpisodicEvent>> =
            std::collections::BTreeMap::new();
        for ev in events {
            by_quarter
                .entry(Quarter::containing(ev.created_at))
                .or_default()
                .push(ev.clone());
        }

        for (q, new_events) in by_quarter {
            self.append_into_quarter(q, new_events)?;
        }
        Ok(())
    }

    fn append_into_quarter(&self, q: Quarter, mut new: Vec<EpisodicEvent>) -> Result<()> {
        let path = self.dir.join(q.filename());
        let mut existing = if path.exists() {
            self.read_bundle(&path)?
        } else {
            ColdBundle {
                schema: COLD_BUNDLE_SCHEMA,
                events: Vec::new(),
            }
        };

        // Drop any existing rows whose id appears in `new` so the
        // newer copy wins on conflict.
        let new_ids: std::collections::HashSet<EventId> = new.iter().map(|e| e.id).collect();
        existing.events.retain(|e| !new_ids.contains(&e.id));
        existing.events.append(&mut new);
        existing
            .events
            .sort_by_key(|e| (e.created_at, e.id.0.to_bytes()));

        self.write_bundle(&path, &existing)
    }

    /// All events in a single quarter. Used by tests + any future
    /// "full audit" path. Returns an empty vec if the bundle doesn't
    /// exist.
    pub fn read_quarter(&self, q: Quarter) -> Result<Vec<EpisodicEvent>> {
        let path = self.dir.join(q.filename());
        if !path.exists() {
            return Ok(Vec::new());
        }
        Ok(self.read_bundle(&path)?.events)
    }

    /// Lookup a single event by id. Linear scan over the bundle that
    /// would contain `created_at`; if you don't know the timestamp,
    /// use [`find_anywhere`](Self::find_anywhere) instead (which
    /// walks every bundle).
    pub fn find(&self, id: EventId, created_at: DateTime<Utc>) -> Result<Option<EpisodicEvent>> {
        let q = Quarter::containing(created_at);
        Ok(self.read_quarter(q)?.into_iter().find(|e| e.id == id))
    }

    /// Lookup a single event by id when the timestamp isn't known.
    /// Walks every bundle; intended for diagnostic / `mneme inspect`
    /// paths, not the hot recall path.
    pub fn find_anywhere(&self, id: EventId) -> Result<Option<EpisodicEvent>> {
        if !self.dir.exists() {
            return Ok(None);
        }
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("zst") {
                continue;
            }
            let bundle = self.read_bundle(&path)?;
            if let Some(e) = bundle.events.into_iter().find(|e| e.id == id) {
                return Ok(Some(e));
            }
        }
        Ok(None)
    }

    /// Quarters currently on disk, ordered oldest-first.
    pub fn list_quarters(&self) -> Result<Vec<Quarter>> {
        let mut out = Vec::new();
        if !self.dir.exists() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if let Some(stem) = s.strip_suffix(".zst")
                && let Some((y, q)) = stem.split_once("-Q")
                && let (Ok(year), Ok(quarter)) = (y.parse::<i32>(), q.parse::<u8>())
                && (1..=4).contains(&quarter)
            {
                out.push(Quarter { year, quarter });
            }
        }
        out.sort();
        Ok(out)
    }

    fn read_bundle(&self, path: &Path) -> Result<ColdBundle> {
        let f = OpenOptions::new().read(true).open(path)?;
        let mut decoder =
            zstd::Decoder::new(f).map_err(|e| MnemeError::Storage(format!("zstd init: {e}")))?;
        let mut buf = Vec::new();
        decoder
            .read_to_end(&mut buf)
            .map_err(|e| MnemeError::Storage(format!("zstd decompress: {e}")))?;
        let bundle: ColdBundle = postcard::from_bytes(&buf)
            .map_err(|e| MnemeError::Storage(format!("decode ColdBundle: {e}")))?;
        if bundle.schema != COLD_BUNDLE_SCHEMA {
            return Err(MnemeError::Storage(format!(
                "cold bundle {path:?} schema {} != supported {COLD_BUNDLE_SCHEMA}",
                bundle.schema
            )));
        }
        Ok(bundle)
    }

    fn write_bundle(&self, path: &Path, bundle: &ColdBundle) -> Result<()> {
        let payload = postcard::to_allocvec(bundle)
            .map_err(|e| MnemeError::Storage(format!("encode ColdBundle: {e}")))?;
        let tmp = tmp_path_for(path);
        {
            let f = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp)?;
            let mut enc = zstd::Encoder::new(f, ZSTD_LEVEL)
                .map_err(|e| MnemeError::Storage(format!("zstd encoder init: {e}")))?;
            enc.write_all(&payload)?;
            let f = enc
                .finish()
                .map_err(|e| MnemeError::Storage(format!("zstd encoder finish: {e}")))?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::episodic::DEFAULT_RETRIEVAL_WEIGHT;
    use chrono::TimeZone;
    use std::time::Instant;
    use tempfile::TempDir;

    fn event_at(year: i32, month: u32, day: u32, kind: &str) -> EpisodicEvent {
        let ts = Utc.with_ymd_and_hms(year, month, day, 12, 0, 0).unwrap();
        EpisodicEvent {
            id: EventId::new(),
            kind: kind.into(),
            scope: "personal".into(),
            payload: format!("\"{kind}-{year}-{month:02}-{day:02}\""),
            retrieval_weight: DEFAULT_RETRIEVAL_WEIGHT,
            last_accessed: ts,
            created_at: ts,
        }
    }

    #[test]
    fn quarter_containing_maps_months_correctly() {
        let assert_q = |month: u32, expected_q: u8| {
            let ts = Utc.with_ymd_and_hms(2026, month, 15, 0, 0, 0).unwrap();
            assert_eq!(Quarter::containing(ts).quarter, expected_q, "month {month}");
        };
        assert_q(1, 1);
        assert_q(3, 1);
        assert_q(4, 2);
        assert_q(6, 2);
        assert_q(7, 3);
        assert_q(9, 3);
        assert_q(10, 4);
        assert_q(12, 4);
    }

    #[test]
    fn append_then_read_round_trips_within_one_quarter() {
        let tmp = TempDir::new().unwrap();
        let a = ColdArchive::new(tmp.path());
        let events = vec![event_at(2026, 4, 1, "a"), event_at(2026, 5, 15, "b")];
        a.append(&events).unwrap();

        let read = a
            .read_quarter(Quarter {
                year: 2026,
                quarter: 2,
            })
            .unwrap();
        assert_eq!(read.len(), 2);
        // Sorted by created_at ascending.
        assert!(read[0].created_at < read[1].created_at);
    }

    #[test]
    fn append_groups_into_separate_quarters() {
        let tmp = TempDir::new().unwrap();
        let a = ColdArchive::new(tmp.path());
        let events = vec![
            event_at(2026, 1, 5, "q1"),
            event_at(2026, 5, 5, "q2"),
            event_at(2026, 9, 5, "q3"),
            event_at(2026, 11, 5, "q4"),
        ];
        a.append(&events).unwrap();

        let qs = a.list_quarters().unwrap();
        assert_eq!(qs.len(), 4);
        for q in 1..=4 {
            let bundle = a
                .read_quarter(Quarter {
                    year: 2026,
                    quarter: q,
                })
                .unwrap();
            assert_eq!(bundle.len(), 1, "expected 1 event in 2026-Q{q}");
        }
    }

    #[test]
    fn second_append_merges_into_existing_quarter() {
        let tmp = TempDir::new().unwrap();
        let a = ColdArchive::new(tmp.path());
        a.append(&[event_at(2026, 4, 1, "first")]).unwrap();
        a.append(&[event_at(2026, 4, 15, "second")]).unwrap();
        let bundle = a
            .read_quarter(Quarter {
                year: 2026,
                quarter: 2,
            })
            .unwrap();
        assert_eq!(bundle.len(), 2);
    }

    #[test]
    fn duplicate_id_overwrites_existing() {
        let tmp = TempDir::new().unwrap();
        let a = ColdArchive::new(tmp.path());
        let mut original = event_at(2026, 4, 1, "v1");
        let id = original.id;
        a.append(&[original.clone()]).unwrap();
        // Re-record with the same id but a new payload.
        original.payload = "\"v2\"".into();
        a.append(&[original]).unwrap();
        let bundle = a
            .read_quarter(Quarter {
                year: 2026,
                quarter: 2,
            })
            .unwrap();
        assert_eq!(bundle.len(), 1);
        assert_eq!(bundle[0].id, id);
        assert_eq!(bundle[0].payload, "\"v2\"");
    }

    #[test]
    fn find_returns_event_when_present() {
        let tmp = TempDir::new().unwrap();
        let a = ColdArchive::new(tmp.path());
        let target = event_at(2026, 5, 10, "needle");
        let id = target.id;
        let ts = target.created_at;
        a.append(&[target, event_at(2026, 5, 20, "noise")]).unwrap();

        let got = a.find(id, ts).unwrap().unwrap();
        assert_eq!(got.id, id);
    }

    #[test]
    fn find_returns_none_when_absent() {
        let tmp = TempDir::new().unwrap();
        let a = ColdArchive::new(tmp.path());
        let stranger = EventId::new();
        let ts = Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap();
        assert!(a.find(stranger, ts).unwrap().is_none());
    }

    #[test]
    fn find_anywhere_walks_every_bundle() {
        let tmp = TempDir::new().unwrap();
        let a = ColdArchive::new(tmp.path());
        let target = event_at(2025, 11, 30, "target");
        let id = target.id;
        a.append(&[
            event_at(2026, 4, 1, "noise_q2"),
            target,
            event_at(2026, 1, 1, "noise_q1"),
        ])
        .unwrap();

        // We don't pass the timestamp — find_anywhere has to scan all
        // three bundles.
        let got = a.find_anywhere(id).unwrap().unwrap();
        assert_eq!(got.id, id);
    }

    #[test]
    fn read_quarter_missing_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let a = ColdArchive::new(tmp.path());
        let got = a
            .read_quarter(Quarter {
                year: 2030,
                quarter: 1,
            })
            .unwrap();
        assert!(got.is_empty());
    }

    /// Phase 4 exit-criterion smoke: read latency for a single id
    /// pull stays under the 500 ms budget at a realistic bundle size.
    /// 1_000 events / quarter × ~150 bytes JSON ≈ 150 KiB pre-zstd ≈
    /// ~30 KiB compressed. Decompression + linear scan should be
    /// well under 50 ms on any modern host; the 500 ms gate is set
    /// for the 10× larger production cardinality.
    #[test]
    fn cold_lookup_under_500ms_at_smoke_scale() {
        let tmp = TempDir::new().unwrap();
        let a = ColdArchive::new(tmp.path());
        let mut events = Vec::with_capacity(1_000);
        for day in 1..=28 {
            for i in 0..36 {
                let ts = Utc.with_ymd_and_hms(2026, 5, day, 0, 0, 0).unwrap()
                    + chrono::Duration::seconds(i);
                events.push(EpisodicEvent {
                    id: EventId::new(),
                    kind: "tool_call".into(),
                    scope: "personal".into(),
                    payload: "\"a moderately wordy payload to make compression do work\"".into(),
                    retrieval_weight: DEFAULT_RETRIEVAL_WEIGHT,
                    last_accessed: ts,
                    created_at: ts,
                });
            }
        }
        let target_id = events[500].id;
        let target_ts = events[500].created_at;
        a.append(&events).unwrap();

        let start = Instant::now();
        let got = a.find(target_id, target_ts).unwrap().unwrap();
        let elapsed = start.elapsed();
        assert_eq!(got.id, target_id);
        assert!(
            elapsed.as_millis() < 500,
            "cold find took {elapsed:?}, exceeds 500ms budget"
        );
    }
}
