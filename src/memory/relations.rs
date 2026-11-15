//! Typed relations between a memory and something else, and the
//! provenance of a memory: the two pieces of L4 metadata added in 1.6.
//!
//! # Why relations
//!
//! Until 1.6 the only link a memory could have was `supersedes`, which
//! *hides* the old memory. That is right when a fact is replaced
//! outright and wrong for everything else: a memory that narrows,
//! corrects one detail of, or disputes another should push the older
//! one down, not make it vanish, and a memory should be able to say
//! what it is based on. Four types cover that:
//!
//! | Type | Meaning | Effect on the target |
//! |---|---|---|
//! | `supersedes` | replaces it | hidden from recall (the `sup:` link, unchanged) |
//! | `refines` | narrows or updates part of it | demoted in recall |
//! | `contradicts` | disputes it | demoted in recall |
//! | `evidence_for` | the target is the source or support | none |
//!
//! The *source* of a relation is always a memory. The *target* is a
//! memory, an episodic event, or an external reference: an opaque
//! string such as `research:pricing-study` or `path:/docs/adr-7.md`
//! that mneme stores and matches but never interprets.
//!
//! # Storage
//!
//! Side tables beside the `mem:` row, like `sup:`, `mts:` and `st:`,
//! so `MemoryItem` and the schema version are untouched:
//!
//! ```text
//! rel:<from:16><type:1><tag:1><to_key>   -> [ver:1][postcard RelationValue]
//! rrel:<tag:1><to_key><type:1><from:16>  -> the same bytes
//! prov:<id:16>                           -> [ver:1][postcard ProvenanceValue]
//! ```
//!
//! `tag` says what `to_key` is: a 16-byte memory ULID, a 16-byte event
//! ULID, or the UTF-8 bytes of a reference (at most
//! [`MAX_REF_BYTES`]). `rrel:` is the same relation keyed from the
//! target, which is what answers "what refines this memory?" and
//! `recall(ref = …)` without a scan of every relation.
//!
//! The two rows of a relation are separate puts (the storage seam has
//! no multi-key transaction). `rel:` is written first and is the
//! authority; a crash between them loses the reverse lookup for that
//! relation, never invents one.
//!
//! # Validity is checked on read
//!
//! `forget` deletes a memory's outgoing relations. Anything it misses
//! (a crash, a target forgotten later) is filtered when relations are
//! read: a relation whose source memory is gone does not exist, and a
//! memory target that is gone is not reported. So undoing a mistaken
//! `refines` is forgetting the memory that carries it, exactly as for
//! `supersedes`.
//!
//! Functions here take a `&dyn Storage` and are free of
//! [`SemanticStore`](super::semantic::SemanticStore) state, so the
//! export paths, which scan storage directly, can use them too.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{EventId, MemoryId};
use crate::storage::{MEM_KEY_PREFIX, PROV_KEY_PREFIX, REL_KEY_PREFIX, RREL_KEY_PREFIX, Storage};
use crate::{MnemeError, Result};

/// Longest `reason` a relation may carry, in characters.
pub const MAX_REASON_CHARS: usize = 300;

/// Longest external reference, in bytes of UTF-8.
pub const MAX_REF_BYTES: usize = 128;

/// Longest provenance `source`, in characters.
pub const MAX_SOURCE_CHARS: usize = 200;

/// Version byte leading every value written here. Lets the value
/// layout change later without a guess at decode time.
const VALUE_VERSION: u8 = 1;

const TAG_MEMORY: u8 = 1;
const TAG_EVENT: u8 = 2;
const TAG_REF: u8 = 3;

/// What a relation says about its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RelationType {
    Supersedes,
    Refines,
    Contradicts,
    EvidenceFor,
}

impl RelationType {
    pub const ALL: [Self; 4] = [
        Self::Supersedes,
        Self::Refines,
        Self::Contradicts,
        Self::EvidenceFor,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Supersedes => "supersedes",
            Self::Refines => "refines",
            Self::Contradicts => "contradicts",
            Self::EvidenceFor => "evidence_for",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == s)
    }

    /// Whether a current, newer source of this type pushes its target
    /// down in recall.
    pub fn demotes(&self) -> bool {
        matches!(self, Self::Refines | Self::Contradicts)
    }

    /// Whether the target has to be a memory. Only `evidence_for` can
    /// point at an event or an external reference: "refines a URL" has
    /// no meaning mneme could act on.
    pub fn needs_memory_target(&self) -> bool {
        !matches!(self, Self::EvidenceFor)
    }

    fn code(self) -> u8 {
        match self {
            Self::Supersedes => 1,
            Self::Refines => 2,
            Self::Contradicts => 3,
            Self::EvidenceFor => 4,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.code() == code)
    }
}

/// What a relation points at.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RelationTarget {
    Memory(MemoryId),
    Event(EventId),
    /// An opaque external reference. Stored and compared byte for
    /// byte; never parsed, resolved or validated beyond its length.
    Ref(String),
}

impl RelationTarget {
    fn tag(&self) -> u8 {
        match self {
            Self::Memory(_) => TAG_MEMORY,
            Self::Event(_) => TAG_EVENT,
            Self::Ref(_) => TAG_REF,
        }
    }

    fn key_bytes(&self) -> Vec<u8> {
        match self {
            Self::Memory(id) => id.0.to_bytes().to_vec(),
            Self::Event(id) => id.0.to_bytes().to_vec(),
            Self::Ref(s) => s.as_bytes().to_vec(),
        }
    }

    fn decode(tag: u8, bytes: &[u8]) -> Option<Self> {
        let ulid = || <[u8; 16]>::try_from(bytes).ok().map(ulid::Ulid::from_bytes);
        match tag {
            TAG_MEMORY => ulid().map(|u| Self::Memory(MemoryId(u))),
            TAG_EVENT => ulid().map(|u| Self::Event(EventId(u))),
            TAG_REF => std::str::from_utf8(bytes)
                .ok()
                .map(|s| Self::Ref(s.to_owned())),
            _ => None,
        }
    }
}

/// One relation, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    pub from: MemoryId,
    pub kind: RelationType,
    pub target: RelationTarget,
    /// Why, in the writer's words. May be empty (`supersedes` written
    /// through the `supersedes` argument has none).
    pub reason: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
struct RelationValue {
    reason: String,
    created_at: DateTime<Utc>,
}

/// Check a relation's free-text parts against the limits. The caller
/// (a tool) turns the message into an invalid-arguments error.
pub fn validate(
    kind: RelationType,
    target: &RelationTarget,
    reason: &str,
) -> std::result::Result<(), String> {
    if reason.chars().count() > MAX_REASON_CHARS {
        return Err(format!(
            "`reason` is longer than {MAX_REASON_CHARS} characters"
        ));
    }
    match target {
        RelationTarget::Ref(r) => {
            if r.trim().is_empty() {
                return Err("`ref` must not be empty".into());
            }
            if r.len() > MAX_REF_BYTES {
                return Err(format!("`ref` is longer than {MAX_REF_BYTES} bytes"));
            }
            if kind.needs_memory_target() {
                return Err(format!(
                    "`{}` needs a memory `id`; only `evidence_for` can point at a `ref`",
                    kind.as_str()
                ));
            }
        }
        RelationTarget::Event(_) if kind.needs_memory_target() => {
            return Err(format!(
                "`{}` needs a memory `id`; only `evidence_for` can point at an event",
                kind.as_str()
            ));
        }
        _ => {}
    }
    Ok(())
}

fn rel_key(from: MemoryId, kind: RelationType, target: &RelationTarget) -> Vec<u8> {
    let to = target.key_bytes();
    let mut k = Vec::with_capacity(REL_KEY_PREFIX.len() + 18 + to.len());
    k.extend_from_slice(REL_KEY_PREFIX);
    k.extend_from_slice(&from.0.to_bytes());
    k.push(kind.code());
    k.push(target.tag());
    k.extend_from_slice(&to);
    k
}

fn rrel_key(from: MemoryId, kind: RelationType, target: &RelationTarget) -> Vec<u8> {
    let mut k = rrel_prefix(target);
    k.push(kind.code());
    k.extend_from_slice(&from.0.to_bytes());
    k
}

/// `rrel:<tag><to_key>`: every relation into `target` starts with it.
/// For a reference it is also a prefix of longer references, so
/// readers compare the decoded target as well.
fn rrel_prefix(target: &RelationTarget) -> Vec<u8> {
    let to = target.key_bytes();
    let mut k = Vec::with_capacity(RREL_KEY_PREFIX.len() + 18 + to.len());
    k.extend_from_slice(RREL_KEY_PREFIX);
    k.push(target.tag());
    k.extend_from_slice(&to);
    k
}

fn rel_prefix(from: MemoryId) -> Vec<u8> {
    let mut k = Vec::with_capacity(REL_KEY_PREFIX.len() + 16);
    k.extend_from_slice(REL_KEY_PREFIX);
    k.extend_from_slice(&from.0.to_bytes());
    k
}

fn encode_value(reason: &str, created_at: DateTime<Utc>) -> Result<Vec<u8>> {
    let mut out = vec![VALUE_VERSION];
    let body = postcard::to_allocvec(&RelationValue {
        reason: reason.to_owned(),
        created_at,
    })
    .map_err(|e| MnemeError::Storage(format!("encode relation: {e}")))?;
    out.extend_from_slice(&body);
    Ok(out)
}

fn decode_value(bytes: &[u8]) -> Option<RelationValue> {
    match bytes.split_first() {
        Some((&VALUE_VERSION, body)) => postcard::from_bytes(body).ok(),
        _ => None,
    }
}

/// Parse a `rel:` row. `None` for a row this version cannot read,
/// which is skipped rather than failing the read.
fn decode_rel(key: &[u8], value: &[u8]) -> Option<Relation> {
    let rest = key.strip_prefix(REL_KEY_PREFIX)?;
    if rest.len() < 18 {
        return None;
    }
    let from = MemoryId(ulid::Ulid::from_bytes(
        <[u8; 16]>::try_from(&rest[..16]).ok()?,
    ));
    let kind = RelationType::from_code(rest[16])?;
    let target = RelationTarget::decode(rest[17], &rest[18..])?;
    let value = decode_value(value)?;
    Some(Relation {
        from,
        kind,
        target,
        reason: value.reason,
        created_at: value.created_at,
    })
}

/// Parse a `rrel:` row.
fn decode_rrel(key: &[u8], value: &[u8]) -> Option<Relation> {
    let rest = key.strip_prefix(RREL_KEY_PREFIX)?;
    // tag + at least one target byte + type + from.
    if rest.len() < 1 + 1 + 1 + 16 {
        return None;
    }
    let (head, from) = rest.split_at(rest.len() - 16);
    let (head, code) = head.split_at(head.len() - 1);
    let from = MemoryId(ulid::Ulid::from_bytes(<[u8; 16]>::try_from(from).ok()?));
    let kind = RelationType::from_code(code[0])?;
    let target = RelationTarget::decode(head[0], &head[1..])?;
    let value = decode_value(value)?;
    Some(Relation {
        from,
        kind,
        target,
        reason: value.reason,
        created_at: value.created_at,
    })
}

/// Write (or overwrite) one relation: the forward row, then the
/// reverse one. Does not check that either end exists; callers do.
pub async fn put(storage: &dyn Storage, relation: &Relation) -> Result<()> {
    let value = encode_value(&relation.reason, relation.created_at)?;
    storage
        .put(
            &rel_key(relation.from, relation.kind, &relation.target),
            &value,
        )
        .await?;
    storage
        .put(
            &rrel_key(relation.from, relation.kind, &relation.target),
            &value,
        )
        .await
}

/// Delete one relation, both rows.
pub async fn delete(storage: &dyn Storage, relation: &Relation) -> Result<()> {
    storage
        .delete(&rrel_key(relation.from, relation.kind, &relation.target))
        .await?;
    storage
        .delete(&rel_key(relation.from, relation.kind, &relation.target))
        .await
}

/// Every relation `from` carries, as stored (targets not checked).
pub async fn outgoing(storage: &dyn Storage, from: MemoryId) -> Result<Vec<Relation>> {
    Ok(storage
        .scan_prefix(&rel_prefix(from))
        .await?
        .iter()
        .filter_map(|(k, v)| decode_rel(k, v))
        .collect())
}

/// Every relation into `target` whose source memory still exists.
pub async fn incoming(storage: &dyn Storage, target: &RelationTarget) -> Result<Vec<Relation>> {
    let mut out = Vec::new();
    for (k, v) in storage.scan_prefix(&rrel_prefix(target)).await? {
        let Some(relation) = decode_rrel(&k, &v) else {
            continue;
        };
        // A reference's prefix also matches longer references.
        if relation.target != *target {
            continue;
        }
        if !memory_exists(storage, relation.from).await? {
            continue;
        }
        out.push(relation);
    }
    Ok(out)
}

/// Remove everything `id` takes part in: the relations it carries and
/// the ones pointing at it. For `forget`.
pub async fn delete_all_for(storage: &dyn Storage, id: MemoryId) -> Result<()> {
    for relation in outgoing(storage, id).await? {
        delete(storage, &relation).await?;
    }
    let target = RelationTarget::Memory(id);
    for (k, v) in storage.scan_prefix(&rrel_prefix(&target)).await? {
        match decode_rrel(&k, &v) {
            Some(relation) if relation.target == target => delete(storage, &relation).await?,
            // Unreadable, but it is in this memory's key range.
            _ => storage.delete(&k).await?,
        }
    }
    Ok(())
}

/// Every stored relation, for building the in-memory side index.
pub async fn scan_all(storage: &dyn Storage) -> Result<Vec<Relation>> {
    Ok(storage
        .scan_prefix(REL_KEY_PREFIX)
        .await?
        .iter()
        .filter_map(|(k, v)| decode_rel(k, v))
        .collect())
}

async fn memory_exists(storage: &dyn Storage, id: MemoryId) -> Result<bool> {
    let mut k = Vec::with_capacity(MEM_KEY_PREFIX.len() + 16);
    k.extend_from_slice(MEM_KEY_PREFIX);
    k.extend_from_slice(&id.0.to_bytes());
    Ok(storage.get(&k).await?.is_some())
}

// ---------- provenance ----------

/// How the writer came to know a memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Provenance {
    /// The user said it.
    UserStated,
    /// The agent saw it (a file, a tool result, a log).
    Observed,
    /// The agent concluded it.
    Inferred,
    /// Brought in from another system.
    Imported,
}

impl Provenance {
    pub const ALL: [Self; 4] = [
        Self::UserStated,
        Self::Observed,
        Self::Inferred,
        Self::Imported,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UserStated => "user_stated",
            Self::Observed => "observed",
            Self::Inferred => "inferred",
            Self::Imported => "imported",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }
}

/// A memory's provenance row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceRecord {
    pub provenance: Provenance,
    /// Where it came from, free-form: `telegram:…`, `research:…`,
    /// `tool:…`. At most [`MAX_SOURCE_CHARS`] characters.
    pub source: Option<String>,
}

fn prov_key(id: MemoryId) -> Vec<u8> {
    let mut k = Vec::with_capacity(PROV_KEY_PREFIX.len() + 16);
    k.extend_from_slice(PROV_KEY_PREFIX);
    k.extend_from_slice(&id.0.to_bytes());
    k
}

pub async fn put_provenance(
    storage: &dyn Storage,
    id: MemoryId,
    record: &ProvenanceRecord,
) -> Result<()> {
    let mut value = vec![VALUE_VERSION];
    value.extend_from_slice(
        &postcard::to_allocvec(record)
            .map_err(|e| MnemeError::Storage(format!("encode provenance: {e}")))?,
    );
    storage.put(&prov_key(id), &value).await
}

/// The provenance of `id`. `None` when none was recorded (every memory
/// written before 1.6, and any written without the argument since).
pub async fn get_provenance(
    storage: &dyn Storage,
    id: MemoryId,
) -> Result<Option<ProvenanceRecord>> {
    Ok(storage
        .get(&prov_key(id))
        .await?
        .and_then(|bytes| decode_provenance(&bytes)))
}

pub async fn delete_provenance(storage: &dyn Storage, id: MemoryId) -> Result<()> {
    storage.delete(&prov_key(id)).await
}

/// Every provenance row, for the side index and for `stats`.
pub async fn scan_provenance(storage: &dyn Storage) -> Result<Vec<(MemoryId, ProvenanceRecord)>> {
    let mut out = Vec::new();
    for (k, v) in storage.scan_prefix(PROV_KEY_PREFIX).await? {
        let Some(id) = k
            .strip_prefix(PROV_KEY_PREFIX)
            .and_then(|rest| <[u8; 16]>::try_from(rest).ok())
        else {
            continue;
        };
        if let Some(record) = decode_provenance(&v) {
            out.push((MemoryId(ulid::Ulid::from_bytes(id)), record));
        }
    }
    Ok(out)
}

fn decode_provenance(bytes: &[u8]) -> Option<ProvenanceRecord> {
    match bytes.split_first() {
        Some((&VALUE_VERSION, body)) => postcard::from_bytes(body).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::memory_impl::MemoryStorage;
    use std::sync::Arc;

    fn now() -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap()
    }

    async fn with_memory(storage: &dyn Storage) -> MemoryId {
        let id = MemoryId::new();
        let mut k = MEM_KEY_PREFIX.to_vec();
        k.extend_from_slice(&id.0.to_bytes());
        storage.put(&k, b"x").await.unwrap();
        id
    }

    fn rel(from: MemoryId, kind: RelationType, target: RelationTarget, reason: &str) -> Relation {
        Relation {
            from,
            kind,
            target,
            reason: reason.into(),
            created_at: now(),
        }
    }

    #[tokio::test]
    async fn every_target_kind_round_trips_both_ways() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let s = storage.as_ref();
        let from = with_memory(s).await;
        let other = with_memory(s).await;
        let targets = [
            RelationTarget::Memory(other),
            RelationTarget::Event(EventId::new()),
            RelationTarget::Ref("research:pricing-study".into()),
        ];
        for (i, target) in targets.iter().enumerate() {
            let r = rel(
                from,
                RelationType::EvidenceFor,
                target.clone(),
                &format!("why {i}"),
            );
            put(s, &r).await.unwrap();
            assert_eq!(incoming(s, target).await.unwrap(), vec![r]);
        }
        let out = outgoing(s, from).await.unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(scan_all(s).await.unwrap().len(), 3);
        // Same key written again replaces the reason.
        let again = rel(
            from,
            RelationType::EvidenceFor,
            targets[2].clone(),
            "changed",
        );
        put(s, &again).await.unwrap();
        assert_eq!(incoming(s, &targets[2]).await.unwrap(), vec![again]);
        assert_eq!(outgoing(s, from).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_ref_does_not_match_a_longer_ref_with_the_same_prefix() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let s = storage.as_ref();
        let from = with_memory(s).await;
        let short = RelationTarget::Ref("research:a".into());
        let long = RelationTarget::Ref("research:ab".into());
        put(s, &rel(from, RelationType::EvidenceFor, short.clone(), ""))
            .await
            .unwrap();
        put(s, &rel(from, RelationType::EvidenceFor, long.clone(), ""))
            .await
            .unwrap();
        assert_eq!(incoming(s, &short).await.unwrap().len(), 1);
        assert_eq!(incoming(s, &short).await.unwrap()[0].target, short);
        assert_eq!(incoming(s, &long).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_relation_from_a_missing_memory_is_not_reported() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let s = storage.as_ref();
        let target = with_memory(s).await;
        // The source was never stored (or is gone).
        let ghost = MemoryId::new();
        put(
            s,
            &rel(
                ghost,
                RelationType::Refines,
                RelationTarget::Memory(target),
                "x",
            ),
        )
        .await
        .unwrap();
        assert!(
            incoming(s, &RelationTarget::Memory(target))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn delete_all_for_clears_both_directions() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let s = storage.as_ref();
        let a = with_memory(s).await;
        let b = with_memory(s).await;
        let c = with_memory(s).await;
        put(
            s,
            &rel(a, RelationType::Refines, RelationTarget::Memory(b), ""),
        )
        .await
        .unwrap();
        put(
            s,
            &rel(
                b,
                RelationType::EvidenceFor,
                RelationTarget::Ref("r:1".into()),
                "",
            ),
        )
        .await
        .unwrap();
        put(
            s,
            &rel(c, RelationType::Contradicts, RelationTarget::Memory(a), ""),
        )
        .await
        .unwrap();

        delete_all_for(s, b).await.unwrap();
        // b's outgoing and the a→b relation are gone; c→a is untouched.
        let left = scan_all(s).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].from, c);
        assert_eq!(s.scan_prefix(RREL_KEY_PREFIX).await.unwrap().len(), 1);
    }

    #[test]
    fn validation_enforces_limits_and_target_kinds() {
        let mem = RelationTarget::Memory(MemoryId::new());
        let event = RelationTarget::Event(EventId::new());
        let reference = RelationTarget::Ref("path:/x".into());
        assert!(validate(RelationType::Refines, &mem, "ok").is_ok());
        assert!(validate(RelationType::EvidenceFor, &event, "").is_ok());
        assert!(validate(RelationType::EvidenceFor, &reference, "").is_ok());
        assert!(validate(RelationType::Refines, &reference, "").is_err());
        assert!(validate(RelationType::Supersedes, &event, "").is_err());
        assert!(validate(RelationType::Refines, &mem, &"r".repeat(301)).is_err());
        assert!(validate(RelationType::Refines, &mem, &"я".repeat(300)).is_ok());
        let long = RelationTarget::Ref("x".repeat(129));
        assert!(validate(RelationType::EvidenceFor, &long, "").is_err());
        let blank = RelationTarget::Ref("  ".into());
        assert!(validate(RelationType::EvidenceFor, &blank, "").is_err());
    }

    #[test]
    fn names_round_trip() {
        for t in RelationType::ALL {
            assert_eq!(RelationType::parse(t.as_str()), Some(t));
            assert_eq!(RelationType::from_code(t.code()), Some(t));
        }
        assert_eq!(RelationType::parse("references"), None);
        for p in Provenance::ALL {
            assert_eq!(Provenance::parse(p.as_str()), Some(p));
        }
        assert_eq!(Provenance::parse("guessed"), None);
    }

    #[tokio::test]
    async fn provenance_round_trips_and_ignores_unknown_versions() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let s = storage.as_ref();
        let id = MemoryId::new();
        assert_eq!(get_provenance(s, id).await.unwrap(), None);
        let record = ProvenanceRecord {
            provenance: Provenance::Inferred,
            source: Some("research:pricing".into()),
        };
        put_provenance(s, id, &record).await.unwrap();
        assert_eq!(get_provenance(s, id).await.unwrap(), Some(record.clone()));
        assert_eq!(scan_provenance(s).await.unwrap(), vec![(id, record)]);
        // The documented layout: a version byte, then postcard.
        let raw = s.get(&prov_key(id)).await.unwrap().unwrap();
        assert_eq!(raw[0], VALUE_VERSION);
        // A future version is skipped, not misread.
        let other = MemoryId::new();
        s.put(&prov_key(other), &[9, 1, 2, 3]).await.unwrap();
        assert_eq!(get_provenance(s, other).await.unwrap(), None);
        delete_provenance(s, id).await.unwrap();
        assert_eq!(get_provenance(s, id).await.unwrap(), None);
    }
}
