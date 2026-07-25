//! Phase 6 §6 — process-lifetime "current scope" state.
//!
//! Mneme tools all accept an optional `scope` argument. When the
//! caller omits it the tool falls back to a default; pre-v0.15 that
//! default was the hardcoded constant `"personal"`. The `switch_scope`
//! tool changes the default for the rest of the session, so a user
//! can do `switch_scope("work")` once and have subsequent
//! `remember`/`recall`/`pin`/etc. land in `work` without typing the
//! arg every time.
//!
//! # Lifetime
//!
//! Process-lifetime only. Each `mneme run` boots with the
//! `[scopes] default` from `config.toml`; `switch_scope` mutates the
//! in-memory cell; restart resets to the config default. We
//! deliberately don't persist the cell on disk — the user's choice
//! to switch is a per-session intent, and resuming "work" mode after
//! a crash would be surprising. (Per spec §14 #5 the default scope
//! is intentionally predictable.)
//!
//! # Why not on `ActiveSession`?
//!
//! Could have been; both are process-lifetime. Splitting them keeps
//! the L1 working-session story (turn log + checkpoint) separate
//! from the L0/L3/L4 scope-routing story. Tools that need scope
//! (`remember`, `recall`, ...) take an `Arc<ScopeState>` directly
//! without dragging in the working-session API surface.

use std::sync::Arc;
use std::sync::RwLock;

/// In-memory cell holding the active default scope. Writes via
/// `switch_scope`; reads on every scope-using tool's argument
/// fall-back path.
#[derive(Debug)]
pub struct ScopeState {
    inner: RwLock<String>,
}

impl ScopeState {
    /// Construct with the user-configured default scope (typically
    /// `"personal"` from `config.toml`'s `[scopes] default`).
    pub fn new(default_scope: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(default_scope.into()),
        })
    }

    /// Current scope. Returns an owned `String` because callers
    /// often need to pass it into async store calls; we don't want
    /// to leak the lock guard across `await` boundaries.
    pub fn current(&self) -> String {
        self.inner.read().expect("scope lock poisoned").clone()
    }

    /// Replace the active scope. Empty/whitespace-only inputs are
    /// rejected so tools can surface the validation error to the
    /// caller before we mutate state.
    pub fn set(&self, new_scope: impl Into<String>) -> Result<(), &'static str> {
        let s: String = new_scope.into();
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err("scope must not be empty");
        }
        let mut g = self.inner.write().expect("scope lock poisoned");
        *g = trimmed.to_owned();
        Ok(())
    }
}

/// Environment variable that overrides `[scopes] default` for the
/// lifetime of the process.
///
/// The point is per-project memory without the agent having to
/// remember `switch_scope`: a repo-local MCP config can set
/// `"env": {"MNEME_SCOPE": "myproj"}` on its `mneme run` entry and
/// every write from that host lands in `myproj`.
///
/// **Only affects `mneme run`.** In daemon mode the host spawns
/// `mneme client`, which is a byte pipe — it never builds a
/// `ScopeState`, and the daemon it connects to has its own
/// environment. Use [`scope_from_roots`] (driven by the MCP
/// `initialize` handshake, which *is* per-connection) for daemon-mode
/// per-project scoping.
pub const SCOPE_ENV_VAR: &str = "MNEME_SCOPE";

/// Longest derived scope we will accept. Long enough for any real
/// directory name, short enough that a pathological root can't bloat
/// every stored row's scope field.
const MAX_DERIVED_SCOPE_LEN: usize = 64;

/// Resolve the boot-time default scope: [`SCOPE_ENV_VAR`] if set and
/// non-blank, otherwise the configured default.
pub fn boot_default_scope(configured: &str) -> String {
    match std::env::var(SCOPE_ENV_VAR) {
        Ok(v) if !v.trim().is_empty() => {
            let sanitized = sanitize_scope(&v);
            match sanitized {
                Some(s) => s,
                None => {
                    tracing::warn!(
                        raw = %v,
                        "MNEME_SCOPE contained no usable characters; falling back to the configured default"
                    );
                    configured.to_owned()
                }
            }
        }
        _ => configured.to_owned(),
    }
}

/// Derive a scope name from the MCP client's declared workspace roots.
///
/// Takes the first root's `name` when the client supplies one,
/// otherwise the last path segment of its `uri`. Returns `None` when
/// there are no roots or nothing survives sanitisation, in which case
/// the caller keeps whatever default it already had.
///
/// Roots reach us through the `initialize` params. The spec's canonical
/// route is a server→client `roots/list` request, which mneme cannot
/// issue (the transport is request-response in one direction only), but
/// hosts that volunteer roots in the handshake get per-project scoping
/// for free — and because `initialize` is per-connection, this works in
/// daemon mode where [`SCOPE_ENV_VAR`] cannot.
pub fn scope_from_roots(roots: &serde_json::Value) -> Option<String> {
    let first = roots.as_array()?.first()?;
    if let Some(name) = first.get("name").and_then(|v| v.as_str())
        && let Some(s) = sanitize_scope(name)
    {
        return Some(s);
    }
    let uri = first.get("uri").and_then(|v| v.as_str())?;
    let path = uri.strip_prefix("file://").unwrap_or(uri);
    let last = path.trim_end_matches('/').rsplit('/').next()?;
    sanitize_scope(last)
}

/// Reduce arbitrary text to a scope name: lowercased, with runs of
/// unsupported characters collapsed to single `-`.
///
/// Scopes are free-form strings everywhere else in mneme, so this is
/// not a validity requirement — it exists so a *derived* scope is
/// predictable and typeable. A user who wants `My Project (v2)` as a
/// scope can still `switch_scope` to it explicitly.
fn sanitize_scope(raw: &str) -> Option<String> {
    // Split on every character we don't keep, then rejoin the surviving
    // tokens with a single `-`. Tokenising rather than streaming avoids
    // the double-separator artefacts a char-at-a-time filter produces
    // (e.g. `café-app`, where the dropped `é` sits next to a real `-`).
    let tokens: Vec<String> = raw
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-')))
        .map(|tok| tok.trim_matches(['-', '.', '_']).to_ascii_lowercase())
        .filter(|tok| !tok.is_empty())
        .collect();
    if tokens.is_empty() {
        return None;
    }

    let mut out = String::with_capacity(MAX_DERIVED_SCOPE_LEN);
    for tok in tokens {
        if !out.is_empty() {
            if out.len() + 1 >= MAX_DERIVED_SCOPE_LEN {
                break;
            }
            out.push('-');
        }
        let room = MAX_DERIVED_SCOPE_LEN - out.len();
        if tok.len() <= room {
            out.push_str(&tok);
        } else {
            out.push_str(&tok[..room]);
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_owned();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_initialises_to_provided_default() {
        let s = ScopeState::new("personal");
        assert_eq!(s.current(), "personal");
    }

    #[test]
    fn set_replaces_current() {
        let s = ScopeState::new("personal");
        s.set("work").unwrap();
        assert_eq!(s.current(), "work");
        s.set("home").unwrap();
        assert_eq!(s.current(), "home");
    }

    #[test]
    fn set_trims_whitespace() {
        let s = ScopeState::new("personal");
        s.set("  client-x  ").unwrap();
        assert_eq!(s.current(), "client-x");
    }

    #[test]
    fn set_rejects_empty() {
        let s = ScopeState::new("personal");
        assert!(s.set("").is_err());
        assert!(s.set("   ").is_err());
        // Original value preserved on rejection.
        assert_eq!(s.current(), "personal");
    }

    // ---------- roots-derived scope ----------

    #[test]
    fn scope_from_roots_prefers_the_declared_name() {
        let roots = serde_json::json!([
            { "uri": "file:///home/user/code/some-dir", "name": "My Project" }
        ]);
        assert_eq!(scope_from_roots(&roots).as_deref(), Some("my-project"));
    }

    #[test]
    fn scope_from_roots_falls_back_to_the_uri_basename() {
        let roots = serde_json::json!([{ "uri": "file:///home/user/code/mneme" }]);
        assert_eq!(scope_from_roots(&roots).as_deref(), Some("mneme"));
    }

    #[test]
    fn scope_from_roots_tolerates_trailing_slash_and_plain_paths() {
        let with_slash = serde_json::json!([{ "uri": "file:///home/user/code/mneme/" }]);
        assert_eq!(scope_from_roots(&with_slash).as_deref(), Some("mneme"));
        let no_scheme = serde_json::json!([{ "uri": "/srv/apps/billing" }]);
        assert_eq!(scope_from_roots(&no_scheme).as_deref(), Some("billing"));
    }

    #[test]
    fn scope_from_roots_uses_only_the_first_root() {
        let roots = serde_json::json!([
            { "uri": "file:///a/first" },
            { "uri": "file:///b/second" },
        ]);
        assert_eq!(scope_from_roots(&roots).as_deref(), Some("first"));
    }

    #[test]
    fn scope_from_roots_returns_none_when_unusable() {
        assert_eq!(scope_from_roots(&serde_json::json!([])), None);
        assert_eq!(scope_from_roots(&serde_json::json!(null)), None);
        assert_eq!(scope_from_roots(&serde_json::json!("not-an-array")), None);
        // No `uri` and no `name`.
        assert_eq!(scope_from_roots(&serde_json::json!([{ "x": 1 }])), None);
        // A name of pure punctuation sanitises away, and there is no
        // uri to fall back to.
        assert_eq!(
            scope_from_roots(&serde_json::json!([{ "name": "***" }])),
            None
        );
    }

    /// A name that sanitises to nothing must fall through to the uri
    /// rather than yielding `None` and losing the derivation.
    #[test]
    fn scope_from_roots_falls_through_an_unusable_name() {
        let roots = serde_json::json!([
            { "uri": "file:///home/user/code/fallback-dir", "name": "///" }
        ]);
        assert_eq!(scope_from_roots(&roots).as_deref(), Some("fallback-dir"));
    }

    #[test]
    fn sanitize_collapses_separators_and_lowercases() {
        assert_eq!(
            sanitize_scope("Client X / Phase 2").as_deref(),
            Some("client-x-phase-2")
        );
        assert_eq!(sanitize_scope("  padded  ").as_deref(), Some("padded"));
        assert_eq!(
            sanitize_scope("keep_under.score-dash").as_deref(),
            Some("keep_under.score-dash")
        );
        assert_eq!(sanitize_scope("").as_deref(), None);
        assert_eq!(sanitize_scope("!!!").as_deref(), None);
    }

    #[test]
    fn sanitize_caps_length() {
        let long = "a".repeat(500);
        let out = sanitize_scope(&long).unwrap();
        assert!(
            out.len() <= MAX_DERIVED_SCOPE_LEN,
            "got {} chars",
            out.len()
        );
    }

    #[test]
    fn sanitize_handles_non_ascii_without_panicking() {
        // Non-ASCII is dropped as a separator; what remains is usable.
        assert_eq!(sanitize_scope("café-app").as_deref(), Some("caf-app"));
        // All non-ASCII sanitises away rather than producing junk.
        assert_eq!(sanitize_scope("日本語").as_deref(), None);
    }

    /// `boot_default_scope` reads a process-global env var, so this
    /// test must not run concurrently with another that touches it.
    /// Both cases live in one test for that reason.
    #[test]
    fn boot_default_scope_prefers_the_env_override() {
        // Unset: configured value wins.
        unsafe { std::env::remove_var(SCOPE_ENV_VAR) };
        assert_eq!(boot_default_scope("global"), "global");

        // Set: override wins, sanitised.
        unsafe { std::env::set_var(SCOPE_ENV_VAR, "My Repo") };
        assert_eq!(boot_default_scope("global"), "my-repo");

        // Blank: treated as unset.
        unsafe { std::env::set_var(SCOPE_ENV_VAR, "   ") };
        assert_eq!(boot_default_scope("global"), "global");

        // Unusable: falls back rather than yielding an empty scope.
        unsafe { std::env::set_var(SCOPE_ENV_VAR, "***") };
        assert_eq!(boot_default_scope("global"), "global");

        unsafe { std::env::remove_var(SCOPE_ENV_VAR) };
    }

    #[test]
    fn arc_clones_share_state() {
        let s = ScopeState::new("personal");
        let s2 = Arc::clone(&s);
        s.set("work").unwrap();
        assert_eq!(s2.current(), "work");
    }
}
