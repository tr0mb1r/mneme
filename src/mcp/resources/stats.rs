//! `mneme://stats` — Phase 6. Reports counts across every memory
//! layer plus storage-state diagnostics (schema version, HNSW
//! applied_lsn, cold-tier quarter count).
//!
//! Hosts hit this on startup or troubleshooting to confirm the
//! agent's memory is alive and roughly the size they expect.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use super::{Resource, ResourceContent, ResourceDescriptor, ResourceError};
use crate::memory::consolidation_scheduler::ConsolidationScheduler;
use crate::memory::episodic::EpisodicStore;
use crate::memory::procedural::ProceduralStore;
use crate::memory::semantic::SemanticStore;
use crate::storage::archive::ColdArchive;

pub struct Stats {
    semantic: Arc<SemanticStore>,
    procedural: Arc<ProceduralStore>,
    episodic: Arc<EpisodicStore>,
    cold: ColdArchive,
    /// Captured once at startup. The schema-version sentinel doesn't
    /// change at runtime; we read it in `cli::run` and hand it in
    /// rather than re-reading from disk on every resource fetch.
    schema_version: u32,
    /// `Some(_)` once the L3 scheduler is wired (production); `None`
    /// in fixtures that don't spawn one. The resource emits the
    /// scheduler block iff this is populated.
    consolidation: Option<Arc<ConsolidationScheduler>>,
}

impl Stats {
    pub fn new(
        semantic: Arc<SemanticStore>,
        procedural: Arc<ProceduralStore>,
        episodic: Arc<EpisodicStore>,
        cold: ColdArchive,
        schema_version: u32,
    ) -> Self {
        Self {
            semantic,
            procedural,
            episodic,
            cold,
            schema_version,
            consolidation: None,
        }
    }

    /// Attach the L3 consolidation scheduler so its observability
    /// counters surface on `mneme://stats`. Builder-style so existing
    /// constructors (and tests) don't need to change.
    pub fn with_consolidation(mut self, sched: Arc<ConsolidationScheduler>) -> Self {
        self.consolidation = Some(sched);
        self
    }
}

#[async_trait]
impl Resource for Stats {
    fn descriptor(&self) -> ResourceDescriptor {
        ResourceDescriptor {
            uri: "mneme://stats",
            name: "stats",
            description: "Memory health metrics: per-layer counts, schema version, snapshot LSN.",
            mime_type: "application/json",
        }
    }

    async fn read(&self) -> Result<ResourceContent, ResourceError> {
        let semantic_count = self.semantic.len();
        let procedural_count = self
            .procedural
            .list(None)
            .map_err(|e| ResourceError::Internal(format!("procedural list: {e}")))?
            .len();
        let hot_count = self
            .episodic
            .count_hot()
            .await
            .map_err(|e| ResourceError::Internal(format!("episodic count_hot: {e}")))?;
        let warm_count = self
            .episodic
            .count_warm()
            .await
            .map_err(|e| ResourceError::Internal(format!("episodic count_warm: {e}")))?;
        let cold_quarters = self
            .cold
            .list_quarters()
            .map_err(|e| ResourceError::Internal(format!("cold list_quarters: {e}")))?
            .len();

        let consolidation = self.consolidation.as_ref().map(|s| {
            let m = s.metrics();
            json!({
                "last_consolidation_at": m.last_consolidation_at
                    .map(|d| d.to_rfc3339()),
                "runs_total": m.runs_total,
                "errors_total": m.errors_total,
                "last_promoted_to_warm": m.last_promoted_to_warm,
                "last_archived_to_cold": m.last_archived_to_cold,
            })
        });

        let body = json!({
            "schema_version": self.schema_version,
            "memories": {
                "semantic": semantic_count,
                "procedural": procedural_count,
                "episodic": {
                    "hot": hot_count,
                    "warm": warm_count,
                    "cold_quarters": cold_quarters,
                },
                "total_redb": semantic_count + hot_count + warm_count,
            },
            "semantic_index": {
                "applied_lsn": self.semantic.applied_lsn(),
                "embed_dim": self.semantic.dim(),
            },
            "consolidation": consolidation,
        });
        let text = serde_json::to_string(&body)
            .map_err(|e| ResourceError::Internal(format!("serialise stats: {e}")))?;
        Ok(ResourceContent {
            uri: self.descriptor().uri.into(),
            mime_type: "application/json",
            text,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::Embedder;
    use crate::embed::stub::StubEmbedder;
    use crate::memory::semantic::MemoryKind;
    use crate::storage::Storage;
    use crate::storage::memory_impl::MemoryStorage;
    use tempfile::TempDir;

    async fn fixture() -> (Stats, TempDir) {
        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&backing), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&backing)));

        // Pre-populate so the test asserts non-zero counts.
        semantic
            .remember("a", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        semantic
            .remember("b", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        procedural
            .pin("rule".into(), vec![], "personal".into())
            .await
            .unwrap();
        episodic.record("k", "personal", "\"x\"").await.unwrap();

        let cold = ColdArchive::new(tmp.path());
        (Stats::new(semantic, procedural, episodic, cold, 1), tmp)
    }

    #[tokio::test]
    async fn read_returns_real_counts() {
        let (s, _tmp) = fixture().await;
        let c = s.read().await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&c.text).unwrap();
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["memories"]["semantic"], 2);
        assert_eq!(v["memories"]["procedural"], 1);
        assert_eq!(v["memories"]["episodic"]["hot"], 1);
        assert_eq!(v["memories"]["episodic"]["warm"], 0);
        assert_eq!(v["memories"]["episodic"]["cold_quarters"], 0);
        assert_eq!(v["memories"]["total_redb"], 3);
        assert_eq!(v["semantic_index"]["embed_dim"], 4);
    }

    #[tokio::test]
    async fn empty_stores_report_zero() {
        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&backing), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(backing));
        let cold = ColdArchive::new(tmp.path());

        let s = Stats::new(semantic, procedural, episodic, cold, 1);
        let c = s.read().await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&c.text).unwrap();
        assert_eq!(v["memories"]["semantic"], 0);
        assert_eq!(v["memories"]["procedural"], 0);
        assert_eq!(v["memories"]["episodic"]["hot"], 0);
    }

    /// Without a scheduler attached, the resource emits
    /// `consolidation: null` so clients can disambiguate "not wired"
    /// from "wired but never run".
    #[tokio::test]
    async fn consolidation_field_is_null_without_scheduler() {
        let (s, _tmp) = fixture().await;
        let c = s.read().await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&c.text).unwrap();
        assert!(v.get("consolidation").is_some());
        assert!(v["consolidation"].is_null());
    }

    /// With a scheduler attached but no pass yet, the resource emits
    /// the metrics block with `last_consolidation_at = null` and
    /// zero counters — proves the wiring without forcing a timed
    /// pass.
    #[tokio::test]
    async fn consolidation_block_surfaces_when_scheduler_attached() {
        use crate::memory::consolidation::ConsolidationParams;
        use crate::memory::consolidation_scheduler::{
            ConsolidationScheduler, SchedulerConfig,
        };

        let (s, _tmp) = fixture().await;
        // Build a disabled scheduler — no timing flakiness.
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let cold = ColdArchive::new(_tmp.path());
        let sched = ConsolidationScheduler::start(
            backing,
            cold,
            ConsolidationParams {
                hot_to_warm_days: 28,
                warm_to_cold_days: 180,
            },
            SchedulerConfig::disabled(),
            vec![],
        );
        let s = s.with_consolidation(Arc::clone(&sched));

        let c = s.read().await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&c.text).unwrap();
        let cons = &v["consolidation"];
        assert!(cons.is_object(), "consolidation should be an object: {cons}");
        assert!(cons["last_consolidation_at"].is_null());
        assert_eq!(cons["runs_total"], 0);
        assert_eq!(cons["errors_total"], 0);
        assert_eq!(cons["last_promoted_to_warm"], 0);
        assert_eq!(cons["last_archived_to_cold"], 0);

        sched.shutdown().await;
    }
}
