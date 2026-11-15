use crate::config::AutoLogToolCalls;
use crate::mcp::jsonrpc::{Id, Response, error_codes};
use crate::mcp::server::FAILED_MESSAGE_CAP;
use crate::mcp::tools::{ContentBlock, ToolError, ToolRegistry, ToolResult};
use crate::memory::checkpoint_scheduler::CheckpointScheduler;
use crate::memory::episodic::EpisodicStore;
use crate::memory::semantic::normalise_content;
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

    // `remember`/`update` take the memory kind as `kind` or its alias
    // `type`; the event records it as `kind` either way, the name every
    // reply uses. (A call that passed both with different values was
    // rejected and never gets here.)
    let pull_kind = |obj: &mut serde_json::Map<String, Value>, args: &Value| {
        if let Some(v) = ["kind", "type"]
            .iter()
            .filter_map(|k| args.get(*k))
            .find(|v| !v.is_null())
        {
            obj.insert("kind".to_owned(), v.clone());
        }
    };

    match name {
        // L4 writers — content carries the user's actual data.
        "remember" => {
            pull(obj, args, "content");
            pull_kind(obj, args);
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
            pull_kind(obj, args);
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

/// The id of the memory a successful write stored or changed, as the
/// tool reported it in `_meta.id` (see `ToolResult::with_id`).
fn written_memory_id(result: &ToolResult) -> Option<&str> {
    result.meta.as_ref()?.get("id")?.as_str()
}

/// Tools whose `tool_call` event mirrors a memory's text.
fn mirrors_memory_content(tool: &str) -> bool {
    matches!(tool, "remember" | "pin" | "update")
}

/// The replacement for a `tool_call` payload that mirrored a memory
/// which has now been forgotten, or `None` when `payload` is not about
/// that memory (or is already redacted).
///
/// `forget` deletes the memory, but `remember`/`pin`/`update` each left
/// a copy of its text on the L3 timeline. A row matches when:
///
/// * `payload.memory_id == memory_id` — rows written since the
///   dispatcher records the id of the memory a write produced;
/// * `payload.id == memory_id` — every `update` row, old or new;
/// * it carries neither id and its `content`, whitespace-normalised,
///   equals `forgotten_content` — `remember`/`pin` rows from before
///   `memory_id` existed. This is a match on text, so a legacy row for
///   another memory with identical text is redacted too; losing a
///   duplicate copy of text the user asked to forget is the safe side.
///   A legacy `remember` row whose memory was later edited no longer
///   equals the final text and is not found.
///
/// The replacement keeps what makes the row useful as history (which
/// tool, which memory) and drops everything else, the scope and kind
/// included: `{"tool", "memory_id", "redacted": "forgotten", "at"}`,
/// where `at` is when the redaction happened.
pub fn redact_forgotten(
    payload: &Value,
    memory_id: &str,
    forgotten_content: Option<&str>,
    at: chrono::DateTime<chrono::Utc>,
) -> Option<Value> {
    let obj = payload.as_object()?;
    let tool = obj.get("tool")?.as_str()?;
    if !mirrors_memory_content(tool) || obj.contains_key("redacted") {
        return None;
    }
    let field = |key: &str| obj.get(key).and_then(Value::as_str);
    let by_id = field("memory_id") == Some(memory_id) || field("id") == Some(memory_id);
    let by_legacy_content = || {
        !obj.contains_key("memory_id")
            && !obj.contains_key("id")
            && match (field("content"), forgotten_content) {
                (Some(row), Some(forgotten)) => {
                    normalise_content(row) == normalise_content(forgotten)
                }
                _ => false,
            }
    };
    (by_id || by_legacy_content()).then(|| {
        json!({
            "tool": tool,
            "memory_id": memory_id,
            "redacted": "forgotten",
            "at": at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        })
    })
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

/// Whether a successful call to a tool with the given `readOnlyHint`
/// pushes an L1 working turn (`role = "tool"`, content = the tool name).
///
/// Reads do not. A turn per call made looking something up count as
/// conversation: a read-only evaluation run moved `turns_total` from 361
/// to 1026, each of those turns took a slot in the working-memory window
/// that auto-context shows the model, and every fifth one triggered a
/// session checkpoint with nothing new in it. It is the same rule
/// `auto_log_tool_calls = "writes"` applies to the L3 mirror.
///
/// Fixed at "writes" for now; 1.6 makes it `[working] record_tool_turns`.
fn records_tool_turn(read_only: bool) -> bool {
    !read_only
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
            // Both the L1 turn and the L3 mirror are decided by the
            // tool's own `readOnlyHint` (cached at registration), so a
            // newly added tool is classified without touching this code.
            let read_only = tools.is_read_only(&name).unwrap_or(false);
            if records_tool_turn(read_only)
                && let (Some(session), Some(sched)) = (session, checkpoint_scheduler)
            {
                session.push_turn("tool", &name);
                sched.poke();
            }
            if let (true, Some(episodic), Some(scope_state)) =
                (auto_log.logs_success(read_only), episodic, scope_state)
            {
                let scope = scope_state.current();
                let mut payload = tool_call_payload(&name, &args);
                if has_secret_advisory(&result) {
                    redact_flagged_content(&mut payload);
                }
                // Tie the event to the memory it wrote, so a later
                // `forget` of that memory can find and redact it. Not
                // derivable from the arguments: `remember` and `pin`
                // mint the id.
                if mirrors_memory_content(&name)
                    && let Some(memory_id) = written_memory_id(&result)
                    && let Some(obj) = payload.as_object_mut()
                {
                    obj.insert("memory_id".to_owned(), json!(memory_id));
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

    /// A full default registry over in-memory storage, plus the
    /// per-session handles `handle_tools_call` takes.
    struct Rig {
        _tmp: tempfile::TempDir,
        registry: ToolRegistry,
        episodic: Arc<EpisodicStore>,
        scope_state: Arc<ScopeState>,
        auto_log: AutoLogToolCalls,
    }

    impl Rig {
        fn new(secrets: SecretsMode, auto_log: AutoLogToolCalls) -> Self {
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
            Self {
                _tmp: tmp,
                registry,
                episodic,
                scope_state,
                auto_log,
            }
        }

        async fn call(&self, tool: &str, args: Value) -> Response {
            handle_tools_call(
                &self.registry,
                None,
                None,
                Some(&self.episodic),
                Some(&self.scope_state),
                self.auto_log,
                Id::Number(1),
                Some(json!({"name": tool, "arguments": args})),
            )
            .await
        }

        /// Recorded `tool_call` payloads, newest first.
        async fn tool_call_payloads(&self) -> Vec<Value> {
            self.episodic
                .recall_recent(&RecentFilters::default(), 50)
                .await
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == "tool_call")
                .map(|e| e.payload_json().unwrap())
                .collect()
        }
    }

    impl Rig {
        /// A call that must succeed; returns the `tools/call` result.
        async fn ok(&self, tool: &str, args: Value) -> Value {
            let resp = self.call(tool, args).await;
            assert!(resp.error.is_none(), "{tool}: {:?}", resp.error);
            let result = resp.result.unwrap();
            assert_eq!(result["isError"], false, "{tool}: {result}");
            result
        }

        /// A successful write; returns the id it reported in `_meta`.
        async fn write(&self, tool: &str, args: Value) -> String {
            self.ok(tool, args).await["_meta"]["id"]
                .as_str()
                .expect("a write reports `_meta.id`")
                .to_owned()
        }

        /// Raw payload text of every `tool_call` row, hot and warm.
        async fn tool_call_rows(&self) -> Vec<String> {
            let mut rows = self.episodic.list_all().await.unwrap();
            rows.extend(self.episodic.list_warm().await.unwrap());
            rows.into_iter()
                .filter(|e| e.kind == "tool_call")
                .map(|e| e.payload)
                .collect()
        }

        /// Move every hot `tool_call` row for `tool` to the warm tier.
        async fn age_to_warm(&self, tool: &str) -> usize {
            let mut moved = 0;
            for e in self.episodic.list_all().await.unwrap() {
                if e.kind == "tool_call" && e.payload_json().unwrap()["tool"] == tool {
                    assert!(self.episodic.promote_to_warm(e.id).await.unwrap());
                    moved += 1;
                }
            }
            moved
        }
    }

    /// Panics if any `tool_call` row, hot or warm, still holds any
    /// 20-character run of `content`.
    async fn assert_no_trace(rig: &Rig, content: &str) {
        let chars: Vec<char> = content.chars().collect();
        assert!(chars.len() >= 20, "test content too short to window");
        let rows = rig.tool_call_rows().await;
        for window in chars.windows(20) {
            let needle: String = window.iter().collect();
            for row in &rows {
                assert!(!row.contains(&needle), "`{needle}` survives in {row}");
            }
        }
    }

    const SECRET_V1: &str = "Mira's locker combination is 31-07-44 at the Riverside gym";
    const SECRET_V2: &str = "Mira's locker combination changed to 08-19-62, same gym";
    const BYSTANDER: &str = "the staging database listens on port 5433 behind the VPN";

    #[tokio::test]
    async fn successful_writes_record_the_memory_id() {
        let rig = Rig::new(SecretsMode::Reject, AutoLogToolCalls::Writes);
        let mem = rig.write("remember", json!({"content": SECRET_V1})).await;
        let pin = rig.write("pin", json!({"content": BYSTANDER})).await;
        rig.write("update", json!({"id": mem, "content": SECRET_V2}))
            .await;
        // An update that changed nothing wrote nothing: no `memory_id`.
        rig.ok(
            "update",
            json!({"id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "content": "no such row"}),
        )
        .await;
        // A retried remember resolves to the memory already stored.
        let again = rig.write("remember", json!({"content": SECRET_V2})).await;
        assert_eq!(again, mem);

        let events = rig.tool_call_payloads().await;
        let ids: Vec<(&str, Option<&str>)> = events
            .iter()
            .rev()
            .map(|e| (e["tool"].as_str().unwrap(), e["memory_id"].as_str()))
            .collect();
        assert_eq!(
            ids,
            [
                ("remember", Some(mem.as_str())),
                ("pin", Some(pin.as_str())),
                ("update", Some(mem.as_str())),
                ("update", None),
                ("remember", Some(mem.as_str())),
            ]
        );
    }

    /// F9: `forget` deleted the memory and left its full text in the
    /// `tool_call` rows of the writes that created and edited it.
    #[tokio::test]
    async fn forget_redacts_the_tool_call_rows_in_hot_and_warm() {
        let rig = Rig::new(SecretsMode::Reject, AutoLogToolCalls::Writes);
        let mem = rig
            .write(
                "remember",
                json!({"content": SECRET_V1, "kind": "fact", "scope": "private"}),
            )
            .await;
        let other = rig.write("remember", json!({"content": BYSTANDER})).await;
        // The remember rows age into the warm tier; the update stays hot.
        assert_eq!(rig.age_to_warm("remember").await, 2);
        rig.write("update", json!({"id": mem, "content": SECRET_V2}))
            .await;
        assert_eq!(rig.episodic.count_warm().await.unwrap(), 2);

        let result = rig.ok("forget", json!({"id": mem})).await;
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("forgot semantic memory"), "{text}");
        assert!(text.contains("redacted 2 tool_call event(s)"), "{text}");
        assert_eq!(result["_meta"]["redacted_tool_calls"], 2);

        assert_no_trace(&rig, SECRET_V1).await;
        assert_no_trace(&rig, SECRET_V2).await;

        // Rows stay where they were, in the documented shape.
        assert_eq!(rig.episodic.count_warm().await.unwrap(), 2);
        let redacted: Vec<Value> = rig
            .tool_call_rows()
            .await
            .iter()
            .map(|r| serde_json::from_str::<Value>(r).unwrap())
            .filter(|p| p.get("redacted").is_some())
            .collect();
        assert_eq!(redacted.len(), 2);
        for p in &redacted {
            let mut keys: Vec<&str> = p.as_object().unwrap().keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(keys, ["at", "memory_id", "redacted", "tool"]);
            assert_eq!(p["memory_id"], mem.as_str());
            assert_eq!(p["redacted"], "forgotten");
            chrono::DateTime::parse_from_rfc3339(p["at"].as_str().unwrap()).unwrap();
        }
        let mut tools: Vec<&str> = redacted
            .iter()
            .map(|p| p["tool"].as_str().unwrap())
            .collect();
        tools.sort_unstable();
        assert_eq!(tools, ["remember", "update"]);

        // The other memory's row is untouched, and so is the audit row
        // of the forget itself.
        let rows = rig.tool_call_rows().await;
        assert!(rows.iter().any(|r| r.contains(BYSTANDER)));
        assert!(rows.iter().any(|r| {
            let p: Value = serde_json::from_str(r).unwrap();
            p == json!({"tool": "forget", "id": mem})
        }));
        let still = rig.ok("recall", json!({"query": "staging database"})).await;
        assert!(
            still["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(&other)
        );
    }

    #[tokio::test]
    async fn forget_redacts_a_pin_and_the_update_of_a_missing_memory() {
        let rig = Rig::new(SecretsMode::Reject, AutoLogToolCalls::Writes);
        let pin = rig.write("pin", json!({"content": SECRET_V1})).await;
        let result = rig.ok("forget", json!({"id": pin})).await;
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("forgot procedural memory"), "{text}");
        assert_eq!(result["_meta"]["redacted_tool_calls"], 1);
        assert_no_trace(&rig, SECRET_V1).await;

        // An update of an id that does not exist stored nothing, but its
        // row still mirrors the text; forgetting that id clears it.
        let ghost = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        rig.ok("update", json!({"id": ghost, "content": SECRET_V2}))
            .await;
        let result = rig.ok("forget", json!({"id": ghost})).await;
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("no such memory"), "{text}");
        assert_eq!(result["_meta"]["redacted_tool_calls"], 1);
        assert_no_trace(&rig, SECRET_V2).await;

        // Nothing left to do the second time.
        let result = rig.ok("forget", json!({"id": ghost})).await;
        assert!(result.get("_meta").is_none(), "{result}");
    }

    /// Rows written before `memory_id` existed carry only the content.
    /// They are matched by text, whitespace-normalised, in both tiers.
    #[tokio::test]
    async fn forget_redacts_legacy_rows_by_content() {
        let rig = Rig::new(SecretsMode::Reject, AutoLogToolCalls::Off);
        let mem = rig.write("remember", json!({"content": SECRET_V1})).await;
        assert!(rig.tool_call_rows().await.is_empty());

        let legacy =
            |content: &str| json!({"tool": "remember", "content": content, "type": "fact"});
        let spaced = SECRET_V1.replace(' ', "  ");
        let hot = rig
            .episodic
            .record_json("tool_call", "work", &legacy(&format!("  {spaced}\n")))
            .await
            .unwrap();
        let warm = rig
            .episodic
            .record_json("tool_call", "work", &legacy(SECRET_V1))
            .await
            .unwrap();
        assert!(rig.episodic.promote_to_warm(warm).await.unwrap());
        // Different text, and a row that already belongs to another id.
        rig.episodic
            .record_json("tool_call", "work", &legacy(BYSTANDER))
            .await
            .unwrap();
        rig.episodic
            .record_json(
                "tool_call",
                "work",
                &json!({"tool": "remember", "content": SECRET_V1, "memory_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV"}),
            )
            .await
            .unwrap();
        // Same text under another event kind is not a tool_call mirror.
        rig.episodic
            .record_json("note", "work", &legacy(SECRET_V1))
            .await
            .unwrap();

        let result = rig.ok("forget", json!({"id": mem})).await;
        assert_eq!(result["_meta"]["redacted_tool_calls"], 2);
        for id in [hot, warm] {
            let p = rig
                .episodic
                .find_warm_or_hot(id)
                .await
                .unwrap()
                .unwrap()
                .payload_json()
                .unwrap();
            assert_eq!(p["redacted"], "forgotten", "{p}");
            assert_eq!(p["memory_id"], mem.as_str());
            assert!(p.get("content").is_none() && p.get("type").is_none());
        }
        let rows = rig.tool_call_rows().await;
        assert_eq!(rows.iter().filter(|r| r.contains("31-07-44")).count(), 1);
        assert!(rows.iter().any(|r| r.contains(BYSTANDER)));
    }

    /// F10: a read-only evaluation run pushed 665 L1 turns.
    #[tokio::test]
    async fn read_only_tools_push_no_working_turn() {
        use crate::memory::checkpoint_scheduler::CheckpointSchedulerConfig;

        let rig = Rig::new(SecretsMode::Reject, AutoLogToolCalls::All);
        let sessions = tempfile::TempDir::new().unwrap();
        let session = ActiveSession::open(sessions.path().to_path_buf()).unwrap();
        let sched =
            CheckpointScheduler::start(Arc::clone(&session), CheckpointSchedulerConfig::disabled());
        let call = |tool: &'static str, args: Value| {
            let (rig, session, sched) = (&rig, &session, &sched);
            async move {
                let resp = handle_tools_call(
                    &rig.registry,
                    Some(session),
                    Some(sched),
                    Some(&rig.episodic),
                    Some(&rig.scope_state),
                    rig.auto_log,
                    Id::Number(1),
                    Some(json!({"name": tool, "arguments": args})),
                )
                .await;
                assert!(resp.error.is_none(), "{tool}: {:?}", resp.error);
                resp.result.unwrap()
            }
        };

        // Writes count, one turn each, named after the tool.
        let stored = call("remember", json!({"content": "we use redb"})).await;
        let id = stored["_meta"]["id"].as_str().unwrap().to_owned();
        call("pin", json!({"content": "run cargo fmt"})).await;
        assert_eq!(session.turns_total(), 2);

        // Every tool that declares `readOnlyHint` leaves the count alone,
        // even with auto-logging on `all` (the L3 mirror is a separate
        // setting and still fires).
        let read_only: Vec<&'static str> = rig
            .registry
            .list()
            .into_iter()
            .filter(|d| d.annotations.read_only)
            .map(|d| d.name)
            .collect();
        for expected in ["recall", "recall_recent", "stats", "list_scopes", "export"] {
            assert!(read_only.contains(&expected), "{expected} not read-only");
        }
        for tool in &read_only {
            let args = match *tool {
                "recall" => json!({"query": "redb"}),
                _ => json!({}),
            };
            call(tool, args).await;
        }
        assert_eq!(
            session.turns_total(),
            2,
            "read-only calls ({read_only:?}) must not push turns"
        );
        let mirrored = rig.tool_call_payloads().await.len();
        assert_eq!(mirrored, 2 + read_only.len(), "L3 mirror is unaffected");

        // Writes after the reads still count.
        call("update", json!({"id": id, "tags": ["db"]})).await;
        call("switch_scope", json!({"scope": "other"})).await;
        call("forget", json!({"id": id})).await;
        assert_eq!(session.turns_total(), 5);
        let turns = session.turns_snapshot();
        let names: Vec<&str> = turns.iter().map(|t| t.content.as_str()).collect();
        assert_eq!(
            names,
            ["remember", "pin", "update", "switch_scope", "forget"]
        );
        assert!(turns.iter().all(|t| t.role == "tool"));

        sched.shutdown().await.unwrap();
    }

    #[test]
    fn redact_forgotten_leaves_unrelated_and_already_redacted_rows() {
        let at = chrono::Utc::now();
        let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let no = |p: Value| assert!(redact_forgotten(&p, id, Some("text"), at).is_none(), "{p}");
        // Not a writer, even though the id matches.
        no(json!({"tool": "forget", "id": id}));
        no(json!({"tool": "unpin", "id": id}));
        no(json!({"tool": "recall", "query": "text"}));
        // Another memory.
        no(json!({"tool": "update", "id": "01BX5ZZKBKACTAV9WEVGEMMVRZ", "content": "text"}));
        no(json!({"tool": "remember", "content": "other text"}));
        // Already done.
        no(json!({"tool": "remember", "memory_id": id, "redacted": "forgotten", "at": "x"}));
        // Not an object.
        no(json!("text"));
        // Without the forgotten text there is no legacy match.
        assert!(
            redact_forgotten(
                &json!({"tool": "remember", "content": "text"}),
                id,
                None,
                at
            )
            .is_none()
        );
        assert!(
            redact_forgotten(
                &json!({"tool": "pin", "content": " text "}),
                id,
                Some("text"),
                at
            )
            .is_some()
        );
    }

    /// Call `tool` under `secrets` with auto-logging on `all`, and
    /// return the recorded `tool_call` payloads.
    async fn tool_call_payloads(secrets: SecretsMode, tool: &str, args: Value) -> Vec<Value> {
        let rig = Rig::new(secrets, AutoLogToolCalls::All);
        let resp = rig.call(tool, args).await;
        assert!(resp.error.is_none(), "{:?}", resp.error);
        rig.tool_call_payloads().await
    }

    /// The JSON-RPC error of a call that must fail with `INVALID_PARAMS`.
    async fn invalid_params(rig: &Rig, tool: &str, args: Value) -> String {
        let err = rig.call(tool, args).await.error.expect("expected an error");
        assert_eq!(err.code, error_codes::INVALID_PARAMS, "{}", err.message);
        err.message
    }

    #[tokio::test]
    async fn remember_update_and_recall_accept_kind() {
        let rig = Rig::new(SecretsMode::Reject, AutoLogToolCalls::Writes);
        let resp = rig
            .call(
                "remember",
                json!({"content": "we deploy on Tuesdays", "kind": "decision"}),
            )
            .await;
        assert!(resp.error.is_none(), "{:?}", resp.error);
        let id = resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .and_then(|t| t.strip_prefix("stored memory "))
            .unwrap()
            .to_owned();

        let resp = rig
            .call("update", json!({"id": id, "kind": "preference"}))
            .await;
        assert!(resp.error.is_none(), "{:?}", resp.error);

        for (key, value, expect) in [
            ("kind", "preference", 1),
            ("type", "preference", 1),
            ("kind", "decision", 0),
        ] {
            let resp = rig
                .call("recall", json!({"query": "deploy Tuesdays", key: value}))
                .await;
            assert!(resp.error.is_none(), "{:?}", resp.error);
            let text = resp.result.unwrap()["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_owned();
            let rows: Vec<Value> = serde_json::from_str(&text).unwrap();
            assert_eq!(rows.len(), expect, "{key}={value}: {text}");
        }

        // The mirrored events name the field `kind`, whichever was passed.
        let events = rig.tool_call_payloads().await;
        assert!(events.iter().all(|e| e.get("type").is_none()), "{events:?}");
        assert_eq!(events[0]["kind"], "preference");
        assert_eq!(events[1]["kind"], "decision");
    }

    #[tokio::test]
    async fn kind_and_type_conflict_is_invalid_params_on_all_three_tools() {
        let rig = Rig::new(SecretsMode::Reject, AutoLogToolCalls::Writes);
        for (tool, args) in [
            (
                "remember",
                json!({"content": "x", "kind": "decision", "type": "fact"}),
            ),
            (
                "update",
                json!({"id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "kind": "decision", "type": "fact"}),
            ),
            (
                "recall",
                json!({"query": "x", "kind": "decision", "type": "fact"}),
            ),
        ] {
            let msg = invalid_params(&rig, tool, args).await;
            assert!(msg.contains("pass either kind or type"), "{tool}: {msg}");
        }
    }

    #[tokio::test]
    async fn recall_since_until_points_at_recall_recent() {
        let rig = Rig::new(SecretsMode::Reject, AutoLogToolCalls::Writes);
        let msg = invalid_params(
            &rig,
            "recall",
            json!({"query": "x", "since": "2026-09-01T00:00:00Z", "until": "2026-10-19T00:00:00Z"}),
        )
        .await;
        assert!(
            msg.contains("unexpected argument(s) `since`, `until`"),
            "{msg}"
        );
        assert_eq!(msg.matches("call `recall_recent`").count(), 1, "{msg}");

        // Any other stray key gets the plain error, without the hint.
        let msg = invalid_params(&rig, "recall", json!({"query": "x", "k": 3})).await;
        assert!(!msg.contains("recall_recent"), "{msg}");
    }

    #[tokio::test]
    async fn forget_no_longer_declares_query_or_scope() {
        let rig = Rig::new(SecretsMode::Reject, AutoLogToolCalls::Writes);
        let forget = rig
            .registry
            .list()
            .into_iter()
            .find(|d| d.name == "forget")
            .unwrap();
        let props = forget.input_schema["properties"].as_object().unwrap();
        assert_eq!(props.keys().collect::<Vec<_>>(), ["id"]);
        assert_eq!(forget.input_schema["required"], json!(["id"]));

        for key in ["query", "scope"] {
            let msg = invalid_params(&rig, "forget", json!({key: "x"})).await;
            assert!(
                msg.contains(&format!("unexpected argument(s) `{key}`")),
                "{msg}"
            );
            assert!(msg.contains("call `forget` once per `id`"), "{msg}");
        }
    }

    #[tokio::test]
    async fn remember_pinned_true_is_invalid_params_naming_pin() {
        let rig = Rig::new(SecretsMode::Reject, AutoLogToolCalls::Writes);
        let msg = invalid_params(
            &rig,
            "remember",
            json!({"content": "always run fmt", "pinned": true}),
        )
        .await;
        assert!(msg.contains("call `pin`"), "{msg}");
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
