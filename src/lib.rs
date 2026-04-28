//! # Memory schema overview
//!
//! Three first-class memory layers, each with its own struct, share
//! the same field vocabulary so cross-layer code (export, backup,
//! orchestrator assembly) can reason uniformly:
//!
//! ```text
//!                       semantic        episodic         procedural
//!                       (L4)            (L3 hot/warm)    (L0 pinned)
//!                       MemoryItem      EpisodicEvent    PinnedItem
//!  --------------------------------------------------------------------
//!  id                   MemoryId        EventId          MemoryId
//!  content / payload    content         payload (JSON-   content
//!                       (text)          encoded text)    (text)
//!  kind                 MemoryKind      String           — *
//!                       (enum: fact /   (free-form
//!                        decision /     "tool_call",
//!                        preference /   "user_message",
//!                        conversation)  "checkpoint", …)
//!  tags                 Vec<String>     Vec<String>      Vec<String>
//!  scope                String          String           String
//!  retrieval_weight     —               f32 ∈ [0,1]      — *
//!  last_accessed        — *             DateTime<Utc>    — *
//!  created_at           DateTime<Utc>   DateTime<Utc>    DateTime<Utc>
//! ```
//!
//! Fields marked `—` are intentionally absent for that layer's
//! semantics. Fields marked `*` are deferred Phase-7 polish:
//! `last_accessed` on semantic + procedural would require touch-on-
//! read plumbing through every read path; `kind` on procedural is
//! redundant today (pinned items are typically preferences/decisions
//! the user hand-edits). Both can land later without breaking the
//! current schema.
//!
//! ## JSON canonical field names
//!
//! Every tool / resource that emits one of these structs uses the
//! struct field name verbatim. In particular:
//!
//! * `kind`, **never** `type`. Earlier versions of the `recall` tool
//!   emitted `"type"` for `MemoryKind`; that was renamed for
//!   consistency with `EpisodicEvent.kind`. Input filters still
//!   accept `"type"` for client-compat.
//! * `created_at`, **never** `added_at`. Earlier versions of
//!   `PinnedItem` used `added_at` on disk; the field is
//!   `#[serde(alias = "added_at")]` so old `pinned.jsonl` files keep
//!   loading for one release cycle.
//!
//! Tests under `tests/e2e_mcp.rs` and the per-tool/per-resource
//! `#[cfg(test)]` modules exercise the JSON shape; if you change a
//! field name you'll see a red bar there.

pub mod cli;
pub mod config;
pub mod embed;
pub mod error;
pub mod ids;
pub mod index;
pub mod mcp;
pub mod memory;
pub mod migrate;
pub mod orchestrator;
pub mod storage;

pub use error::{MnemeError, Result};
