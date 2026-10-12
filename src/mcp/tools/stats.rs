//! `stats` — Phase 6. Tool counterpart to the `mneme://stats`
//! resource. The resource is fine for "agent reads on every turn"
//! patterns; the tool is for explicit `tools/call` paths where the
//! agent wants the same data via a function-call surface.
//!
//! Output JSON mirrors `mneme://stats` exactly.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::{Tool, ToolAnnotations, ToolDescriptor, ToolError, ToolResult};
use crate::memory::checkpoint_scheduler::CheckpointScheduler;
use crate::memory::consolidation_scheduler::ConsolidationScheduler;
use crate::memory::episodic::{EpisodicStore, HotAgeSummary};
use crate::memory::procedural::ProceduralStore;
use crate::memory::semantic::SemanticStore;
use crate::scope::ScopeState;
use crate::storage::archive::ColdArchive;

const DESCRIPTION: &str = "Report memory store health: per-layer \
counts, server version, schema version, HNSW snapshot LSN, and \
consolidation status (run counters, tier thresholds, age of the oldest \
hot event, time to the next promotion). Use this when diagnosing issues \
or confirming the memory subsystem is alive. Output JSON matches the \
`mneme://stats` resource.";

/// Round to two decimals; ages in days need no more precision and
/// the shorter number is easier for an agent to read.
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Build the `consolidation` object shared by this tool and the
/// `mneme://stats` resource, so the two cannot drift.
///
/// Besides the scheduler's run counters it answers "why is nothing
/// promoted?": the configured thresholds, the age of the oldest hot
/// event (on `last_accessed`, the field consolidation ages on), how
/// many hot events are already eligible, and how long until the next
/// one becomes so. With default thresholds nothing is promoted in the
/// first 28 days, which looks like a broken scheduler without these.
pub(crate) fn consolidation_json(
    sched: &ConsolidationScheduler,
    hot: &HotAgeSummary,
    now: DateTime<Utc>,
) -> Value {
    let m = sched.metrics();
    let p = sched.params();
    let oldest_age_days = hot
        .oldest_last_accessed
        .map(|t| round2((now - t).num_milliseconds().max(0) as f64 / 86_400_000.0));
    let next_promotion_in_days = oldest_age_days.map(|age| {
        if hot.eligible > 0 {
            0.0
        } else {
            round2((f64::from(p.hot_to_warm_days) - age).max(0.0))
        }
    });
    json!({
        "last_consolidation_at": m.last_consolidation_at.map(|d| d.to_rfc3339()),
        "runs_total": m.runs_total,
        "errors_total": m.errors_total,
        "last_promoted_to_warm": m.last_promoted_to_warm,
        "last_archived_to_cold": m.last_archived_to_cold,
        "last_orphans_reclaimed": m.last_orphans_reclaimed,
        "thresholds": {
            "hot_to_warm_days": p.hot_to_warm_days,
            "warm_to_cold_days": p.warm_to_cold_days,
        },
        "hot_oldest_age_days": oldest_age_days,
        "hot_eligible_for_warm": hot.eligible,
        "next_promotion_in_days": next_promotion_in_days,
    })
}

/// One scan of the hot tier for both the `hot` count and the
/// consolidation age fields. The eligibility cutoff comes from the
/// scheduler's threshold when one is attached; without a scheduler
/// the consolidation block is not emitted and the cutoff is moot.
pub(crate) async fn hot_summary(
    episodic: &EpisodicStore,
    sched: Option<&ConsolidationScheduler>,
    now: DateTime<Utc>,
) -> crate::Result<HotAgeSummary> {
    let cutoff = match sched {
        Some(s) => now - chrono::Duration::days(i64::from(s.params().hot_to_warm_days)),
        None => now,
    };
    episodic.hot_age_summary(cutoff).await
}

pub struct Stats {
    semantic: Arc<SemanticStore>,
    procedural: Arc<ProceduralStore>,
    episodic: Arc<EpisodicStore>,
    cold: ColdArchive,
    schema_version: u32,
    consolidation: Option<Arc<ConsolidationScheduler>>,
    checkpoints: Option<Arc<CheckpointScheduler>>,
    scope_state: Option<Arc<ScopeState>>,
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
            checkpoints: None,
            scope_state: None,
        }
    }

    /// Attach the L3 consolidation scheduler so its observability
    /// counters surface on the tool output. Builder-style so the
    /// existing constructor stays a drop-in for tests.
    pub fn with_consolidation(mut self, sched: Arc<ConsolidationScheduler>) -> Self {
        self.consolidation = Some(sched);
        self
    }

    /// Attach the L1 checkpoint scheduler so the active session's
    /// counters land in the `working` block.
    pub fn with_checkpoints(mut self, sched: Arc<CheckpointScheduler>) -> Self {
        self.checkpoints = Some(sched);
        self
    }

    /// Attach the scope-state cell so the active default scope
    /// surfaces on the `working` block as `current_scope`.
    pub fn with_scope_state(mut self, state: Arc<ScopeState>) -> Self {
        self.scope_state = Some(state);
        self
    }
}

#[async_trait]
impl Tool for Stats {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "stats",
            title: "Memory statistics",
            annotations: ToolAnnotations::read_only(),
            description: DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    async fn invoke(&self, _args: Value) -> Result<ToolResult, ToolError> {
        let semantic_count = self.semantic.len();
        let procedural_count = self
            .procedural
            .list(None)
            .map_err(|e| ToolError::Internal(format!("procedural list: {e}")))?
            .len();
        let now = Utc::now();
        let hot = hot_summary(&self.episodic, self.consolidation.as_deref(), now)
            .await
            .map_err(|e| ToolError::Internal(format!("episodic hot_age_summary: {e}")))?;
        let hot_count = hot.count;
        let warm_count = self
            .episodic
            .count_warm()
            .await
            .map_err(|e| ToolError::Internal(format!("episodic count_warm: {e}")))?;
        let cold_quarters = self
            .cold
            .list_quarters()
            .map_err(|e| ToolError::Internal(format!("cold list_quarters: {e}")))?
            .len();

        let consolidation = self
            .consolidation
            .as_ref()
            .map(|s| consolidation_json(s, &hot, now));

        let working = self.checkpoints.as_ref().map(|s| {
            let m = s.metrics();
            json!({
                "session_id": m.session_id.to_string(),
                "started_at": m.started_at.to_rfc3339(),
                "last_checkpoint_at": m.last_checkpoint_at
                    .map(|d| d.to_rfc3339()),
                "turns_total": m.turns_total,
                "checkpoints_total": m.checkpoints_total,
                "errors_total": m.errors_total,
                "current_scope": self.scope_state.as_ref().map(|s| s.current()),
            })
        });

        let body = json!({
            "version": env!("CARGO_PKG_VERSION"),
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
            "working": working,
        });
        let text = serde_json::to_string(&body)
            .map_err(|e| ToolError::Internal(format!("serialise stats: {e}")))?;
        Ok(ToolResult::text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::Embedder;
    use crate::embed::stub::StubEmbedder;
    use crate::mcp::tools::ContentBlock;
    use crate::memory::consolidation::ConsolidationParams;
    use crate::memory::consolidation_scheduler::{ConsolidationScheduler, SchedulerConfig};
    use crate::memory::semantic::MemoryKind;
    use crate::storage::Storage;
    use crate::storage::memory_impl::MemoryStorage;
    use tempfile::TempDir;

    fn text(res: ToolResult) -> String {
        match &res.content[0] {
            ContentBlock::Text(t) => t.clone(),
        }
    }

    #[tokio::test]
    async fn no_schedulers_attached_returns_null_blocks() {
        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&backing), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(backing));
        let cold = ColdArchive::new(tmp.path());
        let stats = Stats::new(semantic, procedural, episodic, cold, 1);
        let res = stats.invoke(json!({})).await.unwrap();
        let v: Value = serde_json::from_str(&text(res)).unwrap();
        assert!(v["consolidation"].is_null());
        assert!(v["working"].is_null());
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
        let stats = Stats::new(semantic, procedural, episodic, cold, 1);
        let res = stats.invoke(json!({})).await.unwrap();
        let v: Value = serde_json::from_str(&text(res)).unwrap();
        assert_eq!(v["memories"]["semantic"], 0);
        assert_eq!(v["memories"]["procedural"], 0);
        assert_eq!(v["memories"]["episodic"]["hot"], 0);
        assert_eq!(v["memories"]["episodic"]["warm"], 0);
    }

    #[tokio::test]
    async fn with_semantic_items_reports_count() {
        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&backing), embedder).unwrap();
        semantic
            .remember("a", MemoryKind::Fact, vec![], "s".into())
            .await
            .unwrap();
        semantic
            .remember("b", MemoryKind::Fact, vec![], "s".into())
            .await
            .unwrap();
        semantic
            .remember("c", MemoryKind::Fact, vec![], "s".into())
            .await
            .unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(backing));
        let cold = ColdArchive::new(tmp.path());
        let stats = Stats::new(semantic, procedural, episodic, cold, 1);
        let res = stats.invoke(json!({})).await.unwrap();
        let v: Value = serde_json::from_str(&text(res)).unwrap();
        assert_eq!(v["memories"]["semantic"], 3);
    }

    #[tokio::test]
    async fn consolidation_scheduler_metrics_surface() {
        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&backing), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&backing)));
        let cold = ColdArchive::new(tmp.path());
        let sched = ConsolidationScheduler::start(
            backing,
            ColdArchive::new(tmp.path()),
            ConsolidationParams {
                hot_to_warm_days: 28,
                warm_to_cold_days: 180,
            },
            SchedulerConfig::disabled(),
            vec![],
        );
        let stats = Stats::new(semantic, procedural, episodic, cold, 1).with_consolidation(sched);
        let res = stats.invoke(json!({})).await.unwrap();
        let v: Value = serde_json::from_str(&text(res)).unwrap();
        assert!(v["consolidation"].is_object());
        assert!(v["consolidation"]["last_consolidation_at"].is_null());
        assert_eq!(v["consolidation"]["runs_total"], 0);
        assert_eq!(v["consolidation"]["errors_total"], 0);
    }

    #[tokio::test]
    async fn builder_pattern_chains_all_options() {
        use crate::memory::checkpoint_scheduler::{CheckpointScheduler, CheckpointSchedulerConfig};
        use crate::memory::working::ActiveSession;

        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&backing), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&backing)));
        let cold = ColdArchive::new(tmp.path());
        let cons = ConsolidationScheduler::start(
            backing,
            ColdArchive::new(tmp.path()),
            ConsolidationParams {
                hot_to_warm_days: 28,
                warm_to_cold_days: 180,
            },
            SchedulerConfig::disabled(),
            vec![],
        );
        let active = ActiveSession::open(tmp.path().to_path_buf()).unwrap();
        let cp = CheckpointScheduler::start(active, CheckpointSchedulerConfig::disabled());
        let scope = ScopeState::new("test-scope");
        let stats = Stats::new(semantic, procedural, episodic, cold, 1)
            .with_consolidation(cons)
            .with_checkpoints(cp)
            .with_scope_state(scope);
        let res = stats.invoke(json!({})).await.unwrap();
        let v: Value = serde_json::from_str(&text(res)).unwrap();
        assert!(v["consolidation"].is_object());
        assert!(v["working"].is_object());
        assert_eq!(v["working"]["current_scope"], "test-scope");
    }

    fn disabled_scheduler(
        backing: &Arc<dyn Storage>,
        dir: &std::path::Path,
    ) -> Arc<ConsolidationScheduler> {
        ConsolidationScheduler::start(
            Arc::clone(backing),
            ColdArchive::new(dir),
            ConsolidationParams {
                hot_to_warm_days: 28,
                warm_to_cold_days: 180,
            },
            SchedulerConfig::disabled(),
            vec![],
        )
    }

    async fn stats_with_scheduler(backing: Arc<dyn Storage>, tmp: &TempDir) -> Stats {
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&backing), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&backing)));
        let sched = disabled_scheduler(&backing, tmp.path());
        Stats::new(
            semantic,
            procedural,
            episodic,
            ColdArchive::new(tmp.path()),
            1,
        )
        .with_consolidation(sched)
    }

    #[tokio::test]
    async fn version_is_reported() {
        let tmp = TempDir::new().unwrap();
        let stats = stats_with_scheduler(MemoryStorage::new(), &tmp).await;
        let v: Value = serde_json::from_str(&text(stats.invoke(json!({})).await.unwrap())).unwrap();
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn consolidation_reports_thresholds_and_hot_ages() {
        use crate::memory::consolidation::_record_backdated_for_tests;
        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let now = Utc::now();
        // 30 days old: past the 28-day threshold. 10 days old: not yet.
        _record_backdated_for_tests(
            &backing,
            "k",
            "p",
            "\"a\"",
            now - chrono::Duration::days(30),
        )
        .await
        .unwrap();
        _record_backdated_for_tests(
            &backing,
            "k",
            "p",
            "\"b\"",
            now - chrono::Duration::days(10),
        )
        .await
        .unwrap();
        let stats = stats_with_scheduler(backing, &tmp).await;
        let v: Value = serde_json::from_str(&text(stats.invoke(json!({})).await.unwrap())).unwrap();
        let c = &v["consolidation"];
        assert_eq!(c["thresholds"]["hot_to_warm_days"], 28);
        assert_eq!(c["thresholds"]["warm_to_cold_days"], 180);
        assert_eq!(c["hot_eligible_for_warm"], 1);
        let age = c["hot_oldest_age_days"].as_f64().unwrap();
        assert!((age - 30.0).abs() < 0.1, "age {age}");
        assert_eq!(c["next_promotion_in_days"].as_f64().unwrap(), 0.0);
        assert_eq!(v["memories"]["episodic"]["hot"], 2);
    }

    #[tokio::test]
    async fn next_promotion_counts_down_when_nothing_is_eligible() {
        use crate::memory::consolidation::_record_backdated_for_tests;
        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        _record_backdated_for_tests(
            &backing,
            "k",
            "p",
            "\"a\"",
            Utc::now() - chrono::Duration::days(3),
        )
        .await
        .unwrap();
        let stats = stats_with_scheduler(backing, &tmp).await;
        let v: Value = serde_json::from_str(&text(stats.invoke(json!({})).await.unwrap())).unwrap();
        let c = &v["consolidation"];
        assert_eq!(c["hot_eligible_for_warm"], 0);
        let age = c["hot_oldest_age_days"].as_f64().unwrap();
        assert!((age - 3.0).abs() < 0.1, "age {age}");
        let next = c["next_promotion_in_days"].as_f64().unwrap();
        assert!((next - 25.0).abs() < 0.1, "next {next}");
    }

    #[tokio::test]
    async fn empty_hot_tier_reports_null_ages() {
        let tmp = TempDir::new().unwrap();
        let stats = stats_with_scheduler(MemoryStorage::new(), &tmp).await;
        let v: Value = serde_json::from_str(&text(stats.invoke(json!({})).await.unwrap())).unwrap();
        let c = &v["consolidation"];
        assert!(c["hot_oldest_age_days"].is_null());
        assert!(c["next_promotion_in_days"].is_null());
        assert_eq!(c["hot_eligible_for_warm"], 0);
        assert_eq!(c["thresholds"]["hot_to_warm_days"], 28);
    }
}
