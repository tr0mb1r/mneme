//! `forget` — agent-driven point delete by ULID across all live
//! memory layers.
//!
//! The schema declares only `id`. `query` and `scope` (bulk delete by
//! search or by scope) were advertised as "reserved" up to 1.5.1 while
//! every call that used them failed; they are no longer in the schema,
//! and a caller that still passes one gets the unknown-argument error
//! plus a hint to look the ids up and forget them one at a time.
//!
//! Deleting the memory is half of forgetting it: `remember`, `pin` and
//! `update` each mirror their `content` into an L3 `tool_call` event, so
//! the text would otherwise stay on the timeline (and in `recall_recent`,
//! `export` and auto-context). After the delete, `forget` rewrites every
//! hot- and warm-tier `tool_call` event about that memory to
//! `{"tool", "memory_id", "redacted": "forgotten", "at"}`; see
//! [`redact_forgotten`] for which rows match.
//!
//! `forget(id=…)` resolves the ULID against L4 semantic, then L0
//! procedural, then L3 episodic (hot+warm tiers) — first hit wins.
//! That order matches both write frequency (L4 facts last longest, so
//! agents most often correct them) and the cost of a wrong answer
//! (L4 hits surface in `recall`; cleaning them is the highest-value
//! invalidation per ADR-0010). Cold-archive entries are out of scope:
//! cold quarter files are append-only by design and the 180-day
//! window is a deliberate privacy floor.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use ulid::Ulid;

use super::{Tool, ToolAnnotations, ToolDescriptor, ToolError, ToolResult};
use crate::ids::{EventId, MemoryId};
use crate::mcp::dispatch::redact_forgotten;
use crate::memory::episodic::EpisodicStore;
use crate::memory::procedural::ProceduralStore;
use crate::memory::semantic::SemanticStore;

const DESCRIPTION: &str = "Delete a memory by ULID. Resolves across L4 \
semantic, L0 procedural, and L3 episodic (hot + warm tiers); first \
hit wins. The `tool_call` timeline events that mirrored the memory's \
content are redacted too. Cold-archive entries are not reachable. There is no bulk \
delete: find the ids with `recall` or `recall_recent` and forget them \
one at a time. Deletion is permanent — confirm with the user before \
invoking.";

/// What to do instead of the bulk delete `forget` never had.
const BULK_HINT: &str = "bulk forget by `query`/`scope` is not supported; find the ids \
with `recall` or `recall_recent` and call `forget` once per `id`";

pub(crate) struct Forget {
    semantic: Arc<SemanticStore>,
    procedural: Arc<ProceduralStore>,
    episodic: Arc<EpisodicStore>,
}

impl Forget {
    pub(crate) fn new(
        semantic: Arc<SemanticStore>,
        procedural: Arc<ProceduralStore>,
        episodic: Arc<EpisodicStore>,
    ) -> Self {
        Self {
            semantic,
            procedural,
            episodic,
        }
    }
}

#[async_trait]
impl Tool for Forget {
    fn unknown_arg_hint(&self, arg: &str) -> Option<&'static str> {
        match arg {
            "query" | "scope" => Some(BULK_HINT),
            _ => None,
        }
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "forget",
            title: "Forget a memory",
            annotations: ToolAnnotations::destructive(),
            description: DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Memory ULID." }
                },
                "required": ["id"]
            }),
        }
    }

    async fn invoke(&self, args: Value) -> Result<ToolResult, ToolError> {
        // The dispatcher rejects `query`/`scope` as undeclared before
        // this runs; the check stays for callers that invoke the tool
        // directly.
        if args.get("query").is_some() || args.get("scope").is_some() {
            return Err(ToolError::InvalidArguments(BULK_HINT.into()));
        }
        let id_str = args
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("`id` is required".into()))?;
        let ulid = Ulid::from_string(id_str)
            .map_err(|e| ToolError::InvalidArguments(format!("`id` is not a valid ULID: {e}")))?;

        let memory_id = MemoryId(ulid);

        // Read the text before it is gone: `tool_call` rows written
        // before the dispatcher recorded `memory_id` can only be matched
        // to this memory by their content.
        let forgotten_content = self.content_of(memory_id).await?;

        // L4 semantic, then L0 procedural, then L3 episodic (hot + warm
        // tiers); first hit wins.
        let event_id = EventId(ulid);
        let outcome = if self
            .semantic
            .forget(memory_id)
            .await
            .map_err(|e| ToolError::Internal(format!("forget (semantic) failed: {e}")))?
        {
            format!("forgot semantic memory {memory_id}")
        } else if self
            .procedural
            .unpin(memory_id)
            .await
            .map_err(|e| ToolError::Internal(format!("forget (procedural) failed: {e}")))?
        {
            format!("forgot procedural memory {memory_id}")
        } else if self
            .episodic
            .forget(event_id)
            .await
            .map_err(|e| ToolError::Internal(format!("forget (episodic) failed: {e}")))?
        {
            format!("forgot episodic event {event_id}")
        } else {
            format!("no such memory {memory_id}")
        };

        // The memory is gone; now the copies of its text that
        // `remember`/`pin`/`update` left on the timeline. This runs
        // whatever the outcome above, "no such memory" included: rows
        // are matched by id, so calling `forget` again finishes a
        // redaction that failed half-way, and scrubs the rows of a
        // memory forgotten before this existed.
        let at = chrono::Utc::now();
        let redacted = self
            .episodic
            .rewrite_payloads("tool_call", |event| {
                let payload = event.payload_json().ok()?;
                let replacement =
                    redact_forgotten(&payload, id_str, forgotten_content.as_deref(), at)?;
                serde_json::to_string(&replacement).ok()
            })
            .await
            .map_err(|e| {
                ToolError::Internal(format!(
                    "{outcome}, but redacting its tool_call events failed: {e}; \
                     call forget with the same id again to retry"
                ))
            })?;

        if redacted == 0 {
            return Ok(ToolResult::text(outcome));
        }
        tracing::info!(
            memory_id = %memory_id,
            redacted,
            "forget: redacted tool_call events that mirrored the memory"
        );
        Ok(ToolResult::text(format!(
            "{outcome}; redacted {redacted} tool_call event(s) that mirrored its content"
        ))
        .with_meta(json!({ "redacted_tool_calls": redacted })))
    }
}

impl Forget {
    /// The text of the L4 memory or L0 pin with this id, if there is
    /// one. L3 events are not looked up: no `tool_call` row mirrors an
    /// event's payload.
    async fn content_of(&self, id: MemoryId) -> Result<Option<String>, ToolError> {
        if let Some(item) = self
            .semantic
            .get(id)
            .await
            .map_err(|e| ToolError::Internal(format!("forget (semantic) failed: {e}")))?
        {
            return Ok(Some(item.content));
        }
        Ok(self
            .procedural
            .list(None)
            .map_err(|e| ToolError::Internal(format!("forget (procedural) failed: {e}")))?
            .into_iter()
            .find(|p| p.id == id)
            .map(|p| p.content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::Embedder;
    use crate::embed::stub::StubEmbedder;
    use crate::memory::semantic::{MemoryKind, SemanticStore};
    use crate::storage::memory_impl::MemoryStorage;
    use serde_json::json;
    use tempfile::TempDir;

    struct Fixture {
        _tmp: TempDir,
        semantic: Arc<SemanticStore>,
        procedural: Arc<ProceduralStore>,
        episodic: Arc<EpisodicStore>,
    }

    fn fixture() -> Fixture {
        use crate::storage::Storage;
        let tmp = TempDir::new().unwrap();
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&storage), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(storage));
        Fixture {
            _tmp: tmp,
            semantic,
            procedural,
            episodic,
        }
    }

    fn forget_tool(f: &Fixture) -> Forget {
        Forget::new(
            Arc::clone(&f.semantic),
            Arc::clone(&f.procedural),
            Arc::clone(&f.episodic),
        )
    }

    fn text_of(res: &ToolResult) -> String {
        match &res.content[0] {
            crate::mcp::tools::ContentBlock::Text(t) => t.clone(),
        }
    }

    #[tokio::test]
    async fn invalid_ulid_is_invalid_args() {
        let f = fixture();
        let err = forget_tool(&f)
            .invoke(json!({ "id": "not-a-ulid" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn unknown_id_returns_no_such_memory_across_layers() {
        let f = fixture();
        let res = forget_tool(&f)
            .invoke(json!({ "id": "01H0000000000000000000000Z" }))
            .await
            .unwrap();
        assert!(text_of(&res).starts_with("no such memory"));
    }

    #[tokio::test]
    async fn round_trip_forget_after_remember_l4() {
        let f = fixture();
        let id = f
            .semantic
            .remember("ephemeral", MemoryKind::Fact, vec![], "personal".into())
            .await
            .unwrap();
        let res = forget_tool(&f)
            .invoke(json!({ "id": id.to_string() }))
            .await
            .unwrap();
        assert!(text_of(&res).starts_with("forgot semantic"));
        assert!(f.semantic.get(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn round_trip_forget_after_pin_l0() {
        let f = fixture();
        let id = f
            .procedural
            .pin("rule".into(), vec![], "global".into())
            .await
            .unwrap();
        let res = forget_tool(&f)
            .invoke(json!({ "id": id.to_string() }))
            .await
            .unwrap();
        assert!(text_of(&res).starts_with("forgot procedural"));
        assert!(f.procedural.list(None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn round_trip_forget_after_record_event_l3() {
        let f = fixture();
        let event_id = f
            .episodic
            .record("observation", "global", "\"noted\"")
            .await
            .unwrap();
        let res = forget_tool(&f)
            .invoke(json!({ "id": event_id.to_string() }))
            .await
            .unwrap();
        assert!(text_of(&res).starts_with("forgot episodic"));
        assert!(f.episodic.get(event_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn round_trip_forget_after_record_event_warm_tier() {
        let f = fixture();
        let event_id = f
            .episodic
            .record("observation", "global", "\"warm\"")
            .await
            .unwrap();
        assert!(f.episodic.promote_to_warm(event_id).await.unwrap());
        let res = forget_tool(&f)
            .invoke(json!({ "id": event_id.to_string() }))
            .await
            .unwrap();
        assert!(text_of(&res).starts_with("forgot episodic"));
    }

    #[tokio::test]
    async fn zero_args_invalid() {
        let f = fixture();
        let err = forget_tool(&f).invoke(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn two_args_invalid() {
        let f = fixture();
        let err = forget_tool(&f)
            .invoke(json!({ "id": "01H...", "query": "x" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn query_arg_rejected_as_unsupported() {
        let f = fixture();
        let err = forget_tool(&f)
            .invoke(json!({ "query": "x" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }
}
