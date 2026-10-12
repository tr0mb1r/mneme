//! Secret guard for the write tools (`remember`, `update`, `pin`,
//! `record_event`).
//!
//! An agent can be told "never store secrets in mneme" and still do it:
//! a rule in a pin is advice, not enforcement. This module is the
//! enforcement. It scans content on its way in and, for credentials in
//! a handful of unmistakable formats, refuses the write.
//!
//! # Design: high confidence only
//!
//! A false positive here blocks a legitimate write and teaches the agent
//! to route around the guard, so the reject list is deliberately narrow:
//! formats with a fixed prefix and a long random body, anchored on a
//! word boundary. Everything else is out of scope. A looser
//! `password: <value>` style pattern exists, but it is **warn-only** and
//! never rejects, whatever the configured mode. Prose such as "the
//! office wifi password rotates monthly" matches neither.
//!
//! # Modes (`[guardrails] secrets`)
//!
//! | mode     | high-confidence match            | generic match |
//! |----------|----------------------------------|---------------|
//! | `reject` | write refused (`secret_detected`) | stored, `secret_advisory` |
//! | `warn`   | stored, `secret_advisory`        | stored, `secret_advisory` |
//! | `off`    | no scan                          | no scan       |
//!
//! A rejection is a soft error shaped like the size-tier one
//! (`isError: true` plus `_meta.error.code`), so the automatic
//! `tool_call_failed` event records only the code. Neither the reply nor
//! the logs ever contain the matched text, only the pattern name and the
//! character offset.
//!
//! In `warn` mode the stored write also carries the advisory in the
//! reply text (some hosts never show `_meta`), and the server's
//! automatic `tool_call` event for that call omits the content, so a
//! warned secret does not get a second copy in the episodic log.

use std::sync::LazyLock;

use regex::{Regex, RegexSet};
use serde_json::{Value, json};

use super::{ContentBlock, ToolError, ToolResult};
pub use crate::config::SecretsMode;

/// One compiled detector.
struct Detector {
    name: &'static str,
    re: Regex,
    /// The matched text must also contain an ASCII digit. Used for the
    /// `sk-` family, where plain hyphenated words ("sk-learn-pipeline-
    /// configuration") would otherwise look like keys. The body must
    /// also be 32+ characters (real keys are 48+), which keeps branch
    /// names, ticket slugs and docs placeholders out of the net.
    require_digit: bool,
    /// High-confidence detectors reject; the others only warn.
    high_confidence: bool,
}

/// `(name, regex, require_digit, high_confidence)`.
const SPECS: &[(&str, &str, bool, bool)] = &[
    ("aws_access_key_id", r"\bAKIA[0-9A-Z]{16}\b", false, true),
    (
        "pem_private_key",
        r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----",
        false,
        true,
    ),
    (
        "jwt",
        r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
        false,
        true,
    ),
    ("github_token", r"\bgh[pousr]_[A-Za-z0-9]{36,}", false, true),
    (
        "github_fine_grained_token",
        r"\bgithub_pat_[A-Za-z0-9_]{22,}",
        false,
        true,
    ),
    (
        "slack_token",
        r"\bxox[baprs]-\d+-[A-Za-z0-9-]{8,}",
        false,
        true,
    ),
    (
        "openai_or_anthropic_key",
        r"\bsk-(?:ant-|proj-|svcacct-|admin-)?[A-Za-z0-9_-]{32,}",
        true,
        true,
    ),
    ("google_api_key", r"\bAIza[0-9A-Za-z_-]{35}", false, true),
    (
        "stripe_key",
        r"\b[sr]k_(?:live|test)_[0-9A-Za-z]{16,}",
        false,
        true,
    ),
    (
        "age_secret_key",
        r"\bAGE-SECRET-KEY-1[A-Z0-9]{50,}",
        false,
        true,
    ),
    // Warn-only. Deliberately last, so a high-confidence hit wins.
    (
        "generic_credential_assignment",
        r#"(?i)(?:password|passwd|secret|token|api[_-]?key)["']?\s*[:=]\s*["']?\S{8,}"#,
        false,
        false,
    ),
];

/// All detectors plus a [`RegexSet`] over the same patterns, in
/// [`SPECS`] order. The set answers "which patterns hit anywhere?" in a
/// single pass, so a clean string costs one scan instead of one per
/// pattern; only the patterns it reports are re-run for the match
/// position.
struct Detectors {
    list: Vec<Detector>,
    set: RegexSet,
}

static DETECTORS: LazyLock<Result<Detectors, String>> = LazyLock::new(|| {
    let list = SPECS
        .iter()
        .map(|&(name, pattern, require_digit, high_confidence)| {
            Regex::new(pattern)
                .map(|re| Detector {
                    name,
                    re,
                    require_digit,
                    high_confidence,
                })
                .map_err(|e| format!("secret guard pattern `{name}` failed to compile: {e}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let set = RegexSet::new(SPECS.iter().map(|&(_, pattern, _, _)| pattern))
        .map_err(|e| format!("secret guard pattern set failed to compile: {e}"))?;
    Ok(Detectors { list, set })
});

fn detectors() -> Result<&'static Detectors, ToolError> {
    match &*DETECTORS {
        Ok(d) => Ok(d),
        Err(e) => Err(ToolError::Internal(e.clone())),
    }
}

/// One match. Never carries the matched text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub pattern: &'static str,
    /// Offset of the match start, in characters (not bytes), within the
    /// scanned string.
    pub offset: usize,
    pub high_confidence: bool,
    /// For JSON values: the location of the string that matched
    /// (`""` for a top-level string, `"/content"` for a field).
    pub path: String,
}

/// Scan one string. Returns the first high-confidence finding if there
/// is one, otherwise the first generic finding.
pub fn scan(text: &str) -> Result<Option<Finding>, ToolError> {
    let detectors = detectors()?;
    let matched = detectors.set.matches(text);
    if !matched.matched_any() {
        return Ok(None);
    }
    let mut generic: Option<Finding> = None;
    // `SetMatches::iter` yields indexes in ascending (SPECS) order, so
    // high-confidence detectors are tried before the generic one.
    for idx in matched.iter() {
        let d = &detectors.list[idx];
        if generic.is_some() && !d.high_confidence {
            continue;
        }
        let hit =
            d.re.find_iter(text)
                .find(|m| !d.require_digit || m.as_str().bytes().any(|b| b.is_ascii_digit()));
        if let Some(m) = hit {
            let finding = Finding {
                pattern: d.name,
                offset: text[..m.start()].chars().count(),
                high_confidence: d.high_confidence,
                path: String::new(),
            };
            if d.high_confidence {
                return Ok(Some(finding));
            }
            generic = Some(finding);
        }
    }
    Ok(generic)
}

/// One step of a JSON pointer, borrowed from the value being scanned.
#[derive(Clone, Copy)]
enum Seg<'a> {
    Index(usize),
    Key(&'a str),
}

/// Render the pointer for the arena node `node` (RFC 6901 escaping).
/// Only called when a pattern hits, so clean values never pay for it.
fn pointer(arena: &[(Option<usize>, Seg<'_>)], node: Option<usize>) -> String {
    let mut segs = Vec::new();
    let mut cur = node;
    while let Some(i) = cur {
        let (parent, seg) = arena[i];
        segs.push(seg);
        cur = parent;
    }
    let mut out = String::new();
    for seg in segs.into_iter().rev() {
        out.push('/');
        match seg {
            Seg::Index(i) => out.push_str(&i.to_string()),
            Seg::Key(k) => out.push_str(&k.replace('~', "~0").replace('/', "~1")),
        }
    }
    out
}

/// Scan every string inside a JSON value (leaves only; object keys are
/// the caller's schema, not user data). Scanning leaves rather than the
/// serialised form keeps `\n` escapes from defeating word boundaries
/// and keeps offsets meaningful.
///
/// The traversal records each container step in a small arena and only
/// renders the JSON pointer for a leaf that actually matches, so a clean
/// payload costs no per-leaf string building.
pub fn scan_value(value: &Value) -> Result<Option<Finding>, ToolError> {
    let mut generic: Option<Finding> = None;
    // `(parent node, step)`; a node is the path from the root to a child.
    let mut arena: Vec<(Option<usize>, Seg<'_>)> = Vec::new();
    // `(node for this value, value)`; `None` is the root.
    let mut stack: Vec<(Option<usize>, &Value)> = vec![(None, value)];
    while let Some((node, v)) = stack.pop() {
        match v {
            Value::String(s) => {
                if let Some(mut f) = scan(s)? {
                    f.path = pointer(&arena, node);
                    if f.high_confidence {
                        return Ok(Some(f));
                    }
                    generic.get_or_insert(f);
                }
            }
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate().rev() {
                    arena.push((node, Seg::Index(i)));
                    stack.push((Some(arena.len() - 1), item));
                }
            }
            Value::Object(map) => {
                for (k, item) in map.iter().rev() {
                    arena.push((node, Seg::Key(k)));
                    stack.push((Some(arena.len() - 1), item));
                }
            }
            _ => {}
        }
    }
    Ok(generic)
}

/// What the tool should do after consulting the guard.
#[derive(Debug)]
pub enum Guard {
    /// Nothing found, or the guard is off.
    Clean,
    /// Refuse the write: return this result as-is.
    Reject(ToolResult),
    /// Store, and attach this `_meta` fragment to the reply.
    Advisory(Value),
}

impl Guard {
    /// The advisory fragment, if any (for tools that handle `Reject`
    /// separately).
    pub fn into_advisory(self) -> Option<Value> {
        match self {
            Guard::Advisory(v) => Some(v),
            Guard::Clean | Guard::Reject(_) => None,
        }
    }
}

/// Apply `mode` to a plain-text field (`remember` / `update` / `pin`
/// `content`).
pub fn guard_text(mode: SecretsMode, text: &str) -> Result<Guard, ToolError> {
    if mode == SecretsMode::Off {
        return Ok(Guard::Clean);
    }
    Ok(decide(mode, scan(text)?))
}

/// Apply `mode` to a JSON value (`record_event` `payload`).
pub fn guard_value(mode: SecretsMode, value: &Value) -> Result<Guard, ToolError> {
    if mode == SecretsMode::Off {
        return Ok(Guard::Clean);
    }
    Ok(decide(mode, scan_value(value)?))
}

fn decide(mode: SecretsMode, finding: Option<Finding>) -> Guard {
    let Some(f) = finding else {
        return Guard::Clean;
    };
    if f.high_confidence && mode == SecretsMode::Reject {
        tracing::warn!(
            pattern = f.pattern,
            offset = f.offset,
            "rejected: write contains what looks like a credential"
        );
        let text = format!(
            "Write rejected: the content looks like it contains a credential ({} at \
             character {}). Secrets must not be stored in mneme; store a pointer to \
             where the secret lives (vault item, env var name) instead. If this is a \
             false positive, set `[guardrails] secrets = \"warn\"` in config.toml.",
            f.pattern, f.offset
        );
        let mut error = json!({
            "code": "secret_detected",
            "pattern": f.pattern,
            "offset": f.offset,
        });
        if !f.path.is_empty() {
            error["path"] = json!(f.path);
        }
        return Guard::Reject(
            ToolResult::text(text)
                .with_error()
                .with_meta(json!({ "error": error })),
        );
    }
    let message = if f.high_confidence {
        "Stored, but the content looks like it contains a credential. Prefer storing a pointer to where the secret lives."
    } else {
        "Stored, but the content has a `password=`/`token:`-style assignment. If a real secret is in there, remove it with `forget` and store a pointer instead."
    };
    let mut advisory = json!({
        "pattern": f.pattern,
        "offset": f.offset,
        "high_confidence": f.high_confidence,
        "message": message,
    });
    if !f.path.is_empty() {
        advisory["path"] = json!(f.path);
    }
    Guard::Advisory(json!({ "secret_advisory": advisory }))
}

/// Merge an advisory fragment into a successful result's `_meta`,
/// alongside any size-tier advisory already there, and append a one-line
/// note to the reply text for hosts that never show `_meta`. `id` is the
/// stored item, named so the agent can `forget` it. The note carries the
/// pattern name, offset and (for structured payloads) the JSON path of
/// the matching string, never the matched text.
pub fn attach(
    mut result: ToolResult,
    advisory: Option<Value>,
    id: impl std::fmt::Display,
) -> ToolResult {
    if let Some(Value::Object(adv)) = &advisory
        && let Some(a) = adv.get("secret_advisory")
        && let Some(pattern) = a.get("pattern").and_then(Value::as_str)
    {
        let offset = a.get("offset").and_then(Value::as_u64).unwrap_or(0);
        let location = match a.get("path").and_then(Value::as_str) {
            Some(path) if !path.is_empty() => format!(" of {path}"),
            _ => String::new(),
        };
        let note = format!(
            "warning: content matched credential pattern {pattern} at char {offset}{location}; \
             forget {id} if this is a real secret"
        );
        match result.content.first_mut() {
            Some(ContentBlock::Text(t)) => {
                t.push('\n');
                t.push_str(&note);
            }
            None => result.content.push(ContentBlock::Text(note)),
        }
    }
    attach_meta(result, advisory)
}

/// Merge an advisory fragment into `_meta` only, leaving the reply text
/// alone. For replies where there is no stored item to `forget` (an
/// `update` of an unknown id) but the flag must still reach the
/// dispatcher, which keys its `tool_call` content redaction on it.
pub fn attach_meta(mut result: ToolResult, advisory: Option<Value>) -> ToolResult {
    let Some(Value::Object(adv)) = advisory else {
        return result;
    };
    let mut meta = match result.meta.take() {
        Some(Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    meta.extend(adv);
    result.meta = Some(Value::Object(meta));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Samples are assembled at runtime so this file does not itself
    /// trip repository secret scanners.
    fn samples() -> Vec<(&'static str, String)> {
        let b36 = "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
        vec![
            ("aws_access_key_id", format!("AKIA{}", "IOSFODNN7EXAMPLE")),
            (
                "pem_private_key",
                format!("-----BEGIN {} PRIVATE KEY-----", "RSA"),
            ),
            (
                "pem_private_key",
                format!("-----BEGIN {}PRIVATE KEY-----", ""),
            ),
            (
                "jwt",
                format!(
                    "{}.{}.{}",
                    "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
                    "eyJzdWIiOiIxMjM0NTY3ODkwIn0",
                    "dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U"
                ),
            ),
            ("github_token", format!("ghp_{b36}")),
            ("github_token", format!("gho_{b36}")),
            (
                "github_fine_grained_token",
                format!("github_pat_{}", "11ABCDEFG0abcdefghij_KLMNOPQRSTUV"),
            ),
            ("slack_token", format!("xoxb-{}", "123456789012-abcdefghij")),
            (
                "openai_or_anthropic_key",
                format!("sk-{}", "proj1234567890abcdefghijklmnopqrstuvwxyzABCDEFGH"),
            ),
            (
                "openai_or_anthropic_key",
                format!(
                    "sk-ant-{}",
                    "api03-AbCdEfGhIjKlMnOp0123qRsTuVwXyZ4567_8-9AbCdEfGhIjKl"
                ),
            ),
            (
                "google_api_key",
                format!("AIza{}", "SyA1B2C3D4E5F6G7H8I9J0K1L2M3N4O5P6Q"),
            ),
            (
                "stripe_key",
                format!("sk_live_{}", "4eC39HqLyjWDarjtT1zdp7dc"),
            ),
            (
                "stripe_key",
                format!("rk_test_{}", "4eC39HqLyjWDarjtT1zdp7dc"),
            ),
            (
                "age_secret_key",
                format!(
                    "AGE-SECRET-KEY-1{}",
                    "QQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ"
                ),
            ),
        ]
    }

    #[test]
    fn every_pattern_compiles() {
        let d = detectors().expect("all secret-guard patterns compile");
        assert_eq!(d.list.len(), SPECS.len());
        assert_eq!(d.set.len(), SPECS.len());
    }

    #[test]
    fn every_high_confidence_sample_is_found_with_its_name() {
        for (name, secret) in samples() {
            let text = format!("note: {secret} end");
            let f = scan(&text)
                .unwrap()
                .unwrap_or_else(|| panic!("{name} sample not detected"));
            assert_eq!(f.pattern, name, "wrong pattern for {name} sample");
            assert!(f.high_confidence);
            assert_eq!(f.offset, 6, "offset for {name}");
        }
    }

    #[test]
    fn reject_mode_refuses_each_pattern_without_echoing_the_secret() {
        for (name, secret) in samples() {
            let g = guard_text(SecretsMode::Reject, &format!("use {secret} please")).unwrap();
            let Guard::Reject(r) = g else {
                panic!("{name}: expected Reject");
            };
            assert!(r.is_error);
            let meta = r.meta.as_ref().unwrap();
            assert_eq!(meta["error"]["code"], "secret_detected");
            assert_eq!(meta["error"]["pattern"], name);
            assert_eq!(meta["error"]["offset"], 4);
            let dump = format!("{:?}{}", r.content, meta);
            assert!(
                !dump.contains(&secret[secret.len().saturating_sub(12)..]),
                "{name}: reply echoes the secret"
            );
        }
    }

    #[test]
    fn warn_mode_stores_with_advisory_for_each_pattern() {
        for (name, secret) in samples() {
            let g = guard_text(SecretsMode::Warn, &secret).unwrap();
            let Guard::Advisory(v) = g else {
                panic!("{name}: expected Advisory");
            };
            assert_eq!(v["secret_advisory"]["pattern"], name);
            assert_eq!(v["secret_advisory"]["offset"], 0);
        }
    }

    #[test]
    fn off_mode_skips_everything_including_generic() {
        for (_, secret) in samples() {
            assert!(matches!(
                guard_text(SecretsMode::Off, &secret).unwrap(),
                Guard::Clean
            ));
        }
        assert!(matches!(
            guard_text(SecretsMode::Off, "password = hunter2hunter2").unwrap(),
            Guard::Clean
        ));
    }

    #[test]
    fn generic_assignment_warns_but_never_rejects() {
        for text in [
            "password = hunter2hunter2",
            "db password: correct-horse-battery",
            "API_KEY=abcdef123456",
            "api-key: \"abcdef123456\"",
            "token=0123456789",
            "client_secret = 'xyzxyzxyz'",
        ] {
            for mode in [SecretsMode::Reject, SecretsMode::Warn] {
                match guard_text(mode, text).unwrap() {
                    Guard::Advisory(v) => {
                        assert_eq!(
                            v["secret_advisory"]["pattern"], "generic_credential_assignment",
                            "{text}"
                        );
                        assert_eq!(v["secret_advisory"]["high_confidence"], false);
                    }
                    other => panic!("{text} ({mode:?}): expected Advisory, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn high_confidence_wins_over_generic() {
        let text = format!("password = AKIA{}", "IOSFODNN7EXAMPLE");
        let f = scan(&text).unwrap().unwrap();
        assert_eq!(f.pattern, "aws_access_key_id");
    }

    #[test]
    fn clean_prose_passes() {
        for text in [
            "the office wifi password rotates monthly",
            "user prefers tabs over spaces",
            "rotate the API key every 90 days",
            "token budget is 4000 per request",
            "secret: n/a",
            "we use sk-learn-pipeline-configuration-helpers for modelling",
            "risk-assessment-framework-for-the-quarter",
            "ask-for-the-weekly-report-from-finance-team",
            "AKIA is the prefix of AWS access key ids",
            "an id like AKIAIOSFODNN7EXAMPLE12345 is too long to be an access key id",
            "eyJ is the base64 of {\"",
            "ghp_ prefix tokens are GitHub PATs",
            "AIza prefix, xoxb- prefix, sk_live_ prefix",
            "BEGIN PRIVATE KEY blocks must never be stored",
            // Branch names, ticket slugs and docs placeholders that
            // look like keys to a loose `sk-` / `xox*-` pattern.
            "feat/sk-1234-fix-the-login-redirect",
            "merged feat/sk-1234-fix-the-login-redirect into main",
            "sk-20931-migrate-billing-to-stripe",
            "sk-ant-api03-REPLACE_WITH_YOUR_KEY",
            "xoxb-your-bot-token-here",
        ] {
            assert!(
                matches!(guard_text(SecretsMode::Reject, text).unwrap(), Guard::Clean),
                "false positive on: {text}"
            );
        }
    }

    /// The prose look-alikes above must stay clean, but a key-shaped
    /// string of the same families must still be refused, even next to
    /// the same kind of branch name.
    #[test]
    fn real_shaped_keys_are_still_rejected_next_to_lookalikes() {
        for (name, key) in [
            (
                "openai_or_anthropic_key",
                format!(
                    "sk-proj-{}",
                    "Ab3dEf6hIj9lMn2pQr5tUv8xYz1BcDeFgHiJkLmNoPqRsTuV"
                ),
            ),
            (
                "openai_or_anthropic_key",
                format!(
                    "sk-ant-{}",
                    "api03-Zx9Yw8Vu7Ts6Rq5Po4Nm3Lk2Ji1Hg0FeDcBaAbCdEfGhIjKl"
                ),
            ),
            (
                "openai_or_anthropic_key",
                format!("sk-{}", "Ab3dEf6hIj9lMn2pQr5tUv8xYz1BcDeFgHiJkLmNoPqRsTuV"),
            ),
            (
                "slack_token",
                format!(
                    "xoxb-{}",
                    "123456789012-1234567890123-AbCdEfGhIjKlMnOpQrStUvWx"
                ),
            ),
            ("slack_token", format!("xoxp-{}", "2-3-4-abcdefgh")),
        ] {
            let text = format!("branch feat/sk-1234-fix-the-login-redirect uses {key} now");
            let f = scan(&text)
                .unwrap()
                .unwrap_or_else(|| panic!("{name}: real-shaped key not detected"));
            assert_eq!(f.pattern, name);
            assert!(f.high_confidence);
            assert!(matches!(
                guard_text(SecretsMode::Reject, &text).unwrap(),
                Guard::Reject(_)
            ));
        }
    }

    #[test]
    fn sk_key_needs_a_digit_in_the_body() {
        let all_letters = format!("sk-{}", "abcdefghijklmnopqrstuvwxyzabcdefghij");
        assert!(scan(&all_letters).unwrap().is_none());
        let with_digit = format!("sk-{}", "abcdefghijklmnopqrstuvwxyzabcdefghi1");
        assert_eq!(
            scan(&with_digit).unwrap().unwrap().pattern,
            "openai_or_anthropic_key"
        );
        // A digit-free hyphenated word first must not mask a real key.
        let both = format!(
            "sk-{} then sk-{}",
            "abcdefghijklmnopqrstuvwxyzabcdefghij", "q1w2e3r4t5y6u7i8o9p0aq1w2e3r4t5y6u7i8o9p0a"
        );
        assert_eq!(scan(&both).unwrap().unwrap().offset, 45);
    }

    #[test]
    fn sk_key_needs_a_long_body() {
        // 31 body chars with a digit: a ticket slug, not a key.
        let short = format!("sk-{}", "1abcdefghijklmnopqrstuvwxyzabcd");
        assert!(scan(&short).unwrap().is_none());
        let long = format!("sk-{}", "1abcdefghijklmnopqrstuvwxyzabcde");
        assert!(scan(&long).unwrap().is_some());
    }

    #[test]
    fn json_pointer_escapes_keys_and_indexes() {
        let secret = format!("AKIA{}", "IOSFODNN7EXAMPLE");
        let payload = json!({ "a/b": { "c~d": [0, { "e": secret }] } });
        let f = scan_value(&payload).unwrap().unwrap();
        assert_eq!(f.path, "/a~1b/c~0d/1/e");
        // A top-level string has an empty pointer.
        assert_eq!(scan_value(&json!(secret)).unwrap().unwrap().path, "");
    }

    #[test]
    fn offsets_count_characters_not_bytes() {
        let text = format!("héllo wörld {}", "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(scan(&text).unwrap().unwrap().offset, 12);
    }

    #[test]
    fn json_payload_is_scanned_by_leaf_with_path() {
        let secret = format!("AKIA{}", "IOSFODNN7EXAMPLE");
        let payload =
            json!({ "note": "fine", "nested": { "list": ["ok", format!("x\n{secret}")] } });
        let f = scan_value(&payload).unwrap().unwrap();
        assert_eq!(f.pattern, "aws_access_key_id");
        assert_eq!(f.path, "/nested/list/1");
        assert_eq!(f.offset, 2);

        let g = guard_value(SecretsMode::Reject, &payload).unwrap();
        let Guard::Reject(r) = g else {
            panic!("expected Reject")
        };
        assert_eq!(r.meta.unwrap()["error"]["path"], "/nested/list/1");

        assert!(
            scan_value(&json!({"content": "all good", "n": 3, "b": null}))
                .unwrap()
                .is_none()
        );
        // A bare string payload.
        assert!(scan_value(&json!(secret)).unwrap().is_some());
    }

    #[test]
    fn attach_merges_into_existing_meta() {
        let base =
            ToolResult::text("ok").with_meta(json!({"length_advisory": {"tier": "advisory"}}));
        let adv = Some(json!({"secret_advisory": {"pattern": "jwt"}}));
        let out = attach(base, adv, "ID1");
        let meta = out.meta.unwrap();
        assert!(meta.get("length_advisory").is_some());
        assert_eq!(meta["secret_advisory"]["pattern"], "jwt");

        let bare = attach(
            ToolResult::text("ok"),
            Some(json!({"secret_advisory": {}})),
            "ID1",
        );
        assert!(bare.meta.unwrap().get("secret_advisory").is_some());
        assert!(attach(ToolResult::text("ok"), None, "ID1").meta.is_none());
    }

    fn text_of(r: &ToolResult) -> &str {
        let Some(ContentBlock::Text(t)) = r.content.first() else {
            panic!("no text block");
        };
        t
    }

    #[test]
    fn attach_appends_a_text_note_without_echoing_the_match() {
        let secret = format!("AKIA{}", "IOSFODNN7EXAMPLE");
        let Guard::Advisory(adv) =
            guard_text(SecretsMode::Warn, &format!("key: {secret}")).unwrap()
        else {
            panic!("expected Advisory");
        };
        let out = attach(ToolResult::text("stored memory M1"), Some(adv), "M1");
        let text = text_of(&out);
        assert!(text.starts_with("stored memory M1\n"), "{text}");
        assert!(
            text.ends_with(
                "warning: content matched credential pattern aws_access_key_id at char 5; \
                 forget M1 if this is a real secret"
            ),
            "{text}"
        );
        assert!(!text.contains(&secret));
        // No advisory, no note.
        let clean = attach(ToolResult::text("stored memory M1"), None, "M1");
        assert_eq!(text_of(&clean), "stored memory M1");
    }

    #[test]
    fn attach_names_the_payload_path_when_present() {
        let secret = format!("AKIA{}", "IOSFODNN7EXAMPLE");
        let Guard::Advisory(adv) = guard_value(
            SecretsMode::Warn,
            &json!({"content": format!("key: {secret}")}),
        )
        .unwrap() else {
            panic!("expected Advisory");
        };
        let out = attach(ToolResult::text("recorded event E1"), Some(adv), "E1");
        let text = text_of(&out);
        assert!(
            text.ends_with(
                "warning: content matched credential pattern aws_access_key_id at char 5 \
                 of /content; forget E1 if this is a real secret"
            ),
            "{text}"
        );
        assert!(!text.contains(&secret));
    }

    #[test]
    fn attach_meta_leaves_the_text_alone() {
        let adv = Some(json!({"secret_advisory": {"pattern": "jwt", "offset": 3}}));
        let out = attach_meta(ToolResult::text("no such memory M1"), adv);
        assert_eq!(text_of(&out), "no such memory M1");
        assert_eq!(out.meta.unwrap()["secret_advisory"]["pattern"], "jwt");
    }

    /// The set must agree with running every pattern on its own: same
    /// pattern name and char offset, for any input.
    #[test]
    fn regex_set_prefilter_matches_per_pattern_scan() {
        fn reference(text: &str) -> Option<(&'static str, usize, bool)> {
            let d = detectors().unwrap();
            let mut generic = None;
            for det in &d.list {
                if generic.is_some() && !det.high_confidence {
                    continue;
                }
                let hit = det
                    .re
                    .find_iter(text)
                    .find(|m| !det.require_digit || m.as_str().bytes().any(|b| b.is_ascii_digit()));
                if let Some(m) = hit {
                    let f = (
                        det.name,
                        text[..m.start()].chars().count(),
                        det.high_confidence,
                    );
                    if det.high_confidence {
                        return Some(f);
                    }
                    generic = Some(f);
                }
            }
            generic
        }
        let mut inputs: Vec<String> = samples()
            .into_iter()
            .map(|(_, s)| format!("héllo {s} tail"))
            .collect();
        inputs.extend(
            [
                "",
                "plain prose",
                "password = hunter2hunter2",
                "sk-abcdefghijklmnopqrstuvwxyzabcdefghij",
            ]
            .map(String::from),
        );
        inputs.push(format!("password = AKIA{}", "IOSFODNN7EXAMPLE"));
        for text in inputs {
            let got = scan(&text)
                .unwrap()
                .map(|f| (f.pattern, f.offset, f.high_confidence));
            assert_eq!(got, reference(&text), "{text}");
        }
    }
}
