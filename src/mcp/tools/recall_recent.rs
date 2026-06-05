//! `recall_recent` — Phase 4. Top-N most-recent episodic events,
//! optionally filtered by scope, kind, or a `[since, until)` time
//! window. Distinct from `recall` (semantic similarity); this surface
//! is for "what just happened?" questions.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use ulid::Ulid;

use super::{Tool, ToolDescriptor, ToolError, ToolResult};
use crate::memory::episodic::{EpisodicStore, RecentFilters};

const DESCRIPTION: &str = "Retrieve the most recent episodic events \
(tool calls, user messages, checkpoints) from this and earlier \
sessions. Use when the user asks 'what did we just do?' or you need \
to remind yourself of the immediate working context. Distinct from \
`recall`, which is for semantic match. \
\
Optional `since` / `until` bound the result to a `[since, until)` \
window against `created_at`. Both accept an RFC3339 timestamp \
(e.g. `2026-05-19T00:00:00Z`) or a 26-char ULID whose embedded \
millisecond timestamp is used. The server does not parse natural \
language — convert phrases like 'last Tuesday' to RFC3339 yourself \
before calling. When either bound is set, the `limit` cap rises to \
1000 because the result is already window-bounded.";

const DEFAULT_LIMIT: u64 = 20;
const MAX_LIMIT: u64 = 200;
const MAX_LIMIT_BOUNDED: u64 = 1000;

pub struct RecallRecent {
    store: Arc<EpisodicStore>,
}

impl RecallRecent {
    pub fn new(store: Arc<EpisodicStore>) -> Self {
        Self { store }
    }
}

/// Parse an RFC3339 timestamp or a 26-char ULID into a `DateTime<Utc>`.
/// ULIDs decode to their embedded millisecond timestamp — useful for
/// passing an existing event id straight through as a bound.
fn parse_time_bound(field: &str, raw: &str) -> Result<DateTime<Utc>, ToolError> {
    if raw.len() == 26
        && let Ok(ulid) = Ulid::from_string(raw)
    {
        let ms = ulid.timestamp_ms() as i64;
        return DateTime::<Utc>::from_timestamp_millis(ms).ok_or_else(|| {
            ToolError::InvalidArguments(format!(
                "`{field}` ULID `{raw}` decodes to an out-of-range timestamp"
            ))
        });
    }
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            ToolError::InvalidArguments(format!(
                "`{field}` must be RFC3339 or a 26-char ULID, got `{raw}`: {e}"
            ))
        })
}

#[async_trait]
impl Tool for RecallRecent {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "recall_recent",
            description: DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_LIMIT_BOUNDED,
                        "description": "Max events to return. Defaults to 20. Capped at 200 \
            when no time bound is set, 1000 when `since` or `until` is set."
                    },
                    "scope": { "type": "string", "description": "Optional scope filter." },
                    "kind": { "type": "string", "description": "Optional event-kind filter." },
                    "since": {
                        "type": "string",
                        "description": "Lower bound (inclusive) on `created_at`. RFC3339 \
            timestamp or 26-char ULID."
                    },
                    "until": {
                        "type": "string",
                        "description": "Upper bound (exclusive) on `created_at`. RFC3339 \
            timestamp or 26-char ULID."
                    }
                }
            }),
        }
    }

    async fn invoke(&self, args: Value) -> Result<ToolResult, ToolError> {
        let since = match args.get("since") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(parse_time_bound("since", s)?),
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`since` must be a string".into(),
                ));
            }
        };
        let until = match args.get("until") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(parse_time_bound("until", s)?),
            Some(_) => {
                return Err(ToolError::InvalidArguments(
                    "`until` must be a string".into(),
                ));
            }
        };
        if let (Some(s), Some(u)) = (since, until)
            && s >= u
        {
            return Err(ToolError::InvalidArguments(format!(
                "`since` ({s}) must be strictly before `until` ({u})"
            )));
        }

        let max_limit = if since.is_some() || until.is_some() {
            MAX_LIMIT_BOUNDED
        } else {
            MAX_LIMIT
        };
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
        if limit == 0 || limit > max_limit {
            return Err(ToolError::InvalidArguments(format!(
                "`limit` must be between 1 and {max_limit}"
            )));
        }

        let filters = RecentFilters {
            scope: args.get("scope").and_then(Value::as_str).map(String::from),
            kind: args.get("kind").and_then(Value::as_str).map(String::from),
            since,
            until,
        };
        let events = self
            .store
            .recall_recent(&filters, limit as usize)
            .await
            .map_err(|e| ToolError::Internal(format!("recall_recent failed: {e}")))?;

        let body: Vec<Value> = events
            .iter()
            .map(|e| {
                json!({
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
        let text = serde_json::to_string(&body)
            .map_err(|e| ToolError::Internal(format!("serialise events: {e}")))?;
        Ok(ToolResult::text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::ContentBlock;
    use crate::storage::Storage;
    use crate::storage::memory_impl::MemoryStorage;
    use tempfile::TempDir;

    fn store() -> (Arc<EpisodicStore>, TempDir) {
        let tmp = TempDir::new().unwrap();
        let backing: Arc<dyn Storage> = MemoryStorage::new();
        let store = Arc::new(EpisodicStore::new(backing));
        (store, tmp)
    }

    fn text(res: ToolResult) -> String {
        match &res.content[0] {
            ContentBlock::Text(t) => t.clone(),
        }
    }

    #[tokio::test]
    async fn default_limit_is_20() {
        let (s, _tmp) = store();
        for i in 0..25 {
            s.record(&format!("k{i}"), "g", "\"x\"").await.unwrap();
        }
        let tool = RecallRecent::new(s);
        let res = tool.invoke(json!({})).await.unwrap();
        let v: Vec<Value> = serde_json::from_str(&text(res)).unwrap();
        assert_eq!(v.len(), 20);
    }

    #[tokio::test]
    async fn custom_limit_is_accepted() {
        let (s, _tmp) = store();
        for i in 0..10 {
            s.record(&format!("k{i}"), "g", "\"x\"").await.unwrap();
        }
        let tool = RecallRecent::new(s);
        let res = tool.invoke(json!({"limit": 5})).await.unwrap();
        let v: Vec<Value> = serde_json::from_str(&text(res)).unwrap();
        assert_eq!(v.len(), 5);
    }

    #[tokio::test]
    async fn scope_filter_propagates() {
        let (s, _tmp) = store();
        s.record("k1", "work", "\"a\"").await.unwrap();
        s.record("k2", "personal", "\"b\"").await.unwrap();
        let tool = RecallRecent::new(s);
        let res = tool.invoke(json!({"scope": "work"})).await.unwrap();
        let v: Vec<Value> = serde_json::from_str(&text(res)).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["kind"], "k1");
    }

    #[tokio::test]
    async fn kind_filter_propagates() {
        let (s, _tmp) = store();
        s.record("msg", "g", "\"a\"").await.unwrap();
        s.record("tool_call", "g", "\"b\"").await.unwrap();
        let tool = RecallRecent::new(s);
        let res = tool.invoke(json!({"kind": "msg"})).await.unwrap();
        let v: Vec<Value> = serde_json::from_str(&text(res)).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["kind"], "msg");
    }

    #[tokio::test]
    async fn empty_store_returns_empty_array() {
        let (s, _tmp) = store();
        let tool = RecallRecent::new(s);
        let res = tool.invoke(json!({})).await.unwrap();
        let v: Vec<Value> = serde_json::from_str(&text(res)).unwrap();
        assert!(v.is_empty());
    }

    #[tokio::test]
    async fn limit_zero_returns_invalid_arguments() {
        let (s, _tmp) = store();
        let tool = RecallRecent::new(s);
        let err = tool.invoke(json!({"limit": 0})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn limit_over_max_returns_invalid_arguments() {
        let (s, _tmp) = store();
        let tool = RecallRecent::new(s);
        let err = tool.invoke(json!({"limit": 201})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn invalid_limit_type_returns_invalid_arguments() {
        let (s, _tmp) = store();
        let tool = RecallRecent::new(s);
        let err = tool
            .invoke(json!({"limit": "not-a-number"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    // ---------- Time-range tests ----------

    /// Write three events, then sleep a hair between groups so their
    /// `created_at` timestamps are visibly distinct.
    async fn populate_three_in_time(s: &Arc<EpisodicStore>) -> (DateTime<Utc>, DateTime<Utc>) {
        s.record("a", "g", "\"a\"").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        let mid = Utc::now();
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        s.record("b", "g", "\"b\"").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        s.record("c", "g", "\"c\"").await.unwrap();
        let after = Utc::now() + chrono::Duration::milliseconds(10);
        (mid, after)
    }

    #[tokio::test]
    async fn since_excludes_earlier_events() {
        let (s, _tmp) = store();
        let (mid, _) = populate_three_in_time(&s).await;
        let tool = RecallRecent::new(Arc::clone(&s));
        let res = tool
            .invoke(json!({"since": mid.to_rfc3339()}))
            .await
            .unwrap();
        let v: Vec<Value> = serde_json::from_str(&text(res)).unwrap();
        let kinds: Vec<&str> = v.iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["c", "b"]);
    }

    #[tokio::test]
    async fn until_excludes_later_events() {
        let (s, _tmp) = store();
        let (mid, _) = populate_three_in_time(&s).await;
        let tool = RecallRecent::new(Arc::clone(&s));
        let res = tool
            .invoke(json!({"until": mid.to_rfc3339()}))
            .await
            .unwrap();
        let v: Vec<Value> = serde_json::from_str(&text(res)).unwrap();
        let kinds: Vec<&str> = v.iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["a"]);
    }

    #[tokio::test]
    async fn since_and_until_bound_the_window() {
        let (s, _tmp) = store();
        let (mid, after) = populate_three_in_time(&s).await;
        let tool = RecallRecent::new(Arc::clone(&s));
        let res = tool
            .invoke(json!({"since": mid.to_rfc3339(), "until": after.to_rfc3339()}))
            .await
            .unwrap();
        let v: Vec<Value> = serde_json::from_str(&text(res)).unwrap();
        let kinds: Vec<&str> = v.iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["c", "b"]);
    }

    #[tokio::test]
    async fn ulid_string_accepted_as_time_bound() {
        let (s, _tmp) = store();
        let earlier = s.record("earlier", "g", "\"a\"").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        s.record("later", "g", "\"b\"").await.unwrap();
        let tool = RecallRecent::new(Arc::clone(&s));
        // `earlier.id.to_string()` is a 26-char ULID; its embedded
        // ms-timestamp == event "earlier"'s creation moment to within
        // sub-millisecond drift (id is minted right before created_at
        // is read from `Utc::now()`). `until = earlier_id` is the
        // tighter assertion: "later" was recorded 30ms after, so it's
        // unambiguously outside the upper bound. Only "earlier" can
        // survive a `[anything, earlier]` window.
        let res = tool
            .invoke(json!({"until": earlier.to_string()}))
            .await
            .unwrap();
        let v: Vec<Value> = serde_json::from_str(&text(res)).unwrap();
        let kinds: Vec<&str> = v.iter().map(|e| e["kind"].as_str().unwrap()).collect();
        // "later" must be excluded; "earlier" may be in or out
        // depending on the sub-ms alignment between ULID mint and
        // Utc::now(), but the filter must not return "later".
        assert!(!kinds.contains(&"later"), "unexpected kinds: {kinds:?}");
    }

    #[tokio::test]
    async fn since_after_until_rejected() {
        let (s, _tmp) = store();
        let tool = RecallRecent::new(s);
        let err = tool
            .invoke(json!({
                "since": "2026-05-23T00:00:00Z",
                "until": "2026-05-20T00:00:00Z"
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn since_equal_to_until_rejected() {
        let (s, _tmp) = store();
        let tool = RecallRecent::new(s);
        let err = tool
            .invoke(json!({
                "since": "2026-05-23T00:00:00Z",
                "until": "2026-05-23T00:00:00Z"
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn malformed_since_rejected() {
        let (s, _tmp) = store();
        let tool = RecallRecent::new(s);
        let err = tool
            .invoke(json!({"since": "yesterday"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn since_wrong_type_rejected() {
        let (s, _tmp) = store();
        let tool = RecallRecent::new(s);
        let err = tool.invoke(json!({"since": 12345})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn bounded_query_allows_higher_limit() {
        let (s, _tmp) = store();
        let tool = RecallRecent::new(s);
        // 500 > MAX_LIMIT (200) but ≤ MAX_LIMIT_BOUNDED (1000); valid
        // only because `since` is set.
        tool.invoke(json!({
            "since": "2026-01-01T00:00:00Z",
            "limit": 500
        }))
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn bounded_query_rejects_over_bounded_max() {
        let (s, _tmp) = store();
        let tool = RecallRecent::new(s);
        let err = tool
            .invoke(json!({
                "since": "2026-01-01T00:00:00Z",
                "limit": 1001
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn unbounded_query_rejects_over_200() {
        let (s, _tmp) = store();
        let tool = RecallRecent::new(s);
        let err = tool.invoke(json!({"limit": 500})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }
}
