//! `recall` — Phase 3. Returns the top-N memories most similar to a
//! natural-language query, optionally filtered by scope or type.
//!
//! Since 1.6 it can also search the keyword-indexed L3 events
//! (`layers: ["semantic", "episodic"]`) and bound either layer to a
//! time window; see [`crate::memory::unified_recall`]. The default is
//! still L4 only.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::recall_recent::parse_time_bound;
use super::{Tool, ToolAnnotations, ToolDescriptor, ToolError, ToolResult};
use crate::ids::EventId;
use crate::memory::episodic::{EpisodicStore, RECALL_TOUCH_DEBOUNCE_HOURS, Tier};
use crate::memory::semantic::{RecallFilters, RecallMode, SemanticStore};
use crate::memory::unified_recall::{self, Layers, RecallRequest, TimeField, UnifiedHit};

const DESCRIPTION: &str = "Retrieve memories relevant to a query. \
Use this when you need context the user previously shared but isn't in the \
current conversation. Searches by meaning AND by exact words at once, so \
both questions (\"what did we decide about auth?\") and exact names \
(`live_bt.py`, \"D account\", an env var, an error string) work; each row \
says which search found it in `match`. Returns an empty list when nothing \
matches — that is not an error. \
\
By default only stored memories are searched. Pass \
`layers: [\"semantic\", \"episodic\"]` to also search recorded events \
(decisions, milestones, lessons, summaries and the like from \
`record_event`) by their words; those rows have `layer: \"episodic\"`, \
`match: \"keyword\"` and no `similarity`. All rows are ordered by `rank`. \
`since` / `until` keep only rows inside a `[since, until)` time window \
(RFC3339 or a ULID; the server does not parse phrases like 'last week'). \
`scope` takes one scope or several separated by commas.";

/// Cap on the result count. Above ~100 the LLM context burn dwarfs any
/// retrieval signal. Matches spec §6.1's `recall.limit.maximum`.
const MAX_LIMIT: u64 = 100;

/// Default when the caller omits `limit`. Aligns with
/// [`crate::config::BudgetsConfig::default_recall_limit`].
const DEFAULT_LIMIT: u64 = 10;

pub struct Recall {
    store: Arc<SemanticStore>,
    /// L3, for `layers` containing `episodic`. `None` in fixtures built
    /// without it; asking for the layer is then an error.
    episodic: Option<Arc<EpisodicStore>>,
}

impl Recall {
    pub fn new(store: Arc<SemanticStore>) -> Self {
        Self {
            store,
            episodic: None,
        }
    }

    /// Attach the episodic store so `layers: [.., "episodic"]` works.
    pub fn with_episodic(mut self, episodic: Arc<EpisodicStore>) -> Self {
        self.episodic = Some(episodic);
        self
    }
}

/// Parse `scope`: one scope, or several separated by commas. Blank
/// entries are dropped; nothing left means "any scope".
fn parse_scopes(arg: Option<&Value>) -> Result<Vec<String>, ToolError> {
    match arg {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(s
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect()),
        Some(_) => Err(ToolError::InvalidArguments(
            "`scope` must be a string (one scope, or several separated by commas)".into(),
        )),
    }
}

fn parse_layers(arg: Option<&Value>) -> Result<Layers, ToolError> {
    let bad = || {
        ToolError::InvalidArguments(
            "`layers` must be a non-empty array drawn from \"semantic\" and \"episodic\"".into(),
        )
    };
    match arg {
        None | Some(Value::Null) => Ok(Layers::default()),
        Some(Value::Array(items)) => {
            let names: Vec<&str> = items
                .iter()
                .map(|v| v.as_str().ok_or_else(bad))
                .collect::<Result<_, _>>()?;
            Layers::parse(names).map_err(|_| bad())
        }
        Some(_) => Err(bad()),
    }
}

fn parse_bool(args: &Value, name: &str) -> Result<bool, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(ToolError::InvalidArguments(format!(
            "`{name}` must be a boolean"
        ))),
    }
}

fn parse_time(
    args: &Value,
    name: &str,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => parse_time_bound(name, s).map(Some),
        Some(_) => Err(ToolError::InvalidArguments(format!(
            "`{name}` must be a string"
        ))),
    }
}

/// Refresh `last_accessed` on the hot events a recall returned, so
/// events that keep being used are not promoted out of the hot tier on
/// age alone. Runs after the answer is built and never delays or fails
/// it; an event is rewritten at most once per
/// [`RECALL_TOUCH_DEBOUNCE_HOURS`].
fn touch_in_background(
    store: Arc<EpisodicStore>,
    ids: Vec<EventId>,
) -> Option<tokio::task::JoinHandle<()>> {
    if ids.is_empty() {
        return None;
    }
    Some(tokio::spawn(async move {
        let min_age = chrono::Duration::hours(RECALL_TOUCH_DEBOUNCE_HOURS);
        for id in ids {
            if let Err(e) = store.touch_if_stale(id, min_age).await {
                tracing::debug!(event = %id, error = %e, "recall: touch failed");
            }
        }
    }))
}

#[async_trait]
impl Tool for Recall {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "recall",
            title: "Recall memories",
            annotations: ToolAnnotations::read_only(),
            description: DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural-language query." },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_LIMIT,
                        "description": "Max results. Defaults to 10."
                    },
                    "scope": {
                        "type": "string",
                        "description": "Optional scope filter: one scope, or several \
            separated by commas (`\"projectx,global\"`), matching any of them."
                    },
                    "layers": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["semantic", "episodic"] },
                        "description": "Which layers to search. Defaults to \
            [\"semantic\"] (stored memories). Add \"episodic\" to also search recorded \
            events by keyword."
                    },
                    "since": {
                        "type": "string",
                        "description": "Keep rows at or after this time. RFC3339 \
            timestamp or 26-char ULID."
                    },
                    "until": {
                        "type": "string",
                        "description": "Keep rows before this time (exclusive). RFC3339 \
            timestamp or 26-char ULID."
                    },
                    "time_field": {
                        "type": "string",
                        "enum": ["created_at", "updated_at"],
                        "description": "Which timestamp `since` / `until` compare \
            against. Defaults to created_at. Events are never updated, so for them \
            both mean created_at."
                    },
                    "include_archived": {
                        "type": "boolean",
                        "description": "Also return memories whose status is archived. \
            Defaults to false."
                    },
                    "kind": {
                        "type": "string",
                        "enum": ["fact", "decision", "preference", "conversation"],
                        "description": "Optional kind filter. With the episodic layer, \
            only events of the same kind name pass (e.g. `decision`)."
                    },
                    "type": {
                        "type": "string",
                        "enum": ["fact", "decision", "preference", "conversation"],
                        "description": "Alias of `kind` (the older name). Pass one or the other."
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional tag filter. A memory must carry EVERY \
            tag listed here to match (AND, not OR). Exact, case-sensitive."
                    },
                    "min_similarity": {
                        "type": "number",
                        "minimum": -1.0,
                        "maximum": 1.0,
                        "description": "Optional relevance floor on cosine similarity, \
            where 1.0 is identical and 0.0 is unrelated. Omit to get the nearest \
            `limit` memories however far away they are. ~0.5 is a reasonable cut-off \
            for \"actually about the same thing\"; use it when you would rather get \
            nothing back than get a weak match."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["hybrid", "semantic", "keyword"],
                        "description": "hybrid (default): meaning + exact words, fused. \
            semantic: meaning only. keyword: exact words only — use for a precise \
            name or identifier when near-synonyms would be noise."
                    },
                    "include_superseded": {
                        "type": "boolean",
                        "description": "Also return memories that a newer memory replaced \
            (`remember` with `supersedes`). They come after every current match and \
            carry `superseded_by`. Defaults to false."
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn invoke(&self, args: Value) -> Result<ToolResult, ToolError> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("`query` is required".into()))?;
        if query.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "`query` must not be empty".into(),
            ));
        }

        let limit = match args.get("limit") {
            None => DEFAULT_LIMIT,
            Some(Value::Number(n)) => n.as_u64().ok_or_else(|| {
                ToolError::InvalidArguments("`limit` must be a positive integer".into())
            })?,
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`limit` must be a positive integer".into(),
                ));
            }
        };
        if limit == 0 || limit > MAX_LIMIT {
            return Err(ToolError::InvalidArguments(format!(
                "`limit` must be between 1 and {MAX_LIMIT}"
            )));
        }

        let scopes = parse_scopes(args.get("scope"))?;
        let layers = parse_layers(args.get("layers"))?;
        let since = parse_time(&args, "since")?;
        let until = parse_time(&args, "until")?;
        if let (Some(s), Some(u)) = (since, until)
            && s >= u
        {
            return Err(ToolError::InvalidArguments(format!(
                "`since` ({s}) must be strictly before `until` ({u})"
            )));
        }
        let time_field = match args.get("time_field") {
            None | Some(Value::Null) => TimeField::default(),
            Some(Value::String(s)) => TimeField::parse(s).ok_or_else(|| {
                ToolError::InvalidArguments(format!(
                    "`time_field` must be created_at or updated_at, got `{s}`"
                ))
            })?,
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`time_field` must be a string".into(),
                ));
            }
        };
        let include_archived = parse_bool(&args, "include_archived")?;
        if layers.episodic && self.episodic.is_none() {
            return Err(ToolError::Internal(
                "`layers`: no episodic store attached".into(),
            ));
        }
        // `kind`, or its alias `type`; both set and different is an error.
        let kind = super::parse_kind_arg(&args)?;

        let tags = super::parse_tags_arg(args.get("tags"))?;
        let min_similarity = match args.get("min_similarity") {
            None | Some(Value::Null) => None,
            Some(Value::Number(n)) => {
                let v = n.as_f64().ok_or_else(|| {
                    ToolError::InvalidArguments("`min_similarity` must be a number".into())
                })?;
                if !(-1.0..=1.0).contains(&v) {
                    return Err(ToolError::InvalidArguments(
                        "`min_similarity` must be between -1.0 and 1.0".into(),
                    ));
                }
                Some(v as f32)
            }
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`min_similarity` must be a number".into(),
                ));
            }
        };

        let include_superseded = match args.get("include_superseded") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`include_superseded` must be a boolean".into(),
                ));
            }
        };

        let mode = match args.get("mode") {
            None | Some(Value::Null) => RecallMode::Hybrid,
            Some(Value::String(s)) => RecallMode::parse(s).ok_or_else(|| {
                ToolError::InvalidArguments(format!(
                    "`mode` must be one of hybrid|semantic|keyword, got `{s}`"
                ))
            })?,
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`mode` must be a string".into(),
                ));
            }
        };

        let request = RecallRequest {
            filters: RecallFilters {
                scope: None,
                kind,
                tags,
                min_similarity,
                include_superseded,
                mode,
            },
            layers,
            scopes,
            since,
            until,
            time_field,
            include_archived,
        };
        let hits = unified_recall::recall(
            &self.store,
            self.episodic.as_deref(),
            query,
            limit as usize,
            &request,
        )
        .await
        .map_err(|e| ToolError::Internal(format!("recall failed: {e}")))?;

        // Emit one structured JSON document so MCP hosts can either
        // parse it as JSON or hand it to the model verbatim. An empty
        // list serialises to `[]` — explicit and unambiguous.
        let body: Vec<Value> = hits
            .iter()
            .map(|unified| {
                let (h, rank) = match unified {
                    UnifiedHit::Semantic { hit, rank } => (hit, *rank),
                    UnifiedHit::Episodic(e) => {
                        return json!({
                            "id": e.event.id.to_string(),
                            "layer": "episodic",
                            // The event's text: payload.content (or
                            // .text) plus tags. The raw payload is on
                            // `recall_recent`.
                            "content": e.text,
                            "kind": e.event.kind,
                            "tags": e.event.tags,
                            "scope": e.event.scope,
                            "created_at": e.event.created_at.to_rfc3339(),
                            "retrieval_weight": e.event.retrieval_weight,
                            "tier": e.tier.as_str(),
                            // Events are not embedded: no distance, no
                            // similarity. `rank` is what orders them.
                            "score": Value::Null,
                            "similarity": Value::Null,
                            "match": "keyword",
                            "rank": e.rank,
                        });
                    }
                };
                let mut row = json!({
                    "layer": "semantic",
                    // What the list is ordered by, on one scale for
                    // both layers.
                    "rank": rank,
                    "id": h.item.id.to_string(),
                    "content": h.item.content,
                    // `kind` matches the field name used by every
                    // other tool/resource (recall_recent, recent
                    // resource, context resource). Avoids the
                    // earlier `type` vs `kind` split.
                    "kind": h.item.kind.as_str(),
                    "tags": h.item.tags,
                    "scope": h.item.scope,
                    "created_at": h.item.created_at.to_rfc3339(),
                    // `score` is a cosine DISTANCE — lower is closer.
                    // Kept under its original name for wire
                    // compatibility. `similarity` is the same number
                    // in the orientation agents actually reason about
                    // (1.0 = identical) and is what `min_similarity`
                    // filters on.
                    "score": h.score,
                    "similarity": crate::memory::semantic::similarity_from_distance(h.score),
                    // "semantic", "keyword", or "both".
                    "match": h.matched.as_str(),
                });
                if let Some(newer) = h.superseded_by {
                    row["superseded_by"] = json!(newer.to_string());
                }
                row
            })
            .collect();

        let text = serde_json::to_string(&body)
            .map_err(|e| ToolError::Internal(format!("serialise hits: {e}")))?;

        // The answer is built; refreshing the returned events is
        // bookkeeping that must not hold it up.
        if let Some(store) = self.episodic.as_ref().filter(|s| s.touch_on_recall()) {
            let hot: Vec<EventId> = hits
                .iter()
                .filter_map(|h| match h {
                    UnifiedHit::Episodic(e) if e.tier == Tier::Hot => Some(e.event.id),
                    _ => None,
                })
                .collect();
            touch_in_background(Arc::clone(store), hot);
        }
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

    fn fresh_store(tmp: &TempDir) -> Arc<SemanticStore> {
        let storage = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        SemanticStore::open_disabled(tmp.path(), storage, embedder).unwrap()
    }

    #[tokio::test]
    async fn empty_store_returns_empty_array() {
        let tmp = TempDir::new().unwrap();
        let r = Recall::new(fresh_store(&tmp));
        let res = r.invoke(json!({ "query": "anything" })).await.unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        assert!(v.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn empty_query_invalid() {
        let tmp = TempDir::new().unwrap();
        let r = Recall::new(fresh_store(&tmp));
        let err = r.invoke(json!({ "query": "" })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn limit_out_of_range_invalid() {
        let tmp = TempDir::new().unwrap();
        let r = Recall::new(fresh_store(&tmp));
        let err = r
            .invoke(json!({ "query": "x", "limit": 0 }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
        let err = r
            .invoke(json!({ "query": "x", "limit": 9999 }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn returns_remembered_item_in_json_array() {
        let tmp = TempDir::new().unwrap();
        let s = fresh_store(&tmp);
        let id = s
            .remember("hello world", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();

        let r = Recall::new(Arc::clone(&s));
        let res = r.invoke(json!({ "query": "hello world" })).await.unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        let arr = v.as_array().unwrap();
        assert!(!arr.is_empty());
        assert_eq!(arr[0]["id"], id.to_string());
        assert_eq!(arr[0]["content"], "hello world");
        assert_eq!(arr[0]["kind"], "fact");
    }

    #[tokio::test]
    async fn type_filter_applied() {
        let tmp = TempDir::new().unwrap();
        let s = fresh_store(&tmp);
        s.remember("note A", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        let dec = s
            .remember("note B", MemoryKind::Decision, vec![], "personal".into())
            .await
            .unwrap();

        let r = Recall::new(Arc::clone(&s));
        let res = r
            .invoke(json!({ "query": "note", "type": "decision" }))
            .await
            .unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        let arr = v.as_array().unwrap();
        assert!(!arr.is_empty());
        for hit in arr {
            // Output uses `kind` (canonical field name); `type` is
            // accepted on the input side for client compatibility.
            assert_eq!(hit["kind"], "decision");
            assert!(hit.get("type").is_none(), "output must not emit `type`");
        }
        assert!(arr.iter().any(|h| h["id"] == dec.to_string()));
    }

    #[tokio::test]
    async fn returns_kind_field_not_type_in_output() {
        let tmp = TempDir::new().unwrap();
        let s = fresh_store(&tmp);
        s.remember("hello", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        let r = Recall::new(s);
        let res = r.invoke(json!({ "query": "hello" })).await.unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        let arr = v.as_array().unwrap();
        assert!(!arr.is_empty());
        assert_eq!(arr[0]["kind"], "fact");
    }

    #[tokio::test]
    async fn include_superseded_marks_replaced_memories() {
        let tmp = TempDir::new().unwrap();
        let s = fresh_store(&tmp);
        let old = s
            .remember("balance is 5200", MemoryKind::Fact, vec![], "p".into())
            .await
            .unwrap();
        let new = s
            .remember("balance is 4800", MemoryKind::Fact, vec![], "p".into())
            .await
            .unwrap();
        s.supersede(old, new).await.unwrap();
        let r = Recall::new(Arc::clone(&s));
        let rows = |res: ToolResult| -> Vec<Value> {
            let text = match &res.content[0] {
                crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
            };
            serde_json::from_str::<Value>(&text)
                .unwrap()
                .as_array()
                .unwrap()
                .clone()
        };
        let default = rows(r.invoke(json!({ "query": "balance" })).await.unwrap());
        assert_eq!(default.len(), 1);
        assert!(default[0].get("superseded_by").is_none());
        let all = rows(
            r.invoke(json!({ "query": "balance", "include_superseded": true }))
                .await
                .unwrap(),
        );
        assert_eq!(all.len(), 2);
        assert_eq!(all[1]["id"], old.to_string());
        assert_eq!(all[1]["superseded_by"], new.to_string());
        let err = r
            .invoke(json!({ "query": "x", "include_superseded": "yes" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }
}

/// `layers`, the time window, multi-scope and touch-on-recall (1.6).
#[cfg(test)]
mod layer_tests {
    use super::*;
    use crate::embed::Embedder;
    use crate::embed::stub::StubEmbedder;
    use crate::memory::consolidation::_record_backdated_for_tests;
    use crate::memory::semantic::MemoryKind;
    use crate::storage::Storage;
    use crate::storage::memory_impl::MemoryStorage;
    use chrono::{Duration, Utc};
    use tempfile::TempDir;

    struct Rig {
        tool: Recall,
        semantic: Arc<SemanticStore>,
        episodic: Arc<EpisodicStore>,
        storage: Arc<dyn Storage>,
        _tmp: TempDir,
    }

    fn rig_with(touch_on_recall: bool) -> Rig {
        let tmp = TempDir::new().unwrap();
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(8));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&storage), embedder).unwrap();
        let episodic = Arc::new(
            EpisodicStore::new(Arc::clone(&storage)).with_touch_on_recall(touch_on_recall),
        );
        Rig {
            tool: Recall::new(Arc::clone(&semantic)).with_episodic(Arc::clone(&episodic)),
            semantic,
            episodic,
            storage,
            _tmp: tmp,
        }
    }

    fn rig() -> Rig {
        rig_with(false)
    }

    async fn rows(tool: &Recall, args: Value) -> Vec<Value> {
        let res = tool.invoke(args).await.unwrap();
        let text = match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        };
        serde_json::from_str(&text).unwrap()
    }

    async fn invalid(tool: &Recall, args: Value) -> String {
        match tool.invoke(args).await.unwrap_err() {
            ToolError::InvalidArguments(m) => m,
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn episodic_layer_is_opt_in_and_rows_carry_the_documented_fields() {
        let r = rig();
        let memory = r
            .semantic
            .remember(
                "the cat is called Murka",
                MemoryKind::Fact,
                vec![],
                "p".into(),
            )
            .await
            .unwrap();
        let event = r
            .episodic
            .record_full(
                "decision",
                "p",
                &json!({"content": "switched the nightly backtest to live_bt.py"}).to_string(),
                vec!["backtest".into()],
                0.9,
            )
            .await
            .unwrap();

        // Default: L4 only, exactly as before 1.6.
        let default = rows(&r.tool, json!({ "query": "live_bt.py" })).await;
        assert_eq!(default.len(), 1);
        assert_eq!(default[0]["id"], memory.to_string());
        assert_eq!(default[0]["layer"], "semantic");
        assert!(default[0]["similarity"].is_number());
        assert!(default[0]["rank"].is_number());
        let explicit = rows(
            &r.tool,
            json!({ "query": "live_bt.py", "layers": ["semantic"] }),
        )
        .await;
        assert_eq!(explicit, default);

        // Both layers: the decision that was never a memory is found.
        let both = rows(
            &r.tool,
            json!({ "query": "live_bt.py", "layers": ["semantic", "episodic"] }),
        )
        .await;
        assert_eq!(both.len(), 2);
        let row = both
            .iter()
            .find(|row| row["layer"] == "episodic")
            .expect("an episodic row");
        assert_eq!(row["id"], event.to_string());
        assert_eq!(row["kind"], "decision");
        assert_eq!(row["scope"], "p");
        assert_eq!(row["tags"], json!(["backtest"]));
        assert_eq!(
            row["content"],
            "switched the nightly backtest to live_bt.py\nbacktest"
        );
        assert_eq!(row["match"], "keyword");
        assert!(row["similarity"].is_null());
        assert!(row["score"].is_null());
        assert_eq!(row["tier"], "hot");
        assert!(row["created_at"].is_string());
        assert!((row["retrieval_weight"].as_f64().unwrap() - 0.9).abs() < 1e-6);
        // 0.8 × (0.8125 × 1² + 0.1) × 0.9
        assert!(
            (row["rank"].as_f64().unwrap() - 0.657).abs() < 1e-3,
            "{row}"
        );
        // Ordered by rank, best first.
        assert!(both[0]["rank"].as_f64().unwrap() >= both[1]["rank"].as_f64().unwrap());

        // Episodic alone.
        let only = rows(
            &r.tool,
            json!({ "query": "live_bt.py", "layers": ["episodic"] }),
        )
        .await;
        assert_eq!(only.len(), 1);
        assert_eq!(only[0]["id"], event.to_string());
    }

    #[tokio::test]
    async fn layers_argument_is_validated() {
        let r = rig();
        for bad in [
            json!([]),
            json!(["procedural"]),
            json!("episodic"),
            json!([1]),
        ] {
            let msg = invalid(&r.tool, json!({ "query": "x", "layers": bad })).await;
            assert!(msg.contains("`layers`"), "{msg}");
        }
        // Without an episodic store the layer cannot be served.
        let bare = Recall::new(Arc::clone(&r.semantic));
        let err = bare
            .invoke(json!({ "query": "x", "layers": ["episodic"] }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Internal(_)));
        // ...but the default still works.
        bare.invoke(json!({ "query": "x" })).await.unwrap();
    }

    #[tokio::test]
    async fn since_and_until_bound_the_result() {
        let r = rig();
        r.semantic
            .remember(
                "release train leaves Tuesday",
                MemoryKind::Fact,
                vec![],
                "p".into(),
            )
            .await
            .unwrap();
        // An event from last spring, written straight to storage so it
        // can be backdated; the index is built on the first recall.
        let old_event = _record_backdated_for_tests(
            &r.storage,
            "decision",
            "p",
            r#"{"content":"release train moved to Tuesday"}"#,
            Utc::now() - Duration::days(200),
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let mid = Utc::now();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let late = r
            .semantic
            .remember(
                "release train leaves Friday now",
                MemoryKind::Fact,
                vec![],
                "p".into(),
            )
            .await
            .unwrap();
        let both = json!(["semantic", "episodic"]);

        let all = rows(&r.tool, json!({ "query": "release train", "layers": both })).await;
        assert_eq!(all.len(), 3);

        let after = rows(
            &r.tool,
            json!({ "query": "release train", "layers": both, "since": mid.to_rfc3339() }),
        )
        .await;
        assert_eq!(after.len(), 1);
        assert_eq!(after[0]["id"], late.to_string());

        let before = rows(
            &r.tool,
            json!({ "query": "release train", "layers": both, "until": mid.to_rfc3339() }),
        )
        .await;
        assert_eq!(before.len(), 2);
        assert!(before.iter().all(|row| row["id"] != late.to_string()));

        // A window around the old event only; a ULID works as a bound.
        let window = rows(
            &r.tool,
            json!({
                "query": "release train",
                "layers": both,
                "since": (Utc::now() - Duration::days(210)).to_rfc3339(),
                "until": (Utc::now() - Duration::days(190)).to_rfc3339(),
                "time_field": "created_at",
            }),
        )
        .await;
        assert_eq!(window.len(), 1);
        assert_eq!(window[0]["id"], old_event.to_string());
        let by_ulid = rows(
            &r.tool,
            json!({ "query": "release train", "since": late.to_string() }),
        )
        .await;
        assert_eq!(by_ulid.len(), 1);
        assert_eq!(by_ulid[0]["id"], late.to_string());

        // `updated_at` is accepted.
        rows(
            &r.tool,
            json!({ "query": "release train", "since": mid.to_rfc3339(), "time_field": "updated_at" }),
        )
        .await;
    }

    #[tokio::test]
    async fn time_arguments_are_validated() {
        let r = rig();
        for (args, needle) in [
            (json!({ "query": "x", "since": "yesterday" }), "`since`"),
            (json!({ "query": "x", "until": 5 }), "`until`"),
            (
                json!({ "query": "x", "since": "2026-05-23T00:00:00Z", "until": "2026-05-20T00:00:00Z" }),
                "strictly before",
            ),
            (
                json!({ "query": "x", "time_field": "last_accessed" }),
                "`time_field`",
            ),
            (json!({ "query": "x", "time_field": 1 }), "`time_field`"),
            (
                json!({ "query": "x", "include_archived": "yes" }),
                "`include_archived`",
            ),
            (json!({ "query": "x", "scope": ["a"] }), "`scope`"),
        ] {
            let msg = invalid(&r.tool, args).await;
            assert!(msg.contains(needle), "{msg}");
        }
        r.tool
            .invoke(json!({ "query": "x", "include_archived": true }))
            .await
            .unwrap();
    }

    /// The 1.5.2 hint that sent `since` / `until` callers to
    /// `recall_recent` is gone: `recall` declares both.
    #[test]
    fn schema_declares_the_new_arguments_and_no_redirect_hint() {
        let tool = rig().tool;
        let props = tool.descriptor().input_schema["properties"].clone();
        for name in [
            "layers",
            "since",
            "until",
            "time_field",
            "include_archived",
            "scope",
        ] {
            assert!(props.get(name).is_some(), "missing `{name}`");
        }
        assert!(tool.unknown_arg_hint("since").is_none());
        assert!(tool.unknown_arg_hint("until").is_none());
    }

    #[tokio::test]
    async fn scope_accepts_a_comma_separated_list() {
        let r = rig();
        for scope in ["work", "home", "other"] {
            r.semantic
                .remember(
                    &format!("wifi password hint for {scope}"),
                    MemoryKind::Fact,
                    vec![],
                    scope.into(),
                )
                .await
                .unwrap();
            r.episodic
                .record_json(
                    "decision",
                    scope,
                    &json!({"content": format!("changed the wifi password at {scope}")}),
                )
                .await
                .unwrap();
        }
        let scopes_of = |rows: &[Value]| {
            let mut v: Vec<String> = rows
                .iter()
                .map(|row| row["scope"].as_str().unwrap().to_owned())
                .collect();
            v.sort();
            v
        };
        let both = json!(["semantic", "episodic"]);

        let one = rows(
            &r.tool,
            json!({ "query": "wifi password", "scope": "work", "layers": both }),
        )
        .await;
        assert_eq!(scopes_of(&one), vec!["work", "work"]);

        // Spaces around the commas and an empty entry are tolerated.
        let two = rows(
            &r.tool,
            json!({ "query": "wifi password", "scope": " work , home ,", "layers": both }),
        )
        .await;
        assert_eq!(scopes_of(&two), vec!["home", "home", "work", "work"]);

        let any = rows(
            &r.tool,
            json!({ "query": "wifi password", "scope": " , ", "layers": both }),
        )
        .await;
        assert_eq!(any.len(), 6);
    }

    async fn last_accessed(r: &Rig, id: EventId) -> chrono::DateTime<Utc> {
        r.episodic
            .find_warm_or_hot(id)
            .await
            .unwrap()
            .unwrap()
            .last_accessed
    }

    /// Seed one hot decision whose `last_accessed` is `days` old.
    async fn stale_decision(r: &Rig, days: i64) -> EventId {
        _record_backdated_for_tests(
            &r.storage,
            "decision",
            "p",
            r#"{"content":"keep the staging cluster on spot nodes"}"#,
            Utc::now() - Duration::days(days),
        )
        .await
        .unwrap()
    }

    async fn recall_it(r: &Rig) -> Vec<Value> {
        rows(
            &r.tool,
            json!({ "query": "staging cluster spot nodes", "layers": ["episodic"] }),
        )
        .await
    }

    /// Wait for the background touch, which runs after the response.
    async fn wait_until_touched(r: &Rig, id: EventId, was: chrono::DateTime<Utc>) -> bool {
        for _ in 0..200 {
            if last_accessed(r, id).await != was {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        false
    }

    #[tokio::test]
    async fn recall_touches_returned_events_at_most_once_a_day() {
        let r = rig_with(true);
        let id = stale_decision(&r, 10).await;
        let was = last_accessed(&r, id).await;

        assert_eq!(recall_it(&r).await[0]["id"], id.to_string());
        assert!(
            wait_until_touched(&r, id, was).await,
            "event was not touched"
        );
        let touched = last_accessed(&r, id).await;
        assert!(Utc::now() - touched < Duration::minutes(1));

        // Recalled again straight away: inside the debounce, no write.
        for _ in 0..3 {
            recall_it(&r).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(last_accessed(&r, id).await, touched);
    }

    #[tokio::test]
    async fn touch_on_recall_false_leaves_events_alone() {
        let r = rig_with(false);
        let id = stale_decision(&r, 10).await;
        let was = last_accessed(&r, id).await;
        assert_eq!(recall_it(&r).await.len(), 1);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(last_accessed(&r, id).await, was);
    }

    /// A read does not pull a warm row back to hot or rewrite it.
    #[tokio::test]
    async fn warm_events_are_returned_but_not_touched() {
        let r = rig_with(true);
        let id = stale_decision(&r, 40).await;
        assert!(r.episodic.promote_to_warm(id).await.unwrap());
        let was = last_accessed(&r, id).await;
        let got = recall_it(&r).await;
        assert_eq!(got[0]["id"], id.to_string());
        assert_eq!(got[0]["tier"], "warm");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(last_accessed(&r, id).await, was);
        assert_eq!(r.episodic.count_warm().await.unwrap(), 1);
    }
}
