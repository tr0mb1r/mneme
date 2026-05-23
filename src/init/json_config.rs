//! JSON config-merge primitive for per-agent install
//! (release-planning v2.1 §4.4). Surgically inserts / updates /
//! removes mneme-owned entries inside the user's per-agent settings
//! file (Claude Code's `settings.json`, Claude Desktop's
//! `claude_desktop_config.json`, Cursor's `settings.json`, Cline's
//! MCP config) without touching unrelated keys.
//!
//! All Tier-1 v1.1 agents speak JSON (per release-planning §4.4).
//! TOML / YAML config-merge primitives are intentionally NOT
//! shipped in B.M1 — no Tier-1 agent needs them, and adding them
//! speculatively would invent abstractions before there's a
//! consumer. If a v1.1.x or v1.2 agent surfaces with TOML or YAML
//! config, those primitives can land alongside that agent's
//! integration with their own focused test suite.
//!
//! Design choices:
//!
//! - Path addressing uses `&[&str]` segments rather than dotted
//!   strings. Avoids the "what if a key has a dot in it" edge case
//!   (which JSON keys absolutely can have) at the call site, and
//!   keeps the matchable contract obvious from the type signature.
//!
//! - Read returns `Ok(None)` for absent files so per-agent
//!   installers can treat "fresh install" and "edit existing" as
//!   one code path: read, transform a `Value::Object` (creating
//!   one if read returned None), write back.
//!
//! - Writes are atomic via [`crate::init::assets::write_text`] —
//!   tmpfile + fsync + rename — so a crashed install never
//!   half-clobbers the user's settings file.
//!
//! - Pretty-printed output (2-space indent) preserves the
//!   user-facing readability of the file. Agents typically format
//!   their settings.json this way; matching the convention avoids
//!   surprising users who hand-edit.

use std::io;
use std::path::Path;

use serde_json::{Map, Value};
use thiserror::Error;

use super::assets;

#[derive(Debug, Error)]
pub enum ConfigError {
    /// The path traverses a non-object value (e.g. user set
    /// `mcpServers` to a string by mistake, and we're asked to
    /// set `mcpServers.mneme`). We refuse rather than overwrite —
    /// the user's file isn't shaped the way we expect, and a
    /// silent overwrite would lose data.
    #[error("path segment {segment:?} traverses a non-object value at {at}")]
    NonObjectInPath { segment: String, at: String },
    /// IO failure reading or writing the file.
    #[error("config file IO error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
    /// JSON parse failure on the existing file. We return this
    /// rather than treat the file as empty because clobbering an
    /// invalid-but-non-empty file would lose user content; the
    /// installer should surface the parse error to the user and
    /// let them fix it.
    #[error("config file at {path} is not valid JSON: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}

/// Read a JSON config file. Returns `Ok(None)` when the file is
/// absent (the common "fresh install" case), `Ok(Some(value))`
/// when present and parseable, `Err(Parse)` when present but
/// malformed.
pub fn read(path: &Path) -> Result<Option<Value>, ConfigError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.display().to_string(),
                source,
            });
        }
    };
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        // Treat empty / whitespace-only files as absent — same
        // shape per-agent installers want for "fresh install".
        return Ok(None);
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })
}

/// Set the value at the given segment path inside a JSON object,
/// creating intermediate objects as needed. Top-level `value` must
/// already be an object (call `Value::Object(Map::new())` for a
/// fresh start).
///
/// # Errors
///
/// Returns [`ConfigError::NonObjectInPath`] when an intermediate
/// segment exists but isn't an object (e.g., the user has
/// `"mcpServers": "off"` and we're asked to set
/// `["mcpServers", "mneme"]`).
pub fn set_path(value: &mut Value, segments: &[&str], new: Value) -> Result<(), ConfigError> {
    if segments.is_empty() {
        *value = new;
        return Ok(());
    }
    let mut cursor = value;
    for (i, segment) in segments.iter().enumerate() {
        let is_last = i == segments.len() - 1;
        let map = cursor
            .as_object_mut()
            .ok_or_else(|| ConfigError::NonObjectInPath {
                segment: (*segment).to_owned(),
                at: segments[..i].join("."),
            })?;
        if is_last {
            map.insert((*segment).to_owned(), new);
            return Ok(());
        }
        // Create the intermediate object if it doesn't exist; if
        // it exists but isn't an object, the next iteration's
        // `as_object_mut` will return the structured error.
        cursor = map
            .entry((*segment).to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    Ok(())
}

/// Remove the entry at the given segment path. No-op when the
/// path doesn't exist (so `--uninstall` is idempotent — running
/// twice is fine). Returns `Ok(true)` if a key was actually
/// removed, `Ok(false)` if nothing matched.
pub fn remove_path(value: &mut Value, segments: &[&str]) -> Result<bool, ConfigError> {
    if segments.is_empty() {
        return Ok(false);
    }
    let mut cursor = value;
    for (i, segment) in segments[..segments.len() - 1].iter().enumerate() {
        let map = match cursor.as_object_mut() {
            Some(m) => m,
            // Hit a non-object intermediate — nothing to remove.
            // Idempotent semantics: no error, just `false`.
            None => return Ok(false),
        };
        match map.get_mut(*segment) {
            Some(next) => cursor = next,
            None => return Ok(false),
            // Walk one level deeper; idle gap caught by next loop.
        }
        let _ = i;
    }
    let last = segments[segments.len() - 1];
    let map = match cursor.as_object_mut() {
        Some(m) => m,
        None => return Ok(false),
    };
    Ok(map.remove(last).is_some())
}

/// Read, transform, and atomically write back a JSON config file.
/// If the file is absent (or empty), starts the transform with a
/// fresh empty object — per-agent installers don't have to special-
/// case "fresh vs edit". If the file exists but is malformed JSON,
/// returns the parse error rather than silently clobbering.
///
/// Output is pretty-printed (2-space indent) to match the
/// convention for user-facing config files.
pub fn upsert_file<F>(path: &Path, transform: F) -> Result<(), ConfigError>
where
    F: FnOnce(&mut Value) -> Result<(), ConfigError>,
{
    let mut value = read(path)?.unwrap_or_else(|| Value::Object(Map::new()));
    transform(&mut value)?;
    let mut serialized =
        serde_json::to_string_pretty(&value).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })?;
    // Pretty-printer omits the trailing newline; add one so
    // editors that flag "no newline at end of file" don't bark
    // at the user every time they open settings.json.
    if !serialized.ends_with('\n') {
        serialized.push('\n');
    }
    assets::write_text(path, &serialized).map_err(|source| ConfigError::Io {
        path: path.display().to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn read_returns_none_for_missing_file() {
        let tmp = TempDir::new().unwrap();
        let result = read(&tmp.path().join("does-not-exist.json")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn read_returns_none_for_empty_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.json");
        std::fs::write(&path, "").unwrap();
        assert!(read(&path).unwrap().is_none());
    }

    #[test]
    fn read_returns_none_for_whitespace_only_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ws.json");
        std::fs::write(&path, "  \n\t\n").unwrap();
        assert!(read(&path).unwrap().is_none());
    }

    #[test]
    fn read_parses_valid_json() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("ok.json");
        std::fs::write(&path, r#"{"hello":"world"}"#).unwrap();
        let v = read(&path).unwrap().expect("parsed");
        assert_eq!(v, json!({"hello":"world"}));
    }

    #[test]
    fn read_errors_on_malformed_json() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(matches!(read(&path), Err(ConfigError::Parse { .. })));
    }

    #[test]
    fn set_path_inserts_new_leaf() {
        let mut v = json!({});
        set_path(
            &mut v,
            &["mcpServers", "mneme"],
            json!({"command": "mneme"}),
        )
        .unwrap();
        assert_eq!(v, json!({"mcpServers": {"mneme": {"command": "mneme"}}}));
    }

    #[test]
    fn set_path_overwrites_existing_leaf() {
        let mut v = json!({"mcpServers": {"mneme": {"command": "old"}}});
        set_path(&mut v, &["mcpServers", "mneme"], json!({"command": "new"})).unwrap();
        assert_eq!(v["mcpServers"]["mneme"]["command"], "new");
    }

    #[test]
    fn set_path_preserves_sibling_keys() {
        let mut v = json!({
            "mcpServers": {
                "other-server": {"command": "elsewhere"},
                "mneme": {"command": "old"}
            },
            "unrelated": "preserved"
        });
        set_path(&mut v, &["mcpServers", "mneme"], json!({"command": "new"})).unwrap();
        assert_eq!(v["mcpServers"]["other-server"]["command"], "elsewhere");
        assert_eq!(v["mcpServers"]["mneme"]["command"], "new");
        assert_eq!(v["unrelated"], "preserved");
    }

    #[test]
    fn set_path_creates_intermediate_objects() {
        let mut v = json!({});
        set_path(&mut v, &["a", "b", "c", "d"], json!(42)).unwrap();
        assert_eq!(v, json!({"a": {"b": {"c": {"d": 42}}}}));
    }

    #[test]
    fn set_path_errors_on_non_object_intermediate() {
        let mut v = json!({"mcpServers": "off"});
        let result = set_path(&mut v, &["mcpServers", "mneme"], json!({}));
        assert!(matches!(result, Err(ConfigError::NonObjectInPath { .. })));
        // User's value untouched — we error rather than overwrite.
        assert_eq!(v, json!({"mcpServers": "off"}));
    }

    #[test]
    fn remove_path_returns_true_on_hit() {
        let mut v = json!({"mcpServers": {"mneme": {"x": 1}, "other": {"y": 2}}});
        assert!(remove_path(&mut v, &["mcpServers", "mneme"]).unwrap());
        assert_eq!(v, json!({"mcpServers": {"other": {"y": 2}}}));
    }

    #[test]
    fn remove_path_returns_false_on_miss() {
        let mut v = json!({"mcpServers": {"other": {"y": 2}}});
        assert!(!remove_path(&mut v, &["mcpServers", "mneme"]).unwrap());
        assert_eq!(v, json!({"mcpServers": {"other": {"y": 2}}}));
    }

    #[test]
    fn remove_path_is_idempotent() {
        let mut v = json!({"mcpServers": {"mneme": {}}});
        let first = remove_path(&mut v, &["mcpServers", "mneme"]).unwrap();
        let second = remove_path(&mut v, &["mcpServers", "mneme"]).unwrap();
        assert!(first);
        assert!(!second);
    }

    #[test]
    fn remove_path_handles_missing_intermediate_gracefully() {
        let mut v = json!({});
        let result = remove_path(&mut v, &["mcpServers", "mneme"]).unwrap();
        assert!(!result);
    }

    #[test]
    fn upsert_file_creates_when_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.json");
        upsert_file(&path, |v| {
            set_path(v, &["mcpServers", "mneme"], json!({"command": "mneme"}))
        })
        .unwrap();
        let read_back: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read_back["mcpServers"]["mneme"]["command"], "mneme");
    }

    #[test]
    fn upsert_file_preserves_unrelated_keys() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{
  "mcpServers": {
    "other": {"command": "elsewhere"}
  },
  "unrelated_top_level": 42
}
"#,
        )
        .unwrap();
        upsert_file(&path, |v| {
            set_path(v, &["mcpServers", "mneme"], json!({"command": "mneme"}))
        })
        .unwrap();
        let read_back: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read_back["mcpServers"]["other"]["command"], "elsewhere");
        assert_eq!(read_back["mcpServers"]["mneme"]["command"], "mneme");
        assert_eq!(read_back["unrelated_top_level"], 42);
    }

    #[test]
    fn upsert_file_pretty_prints_with_trailing_newline() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.json");
        upsert_file(&path, |v| set_path(v, &["mneme"], json!({"a": 1}))).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        // 2-space pretty-print preserves human-readability.
        assert!(text.contains("  \"a\": 1"));
        // Trailing newline so editors don't bark.
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn upsert_file_propagates_parse_error_without_clobbering() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.json");
        let original = "{not valid json";
        std::fs::write(&path, original).unwrap();
        let result = upsert_file(&path, |v| set_path(v, &["mneme"], json!({})));
        assert!(matches!(result, Err(ConfigError::Parse { .. })));
        // The malformed file is still there, untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    /// Round-trip: set, then remove via upsert_file. End state
    /// matches starting state (preserving every other key the user
    /// had). Mirrors the `mneme init <agent> --uninstall` flow.
    #[test]
    fn upsert_file_set_then_remove_round_trips_to_original_keys() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.json");
        let original = json!({
            "mcpServers": {
                "other": {"command": "elsewhere"}
            },
            "extra": "stays"
        });
        std::fs::write(&path, serde_json::to_string_pretty(&original).unwrap()).unwrap();

        upsert_file(&path, |v| {
            set_path(v, &["mcpServers", "mneme"], json!({"command": "mneme"}))
        })
        .unwrap();
        upsert_file(&path, |v| {
            remove_path(v, &["mcpServers", "mneme"])?;
            Ok(())
        })
        .unwrap();

        let read_back: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read_back["mcpServers"]["other"]["command"], "elsewhere");
        assert!(read_back["mcpServers"].get("mneme").is_none());
        assert_eq!(read_back["extra"], "stays");
    }
}
