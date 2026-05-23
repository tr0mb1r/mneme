//! `stats` — Phase 6. Tool counterpart to the `mneme://stats`
//! resource. The resource is fine for "agent reads on every turn"
//! patterns; the tool is for explicit `tools/call` paths where the
//! agent wants the same data via a function-call surface.
//!
//! Output JSON mirrors `mneme://stats` exactly.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolDescriptor, ToolError, ToolResult};
use crate::memory::checkpoint_scheduler::CheckpointScheduler;
use crate::memory::consolidation_scheduler::ConsolidationScheduler;
use crate::memory::episodic::EpisodicStore;
use crate::memory::procedural::ProceduralStore;
use crate::memory::semantic::SemanticStore;
use crate::scope::ScopeState;
use crate::storage::archive::ColdArchive;

const DESCRIPTION: &str = "Report memory store health: per-layer \
counts, schema version, HNSW snapshot LSN. Use this when diagnosing \
issues or confirming the memory subsystem is alive. Output JSON \
matches the `mneme://stats` resource.";

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
        let hot_count = self
            .episodic
            .count_hot()
            .await
            .map_err(|e| ToolError::Internal(format!("episodic count_hot: {e}")))?;
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
}
