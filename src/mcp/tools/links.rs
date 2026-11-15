//! Argument parsing and row rendering shared by the tools that read or
//! write relations and provenance (`remember`, `update`, `recall`,
//! `record_event`, `export`, and the context resource).
//!
//! Everything a caller can get wrong is rejected here, before anything
//! is written: an unknown type, both or neither of `id` / `ref`, a
//! target that does not exist, a reason or reference over its limit.
//! All of them are invalid-arguments errors (JSON-RPC -32602).

use serde_json::{Map, Value, json};

use super::ToolError;
use crate::ids::{EventId, MemoryId};
use crate::memory::episodic::EpisodicStore;
use crate::memory::relations::{
    self, MAX_SOURCE_CHARS, Provenance, ProvenanceRecord, Relation, RelationTarget, RelationType,
};
use crate::memory::semantic::{HitRelations, SemanticStore};

/// How many characters of a linked memory `recall(expand = 1)` quotes.
pub(crate) const STUB_CHARS: usize = 120;

/// JSON-schema fragment for the `relations` argument.
pub(crate) fn relations_schema() -> Value {
    json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "type": {
                    "type": "string",
                    "enum": ["supersedes", "refines", "contradicts", "evidence_for"]
                },
                "id": {
                    "type": "string",
                    "description": "Id of the memory this one relates to. For `evidence_for` it may also be an event id."
                },
                "ref": {
                    "type": "string",
                    "description": "External reference instead of an id, `evidence_for` only: an opaque string such as `research:pricing-study` or `path:/docs/adr-7.md`, at most 128 bytes."
                },
                "reason": {
                    "type": "string",
                    "description": "Why, in a sentence. At most 300 characters."
                }
            },
            "required": ["type"]
        },
        "description": "How this memory relates to others. `refines` (narrows or updates part of an older memory) and `contradicts` push the older memory below this one in recall without hiding it. `supersedes` hides it, like the `supersedes` argument. `evidence_for` names what this memory is based on: a memory, an event, or an external `ref`."
    })
}

/// JSON-schema fragments for `provenance` and `source`.
pub(crate) fn provenance_schema() -> (Value, Value) {
    (
        json!({
            "type": "string",
            "enum": ["user_stated", "observed", "inferred", "imported"],
            "description": "How you came to know this: the user said it, you saw it, you concluded it, or it was brought in from another system."
        }),
        json!({
            "type": "string",
            "description": "Where it came from, free-form (`telegram:…`, `research:…`, `tool:…`). At most 200 characters. Needs `provenance`."
        }),
    )
}

/// One parsed, validated relation, ready to write.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParsedRelation {
    pub kind: RelationType,
    pub target: RelationTarget,
    pub reason: String,
}

fn invalid(msg: impl Into<String>) -> ToolError {
    ToolError::InvalidArguments(msg.into())
}

/// Parse and validate `relations`. Every memory target must exist;
/// an `id` that is not a memory is looked up as an event when
/// `episodic` is given. `own_id` is the memory the relations will hang
/// from, when it already exists (`update`): relating it to itself is
/// rejected.
pub(crate) async fn parse_relations(
    arg: Option<&Value>,
    semantic: &SemanticStore,
    episodic: Option<&EpisodicStore>,
    own_id: Option<MemoryId>,
) -> Result<Vec<ParsedRelation>, ToolError> {
    let items = match arg {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(invalid("`relations` must be an array of objects")),
    };
    let mut out: Vec<ParsedRelation> = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let at = format!("`relations[{i}]`");
        let obj = item
            .as_object()
            .ok_or_else(|| invalid(format!("{at} must be an object")))?;
        if let Some(stray) = obj
            .keys()
            .find(|k| !["type", "id", "ref", "reason"].contains(&k.as_str()))
        {
            return Err(invalid(format!(
                "{at}: unexpected key `{stray}`; use type, id or ref, reason"
            )));
        }
        let kind = string_field(obj, "type", &at)?
            .ok_or_else(|| invalid(format!("{at}: `type` is required")))?;
        let kind = RelationType::parse(kind).ok_or_else(|| {
            invalid(format!(
                "{at}: `type` must be one of supersedes|refines|contradicts|evidence_for, got `{kind}`"
            ))
        })?;
        let reason = string_field(obj, "reason", &at)?
            .unwrap_or("")
            .trim()
            .to_owned();
        let id = string_field(obj, "id", &at)?;
        let reference = string_field(obj, "ref", &at)?;
        let target = match (id, reference) {
            (Some(_), Some(_)) => {
                return Err(invalid(format!("{at}: pass `id` or `ref`, not both")));
            }
            (None, None) => return Err(invalid(format!("{at}: `id` or `ref` is required"))),
            (None, Some(r)) => RelationTarget::Ref(r.trim().to_owned()),
            (Some(raw), None) => {
                let ulid = ulid::Ulid::from_string(raw.trim())
                    .map_err(|_| invalid(format!("{at}: `{raw}` is not an id")))?;
                resolve_id(ulid, kind, semantic, episodic, &at).await?
            }
        };
        relations::validate(kind, &target, &reason).map_err(|m| invalid(format!("{at}: {m}")))?;
        if own_id.is_some_and(|own| target == RelationTarget::Memory(own)) {
            return Err(invalid(format!("{at}: a memory cannot relate to itself")));
        }
        if !out.iter().any(|r| r.kind == kind && r.target == target) {
            out.push(ParsedRelation {
                kind,
                target,
                reason,
            });
        }
    }
    Ok(out)
}

fn string_field<'a>(
    obj: &'a Map<String, Value>,
    name: &str,
    at: &str,
) -> Result<Option<&'a str>, ToolError> {
    match obj.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(invalid(format!("{at}: `{name}` must be a string"))),
    }
}

/// Decide what a ULID names: a memory, or (for `evidence_for`) an event.
async fn resolve_id(
    ulid: ulid::Ulid,
    kind: RelationType,
    semantic: &SemanticStore,
    episodic: Option<&EpisodicStore>,
    at: &str,
) -> Result<RelationTarget, ToolError> {
    let internal = |e: crate::MnemeError| ToolError::Internal(format!("relation lookup: {e}"));
    if semantic
        .get(MemoryId(ulid))
        .await
        .map_err(internal)?
        .is_some()
    {
        return Ok(RelationTarget::Memory(MemoryId(ulid)));
    }
    if !kind.needs_memory_target()
        && let Some(episodic) = episodic
        && episodic
            .find_warm_or_hot(EventId(ulid))
            .await
            .map_err(internal)?
            .is_some()
    {
        return Ok(RelationTarget::Event(EventId(ulid)));
    }
    Err(invalid(if kind.needs_memory_target() {
        format!("{at}: no memory with id {ulid}")
    } else {
        format!("{at}: no memory or event with id {ulid}")
    }))
}

/// Parse `provenance` and `source`. `Ok(None)` when neither is given.
pub(crate) fn parse_provenance(args: &Value) -> Result<Option<ProvenanceRecord>, ToolError> {
    let provenance = match args.get("provenance") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(Provenance::parse(s).ok_or_else(|| {
            invalid(format!(
                "`provenance` must be one of user_stated|observed|inferred|imported, got `{s}`"
            ))
        })?),
        Some(_) => return Err(invalid("`provenance` must be a string")),
    };
    let source = match args.get("source") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.trim().is_empty() => None,
        Some(Value::String(s)) => {
            if s.chars().count() > MAX_SOURCE_CHARS {
                return Err(invalid(format!(
                    "`source` is longer than {MAX_SOURCE_CHARS} characters"
                )));
            }
            Some(s.trim().to_owned())
        }
        Some(_) => return Err(invalid("`source` must be a string")),
    };
    match (provenance, source) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err(invalid(
            "`source` needs `provenance` (user_stated|observed|inferred|imported)",
        )),
        (Some(provenance), source) => Ok(Some(ProvenanceRecord { provenance, source })),
    }
}

/// Write parsed relations and provenance onto `id`.
pub(crate) async fn apply(
    semantic: &SemanticStore,
    id: MemoryId,
    parsed: &[ParsedRelation],
    provenance: Option<&ProvenanceRecord>,
) -> Result<(), ToolError> {
    for r in parsed {
        semantic
            .relate(id, r.kind, r.target.clone(), &r.reason)
            .await
            .map_err(|e| ToolError::Internal(format!("relation write failed: {e}")))?;
    }
    if let Some(record) = provenance {
        semantic
            .set_provenance(id, record.clone())
            .await
            .map_err(|e| ToolError::Internal(format!("provenance write failed: {e}")))?;
    }
    Ok(())
}

/// `" (refines 01H…; evidence_for research:x)"` for a reply text.
pub(crate) fn summary(parsed: &[ParsedRelation]) -> String {
    parsed
        .iter()
        .map(|r| format!("{} {}", r.kind.as_str(), target_label(&r.target)))
        .collect::<Vec<_>>()
        .join("; ")
}

fn target_label(target: &RelationTarget) -> String {
    match target {
        RelationTarget::Memory(id) => id.to_string(),
        RelationTarget::Event(id) => id.to_string(),
        RelationTarget::Ref(r) => r.clone(),
    }
}

/// The parsed relations as they are echoed in `_meta.relations`.
pub(crate) fn parsed_json(parsed: &[ParsedRelation]) -> Value {
    Value::Array(
        parsed
            .iter()
            .map(|r| {
                let mut row = Map::new();
                row.insert("type".into(), json!(r.kind.as_str()));
                insert_target(&mut row, &r.target);
                if !r.reason.is_empty() {
                    row.insert("reason".into(), json!(r.reason));
                }
                Value::Object(row)
            })
            .collect(),
    )
}

fn insert_target(row: &mut Map<String, Value>, target: &RelationTarget) {
    match target {
        RelationTarget::Memory(id) => {
            row.insert("id".into(), json!(id.to_string()));
        }
        RelationTarget::Event(id) => {
            row.insert("id".into(), json!(id.to_string()));
            row.insert("layer".into(), json!("episodic"));
        }
        RelationTarget::Ref(r) => {
            row.insert("ref".into(), json!(r));
        }
    }
}

/// The `relations` object of a recall / context row, or `None` when
/// the memory has none (the key is then left out of the row).
pub(crate) fn hit_relations_json(
    relations: &HitRelations,
    superseded_by: Option<MemoryId>,
) -> Option<Value> {
    let mut out = Map::new();
    if let Some(newer) = superseded_by {
        out.insert("superseded_by".into(), json!(newer.to_string()));
    }
    let links = |links: &[crate::memory::semantic::Link]| -> Value {
        Value::Array(
            links
                .iter()
                .map(|l| {
                    let mut row = Map::new();
                    row.insert("id".into(), json!(l.id.to_string()));
                    if !l.reason.is_empty() {
                        row.insert("reason".into(), json!(l.reason));
                    }
                    Value::Object(row)
                })
                .collect(),
        )
    };
    for (key, list) in [
        ("refined_by", &relations.refined_by),
        ("contradicted_by", &relations.contradicted_by),
        ("refines", &relations.refines),
        ("contradicts", &relations.contradicts),
        ("supersedes", &relations.supersedes),
    ] {
        if !list.is_empty() {
            out.insert(key.into(), links(list));
        }
    }
    if !relations.evidence.is_empty() {
        out.insert(
            "evidence".into(),
            Value::Array(
                relations
                    .evidence
                    .iter()
                    .map(|e| {
                        let mut row = Map::new();
                        insert_target(&mut row, &e.target);
                        if !e.reason.is_empty() {
                            row.insert("reason".into(), json!(e.reason));
                        }
                        Value::Object(row)
                    })
                    .collect(),
            ),
        );
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

/// Stored relations as `export` writes them: one object per relation,
/// everything needed to write it back.
pub(crate) fn export_json(relations: &[Relation]) -> Value {
    Value::Array(
        relations
            .iter()
            .map(|r| {
                let mut row = Map::new();
                row.insert("type".into(), json!(r.kind.as_str()));
                match &r.target {
                    RelationTarget::Memory(id) => {
                        row.insert("id".into(), json!(id.to_string()));
                    }
                    RelationTarget::Event(id) => {
                        row.insert("event_id".into(), json!(id.to_string()));
                    }
                    RelationTarget::Ref(reference) => {
                        row.insert("ref".into(), json!(reference));
                    }
                }
                row.insert("reason".into(), json!(r.reason));
                row.insert("created_at".into(), json!(r.created_at.to_rfc3339()));
                Value::Object(row)
            })
            .collect(),
    )
}

/// The first [`STUB_CHARS`] characters of `content` on one line.
pub(crate) fn stub(content: &str) -> String {
    let one_line = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= STUB_CHARS {
        one_line
    } else {
        let mut cut: String = one_line.chars().take(STUB_CHARS).collect();
        cut.push('…');
        cut
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::Embedder;
    use crate::embed::stub::StubEmbedder;
    use crate::memory::semantic::MemoryKind;
    use crate::storage::Storage;
    use crate::storage::memory_impl::MemoryStorage;
    use std::sync::Arc;
    use tempfile::TempDir;

    struct Fx {
        semantic: Arc<SemanticStore>,
        episodic: EpisodicStore,
        _tmp: TempDir,
    }

    fn fx() -> Fx {
        let tmp = TempDir::new().unwrap();
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::with_dim(4));
        let semantic =
            SemanticStore::open_disabled(tmp.path(), Arc::clone(&storage), embedder).unwrap();
        Fx {
            semantic,
            episodic: EpisodicStore::new(storage),
            _tmp: tmp,
        }
    }

    async fn parse(f: &Fx, v: Value) -> Result<Vec<ParsedRelation>, ToolError> {
        parse_relations(Some(&v), &f.semantic, Some(&f.episodic), None).await
    }

    #[tokio::test]
    async fn parses_memory_event_and_ref_targets() {
        let f = fx();
        let mem = f
            .semantic
            .remember("a", MemoryKind::Fact, vec![], "p".into())
            .await
            .unwrap();
        let event = f.episodic.record("decision", "p", "\"e\"").await.unwrap();
        let parsed = parse(
            &f,
            json!([
                { "type": "refines", "id": mem.to_string(), "reason": " narrower " },
                { "type": "evidence_for", "id": event.to_string() },
                { "type": "evidence_for", "ref": "research:pricing" },
                // A repeat of the first is dropped.
                { "type": "refines", "id": mem.to_string() },
            ]),
        )
        .await
        .unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].target, RelationTarget::Memory(mem));
        assert_eq!(parsed[0].reason, "narrower");
        assert_eq!(parsed[1].target, RelationTarget::Event(event));
        assert_eq!(
            parsed[2].target,
            RelationTarget::Ref("research:pricing".into())
        );
        assert_eq!(
            summary(&parsed),
            format!("refines {mem}; evidence_for {event}; evidence_for research:pricing")
        );
    }

    #[tokio::test]
    async fn bad_relations_are_invalid_arguments() {
        let f = fx();
        let mem = f
            .semantic
            .remember("a", MemoryKind::Fact, vec![], "p".into())
            .await
            .unwrap();
        let event = f.episodic.record("decision", "p", "\"e\"").await.unwrap();
        let ghost = MemoryId::new();
        for bad in [
            json!("refines"),
            json!(["refines"]),
            json!([{ "id": mem.to_string() }]),
            json!([{ "type": "references", "id": mem.to_string() }]),
            json!([{ "type": "refines" }]),
            json!([{ "type": "refines", "id": mem.to_string(), "ref": "x:y" }]),
            json!([{ "type": "refines", "id": "not-an-id" }]),
            json!([{ "type": "refines", "id": ghost.to_string() }]),
            json!([{ "type": "evidence_for", "id": ghost.to_string() }]),
            // Only evidence_for may point at an event or a ref.
            json!([{ "type": "refines", "id": event.to_string() }]),
            json!([{ "type": "contradicts", "ref": "x:y" }]),
            json!([{ "type": "evidence_for", "ref": "x".repeat(129) }]),
            json!([{ "type": "evidence_for", "ref": "  " }]),
            json!([{ "type": "refines", "id": mem.to_string(), "reason": "r".repeat(301) }]),
            json!([{ "type": "refines", "id": mem.to_string(), "reason": 3 }]),
            json!([{ "type": "refines", "id": mem.to_string(), "because": "x" }]),
        ] {
            let err = parse(&f, bad.clone()).await.unwrap_err();
            assert!(
                matches!(err, ToolError::InvalidArguments(_)),
                "{bad} gave {err:?}"
            );
        }
        // Self-relation, when the owner is known.
        let err = parse_relations(
            Some(&json!([{ "type": "refines", "id": mem.to_string() }])),
            &f.semantic,
            None,
            Some(mem),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
        // Absent and null are "no relations".
        assert!(parse(&f, Value::Null).await.unwrap().is_empty());
    }

    #[test]
    fn provenance_args() {
        assert_eq!(parse_provenance(&json!({})).unwrap(), None);
        assert_eq!(
            parse_provenance(&json!({ "provenance": "inferred", "source": " tool:grep " }))
                .unwrap(),
            Some(ProvenanceRecord {
                provenance: Provenance::Inferred,
                source: Some("tool:grep".into()),
            })
        );
        assert_eq!(
            parse_provenance(&json!({ "provenance": "user_stated" }))
                .unwrap()
                .unwrap()
                .source,
            None
        );
        for bad in [
            json!({ "provenance": "guessed" }),
            json!({ "provenance": 1 }),
            json!({ "source": "tool:grep" }),
            json!({ "provenance": "observed", "source": "s".repeat(201) }),
            json!({ "provenance": "observed", "source": 4 }),
        ] {
            assert!(
                matches!(parse_provenance(&bad), Err(ToolError::InvalidArguments(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn stub_is_one_line_and_bounded() {
        assert_eq!(stub("a\n  b"), "a b");
        let long = stub(&"word ".repeat(100));
        assert_eq!(long.chars().count(), STUB_CHARS + 1);
        assert!(long.ends_with('…'));
    }
}
