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

    #[test]
    fn arc_clones_share_state() {
        let s = ScopeState::new("personal");
        let s2 = Arc::clone(&s);
        s.set("work").unwrap();
        assert_eq!(s2.current(), "work");
    }
}
