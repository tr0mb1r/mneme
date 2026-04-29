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
use crate::memory::consolidation_scheduler::ConsolidationScheduler;
use crate::memory::episodic::EpisodicStore;
use crate::memory::procedural::ProceduralStore;
use crate::memory::semantic::SemanticStore;
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
    /// counters surface on the tool output. Builder-style so the
    /// existing constructor stays a drop-in for tests.
    pub fn with_consolidation(mut self, sched: Arc<ConsolidationScheduler>) -> Self {
        self.consolidation = Some(sched);
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
            .map_err(|e| ToolError::Internal(format!("serialise stats: {e}")))?;
        Ok(ToolResult::text(text))
    }
}
