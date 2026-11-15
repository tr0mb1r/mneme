//! `export` — Phase 6. Emit every memory across all three layers as
//! a single structured JSON document. Designed for one-shot
//! portability: the JSON the agent receives can be diffed across
//! versions, fed to a script for migration, or piped to `jq`.
//!
//! For at-rest backups use the `mneme backup` CLI command instead —
//! it captures the on-disk state byte-for-byte (incl. cold-tier
//! zstd archives, the WAL, the snapshot). `export` is for
//! human-readable, host-LLM-readable shape.
//!
//! The `episodic` array covers the hot and warm tiers (each row has a
//! `tier` field). Until 1.5.2 it was hot only, so an export taken as
//! a backup lost every event older than `hot_to_warm_days`. The cold
//! archive is still not included.
//!
//! Each `semantic` row carries the metadata that lives beside the
//! memory rather than in it: `updated_at`, `status`, and (when set)
//! `superseded_by`. Archived and superseded memories are always in an
//! export; until 1.6 the supersession links were silently left out.
//! A row also carries `provenance` / `source` and a
//! `relations` array (the relations the memory carries, each with its
//! type, target, reason and creation time) when it has them.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolAnnotations, ToolDescriptor, ToolError, ToolResult};
use crate::memory::episodic::{EpisodicStore, newest_first, with_cold_rows};
use crate::memory::procedural::ProceduralStore;
use crate::memory::semantic::{MemoryItem, read_memory_meta};
use crate::storage::MEM_KEY_PREFIX;
use crate::storage::Storage;
use crate::storage::archive::ColdArchive;

const DESCRIPTION: &str = "Export every memory the agent currently \
holds as one JSON document, optionally filtered by scope. Use this \
for human-readable inspection or for piping to `jq` / external \
scripts. Episodic events come from the hot and warm tiers (each row \
has a `tier` field); pass `include_warm: false` for hot only. Events \
in the cold archive are left out unless `include_cold: true`, which \
decompresses the archive and is slow. Memories include archived and \
superseded ones, marked by `status` and `superseded_by`. Each layer is capped at \
`limit_per_layer` rows, newest first, so raise it (max 5000) for a \
complete dump. For at-rest, full-fidelity copies use the \
`mneme backup` CLI command instead.";

/// Default cap on rows per layer when the caller doesn't pass
/// `limit`. Picked low enough that an LLM context fits the response.
const DEFAULT_LIMIT_PER_LAYER: usize = 200;

pub struct Export {
    procedural: Arc<ProceduralStore>,
    episodic: Arc<EpisodicStore>,
    storage: Arc<dyn Storage>,
    /// Cold archive for `include_cold`. `None` in fixtures built
    /// without one; `include_cold: true` is then an error.
    cold: Option<ColdArchive>,
}

impl Export {
    pub fn new(
        procedural: Arc<ProceduralStore>,
        episodic: Arc<EpisodicStore>,
        storage: Arc<dyn Storage>,
    ) -> Self {
        Self {
            procedural,
            episodic,
            storage,
            cold: None,
        }
    }

    /// Attach the cold archive so `include_cold` can read it.
    pub fn with_cold(mut self, cold: ColdArchive) -> Self {
        self.cold = Some(cold);
        self
    }
}

#[async_trait]
impl Tool for Export {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "export",
            title: "Export all memories",
            annotations: ToolAnnotations::read_only(),
            description: DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "scope": { "type": "string", "description": "Optional scope filter." },
                    "limit_per_layer": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 5000,
                        "description": "Max rows to include per layer. Defaults to 200."
                    },
                    "include_warm": {
                        "type": "boolean",
                        "description": "Include warm-tier episodic events (older than the \
            hot-to-warm threshold). Defaults to true."
                    },
                    "include_cold": {
                        "type": "boolean",
                        "description": "Also include episodic events from the cold archive \
            (past the warm-to-cold threshold). Defaults to false; slow, and counts \
            toward `limit_per_layer`."
                    }
                }
            }),
        }
    }

    async fn invoke(&self, args: Value) -> Result<ToolResult, ToolError> {
        let scope_filter = args.get("scope").and_then(Value::as_str).map(String::from);
        let limit = match args.get("limit_per_layer") {
            None => DEFAULT_LIMIT_PER_LAYER,
            Some(Value::Number(n)) => n.as_u64().ok_or_else(|| {
                ToolError::InvalidArguments("`limit_per_layer` must be a positive integer".into())
            })? as usize,
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`limit_per_layer` must be a positive integer".into(),
                ));
            }
        };
        if !(1..=5000).contains(&limit) {
            return Err(ToolError::InvalidArguments(
                "`limit_per_layer` must be between 1 and 5000".into(),
            ));
        }

        let include_warm = match args.get("include_warm") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`include_warm` must be a boolean".into(),
                ));
            }
        };

        let include_cold = match args.get("include_cold") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`include_cold` must be a boolean".into(),
                ));
            }
        };

        // L0 procedural.
        let proc_items = self
            .procedural
            .list(scope_filter.as_deref())
            .map_err(|e| ToolError::Internal(format!("procedural list: {e}")))?;
        let proc_json: Vec<Value> = proc_items
            .into_iter()
            .take(limit)
            .map(|p| {
                json!({
                    "id": p.id.to_string(),
                    "content": p.content,
                    "tags": p.tags,
                    "scope": p.scope,
                    "created_at": p.created_at.to_rfc3339(),
                })
            })
            .collect();

        // L3 episodic — hot and warm, newest first.
        let mut all_episodic = self
            .episodic
            .list_hot_and_warm(include_warm)
            .await
            .map_err(|e| ToolError::Internal(format!("episodic list: {e}")))?;
        if let Some(ref s) = scope_filter {
            all_episodic.retain(|t| &t.event.scope == s);
        }
        if include_cold {
            let cold = self.cold.clone().ok_or_else(|| {
                ToolError::Internal("`include_cold`: no cold archive attached".into())
            })?;
            let scope = scope_filter.clone();
            // Bundle reads are blocking file I/O plus zstd. The newest
            // `limit` cold rows are enough: older ones cannot make the
            // cut after the merge.
            let cold_rows = tokio::task::spawn_blocking(move || {
                cold.read_window(None, None, limit, |e| {
                    scope.as_ref().is_none_or(|s| &e.scope == s)
                })
            })
            .await
            .map_err(|e| ToolError::Internal(format!("cold read task: {e}")))?
            .map_err(|e| ToolError::Internal(format!("cold read failed: {e}")))?;
            all_episodic = with_cold_rows(all_episodic, cold_rows);
            all_episodic.sort_by(|a, b| newest_first(&a.event, &b.event));
        }
        let epi_json: Vec<Value> = all_episodic
            .into_iter()
            .take(limit)
            .map(|t| {
                let e = t.event;
                json!({
                    "tier": t.tier.as_str(),
                    "id": e.id.to_string(),
                    "kind": e.kind,
                    "scope": e.scope,
                    "payload": e.payload,
                    "tags": e.tags,
                    "retrieval_weight": e.retrieval_weight,
                    "last_accessed": e.last_accessed.to_rfc3339(),
                    "created_at": e.created_at.to_rfc3339(),
                })
            })
            .collect();

        // L4 semantic — scan b"mem:" and decode. Apply scope filter
        // BEFORE the limit so a scope-filtered export isn't biased
        // towards whatever happens to come first off the prefix scan.
        let raw = self
            .storage
            .scan_prefix(MEM_KEY_PREFIX)
            .await
            .map_err(|e| ToolError::Internal(format!("semantic scan: {e}")))?;
        let mut sem_items: Vec<MemoryItem> = Vec::new();
        for (_k, v) in raw {
            let item: MemoryItem = postcard::from_bytes(&v)
                .map_err(|e| ToolError::Internal(format!("decode MemoryItem: {e}")))?;
            if let Some(ref s) = scope_filter
                && &item.scope != s
            {
                continue;
            }
            sem_items.push(item);
        }
        sem_items.sort_by_key(|m| std::cmp::Reverse(m.created_at));
        // Side-table metadata is read only for the rows that survive
        // the limit: three point reads each.
        let mut sem_json: Vec<Value> = Vec::new();
        for m in sem_items.into_iter().take(limit) {
            let meta = read_memory_meta(self.storage.as_ref(), &m)
                .await
                .map_err(|e| ToolError::Internal(format!("semantic metadata: {e}")))?;
            let mut row = json!({
                "id": m.id.to_string(),
                "content": m.content,
                "kind": m.kind.as_str(),
                "tags": m.tags,
                "scope": m.scope,
                "created_at": m.created_at.to_rfc3339(),
                "updated_at": meta.updated_at.to_rfc3339(),
                "status": meta.status.as_str(),
            });
            if let Some(newer) = meta.superseded_by {
                row["superseded_by"] = json!(newer.to_string());
            }
            if let Some(record) = &meta.provenance {
                row["provenance"] = json!(record.provenance.as_str());
                if let Some(source) = &record.source {
                    row["source"] = json!(source);
                }
            }
            if !meta.relations.is_empty() {
                row["relations"] = super::links::export_json(&meta.relations);
            }
            sem_json.push(row);
        }

        let body = json!({
            "procedural": proc_json,
            "episodic": epi_json,
            "semantic": sem_json,
        });
        let text = serde_json::to_string(&body)
            .map_err(|e| ToolError::Internal(format!("serialise export: {e}")))?;
        Ok(ToolResult::text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::Embedder;
    use crate::embed::stub::StubEmbedder;
    use crate::memory::semantic::{MemoryKind, SemanticStore};
    use crate::storage::memory_impl::MemoryStorage;
    use tempfile::TempDir;

    async fn fixture() -> (Export, Arc<SemanticStore>, TempDir) {
        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&backing), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&backing)));

        // Populate.
        semantic
            .remember(
                "hello",
                MemoryKind::Fact,
                vec!["t".into()],
                "personal".into(),
            )
            .await
            .unwrap();
        procedural
            .pin("rule".into(), vec![], "personal".into())
            .await
            .unwrap();
        episodic.record("k", "personal", "\"x\"").await.unwrap();

        (Export::new(procedural, episodic, backing), semantic, tmp)
    }

    #[tokio::test]
    async fn export_returns_each_layer() {
        let (e, _semantic, _tmp) = fixture().await;
        let res = e.invoke(json!({})).await.unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["procedural"].as_array().unwrap().len(), 1);
        assert_eq!(v["episodic"].as_array().unwrap().len(), 1);
        assert_eq!(v["semantic"].as_array().unwrap().len(), 1);
        assert_eq!(v["semantic"][0]["kind"], "fact");
    }

    #[tokio::test]
    async fn export_filters_by_scope() {
        let (e, semantic, _tmp) = fixture().await;
        // Add a "work" memory; export with scope="work" should
        // surface only that one.
        semantic
            .remember("ship", MemoryKind::Decision, vec![], "work".into())
            .await
            .unwrap();
        let res = e.invoke(json!({ "scope": "work" })).await.unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["semantic"].as_array().unwrap().len(), 1);
        assert_eq!(v["semantic"][0]["scope"], "work");
        assert_eq!(v["procedural"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn export_limit_enforced() {
        let (e, semantic, _tmp) = fixture().await;
        for i in 0..5 {
            semantic
                .remember(
                    &format!("m{i}"),
                    MemoryKind::Fact,
                    vec![],
                    "personal".into(),
                )
                .await
                .unwrap();
        }
        let res = e.invoke(json!({ "limit_per_layer": 2 })).await.unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        assert!(v["semantic"].as_array().unwrap().len() <= 2);
    }

    /// Fixture plus three events promoted to warm. The fixture
    /// already holds one hot event, so episodic is 1 hot + 3 warm.
    async fn fixture_with_warm() -> (Export, Arc<SemanticStore>, TempDir) {
        use crate::memory::consolidation::_record_backdated_for_tests;
        let (e, semantic, tmp) = fixture().await;
        let now = chrono::Utc::now();
        for d in [40, 50, 60] {
            let id = _record_backdated_for_tests(
                &e.storage,
                &format!("warm-{d}d"),
                "personal",
                "\"w\"",
                now - chrono::Duration::days(d),
            )
            .await
            .unwrap();
            assert!(e.episodic.promote_to_warm(id).await.unwrap());
        }
        (e, semantic, tmp)
    }

    async fn episodic_rows(e: &Export, args: Value) -> Vec<Value> {
        let res = e.invoke(args).await.unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        v["episodic"].as_array().unwrap().clone()
    }

    /// The 1.5.2 bug: export called `list_all()` (hot only), so a
    /// backup silently dropped every warm row.
    #[tokio::test]
    async fn export_includes_warm_rows_by_default() {
        let (e, _semantic, _tmp) = fixture_with_warm().await;
        let hot = e.episodic.count_hot().await.unwrap();
        let warm = e.episodic.count_warm().await.unwrap();
        assert_eq!((hot, warm), (1, 3));

        let rows = episodic_rows(&e, json!({})).await;
        assert_eq!(rows.len(), hot + warm);
        // Newest first across tiers, each row tagged.
        let got: Vec<(&str, &str)> = rows
            .iter()
            .map(|r| (r["kind"].as_str().unwrap(), r["tier"].as_str().unwrap()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("k", "hot"),
                ("warm-40d", "warm"),
                ("warm-50d", "warm"),
                ("warm-60d", "warm"),
            ]
        );
        // Stable across calls.
        assert_eq!(episodic_rows(&e, json!({})).await, rows);
    }

    #[tokio::test]
    async fn export_include_warm_false_is_hot_only() {
        let (e, _semantic, _tmp) = fixture_with_warm().await;
        let rows = episodic_rows(&e, json!({ "include_warm": false })).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["tier"], "hot");
    }

    #[tokio::test]
    async fn export_scope_filter_and_limit_apply_to_warm_rows() {
        let (e, _semantic, _tmp) = fixture_with_warm().await;
        assert!(
            episodic_rows(&e, json!({ "scope": "work" }))
                .await
                .is_empty()
        );
        let rows = episodic_rows(&e, json!({ "limit_per_layer": 2 })).await;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["kind"], "warm-40d");
    }

    /// One event in the cold archive, 300 days old, in `scope`.
    fn cold_with_one_event(tmp: &TempDir, scope: &str) -> ColdArchive {
        use crate::memory::episodic::{DEFAULT_RETRIEVAL_WEIGHT, EpisodicEvent};
        let cold = ColdArchive::new(&tmp.path().join("coldroot"));
        let at = chrono::Utc::now() - chrono::Duration::days(300);
        cold.append(&[EpisodicEvent {
            id: crate::ids::EventId::new(),
            kind: "cold-300d".into(),
            scope: scope.into(),
            payload: "\"c\"".into(),
            tags: vec![],
            retrieval_weight: DEFAULT_RETRIEVAL_WEIGHT,
            last_accessed: at,
            created_at: at,
        }])
        .unwrap();
        cold
    }

    #[tokio::test]
    async fn export_include_cold_is_opt_in() {
        let (e, _semantic, tmp) = fixture_with_warm().await;
        let e = e.with_cold(cold_with_one_event(&tmp, "personal"));

        // Default: hot + warm, as in 1.5.2.
        assert_eq!(episodic_rows(&e, json!({})).await.len(), 4);

        let rows = episodic_rows(&e, json!({ "include_cold": true })).await;
        let got: Vec<(&str, &str)> = rows
            .iter()
            .map(|r| (r["kind"].as_str().unwrap(), r["tier"].as_str().unwrap()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("k", "hot"),
                ("warm-40d", "warm"),
                ("warm-50d", "warm"),
                ("warm-60d", "warm"),
                ("cold-300d", "cold"),
            ]
        );
        // Scope filter and the per-layer cap cover cold rows.
        assert!(
            episodic_rows(&e, json!({ "include_cold": true, "scope": "work" }))
                .await
                .is_empty()
        );
        assert_eq!(
            episodic_rows(&e, json!({ "include_cold": true, "limit_per_layer": 4 }))
                .await
                .len(),
            4
        );
    }

    #[tokio::test]
    async fn export_include_cold_is_validated() {
        let (e, _, _tmp) = fixture().await;
        let err = e.invoke(json!({ "include_cold": 1 })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
        // No archive attached.
        let err = e.invoke(json!({ "include_cold": true })).await.unwrap_err();
        assert!(matches!(err, ToolError::Internal(_)));
    }

    #[tokio::test]
    async fn export_include_warm_wrong_type_rejected() {
        let (e, _, _tmp) = fixture().await;
        let err = e.invoke(json!({ "include_warm": 1 })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn export_invalid_limit_rejected() {
        let (e, _, _tmp) = fixture().await;
        let err = e.invoke(json!({ "limit_per_layer": 0 })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
        let err = e
            .invoke(json!({ "limit_per_layer": 99999 }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    /// Side-table metadata reaches the export: an edited memory's
    /// `updated_at`, an archived one's `status`, and the supersession
    /// link that exports before 1.6 dropped.
    #[tokio::test]
    async fn export_carries_updated_at_status_and_supersession() {
        use crate::memory::semantic::{MemoryStatus, UpdatePatch};
        let (e, semantic, _tmp) = fixture().await;
        let old = semantic
            .remember("port is 8080", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        let new = semantic
            .remember("port is 9090", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        assert!(semantic.supersede(old, new).await.unwrap());
        let archived = semantic
            .remember("old plan", MemoryKind::Decision, vec![], "personal".into())
            .await
            .unwrap();
        assert!(
            semantic
                .set_status(archived, MemoryStatus::Archived)
                .await
                .unwrap()
        );
        assert!(
            semantic
                .update(
                    new,
                    UpdatePatch {
                        content: Some("port is 9191".into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
        );

        let res = e.invoke(json!({})).await.unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        let rows = v["semantic"].as_array().unwrap();
        // Nothing is hidden from an export.
        assert_eq!(rows.len(), 4);
        let row = |id: crate::ids::MemoryId| {
            rows.iter()
                .find(|r| r["id"] == id.to_string())
                .unwrap()
                .clone()
        };
        assert_eq!(row(old)["superseded_by"], new.to_string());
        assert_eq!(row(old)["status"], "active");
        assert!(row(new).get("superseded_by").is_none());
        assert_eq!(row(archived)["status"], "archived");
        // Never edited: updated_at is created_at. Edited: it moved.
        assert_eq!(row(old)["updated_at"], row(old)["created_at"]);
        let parse = |v: &Value| chrono::DateTime::parse_from_rfc3339(v.as_str().unwrap()).unwrap();
        assert!(parse(&row(new)["updated_at"]) >= parse(&row(new)["created_at"]));
        assert_eq!(
            semantic
                .updated_at(new)
                .await
                .unwrap()
                .unwrap()
                .to_rfc3339(),
            row(new)["updated_at"].as_str().unwrap()
        );
    }
}
