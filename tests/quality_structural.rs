//! Structural quality checks: properties of recall and auto-context
//! that must hold whatever the embedding model is.
//!
//! The recall-quality evaluation that motivated 1.5.2 and 1.6 ran
//! against a private store with a real model. These tests encode the
//! *shape* of what it found (a scoped read lost the global rules, events
//! crowded out long-term memories, warm events were unreachable, `forget`
//! left the text behind, sessions shared working memory, `kind` was
//! rejected, reads counted as conversation) on a synthetic corpus with
//! the stub embedder, so they run in `cargo test` on every PR with no
//! model and no network. Ranking quality proper (hit@k on real questions)
//! needs a model and belongs in the nightly job.
//!
//! Everything goes through MCP frames (`tools/call`, `resources/read`)
//! against an in-process session wired like the daemon's.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Value, json};

use common::corpus::{self, Corpus};
use common::{Harness, assert_no_trace, estimate_tokens};

/// Ids of the rows in a JSON array of objects.
fn ids(rows: &Value) -> BTreeSet<String> {
    rows.as_array()
        .unwrap_or_else(|| panic!("expected an array, got {rows}"))
        .iter()
        .map(|r| r["id"].as_str().expect("row id").to_owned())
        .collect()
}

fn contents(rows: &Value, field: &str) -> Vec<String> {
    rows.as_array()
        .unwrap_or_else(|| panic!("expected an array, got {rows}"))
        .iter()
        .map(|r| r[field].to_string())
        .collect()
}

// ---------------------------------------------------------------------
// The corpus itself
// ---------------------------------------------------------------------

#[tokio::test]
async fn corpus_has_the_documented_shape() {
    let h = Harness::new();
    let c = h.seed().await;

    assert_eq!(c.facts.len(), 300);
    for scope in corpus::SCOPES {
        assert_eq!(c.facts_in(scope).count(), 50, "{scope}");
    }
    assert_eq!(c.id_heavy().count(), 60);
    assert_eq!(c.pairs.len(), 10);
    assert_eq!(c.curated.len(), 20);
    assert_eq!(c.warm.len(), 60);
    assert_eq!(c.pins.len(), 6);
    assert_eq!(c.pins_in("global").count(), 2);

    // The stores agree with the manifest.
    let client = h.connect().await;
    let stats = client.json("stats", json!({})).await;
    assert_eq!(stats["memories"]["semantic"], c.semantic_rows(), "{stats}");
    assert_eq!(stats["memories"]["procedural"], 6, "{stats}");
    assert_eq!(stats["memories"]["episodic"]["hot"], 20, "{stats}");
    assert_eq!(stats["memories"]["episodic"]["warm"], 60, "{stats}");

    // Every "unique" token really is unique across the corpus text.
    let all = c.fingerprint().join("\n");
    let tokens = c
        .id_heavy()
        .filter_map(|f| f.exact_token.clone())
        .chain(c.curated.iter().map(|e| e.answer_token.clone()))
        .chain(c.warm.iter().map(|w| w.token.clone()));
    for token in tokens {
        assert_eq!(all.matches(&token).count(), 1, "`{token}` is not unique");
    }
    for pair in &c.pairs {
        assert_eq!(all.matches(&pair.subject).count(), 2, "{}", pair.subject);
    }
    // L3-only answers are in no L4 row.
    for event in &c.curated {
        assert!(
            !c.facts
                .iter()
                .any(|f| f.content.contains(&event.answer_token)),
            "{} leaked into L4",
            event.answer_token
        );
    }
}

#[tokio::test]
async fn corpus_is_deterministic() {
    let (a, b) = (Harness::new(), Harness::new());
    let (ca, cb) = (a.seed().await, b.seed().await);
    assert_eq!(ca.fingerprint(), cb.fingerprint());
    assert_eq!(ca.fingerprint().len(), 6 + 300 + 10 + 60 + 20);
}

// ---------------------------------------------------------------------
// 0 argument errors on canonical calls
// ---------------------------------------------------------------------

/// One well-formed call per tool, in the spellings agents actually use
/// (`kind` as well as `type`). None may fail, and none may leave a
/// `tool_call_failed` event. In the evaluation 9 of 116 `remember` calls
/// failed on `kind`.
#[tokio::test]
async fn canonical_calls_have_no_argument_errors() {
    let h = Harness::new();
    let c = h.seed().await;
    let client = h.connect().await;

    let stored = client
        .ok(
            "remember",
            json!({"content": "the canonical call suite ran", "kind": "decision",
                   "tags": ["suite"], "scope": "ops"}),
        )
        .await;
    let id = stored["_meta"]["id"].as_str().expect("_meta.id").to_owned();
    let pinned = client
        .ok(
            "pin",
            json!({"content": "suite rule", "tags": ["suite"], "scope": "ops"}),
        )
        .await;
    let pin_id = pinned["_meta"]["id"].as_str().expect("_meta.id").to_owned();
    let event = client
        .text(
            "record_event",
            json!({"kind": "decision", "scope": "ops",
                   "payload": {"content": "the suite records an event"}}),
        )
        .await;
    assert!(!event.is_empty());

    let calls = [
        (
            "remember",
            json!({"content": "typed with the old name", "type": "preference"}),
        ),
        (
            "remember",
            json!({"content": "replaces the first", "kind": "fact", "supersedes": id}),
        ),
        (
            "update",
            json!({"id": id, "kind": "fact", "tags": ["suite", "edited"]}),
        ),
        ("update", json!({"id": id, "type": "decision"})),
        (
            "update",
            json!({"id": id, "content": "the canonical call suite ran twice"}),
        ),
        ("recall", json!({"query": "scheduler retry limit"})),
        (
            "recall",
            json!({"query": "scheduler", "kind": "fact", "scope": "ops", "limit": 5}),
        ),
        (
            "recall",
            json!({"query": "scheduler", "type": "decision", "tags": ["scheduler"]}),
        ),
        (
            "recall",
            json!({"query": "ops_handler_45.rs", "mode": "keyword"}),
        ),
        (
            "recall",
            json!({"query": "gateway", "mode": "semantic", "min_similarity": 0.1,
                          "include_superseded": true}),
        ),
        ("recall_recent", json!({})),
        (
            "recall_recent",
            json!({"limit": 5, "kind": "decision", "scope": "ops"}),
        ),
        (
            "recall_recent",
            json!({"since": "2020-01-01T00:00:00Z", "until": "2100-01-01T00:00:00Z",
                                 "include_warm": true, "limit": 500}),
        ),
        ("summarize_session", json!({})),
        ("stats", json!({})),
        ("list_scopes", json!({})),
        ("export", json!({})),
        (
            "export",
            json!({"scope": "ops", "limit_per_layer": 10, "include_warm": true}),
        ),
        ("switch_scope", json!({"scope": "research"})),
        ("unpin", json!({"id": pin_id})),
        ("forget", json!({"id": id})),
        ("forget", json!({"id": c.curated[0].id.to_string()})),
    ];
    for (tool, args) in calls {
        client.ok(tool, args).await;
    }

    let kinds = h.event_kinds().await;
    assert!(
        !kinds.iter().any(|k| k == "tool_call_failed"),
        "a canonical call failed: {kinds:?}"
    );

    // The suite covers every tool the server lists.
    let listed = client.request("tools/list", json!({})).await;
    let names: BTreeSet<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    let covered: BTreeSet<&str> = [
        "remember",
        "pin",
        "record_event",
        "update",
        "recall",
        "recall_recent",
        "summarize_session",
        "stats",
        "list_scopes",
        "export",
        "switch_scope",
        "unpin",
        "forget",
    ]
    .into();
    assert_eq!(
        names, covered,
        "a tool was added without a canonical call here"
    );
}

// ---------------------------------------------------------------------
// Warm tier reachable
// ---------------------------------------------------------------------

/// Events consolidation moved to the warm tier are still readable, by a
/// time window and by `export`. In the evaluation a seven-week window
/// returned 7 hot rows with 344 warm rows present.
#[tokio::test]
async fn warm_events_are_reachable_by_recall_recent_and_export() {
    let h = Harness::new();
    let c = h.seed().await;
    let client = h.connect().await;
    let warm_ids: BTreeSet<String> = c.warm.iter().map(|w| w.id.to_string()).collect();
    let hot_ids: BTreeSet<String> = c.curated.iter().map(|e| e.id.to_string()).collect();

    // A window that covers them, by timestamp and by ULID.
    let window = json!({"since": "2020-01-01T00:00:00Z", "until": "2100-01-01T00:00:00Z",
                        "include_warm": true, "limit": 1000});
    let rows = client.json("recall_recent", window).await;
    let got = ids(&rows);
    assert!(
        warm_ids.is_subset(&got),
        "warm rows missing from the window"
    );
    assert!(hot_ids.is_subset(&got), "hot rows missing from the window");
    let warm_rows = rows
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["tier"] == "warm")
        .count();
    assert_eq!(warm_rows, 60);

    // `until` alone means "older than": warm is read without asking.
    let first_hot = c.curated[0].id.to_string();
    let rows = client
        .json("recall_recent", json!({"until": first_hot, "limit": 1000}))
        .await;
    assert_eq!(
        ids(&rows),
        warm_ids,
        "everything before the first hot event is warm"
    );

    // Scope and kind filters apply to warm rows too.
    let rows = client
        .json(
            "recall_recent",
            json!({"until": first_hot, "limit": 1000, "kind": "decision", "scope": "trading"}),
        )
        .await;
    let expected: BTreeSet<String> = c
        .warm
        .iter()
        .filter(|w| w.kind == "decision" && w.scope == "trading")
        .map(|w| w.id.to_string())
        .collect();
    assert!(!expected.is_empty());
    assert_eq!(ids(&rows), expected);

    // Without a time bound the default view stays the hot tier.
    let rows = client.json("recall_recent", json!({"limit": 200})).await;
    assert_eq!(ids(&rows), hot_ids);

    // Export carries both tiers.
    let export = client
        .json("export", json!({"limit_per_layer": 5000}))
        .await;
    let exported = ids(&export["episodic"]);
    assert!(
        warm_ids.is_subset(&exported),
        "warm rows missing from export"
    );
    assert!(hot_ids.is_subset(&exported), "hot rows missing from export");
    assert_eq!(ids(&export["semantic"]).len(), c.semantic_rows());
    assert_eq!(ids(&export["procedural"]).len(), c.pins.len());
}

// ---------------------------------------------------------------------
// Redaction on forget
// ---------------------------------------------------------------------

/// `forget` removes the memory and every copy of its text that the
/// write tools mirrored to the timeline, in the hot and the warm tier.
/// In the evaluation 7 of 7 forgotten memories still had their full
/// content in `tool_call` rows.
#[tokio::test]
async fn forget_leaves_no_copy_of_the_text_on_the_timeline() {
    let h = Harness::new();
    h.seed().await;
    let client = h.connect().await;
    let first = "Mira's locker combination is 31-07-44 at the Riverside gym";
    let second = "Mira's locker combination changed to 08-19-62, same gym";

    let stored = client
        .ok(
            "remember",
            json!({"content": first, "kind": "fact", "scope": "personal"}),
        )
        .await;
    let id = stored["_meta"]["id"].as_str().unwrap().to_owned();
    // The `remember` row ages into the warm tier before the edit.
    for event in h.episodic.list_all().await.unwrap() {
        if event.kind == "tool_call" {
            assert!(h.episodic.promote_to_warm(event.id).await.unwrap());
        }
    }
    client
        .ok("update", json!({"id": id, "content": second}))
        .await;

    let timeline = || async {
        client
            .text(
                "recall_recent",
                json!({"kind": "tool_call", "include_warm": true, "limit": 200}),
            )
            .await
    };
    let before = timeline().await;
    assert!(
        before.contains("31-07-44") && before.contains("08-19-62"),
        "{before}"
    );

    let result = client.ok("forget", json!({"id": id})).await;
    assert_eq!(result["_meta"]["redacted_tool_calls"], 2, "{result}");

    let after = timeline().await;
    let export = client
        .text("export", json!({"limit_per_layer": 5000}))
        .await;
    let context = client
        .read_text("mneme://context?q=locker%20combination&kinds=tool_call")
        .await;
    for content in [first, second] {
        assert_no_trace(&after, content);
        assert_no_trace(&export, content);
        assert_no_trace(&context, content);
    }
    assert_eq!(
        after.matches("\\\"redacted\\\":\\\"forgotten\\\"").count(),
        2,
        "{after}"
    );
    let hits = client
        .json("recall", json!({"query": "locker combination"}))
        .await;
    assert!(!ids(&hits).contains(&id));
}

// ---------------------------------------------------------------------
// L1: isolation and what counts as a turn
// ---------------------------------------------------------------------

/// Working memory belongs to one MCP session. Checked through the real
/// HTTP router: two `Mcp-Session-Id`s against one server.
#[tokio::test]
async fn working_turns_are_isolated_across_http_sessions() {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use mneme::mcp::transport::http::{
        DEFAULT_SESSION_IDLE_TTL, HttpOptions, MCP_PATH, SESSION_HEADER, TokenSource, router,
    };
    use tower::ServiceExt;

    const TOKEN: &str = "structural-token";
    let h = Arc::new(Harness::new());
    h.seed().await;
    let app = router(
        &HttpOptions {
            bind: "127.0.0.1:0".parse().unwrap(),
            token: TokenSource::Static(TOKEN.into()),
            allowed_origins: vec![],
            max_sessions: 8,
            session_idle_ttl: DEFAULT_SESSION_IDLE_TTL,
        },
        h.http_session_factory(),
    );

    // POST one JSON-RPC message; returns (status, session header, body).
    let post = |session: Option<String>, body: Value| {
        let app = app.clone();
        async move {
            let mut req = Request::builder()
                .method(Method::POST)
                .uri(MCP_PATH)
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream");
            if let Some(s) = &session {
                req = req.header(SESSION_HEADER, s);
            }
            let resp = app
                .oneshot(req.body(Body::from(body.to_string())).unwrap())
                .await
                .unwrap();
            let status = resp.status();
            let sid = resp
                .headers()
                .get(SESSION_HEADER)
                .map(|v| v.to_str().unwrap().to_owned());
            let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            (status, sid, body)
        }
    };
    let open = || async {
        let (status, sid, _) = post(
            None,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "structural", "version": "0"}}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let sid = sid.expect("Mcp-Session-Id");
        let (status, _, _) = post(
            Some(sid.clone()),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        sid
    };
    let tool = |sid: String, name: &'static str, args: Value| async move {
        let (status, _, body) = post(
            Some(sid),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                   "params": {"name": name, "arguments": args}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["isError"], false, "{name}: {body}");
        body["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let working = |sid: String| async move {
        let (_, _, body) = post(
            Some(sid),
            json!({"jsonrpc": "2.0", "id": 3, "method": "resources/read",
                   "params": {"uri": "mneme://context"}}),
        )
        .await;
        let text = body["result"]["contents"][0]["text"].as_str().unwrap();
        serde_json::from_str::<Value>(text).unwrap()["working"].clone()
    };
    let turns_total = |sid: String| async move {
        let stats: Value = serde_json::from_str(&tool(sid, "stats", json!({})).await).unwrap();
        (
            stats["working"]["session_id"].as_str().unwrap().to_owned(),
            stats["working"]["turns_total"].as_u64().unwrap(),
        )
    };

    let (a, b) = (open().await, open().await);
    let said = "which tulip varieties did we order for the north bed";
    tool(
        a.clone(),
        "record_event",
        json!({"kind": "user_message", "payload": {"content": said}}),
    )
    .await;
    tool(
        a.clone(),
        "remember",
        json!({"content": "the north bed gets Queen of Night"}),
    )
    .await;

    let (session_a, turns_a) = turns_total(a.clone()).await;
    let (session_b, turns_b) = turns_total(b.clone()).await;
    assert_ne!(
        session_a, session_b,
        "one working session per Mcp-Session-Id"
    );
    assert_eq!(turns_a, 3, "message turn + two tool turns");
    assert_eq!(turns_b, 0, "A's turns leaked into B");

    let working_a = working(a.clone()).await;
    assert!(working_a.to_string().contains(said), "{working_a}");
    assert_eq!(working(b.clone()).await, json!([]), "A's turns served to B");

    // And the other way round.
    tool(
        b.clone(),
        "pin",
        json!({"content": "water the north bed on Mondays"}),
    )
    .await;
    let working_b = working(b.clone()).await;
    assert_eq!(working_b.as_array().unwrap().len(), 1, "{working_b}");
    assert!(!working_b.to_string().contains("tulip"));
    assert_eq!(working(a.clone()).await, working_a);

    // Stored memories are shared; only L1 is per session.
    let hits = tool(
        b,
        "recall",
        json!({"query": "Queen of Night", "mode": "keyword"}),
    )
    .await;
    assert!(hits.contains("Queen of Night"), "{hits}");
}

/// Reading is not conversation: a read-only run over the whole corpus
/// adds no working turn and writes nothing. In the evaluation, which
/// only read, `turns_total` went from 361 to 1026.
#[tokio::test]
async fn read_only_calls_add_no_turns_and_write_nothing() {
    // An evaluation run turns `touch_on_recall` off so that reads cannot
    // refresh `last_accessed`.
    let mut config = mneme::config::Config::default();
    config.episodic.touch_on_recall = false;
    let h = Harness::with_config(config);
    let c = h.seed().await;
    let client = h.connect().await;

    let counts = |stats: &Value| stats["memories"].clone();
    let before = client.json("stats", json!({})).await;
    assert_eq!(before["working"]["turns_total"], 0);
    let kinds_before = h.event_kinds().await;

    let mut calls = 0;
    for scope in corpus::SCOPES {
        for mode in ["hybrid", "semantic", "keyword"] {
            client
                .json(
                    "recall",
                    json!({"query": "scheduler retry limit", "scope": scope, "mode": mode}),
                )
                .await;
            calls += 1;
        }
        client
            .json("recall_recent", json!({"scope": scope, "limit": 50}))
            .await;
        client.json("export", json!({"scope": scope})).await;
        client
            .read(&format!(
                "mneme://context?q=gateway%20timeout&scope={scope}"
            ))
            .await;
        calls += 3;
    }
    for fact in c.id_heavy().take(10) {
        let token = fact.exact_token.as_deref().unwrap();
        client
            .json("recall", json!({"query": token, "mode": "keyword"}))
            .await;
        calls += 1;
    }
    for (tool, args) in [
        ("stats", json!({})),
        ("list_scopes", json!({})),
        ("summarize_session", json!({})),
        (
            "recall_recent",
            json!({"since": "2020-01-01T00:00:00Z", "limit": 1000}),
        ),
    ] {
        client.ok(tool, args).await;
        calls += 1;
    }
    for uri in [
        "mneme://stats",
        "mneme://procedural",
        "mneme://recent",
        "mneme://context",
    ] {
        client.read_text(uri).await;
        calls += 1;
    }
    assert!(
        calls >= 50,
        "the run should be a real workload, got {calls} calls"
    );

    let after = client.json("stats", json!({})).await;
    assert_eq!(
        after["working"]["turns_total"], 0,
        "reads pushed working turns"
    );
    assert_eq!(client.working.turns_total(), 0);
    assert_eq!(
        counts(&after),
        counts(&before),
        "a read-only run changed the store"
    );
    assert_eq!(
        h.event_kinds().await,
        kinds_before,
        "a read-only run wrote events"
    );

    // A session that only read leaves no snapshot behind.
    let snapshot = client.working.snapshot_path();
    client.close().await;
    assert!(!snapshot.exists());
}

// ---------------------------------------------------------------------
// Deterministic context
// ---------------------------------------------------------------------

/// The same read gives the same context: byte for byte on one store,
/// and the same rows in the same order on two stores seeded alike.
#[tokio::test]
async fn auto_context_is_deterministic() {
    let uris = [
        "mneme://context",
        "mneme://context?scope=trading",
        "mneme://context?q=scheduler%20retry%20limit",
        "mneme://context?q=gateway%20timeout&scope=ops",
        "mneme://context?q=ops_handler_45.rs&limit=5",
    ];

    let h = Harness::new();
    h.seed().await;
    let client = h.connect().await;
    for uri in uris {
        let first = client.read_text(uri).await;
        for _ in 0..3 {
            assert_eq!(
                client.read_text(uri).await,
                first,
                "{uri} changed between reads"
            );
        }
        let body: Value = serde_json::from_str(&first).unwrap();
        assert!(
            body["total_tokens"].as_u64().unwrap() <= body["max_tokens"].as_u64().unwrap(),
            "{uri}: over budget"
        );
    }

    // A second store with the same corpus picks the same rows. Ids and
    // timestamps differ between stores, so compare what was chosen.
    let other = Harness::new();
    other.seed().await;
    let other_client = other.connect().await;
    for uri in uris {
        let (a, b) = (client.read(uri).await, other_client.read(uri).await);
        assert_eq!(
            contents(&a["procedural"], "content"),
            contents(&b["procedural"], "content"),
            "{uri}"
        );
        assert_eq!(
            contents(&a["episodic"], "payload"),
            contents(&b["episodic"], "payload"),
            "{uri}"
        );
        // Semantic rows: the stub embedder maps many texts to the same
        // vector (it keys on first byte and length), and exact ties are
        // broken by id, which differs between stores. The number of rows
        // and their similarities must still agree.
        assert_eq!(
            contents(&a["semantic"], "similarity"),
            contents(&b["semantic"], "similarity"),
            "{uri}"
        );
    }
}

// ---------------------------------------------------------------------
// Waiting on the auto-context rework (W1)
// ---------------------------------------------------------------------

fn pin_contents(corpus: &Corpus, scope: &str) -> BTreeSet<String> {
    corpus.pins_in(scope).map(|p| p.content.clone()).collect()
}

/// A scoped read keeps the global rules. In the evaluation `scope=x`
/// took the procedural section from 6 pins to 1 or 0.
#[tokio::test]
async fn scoped_context_keeps_the_global_pins() {
    let h = Harness::new();
    let c = h.seed().await;
    let client = h.connect().await;
    let global = pin_contents(&c, "global");
    assert_eq!(global.len(), 2);

    for scope in ["trading", "ops", "research", "hermes"] {
        let own = pin_contents(&c, scope);
        let expected: BTreeSet<String> = global.union(&own).cloned().collect();
        for uri in [
            // `[auto_context] pin_scopes = ["global"]` adds them...
            format!("mneme://context?scope={scope}"),
            // ...and asking for both scopes explicitly does too.
            format!("mneme://context?scope={scope},global"),
            format!("mneme://context?q=scheduler&scope={scope}"),
        ] {
            let body = client.read(&uri).await;
            let got: BTreeSet<String> = body["procedural"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["content"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(got, expected, "{uri}");
        }
    }
    // `personal` has no pin of its own: the two global ones remain.
    let body = client.read("mneme://context?scope=personal").await;
    assert_eq!(body["procedural"].as_array().unwrap().len(), 2);
    // Unscoped: all six.
    let body = client.read("mneme://context").await;
    assert_eq!(body["procedural"].as_array().unwrap().len(), 6);
}

/// Each layer stays within its share of the budget, so events cannot
/// starve long-term memories. In the evaluation L3 was charged 77% of
/// the budget and two semantic rows fitted.
#[tokio::test]
async fn context_layers_stay_within_their_shares() {
    let h = Harness::new();
    let c = h.seed().await;
    let client = h.connect().await;
    // Some conversation, so L1 has something to cap.
    for i in 0..30 {
        client
            .ok(
                "record_event",
                json!({"kind": "user_message",
                       "payload": {"content": format!("turn {i}: what is the scheduler retry limit in ops?")}}),
            )
            .await;
    }
    let cfg = &h.config.auto_context;
    // One row of slack: packing is by whole rows.
    let slack = 0.03;

    for uri in [
        "mneme://context?q=scheduler%20retry%20limit".to_owned(),
        "mneme://context?q=gateway%20timeout&scope=ops".to_owned(),
        format!("mneme://context?q={}", c.curated[0].answer_token),
    ] {
        let body = client.read(&uri).await;
        let max = body["max_tokens"].as_u64().unwrap() as f32;
        let tokens = |section: &str, field: &str| -> f32 {
            body[section]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| estimate_tokens(r[field].as_str().unwrap_or_default()) as f32)
                .sum()
        };
        let episodic = tokens("episodic", "payload") / max;
        let working = tokens("working", "content") / max;
        assert!(
            episodic <= cfg.episodic_max_share + slack,
            "{uri}: episodic takes {episodic:.2} of the budget"
        );
        assert!(
            working <= cfg.working_max_share + slack,
            "{uri}: working takes {working:.2} of the budget"
        );
        // Events are clipped, and no single kind fills the section.
        let events = body["episodic"].as_array().unwrap();
        for e in events {
            let len = e["payload"].as_str().unwrap_or_default().chars().count();
            assert!(
                len <= cfg.event_clip_chars + 16,
                "{uri}: unclipped event ({len} chars)"
            );
        }
        for kind in corpus::CURATED_KINDS {
            let n = events.iter().filter(|e| e["kind"] == kind).count();
            assert!(n <= cfg.max_events_per_kind, "{uri}: {n} `{kind}` events");
        }
        // With a query and 320 memories to draw on, the reserved
        // semantic share is used.
        let semantic = body["semantic"].as_array().unwrap().len();
        assert!(
            semantic >= cfg.semantic_min_items.min(5),
            "{uri}: only {semantic} semantic rows"
        );
        assert!(body["total_tokens"].as_u64().unwrap() <= body["max_tokens"].as_u64().unwrap());
    }
}

/// An answer that exists only on the timeline is found by a query for
/// it, in auto-context and in `recall` when asked for the episodic
/// layer, whether the event is hot or warm.
#[tokio::test]
async fn l3_only_answers_are_found_by_query() {
    let h = Harness::new();
    let c = h.seed().await;
    let client = h.connect().await;

    for event in c.curated.iter().step_by(4) {
        let body = client
            .read(&format!("mneme://context?q={}", event.answer_token))
            .await;
        assert!(
            ids(&body["episodic"]).contains(&event.id.to_string()),
            "context for `{}` lacks the event that answers it",
            event.answer_token
        );
        let rows = client
            .json(
                "recall",
                // Episodic layer only: the stub embedder puts unrelated
                // memories at a cosine near 1, which no event rank can
                // beat, so a mixed list here would measure the stub.
                json!({"query": event.answer_token, "layers": ["episodic"], "limit": 5}),
            )
            .await;
        assert!(
            ids(&rows).contains(&event.id.to_string()),
            "recall(layers) for `{}` lacks the event",
            event.answer_token
        );
    }
    // Warm curated events are indexed too.
    let warm = c.warm.iter().find(|w| w.kind == "decision").unwrap();
    let rows = client
        .json(
            "recall",
            json!({"query": warm.token, "layers": ["episodic"], "limit": 5}),
        )
        .await;
    assert!(
        ids(&rows).contains(&warm.id.to_string()),
        "warm event not found by its token"
    );
}

/// Of two memories about one subject, the one edited later comes first
/// in auto-context.
#[tokio::test]
async fn current_memory_outranks_the_stale_one_in_context() {
    let h = Harness::new();
    let c = h.seed().await;
    let client = h.connect().await;
    for pair in &c.pairs {
        let body = client
            .read(&format!(
                "mneme://context?q={}%20balance&scope={}",
                pair.subject, pair.scope
            ))
            .await;
        let order: Vec<&str> = body["semantic"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        let pos = |id: String| order.iter().position(|x| *x == id);
        let (cur, stale) = (pos(pair.current.to_string()), pos(pair.stale.to_string()));
        assert!(
            cur.is_some(),
            "{}: current memory missing from context",
            pair.subject
        );
        if let Some(stale) = stale {
            assert!(
                cur.unwrap() < stale,
                "{}: stale ranks above current",
                pair.subject
            );
        }
    }
}
