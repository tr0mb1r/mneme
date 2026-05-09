//! `mneme init <agent>` framework — release-planning v2.1 §4
//! (B.M1 onward). Per-agent integration commands ship in B.M2-M4;
//! this module owns the cross-agent primitives:
//!
//! - [`marker`] — find / upsert / remove mneme's managed block in a
//!   user's primary instructions file (CLAUDE.md, .cursorrules,
//!   AGENTS.md, etc.) without touching surrounding content.
//! - (later) `template` — load and render the MNEME.md content the
//!   `init` command writes per integration.
//! - (later) `config_merge` — surgically add / update mneme entries
//!   in JSON / TOML / YAML config files (settings.json,
//!   claude_desktop_config.json, etc.) preserving the user's other
//!   entries.
//!
//! All primitives are agent-agnostic. Per-agent specifics (what to
//! write, where) live in `src/cli/init.rs` once B.M2 lands the
//! Claude Code reference implementation.

pub mod marker;
