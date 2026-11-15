//! Relations and provenance through the tool surface: what an agent
//! sends and what it gets back. The storage and ranking rules have
//! their own tests in `memory::semantic` and `memory::relations`.

use std::sync::Arc;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::export::Export;
use super::recall::Recall;
use super::record_event::RecordEvent;
use super::remember::Remember;
use super::stats::Stats;
use super::update::Update;
use super::{ContentBlock, Tool, ToolError, ToolResult};
use crate::embed::Embedder;
use crate::embed::stub::StubEmbedder;
use crate::ids::MemoryId;
use crate::memory::episodic::EpisodicStore;
use crate::memory::procedural::ProceduralStore;
use crate::memory::relations::{RelationTarget, RelationType};
use crate::memory::semantic::SemanticStore;
use crate::scope::ScopeState;
use crate::storage::Storage;
use crate::storage::archive::ColdArchive;
use crate::storage::memory_impl::MemoryStorage;

struct Fx {
    semantic: Arc<SemanticStore>,
    remember: Remember,
    update: Update,
    recall: Recall,
    record_event: RecordEvent,
    stats: Stats,
    export: Export,
    _tmp: TempDir,
}

fn fx() -> Fx {
    let tmp = TempDir::new().unwrap();
    let storage: Arc<dyn Storage> = MemoryStorage::new();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
    let semantic =
        SemanticStore::open_disabled(tmp.path(), Arc::clone(&storage), embedder).unwrap();
    let episodic = Arc::new(EpisodicStore::new(Arc::clone(&storage)));
    let procedural = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
    let scope = ScopeState::new("p");
    Fx {
        remember: Remember::new(Arc::clone(&semantic), Arc::clone(&scope))
            .with_episodic(Arc::clone(&episodic)),
        update: Update::new(Arc::clone(&semantic)).with_episodic(Arc::clone(&episodic)),
        recall: Recall::new(Arc::clone(&semantic)).with_episodic(Arc::clone(&episodic)),
        record_event: RecordEvent::new(Arc::clone(&episodic), scope)
            .with_semantic(Arc::clone(&semantic)),
        stats: Stats::new(
            Arc::clone(&semantic),
            Arc::clone(&procedural),
            Arc::clone(&episodic),
            ColdArchive::new(tmp.path()),
            1,
        ),
        export: Export::new(procedural, episodic, storage),
        semantic,
        _tmp: tmp,
    }
}

fn text(res: &ToolResult) -> String {
    match &res.content[0] {
        ContentBlock::Text(t) => t.clone(),
    }
}

fn meta(res: &ToolResult) -> Value {
    res.meta.clone().unwrap_or(Value::Null)
}

async fn ok(tool: &dyn Tool, args: Value) -> ToolResult {
    let res = tool.invoke(args.clone()).await;
    match res {
        Ok(r) if !r.is_error => r,
        other => panic!("{args} failed: {other:?}"),
    }
}

async fn rows(tool: &dyn Tool, args: Value) -> Vec<Value> {
    serde_json::from_str(&text(&ok(tool, args).await)).unwrap()
}

async fn invalid(tool: &dyn Tool, args: Value) {
    match tool.invoke(args.clone()).await {
        Err(ToolError::InvalidArguments(_)) => {}
        other => panic!("{args} should be invalid arguments, got {other:?}"),
    }
}

fn id_of(res: &ToolResult) -> String {
    meta(res)["id"].as_str().expect("_meta.id").to_owned()
}

/// Same first byte and length: the stub embedder gives all three the
/// same vector, so order is decided by relations alone.
const OLD: &str = "port for the gateway is 8080";
const NEW: &str = "port for the gateway is 9090";
const QUERY: &str = "port for the gateway is 7070";

#[tokio::test]
async fn remember_with_refines_demotes_the_older_memory_in_recall() {
    let f = fx();
    let old = id_of(&ok(&f.remember, json!({ "content": OLD })).await);
    let res = ok(
        &f.remember,
        json!({
            "content": NEW,
            "relations": [{ "type": "refines", "id": old, "reason": "moved in March" }],
            "provenance": "observed",
            "source": "tool:cat gateway.toml",
        }),
    )
    .await;
    let new = id_of(&res);
    assert!(
        text(&res).contains(&format!("refines {old}")),
        "{}",
        text(&res)
    );
    assert_eq!(meta(&res)["relations"][0]["type"], "refines");
    assert_eq!(meta(&res)["relations"][0]["id"], old);
    assert_eq!(meta(&res)["provenance"], "observed");
    // The neighbour it was just related to is not reported as a
    // possible duplicate.
    assert!(meta(&res).get("related_memory").is_none());
    assert!(meta(&res).get("duplicate_advisory").is_none());

    let got = rows(&f.recall, json!({ "query": QUERY })).await;
    assert_eq!(got[0]["id"], new);
    assert_eq!(got[1]["id"], old);
    assert_eq!(got[1]["demoted"], true);
    assert!(got[0].get("demoted").is_none());
    assert!(got[0]["rank"].as_f64().unwrap() > got[1]["rank"].as_f64().unwrap());
    assert_eq!(
        got[1]["relations"]["refined_by"],
        json!([{ "id": new, "reason": "moved in March" }])
    );
    assert_eq!(got[0]["relations"]["refines"][0]["id"], old);
    assert_eq!(got[0]["provenance"], "observed");
    assert_eq!(got[0]["source"], "tool:cat gateway.toml");
    // A row with neither has neither key.
    assert!(got[1].get("provenance").is_none());
    assert!(got[1].get("linked").is_none());
}

#[tokio::test]
async fn rows_without_relations_have_no_relations_key() {
    let f = fx();
    ok(&f.remember, json!({ "content": OLD })).await;
    let got = rows(&f.recall, json!({ "query": QUERY })).await;
    for key in ["relations", "demoted", "provenance", "source", "linked"] {
        assert!(got[0].get(key).is_none(), "unexpected `{key}`");
    }
}

#[tokio::test]
async fn bad_relations_and_provenance_are_rejected_and_nothing_is_stored() {
    let f = fx();
    let old = id_of(&ok(&f.remember, json!({ "content": OLD })).await);
    let ghost = MemoryId::new().to_string();
    for bad in [
        json!({ "content": NEW, "relations": [{ "type": "refines", "id": ghost }] }),
        json!({ "content": NEW, "relations": [{ "type": "amends", "id": old }] }),
        json!({ "content": NEW, "relations": [{ "type": "refines", "ref": "x:y" }] }),
        json!({ "content": NEW, "relations": [{ "type": "evidence_for", "ref": "x".repeat(129) }] }),
        json!({ "content": NEW, "relations": [{ "type": "refines", "id": old, "reason": "r".repeat(301) }] }),
        json!({ "content": NEW, "relations": "refines" }),
        json!({ "content": NEW, "provenance": "guessed" }),
        json!({ "content": NEW, "source": "tool:x" }),
    ] {
        invalid(&f.remember, bad).await;
    }
    assert_eq!(f.semantic.len(), 1, "a rejected remember stored something");
    // `update` rejects the same, plus relating a memory to itself.
    invalid(
        &f.update,
        json!({ "id": old, "relations": [{ "type": "refines", "id": old }] }),
    )
    .await;
    invalid(&f.update, json!({ "id": old, "provenance": "nope" })).await;
    invalid(&f.recall, json!({ "query": "x", "provenance": "nope" })).await;
    invalid(&f.recall, json!({ "query": "x", "expand": 2 })).await;
    invalid(&f.recall, json!({ "ref": "  " })).await;
    invalid(&f.recall, json!({ "ref": "x".repeat(129) })).await;
    invalid(&f.recall, json!({})).await;
}

#[tokio::test]
async fn the_related_memory_advisory_suggests_a_refines_relation() {
    let f = fx();
    let old = id_of(&ok(&f.remember, json!({ "content": OLD })).await);
    let res = ok(&f.remember, json!({ "content": NEW })).await;
    let m = meta(&res);
    let advisory = m
        .get("duplicate_advisory")
        .or_else(|| m.get("related_memory"))
        .expect("an advisory for an identical-vector neighbour");
    assert_eq!(
        advisory["suggested_relation"],
        json!({ "type": "refines", "id": old })
    );
    assert!(advisory["message"].as_str().unwrap().contains("refines"));
    assert!(
        text(&res).contains("\"type\": \"refines\""),
        "{}",
        text(&res)
    );
}

#[tokio::test]
async fn update_adds_relations_and_provenance_after_the_fact() {
    let f = fx();
    let old = id_of(&ok(&f.remember, json!({ "content": OLD })).await);
    let new = id_of(&ok(&f.remember, json!({ "content": NEW })).await);
    let res = ok(
        &f.update,
        json!({
            "id": new,
            "relations": [{ "type": "contradicts", "id": old, "reason": "measured again" }],
            "provenance": "inferred",
        }),
    )
    .await;
    assert!(text(&res).contains("contradicts"), "{}", text(&res));
    let got = rows(&f.recall, json!({ "query": QUERY })).await;
    assert_eq!(got[0]["id"], new);
    assert_eq!(got[0]["provenance"], "inferred");
    assert_eq!(got[1]["demoted"], true);
    assert_eq!(
        got[1]["relations"]["contradicted_by"],
        json!([{ "id": new, "reason": "measured again" }])
    );
    // A metadata-only update like this one is not an edit.
    let item = f
        .semantic
        .get(new.parse::<ulid::Ulid>().map(MemoryId).unwrap());
    let item = item.await.unwrap().unwrap();
    assert_eq!(
        f.semantic.updated_at(item.id).await.unwrap(),
        Some(item.created_at)
    );
}

#[tokio::test]
async fn supersedes_argument_still_hides_and_shows_as_a_relation() {
    let f = fx();
    let old = id_of(&ok(&f.remember, json!({ "content": OLD })).await);
    let new = id_of(&ok(&f.remember, json!({ "content": NEW, "supersedes": old })).await);
    let got = rows(&f.recall, json!({ "query": QUERY })).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["id"], new);
    assert_eq!(got[0]["relations"]["supersedes"][0]["id"], old);
    let all = rows(
        &f.recall,
        json!({ "query": QUERY, "include_superseded": true }),
    )
    .await;
    assert_eq!(all[1]["id"], old);
    assert_eq!(all[1]["superseded_by"], new);
    assert_eq!(all[1]["relations"]["superseded_by"], new);
}

#[tokio::test]
async fn recall_by_ref_returns_the_memories_that_cite_it() {
    let f = fx();
    let cited = id_of(
        &ok(
            &f.remember,
            json!({
                "content": "pricing tiers were cut to three",
                "relations": [{ "type": "evidence_for", "ref": "research:pricing-study", "reason": "section 4" }],
            }),
        )
        .await,
    );
    ok(&f.remember, json!({ "content": "lunch was a sandwich" })).await;

    // No query needed; rows have no similarity to report.
    let got = rows(&f.recall, json!({ "ref": "research:pricing-study" })).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["id"], cited);
    assert_eq!(got[0]["match"], "ref");
    assert!(got[0]["similarity"].is_null());
    assert!(got[0]["rank"].is_null());
    assert_eq!(
        got[0]["relations"]["evidence"],
        json!([{ "ref": "research:pricing-study", "reason": "section 4" }])
    );
    // With a query the similarity is real.
    let got = rows(
        &f.recall,
        json!({ "ref": "research:pricing-study", "query": "pricing tiers were cut to three" }),
    )
    .await;
    assert!(got[0]["similarity"].as_f64().unwrap() > 0.99);
    // An unknown reference is an empty list, not an error.
    assert!(
        rows(&f.recall, json!({ "ref": "research:other" }))
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn expand_adds_stubs_of_linked_memories_and_events() {
    let f = fx();
    let long = format!("{OLD} {}", "and a long tail of detail ".repeat(12));
    let old = id_of(&ok(&f.remember, json!({ "content": long })).await);
    let event = text(
        &ok(
            &f.record_event,
            json!({ "kind": "decision", "payload": { "content": "gateway port change approved" } }),
        )
        .await,
    )
    .trim_start_matches("recorded event ")
    .to_owned();
    let new = id_of(
        &ok(
            &f.remember,
            json!({
                "content": NEW,
                "relations": [
                    { "type": "refines", "id": old },
                    { "type": "evidence_for", "id": event },
                    { "type": "evidence_for", "ref": "path:/etc/gateway.toml" },
                ],
            }),
        )
        .await,
    );

    let plain = rows(&f.recall, json!({ "query": NEW })).await;
    assert!(plain.iter().all(|r| r.get("linked").is_none()));

    let got = rows(&f.recall, json!({ "query": NEW, "expand": 1 })).await;
    let row = got.iter().find(|r| r["id"] == new).unwrap();
    let linked = row["linked"].as_array().unwrap();
    // The refined memory and the event; a bare reference has no text.
    assert_eq!(linked.len(), 2);
    assert_eq!(linked[0]["id"], old);
    assert_eq!(linked[0]["layer"], "semantic");
    let stub = linked[0]["content"].as_str().unwrap();
    assert_eq!(stub.chars().count(), 121);
    assert!(stub.starts_with(OLD) && stub.ends_with('…'));
    assert_eq!(linked[1]["id"], event);
    assert_eq!(linked[1]["layer"], "episodic");
    assert_eq!(linked[1]["content"], "gateway port change approved");
    // The event shows in `evidence` as an episodic id.
    assert_eq!(row["relations"]["evidence"][0]["layer"], "episodic");
    // The other end of the relation expands too.
    let other = got.iter().find(|r| r["id"] == old).unwrap();
    assert_eq!(other["linked"][0]["id"], new);
}

#[tokio::test]
async fn provenance_filter_and_stats_counts() {
    let f = fx();
    let stated = id_of(
        &ok(
            &f.remember,
            json!({ "content": "shared topic one", "provenance": "user_stated", "source": "telegram:chat-12" }),
        )
        .await,
    );
    ok(
        &f.remember,
        json!({ "content": "shared topic two", "provenance": "inferred" }),
    )
    .await;
    let plain = id_of(&ok(&f.remember, json!({ "content": "shared topic six" })).await);

    let got = rows(
        &f.recall,
        json!({ "query": "shared topic", "provenance": "user_stated" }),
    )
    .await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["id"], stated);
    assert_eq!(got[0]["source"], "telegram:chat-12");
    // Events carry no provenance, so they never match the filter.
    ok(
        &f.record_event,
        json!({ "kind": "decision", "payload": { "content": "shared topic decided" } }),
    )
    .await;
    let got = rows(
        &f.recall,
        json!({ "query": "shared topic", "provenance": "inferred", "layers": ["semantic", "episodic"] }),
    )
    .await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["layer"], "semantic");

    let stats: Value = serde_json::from_str(&text(&ok(&f.stats, json!({})).await)).unwrap();
    assert_eq!(
        stats["memories"]["by_provenance"],
        json!({ "user_stated": 1, "observed": 0, "inferred": 1, "imported": 0, "unspecified": 1 })
    );

    // `update` sets it later; a second update replaces it.
    ok(
        &f.update,
        json!({ "id": plain, "provenance": "imported", "source": "csv:2026-11" }),
    )
    .await;
    let stats: Value = serde_json::from_str(&text(&ok(&f.stats, json!({})).await)).unwrap();
    assert_eq!(stats["memories"]["by_provenance"]["imported"], 1);
    assert_eq!(stats["memories"]["by_provenance"]["unspecified"], 0);
}

#[tokio::test]
async fn promote_writes_the_memory_and_the_evidence_link() {
    let f = fx();
    let res = ok(
        &f.record_event,
        json!({
            "kind": "decision",
            "payload": { "content": "we will ship the importer behind a flag" },
            "tags": ["importer"],
            "scope": "work",
            "promote": { "type": "decision" },
        }),
    )
    .await;
    let memory_id = meta(&res)["promoted_memory_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let event_id = text(&res)
        .trim_start_matches("recorded event ")
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert!(text(&res).contains(&format!("promoted to memory {memory_id}")));

    let id = MemoryId(memory_id.parse().unwrap());
    let item = f.semantic.get(id).await.unwrap().unwrap();
    assert_eq!(item.content, "we will ship the importer behind a flag");
    assert_eq!(item.kind.as_str(), "decision");
    assert_eq!(item.scope, "work");
    // Tags default to the event's.
    assert_eq!(item.tags, vec!["importer".to_owned()]);
    let out = f.semantic.outgoing_relations(id).await.unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].kind, RelationType::EvidenceFor);
    assert_eq!(
        out[0].target,
        RelationTarget::Event(crate::ids::EventId(event_id.parse().unwrap()))
    );

    // Found by meaning through `recall`, with its event one hop away.
    let got = rows(
        &f.recall,
        json!({ "query": "we will ship the importer behind a flag", "expand": 1 }),
    )
    .await;
    assert_eq!(got[0]["id"], memory_id);
    assert_eq!(got[0]["relations"]["evidence"][0]["id"], event_id);
    assert_eq!(got[0]["linked"][0]["layer"], "episodic");

    // A retry records a second event but not a second memory.
    let again = ok(
        &f.record_event,
        json!({
            "kind": "decision",
            "payload": { "content": "we will ship the importer behind a flag" },
            "scope": "work",
            "promote": { "type": "decision", "tags": ["x"] },
        }),
    )
    .await;
    assert_eq!(meta(&again)["promoted_memory_id"], memory_id);
    assert_eq!(f.semantic.len(), 1);
    assert_eq!(f.semantic.outgoing_relations(id).await.unwrap().len(), 2);

    // Without `promote` nothing reaches L4.
    let plain = ok(
        &f.record_event,
        json!({ "kind": "observation", "payload": { "content": "nothing to keep" } }),
    )
    .await;
    assert!(meta(&plain).get("promoted_memory_id").is_none());
    assert_eq!(f.semantic.len(), 1);
}

#[tokio::test]
async fn promote_is_validated_before_the_event_is_written() {
    let f = fx();
    for bad in [
        json!({ "kind": "decision", "payload": { "content": "x" }, "promote": "yes" }),
        json!({ "kind": "decision", "payload": { "content": "x" }, "promote": { "type": "rule" } }),
        json!({ "kind": "decision", "payload": { "content": "x" }, "promote": { "why": "x" } }),
        // Nothing to promote: the payload has no text field.
        json!({ "kind": "decision", "payload": { "n": 3 }, "promote": {} }),
    ] {
        invalid(&f.record_event, bad).await;
    }
    assert_eq!(f.semantic.len(), 0);
    let stats: Value = serde_json::from_str(&text(&ok(&f.stats, json!({})).await)).unwrap();
    assert_eq!(stats["memories"]["episodic"]["hot"], 0);
}

#[tokio::test]
async fn export_carries_relations_and_provenance() {
    let f = fx();
    let old = id_of(&ok(&f.remember, json!({ "content": OLD })).await);
    let new = id_of(
        &ok(
            &f.remember,
            json!({
                "content": NEW,
                "relations": [
                    { "type": "refines", "id": old, "reason": "moved" },
                    { "type": "evidence_for", "ref": "path:/etc/gateway.toml" },
                ],
                "provenance": "observed",
                "source": "tool:cat",
            }),
        )
        .await,
    );
    let v: Value = serde_json::from_str(&text(&ok(&f.export, json!({})).await)).unwrap();
    let rows = v["semantic"].as_array().unwrap();
    let row = rows.iter().find(|r| r["id"] == new).unwrap();
    assert_eq!(row["provenance"], "observed");
    assert_eq!(row["source"], "tool:cat");
    let relations = row["relations"].as_array().unwrap();
    assert_eq!(relations.len(), 2);
    let refines = relations.iter().find(|r| r["type"] == "refines").unwrap();
    assert_eq!(refines["id"], old);
    assert_eq!(refines["reason"], "moved");
    assert!(refines["created_at"].is_string());
    let evidence = relations
        .iter()
        .find(|r| r["type"] == "evidence_for")
        .unwrap();
    assert_eq!(evidence["ref"], "path:/etc/gateway.toml");
    // The plain memory has neither key.
    let plain = rows.iter().find(|r| r["id"] == old).unwrap();
    assert!(plain.get("relations").is_none());
    assert!(plain.get("provenance").is_none());
}
