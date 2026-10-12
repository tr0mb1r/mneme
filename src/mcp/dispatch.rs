use crate::config::AutoLogToolCalls;
use crate::mcp::jsonrpc::{Id, Response, error_codes};
use crate::mcp::server::FAILED_MESSAGE_CAP;
use crate::mcp::tools::{ContentBlock, ToolError, ToolRegistry, ToolResult};
use crate::memory::checkpoint_scheduler::CheckpointScheduler;
use crate::memory::episodic::EpisodicStore;
use crate::memory::working::ActiveSession;
use crate::scope::ScopeState;
use serde_json::{Value, json};

/// Build the L3 `tool_call` event payload for a successful invocation.
/// Per ADR-0009: extract only the per-tool value-bearing arg(s);
/// **never** the full `arguments` object. The resulting payload feeds
/// into the L3 hot tier and ages into the cold zstd archive on disk,
/// so leaking secrets through this path would be a privacy bug.
///
/// New tools added to the MCP surface MUST add an entry here in the
/// same PR. The default for unknown tools is the bare
/// `{"tool": <name>}` shape — explicit allowlist, not denylist.
pub fn tool_call_payload(name: &str, args: &Value) -> Value {
    let mut p = json!({ "tool": name });
    let obj = p
        .as_object_mut()
        .expect("json!({...}) yields an object literal");

    let pull = |obj: &mut serde_json::Map<String, Value>, args: &Value, key: &str| {
        if let Some(v) = args.get(key)
            && !matches!(v, Value::Null)
        {
            obj.insert(key.to_owned(), v.clone());
        }
    };

    match name {
        // L4 writers — content carries the user's actual data.
        "remember" => {
            pull(obj, args, "content");
            // The schema argument is `type`; `kind` was never a remember
            // argument, so the old pull silently captured nothing.
            pull(obj, args, "type");
            pull(obj, args, "scope");
        }
        // L0 writer — same shape as remember, no embedder involvement.
        "pin" => {
            pull(obj, args, "content");
            pull(obj, args, "scope");
        }
        // Mutators — id + new content if provided.
        "update" => {
            pull(obj, args, "id");
            pull(obj, args, "content");
            pull(obj, args, "type");
        }
        "forget" => {
            pull(obj, args, "id");
        }
        "unpin" => {
            pull(obj, args, "id");
        }
        // Readers — record the search intent.
        "recall" => {
            pull(obj, args, "query");
            // The schema argument is `limit`; `k` never existed.
            pull(obj, args, "limit");
        }
        "recall_recent" => {
            pull(obj, args, "limit");
            pull(obj, args, "kind");
            pull(obj, args, "scope");
        }
        "summarize_session" => {
            pull(obj, args, "events");
            pull(obj, args, "scope");
        }
        // State change.
        "switch_scope" => {
            // Tool's arg is `scope`; expose under `new_scope` so the
            // L3 stream reads naturally ("we switched to <new>") on
            // recall_recent.
            if let Some(v) = args.get("scope") {
                obj.insert("new_scope".to_owned(), v.clone());
            }
        }
        // L3 producer (this very tool!) — capture the kind only, NOT
        // the payload. The payload is already being recorded by the
        // tool itself; double-recording would inflate the hot tier
        // with duplicate content.
        "record_event" => {
            pull(obj, args, "kind");
            pull(obj, args, "scope");
        }
        // Diagnostic / portability — no value-bearing args worth
        // mirroring. Bare `{"tool": <name>}` is enough.
        "stats" | "list_scopes" | "export" => {}
        // Unknown tool — bare payload, intentional default.
        _ => {}
    }
    p
}

/// True when the guard flagged the stored content (`warn` mode, or the
/// generic warn-only pattern) and attached a `secret_advisory`.
fn has_secret_advisory(result: &ToolResult) -> bool {
    result
        .meta
        .as_ref()
        .is_some_and(|m| m.get("secret_advisory").is_some())
}

/// Drop the content-bearing fields from a `tool_call` payload. A
/// flagged write must not leave a second copy of the credential in L3
/// that `forget` on the memory would not reach; the tool name, ids and
/// scope stay.
fn redact_flagged_content(payload: &mut Value) {
    if let Some(obj) = payload.as_object_mut() {
        obj.remove("content");
        obj.remove("text");
    }
}

/// Extract a short diagnostic message from a soft-error
/// `ToolResult` (one returned via `Ok(ToolResult::with_error())`,
/// e.g. the size-tier rejection from `remember`/`update`). We
/// prefer the structured `_meta.error.code` when present so
/// downstream queries can filter on it; otherwise fall back to the
/// human-facing text. `tool_call_failed_payload` will further cap
/// the message at `FAILED_MESSAGE_CAP` chars on the way to L3.
pub fn soft_error_message(result: &ToolResult) -> String {
    // Structured error code wins — e.g. "memory_too_large".
    if let Some(meta) = &result.meta
        && let Some(code) = meta
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_str)
    {
        return code.to_owned();
    }
    // Otherwise pull the first text block; "<no text>" if none.
    // The `match` is exhaustive against today's single-variant
    // ContentBlock; if a future variant is added, the compiler
    // surfaces the gap here.
    match result.content.first() {
        Some(ContentBlock::Text(s)) => s.clone(),
        None => "<no text>".to_owned(),
    }
}

/// Build the L3 `tool_call_failed` event payload. Capped message;
/// no args (privacy: failed remember/pin still received the args, we
/// don't re-emit them here).
pub fn tool_call_failed_payload(name: &str, error_kind: &str, message: &str) -> Value {
    let truncated: String = if message.len() > FAILED_MESSAGE_CAP {
        let mut s = message.chars().take(FAILED_MESSAGE_CAP).collect::<String>();
        s.push('…');
        s
    } else {
        message.to_owned()
    };
    json!({
        "tool": name,
        "error_kind": error_kind,
        "message": truncated,
    })
}

/// Emit a `tool_call_failed` L3 event when a tool dispatch fails.
/// Mirrors the success path's emit but skips the L1 turn / checkpoint
/// poke (failed calls don't count as turns — same logic that
/// protected `turns_total` pre-v0.2.4). Per ADR-0009.
async fn emit_tool_call_failed(
    episodic: Option<&EpisodicStore>,
    scope_state: Option<&ScopeState>,
    auto_log: AutoLogToolCalls,
    tool_name: &str,
    error_kind: &str,
    message: &str,
) {
    if !auto_log.logs_failures() {
        return;
    }
    if let (Some(episodic), Some(scope_state)) = (episodic, scope_state) {
        let scope = scope_state.current();
        let payload = tool_call_failed_payload(tool_name, error_kind, message);
        if let Err(e) = episodic
            .record_json("tool_call_failed", &scope, &payload)
            .await
        {
            tracing::warn!(
                error = %e,
                tool = %tool_name,
                "failed to record episodic tool_call_failed event"
            );
        }
    }
}

/// Dispatch a `tools/call` request: extract name/args, look up the
/// tool in the registry, invoke it, and return the appropriate
/// JSON-RPC response. Also handles L3 event emission for both
/// success and failure paths per ADR-0009.
///
/// The argument list mirrors the per-session wiring `McpSession`
/// holds; the same allowance already covers `ToolRegistry::defaults*`.
#[allow(clippy::too_many_arguments)]
pub async fn handle_tools_call(
    tools: &ToolRegistry,
    session: Option<&ActiveSession>,
    checkpoint_scheduler: Option<&CheckpointScheduler>,
    episodic: Option<&EpisodicStore>,
    scope_state: Option<&ScopeState>,
    auto_log: AutoLogToolCalls,
    id: Id,
    params: Option<Value>,
) -> Response {
    let params = match params {
        Some(p) => p,
        None => {
            return Response::error(id, error_codes::INVALID_PARAMS, "missing params");
        }
    };
    let name = match params.get("name").and_then(Value::as_str) {
        Some(n) => n.to_string(),
        None => {
            return Response::error(id, error_codes::INVALID_PARAMS, "missing `name`");
        }
    };
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let tool = match tools.get(&name) {
        Some(t) => t,
        None => {
            emit_tool_call_failed(
                episodic,
                scope_state,
                auto_log,
                &name,
                "MethodNotFound",
                &format!("unknown tool: {name}"),
            )
            .await;
            return Response::error(
                id,
                error_codes::METHOD_NOT_FOUND,
                format!("unknown tool: {name}"),
            );
        }
    };

    // Keys outside the tool's declared input schema are an error, not a
    // silent drop (cached set; no allocation unless something is wrong).
    let outcome = match tools.check_args(&name, &args) {
        Ok(()) => tool.invoke(args.clone()).await,
        Err(e) => Err(e),
    };
    match outcome {
        Ok(result) if result.is_error => {
            let message = soft_error_message(&result);
            emit_tool_call_failed(episodic, scope_state, auto_log, &name, "Rejected", &message)
                .await;
            Response::success(id, result.to_json())
        }
        Ok(result) => {
            if let (Some(session), Some(sched)) = (session, checkpoint_scheduler) {
                session.push_turn("tool", &name);
                sched.poke();
            }
            // The mirror is decided by the tool's own `readOnlyHint`
            // (cached at registration), so a newly added tool is
            // classified without touching this code.
            if let (true, Some(episodic), Some(scope_state)) = (
                auto_log.logs_success(tools.is_read_only(&name).unwrap_or(false)),
                episodic,
                scope_state,
            ) {
                let scope = scope_state.current();
                let mut payload = tool_call_payload(&name, &args);
                if has_secret_advisory(&result) {
                    redact_flagged_content(&mut payload);
                }
                if let Err(e) = episodic.record_json("tool_call", &scope, &payload).await {
                    tracing::warn!(
                        error = %e,
                        tool = %name,
                        "failed to record episodic tool_call event"
                    );
                }
            }
            Response::success(id, result.to_json())
        }
        Err(ToolError::InvalidArguments(msg)) => {
            emit_tool_call_failed(
                episodic,
                scope_state,
                auto_log,
                &name,
                "InvalidArguments",
                &msg,
            )
            .await;
            Response::error(id, error_codes::INVALID_PARAMS, msg)
        }
        Err(ToolError::NotFound(msg)) => {
            emit_tool_call_failed(episodic, scope_state, auto_log, &name, "NotFound", &msg).await;
            Response::error(id, error_codes::METHOD_NOT_FOUND, msg)
        }
        Err(ToolError::Internal(msg)) => {
            emit_tool_call_failed(episodic, scope_state, auto_log, &name, "Internal", &msg).await;
            Response::error(id, error_codes::INTERNAL_ERROR, msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SecretsMode;
    use crate::embed::Embedder;
    use crate::embed::stub::StubEmbedder;
    use crate::memory::episodic::RecentFilters;
    use crate::memory::procedural::ProceduralStore;
    use crate::memory::semantic::SemanticStore;
    use crate::storage::Storage;
    use crate::storage::archive::ColdArchive;
    use crate::storage::memory_impl::MemoryStorage;
    use std::sync::Arc;

    /// A credential-shaped string, assembled at runtime so this file
    /// does not itself trip repository secret scanners.
    fn aws_key() -> String {
        format!("AKIA{}", "IOSFODNN7EXAMPLE")
    }

    /// Call `tool` under `secrets` with auto-logging on `all`, and
    /// return the recorded `tool_call` payloads.
    async fn tool_call_payloads(secrets: SecretsMode, tool: &str, args: Value) -> Vec<Value> {
        let tmp = tempfile::TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&backing), embedder).unwrap();
        let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        let episodic = Arc::new(EpisodicStore::new(Arc::clone(&backing)));
        let scope_state = ScopeState::new("work");
        let registry = ToolRegistry::defaults_with_schedulers(
            semantic,
            procedural,
            Arc::clone(&episodic),
            Arc::clone(&backing),
            ColdArchive::new(tmp.path()),
            1,
            None,
            None,
            Arc::clone(&scope_state),
            None,
            crate::mcp::tools::size_tier::DEFAULT_MAX_CHARS,
            secrets,
        );
        let resp = handle_tools_call(
            &registry,
            None,
            None,
            Some(&episodic),
            Some(&scope_state),
            AutoLogToolCalls::All,
            Id::Number(1),
            Some(json!({"name": tool, "arguments": args})),
        )
        .await;
        assert!(resp.error.is_none(), "{:?}", resp.error);
        episodic
            .recall_recent(&RecentFilters::default(), 10)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "tool_call")
            .map(|e| e.payload_json().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn flagged_remember_leaves_no_content_in_the_tool_call_event() {
        let secret = aws_key();
        let events = tool_call_payloads(
            SecretsMode::Warn,
            "remember",
            json!({"content": format!("deploy key {secret}"), "scope": "work"}),
        )
        .await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["tool"], "remember");
        assert_eq!(events[0]["scope"], "work");
        assert!(events[0].get("content").is_none(), "{}", events[0]);
        assert!(!events[0].to_string().contains(&secret));
    }

    #[tokio::test]
    async fn flagged_pin_and_generic_pattern_are_redacted_too() {
        let events = tool_call_payloads(
            SecretsMode::Warn,
            "pin",
            json!({"content": format!("use {}", aws_key())}),
        )
        .await;
        assert!(events[0].get("content").is_none(), "{}", events[0]);

        // The generic warn-only pattern fires in the default `reject` mode.
        let events = tool_call_payloads(
            SecretsMode::Reject,
            "remember",
            json!({"content": "the db password = hunter2hunter2"}),
        )
        .await;
        assert_eq!(events.len(), 1);
        assert!(events[0].get("content").is_none(), "{}", events[0]);
    }

    /// An update of an unknown id is a plain "no such memory" reply, not an
    /// error, yet its arguments still carry the flagged text.
    #[tokio::test]
    async fn flagged_update_of_unknown_id_leaves_no_content_in_the_tool_call_event() {
        let secret = aws_key();
        let events = tool_call_payloads(
            SecretsMode::Warn,
            "update",
            json!({
                "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "content": format!("deploy key {secret}"),
            }),
        )
        .await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["tool"], "update");
        assert_eq!(events[0]["id"], "01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert!(events[0].get("content").is_none(), "{}", events[0]);
        assert!(events[0].get("text").is_none(), "{}", events[0]);
        assert!(!events[0].to_string().contains(&secret));
    }

    #[tokio::test]
    async fn clean_remember_still_mirrors_its_content() {
        let events = tool_call_payloads(
            SecretsMode::Reject,
            "remember",
            json!({"content": "we use redb"}),
        )
        .await;
        assert_eq!(events[0]["content"], "we use redb");
    }
}
