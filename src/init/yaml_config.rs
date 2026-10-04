//! Comment-preserving edits to a YAML config file — just enough to own
//! one entry under one top-level mapping (Hermes Agent's
//! `mcp_servers.mneme` in `~/.hermes/config.yaml`).
//!
//! Why not parse and re-serialise: Hermes seeds a heavily commented
//! `config.yaml`, and a YAML round-trip through serde drops every
//! comment and reorders keys. Users would lose their annotations the
//! first time they ran `mneme init hermes`. Instead this module edits
//! the text line by line, touching only the lines of the entry it owns.
//!
//! Supported shapes for the top-level key (`mcp_servers` below):
//!
//! - absent → a new block is appended at the end of the file;
//! - `mcp_servers:` followed by an indented block mapping (the normal
//!   case) → the entry is added at the block's indentation, or replaced
//!   in place if it is already there;
//! - `mcp_servers:` with nothing under it, `mcp_servers: {}`,
//!   `mcp_servers: null` / `~` → rewritten as a block holding the entry.
//!
//! Anything else (a non-empty flow mapping, an anchor or alias, a
//! whole-document flow mapping, tab indentation, a duplicated key) is
//! refused with [`YamlEditError::Unsupported`] rather than guessed at.
//! The caller prints the snippet for the user to paste by hand.
//!
//! Detection relies on one YAML property: a top-level key starts at
//! column 0, and nothing inside a nested value can (block scalars and
//! nested mappings must be indented further than their parent key).

use thiserror::Error;

/// Comment written on the line above an entry this module manages, so
/// a user reading the file knows where it came from. Recognised (and
/// replaced or removed with the entry) on later edits.
pub const MANAGED_COMMENT: &str = "# managed by mneme — undo with: mneme init hermes --uninstall";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum YamlEditError {
    #[error("unsupported YAML layout: {0}")]
    Unsupported(String),
}

/// Insert or replace `child_key` under the top-level `top_key` mapping.
///
/// `body` is the entry's value as block-mapping lines *without*
/// indentation, e.g. `["command: mneme", "args: [\"client\"]"]`; this
/// function indents them to fit the file.
pub fn upsert_entry(
    input: &str,
    top_key: &str,
    child_key: &str,
    body: &[String],
) -> Result<String, YamlEditError> {
    let doc = Doc::parse(input)?;
    let mut lines = doc.lines.clone();

    let Some(top) = doc.find_top(top_key)? else {
        // No top-level key: append a fresh block.
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(format!("{top_key}:"));
        lines.extend(render_entry(2, child_key, body));
        return Ok(doc.join(lines));
    };

    let block = doc.block_after(top.line);
    let child_indent = doc.child_indent(&block).unwrap_or(2);
    let rendered = render_entry(child_indent, child_key, body);

    if top.empty_value {
        // `mcp_servers: {}` / `null` / `~` → plain block header.
        lines[top.line] = format!("{top_key}:{}", top.trailing_comment);
    }

    match doc.find_child(&block, child_indent, child_key)? {
        Some((start, end)) => {
            lines.splice(start..end, rendered);
        }
        None => {
            // After the last indented line of the block, so trailing
            // column-0 comments that introduce the next key stay put.
            let insert_at = block
                .clone()
                .rev()
                .find(|&i| !lines[i].trim().is_empty() && indent_of(&lines[i]) > 0)
                .map(|i| i + 1)
                .unwrap_or(top.line + 1);
            lines.splice(insert_at..insert_at, rendered);
        }
    }
    Ok(doc.join(lines))
}

/// Remove `child_key` from under `top_key`. No-op if absent. If that
/// leaves the mapping with no entries, the header is removed too (or
/// turned into `top_key: {}` when comments remain under it), because a
/// bare `top_key:` parses as null and some loaders choke on that.
pub fn remove_entry(input: &str, top_key: &str, child_key: &str) -> Result<String, YamlEditError> {
    let doc = Doc::parse(input)?;
    let Some(top) = doc.find_top(top_key)? else {
        return Ok(input.to_owned());
    };
    let block = doc.block_after(top.line);
    let Some(child_indent) = doc.child_indent(&block) else {
        return Ok(input.to_owned());
    };
    let Some((start, end)) = doc.find_child(&block, child_indent, child_key)? else {
        return Ok(input.to_owned());
    };

    let mut lines = doc.lines.clone();
    lines.drain(start..end);
    let removed = end - start;
    let block_end = block.end - removed;
    let remaining = &lines[top.line + 1..block_end];
    let has_entries = remaining.iter().any(|l| is_significant(l));
    if !has_entries {
        let has_comments = remaining.iter().any(|l| !l.trim().is_empty());
        if has_comments {
            lines[top.line] = format!("{top_key}: {{}}{}", top.trailing_comment);
        } else {
            lines.drain(top.line..block_end);
            // Don't leave a double blank line where the block was.
            if top.line > 0
                && top.line < lines.len()
                && lines[top.line - 1].trim().is_empty()
                && lines[top.line].trim().is_empty()
            {
                lines.remove(top.line);
            }
            while lines.last().is_some_and(|l| l.trim().is_empty()) {
                lines.pop();
            }
        }
    }
    Ok(doc.join(lines))
}

/// Whether `child_key` exists under `top_key`.
pub fn has_entry(input: &str, top_key: &str, child_key: &str) -> Result<bool, YamlEditError> {
    let doc = Doc::parse(input)?;
    let Some(top) = doc.find_top(top_key)? else {
        return Ok(false);
    };
    let block = doc.block_after(top.line);
    let Some(child_indent) = doc.child_indent(&block) else {
        return Ok(false);
    };
    Ok(doc.find_child(&block, child_indent, child_key)?.is_some())
}

/// Render a YAML double-quoted scalar. Escapes backslash, quote, and
/// control characters so arbitrary text round-trips.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn render_entry(indent: usize, child_key: &str, body: &[String]) -> Vec<String> {
    let pad = " ".repeat(indent);
    let inner = " ".repeat(indent * 2);
    let mut out = vec![
        format!("{pad}{MANAGED_COMMENT}"),
        format!("{pad}{child_key}:"),
    ];
    out.extend(body.iter().map(|l| format!("{inner}{l}")));
    out
}

struct Doc {
    lines: Vec<String>,
    newline: &'static str,
    trailing_newline: bool,
}

struct Top {
    line: usize,
    /// Value was `{}`, `null`, or `~`.
    empty_value: bool,
    /// ` # ...` after the key, kept when the header is rewritten.
    trailing_comment: String,
}

impl Doc {
    fn parse(input: &str) -> Result<Self, YamlEditError> {
        let newline = if input.contains("\r\n") { "\r\n" } else { "\n" };
        let trailing_newline = input.is_empty() || input.ends_with('\n');
        let body = input.strip_suffix(newline).unwrap_or(input);
        let lines: Vec<String> = if input.is_empty() {
            Vec::new()
        } else {
            body.split(newline).map(str::to_owned).collect()
        };
        if lines
            .iter()
            .any(|l| is_significant(l) && l[..l.len() - l.trim_start().len()].contains('\t'))
        {
            return Err(YamlEditError::Unsupported(
                "tab indentation (YAML requires spaces)".into(),
            ));
        }
        if let Some(first) = lines.iter().find(|l| is_significant(l)) {
            let t = first.trim_start();
            if t.starts_with('{') || t.starts_with('[') {
                return Err(YamlEditError::Unsupported(
                    "the document is a flow collection; only block-style config files can be edited"
                        .into(),
                ));
            }
        }
        Ok(Self {
            lines,
            newline,
            trailing_newline,
        })
    }

    fn join(&self, lines: Vec<String>) -> String {
        if lines.is_empty() {
            return String::new();
        }
        let mut out = lines.join(self.newline);
        if self.trailing_newline || !out.is_empty() {
            out.push_str(self.newline);
        }
        out
    }

    fn find_top(&self, key: &str) -> Result<Option<Top>, YamlEditError> {
        let mut found: Option<Top> = None;
        for (i, line) in self.lines.iter().enumerate() {
            if indent_of(line) != 0 || !is_significant(line) {
                continue;
            }
            let Some(rest) = strip_key(line, key) else {
                continue;
            };
            if found.is_some() {
                return Err(YamlEditError::Unsupported(format!(
                    "`{key}` appears more than once at the top level"
                )));
            }
            let (value, comment) = split_comment(rest);
            let empty_value = match value {
                "" => false,
                "{}" | "null" | "~" | "Null" | "NULL" => true,
                other => {
                    return Err(YamlEditError::Unsupported(format!(
                        "`{key}: {other}` is not a block mapping; rewrite it as `{key}:` followed by indented entries"
                    )));
                }
            };
            found = Some(Top {
                line: i,
                empty_value,
                trailing_comment: comment.map(|c| format!(" {c}")).unwrap_or_default(),
            });
        }
        Ok(found)
    }

    /// Lines belonging to the value of the top-level key on `header`:
    /// everything up to the next significant column-0 line, minus any
    /// blank lines and column-0 comments that sit directly in front of
    /// that next key (they introduce it, not us).
    fn block_after(&self, header: usize) -> std::ops::Range<usize> {
        let mut end = self.lines.len();
        for i in header + 1..self.lines.len() {
            let l = &self.lines[i];
            if is_significant(l) && indent_of(l) == 0 {
                end = i;
                break;
            }
        }
        while end > header + 1 {
            let l = &self.lines[end - 1];
            if l.trim().is_empty() || indent_of(l) == 0 {
                end -= 1;
            } else {
                break;
            }
        }
        header + 1..end
    }

    fn child_indent(&self, block: &std::ops::Range<usize>) -> Option<usize> {
        block
            .clone()
            .map(|i| &self.lines[i])
            .find(|l| is_significant(l))
            .map(|l| indent_of(l))
    }

    /// Line range of `key`'s entry (including our managed comment, if
    /// directly above it) within `block`.
    fn find_child(
        &self,
        block: &std::ops::Range<usize>,
        indent: usize,
        key: &str,
    ) -> Result<Option<(usize, usize)>, YamlEditError> {
        let Some(start) = block.clone().find(|&i| {
            let l = &self.lines[i];
            is_significant(l) && indent_of(l) == indent && strip_key(&l[indent..], key).is_some()
        }) else {
            return Ok(None);
        };
        let mut end = block.end;
        for i in start + 1..block.end {
            let l = &self.lines[i];
            if is_significant(l) && indent_of(l) <= indent {
                end = i;
                break;
            }
        }
        // Trailing blank lines / shallower comments belong to whatever
        // follows, not to this entry.
        while end > start + 1 {
            let l = &self.lines[end - 1];
            if l.trim().is_empty() || (l.trim_start().starts_with('#') && indent_of(l) <= indent) {
                end -= 1;
            } else {
                break;
            }
        }
        let start = if start > block.start && self.lines[start - 1].trim() == MANAGED_COMMENT {
            start - 1
        } else {
            start
        };
        Ok(Some((start, end)))
    }
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// Not blank, not a comment, not a document marker.
fn is_significant(line: &str) -> bool {
    let t = line.trim();
    !(t.is_empty() || t.starts_with('#') || t == "---" || t == "...")
}

/// If `line` (already de-indented) is `key:` — bare or quoted — return
/// what follows the colon.
fn strip_key<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    for candidate in [key.to_owned(), format!("\"{key}\""), format!("'{key}'")] {
        if let Some(rest) = line.strip_prefix(candidate.as_str())
            && let Some(after) = rest.strip_prefix(':')
            && (after.is_empty() || after.starts_with([' ', '\t']))
        {
            return Some(after);
        }
    }
    None
}

/// Split `  value  # comment` into (`value`, Some(`# comment`)). A `#`
/// only starts a comment after whitespace, and not inside quotes.
fn split_comment(rest: &str) -> (&str, Option<&str>) {
    let mut in_single = false;
    let mut in_double = false;
    let mut prev_ws = true;
    for (i, c) in rest.char_indices() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double && prev_ws => {
                return (rest[..i].trim(), Some(rest[i..].trim_end()));
            }
            _ => {}
        }
        prev_ws = c.is_whitespace();
    }
    (rest.trim(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> Vec<String> {
        vec!["command: mneme".into(), "args: [\"client\"]".into()]
    }

    fn entry(indent: usize) -> String {
        let p = " ".repeat(indent);
        let q = " ".repeat(indent * 2);
        format!("{p}{MANAGED_COMMENT}\n{p}mneme:\n{q}command: mneme\n{q}args: [\"client\"]\n")
    }

    #[test]
    fn empty_file_gets_a_new_block() {
        let out = upsert_entry("", "mcp_servers", "mneme", &body()).unwrap();
        assert_eq!(out, format!("mcp_servers:\n{}", entry(2)));
    }

    #[test]
    fn missing_key_is_appended_after_existing_content() {
        let input = "# Hermes config\nmodel:\n  default: x  # pick one\n";
        let out = upsert_entry(input, "mcp_servers", "mneme", &body()).unwrap();
        assert_eq!(out, format!("{input}\nmcp_servers:\n{}", entry(2)));
    }

    #[test]
    fn entry_is_added_next_to_existing_servers_at_their_indent() {
        let input = "\
mcp_servers:
    github:
        command: npx   # keep me
        args: [\"-y\", \"gh\"]

# Next section
memory:
  enabled: true
";
        let out = upsert_entry(input, "mcp_servers", "mneme", &body()).unwrap();
        let expected = format!(
            "\
mcp_servers:
    github:
        command: npx   # keep me
        args: [\"-y\", \"gh\"]
{}
# Next section
memory:
  enabled: true
",
            entry(4)
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn existing_entry_is_replaced_in_place() {
        let input = "\
mcp_servers:
  mneme:
    command: old
    args: [\"run\"]
    env:
      A: b
  other:
    url: http://x
logging: {}
";
        let out = upsert_entry(input, "mcp_servers", "mneme", &body()).unwrap();
        let expected = format!(
            "mcp_servers:\n{}  other:\n    url: http://x\nlogging: {{}}\n",
            entry(2)
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn upsert_is_idempotent() {
        let once = upsert_entry("a: 1\n", "mcp_servers", "mneme", &body()).unwrap();
        let twice = upsert_entry(&once, "mcp_servers", "mneme", &body()).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn empty_flow_mapping_and_null_become_a_block() {
        for v in ["{}", "null", "~"] {
            let input = format!("mcp_servers: {v}   # servers go here\nx: 1\n");
            let out = upsert_entry(&input, "mcp_servers", "mneme", &body()).unwrap();
            assert_eq!(
                out,
                format!("mcp_servers: # servers go here\n{}x: 1\n", entry(2)),
                "value {v}"
            );
        }
    }

    #[test]
    fn bare_header_with_only_comments_under_it() {
        let input = "mcp_servers:\n  # add servers below\n\nother: 2\n";
        let out = upsert_entry(input, "mcp_servers", "mneme", &body()).unwrap();
        assert_eq!(
            out,
            format!(
                "mcp_servers:\n  # add servers below\n{}\nother: 2\n",
                entry(2)
            )
        );
    }

    #[test]
    fn non_empty_flow_mapping_is_refused() {
        let err = upsert_entry(
            "mcp_servers: {a: {command: x}}\n",
            "mcp_servers",
            "mneme",
            &body(),
        )
        .unwrap_err();
        assert!(matches!(err, YamlEditError::Unsupported(_)));
    }

    #[test]
    fn flow_document_is_refused() {
        assert!(upsert_entry("{a: 1}\n", "mcp_servers", "mneme", &body()).is_err());
    }

    #[test]
    fn duplicate_top_level_key_is_refused() {
        let input = "mcp_servers:\n  a:\n    url: x\nmcp_servers:\n  b:\n    url: y\n";
        assert!(upsert_entry(input, "mcp_servers", "mneme", &body()).is_err());
    }

    #[test]
    fn tabs_are_refused() {
        let input = "mcp_servers:\n\tgithub:\n\t\tcommand: x\n";
        assert!(upsert_entry(input, "mcp_servers", "mneme", &body()).is_err());
    }

    #[test]
    fn nested_key_with_same_name_is_not_a_top_level_match() {
        let input = "profiles:\n  work:\n    mcp_servers:\n      x:\n        url: y\n";
        let out = upsert_entry(input, "mcp_servers", "mneme", &body()).unwrap();
        assert!(out.starts_with(input));
        assert!(out.ends_with(&format!("\nmcp_servers:\n{}", entry(2))));
    }

    #[test]
    fn crlf_files_stay_crlf() {
        let input = "a: 1\r\nmcp_servers:\r\n  x:\r\n    url: y\r\n";
        let out = upsert_entry(input, "mcp_servers", "mneme", &body()).unwrap();
        assert!(!out.replace("\r\n", "").contains('\n'), "{out:?}");
        assert!(out.contains("  mneme:\r\n"));
    }

    #[test]
    fn quoted_child_key_is_recognised() {
        let input = "mcp_servers:\n  \"mneme\":\n    command: old\n";
        let out = upsert_entry(input, "mcp_servers", "mneme", &body()).unwrap();
        assert_eq!(out, format!("mcp_servers:\n{}", entry(2)));
    }

    #[test]
    fn remove_drops_entry_and_keeps_siblings() {
        let input = format!(
            "mcp_servers:\n  github:\n    command: npx\n{}  zed:\n    url: z\nother: 1\n",
            entry(2)
        );
        let out = remove_entry(&input, "mcp_servers", "mneme").unwrap();
        assert_eq!(
            out,
            "mcp_servers:\n  github:\n    command: npx\n  zed:\n    url: z\nother: 1\n"
        );
    }

    #[test]
    fn remove_last_entry_drops_the_header() {
        let input = format!("a: 1\n\nmcp_servers:\n{}\nb: 2\n", entry(2));
        let out = remove_entry(&input, "mcp_servers", "mneme").unwrap();
        assert_eq!(out, "a: 1\n\nb: 2\n");
    }

    #[test]
    fn install_then_uninstall_restores_the_original() {
        for input in [
            "",
            "a: 1\n",
            "mcp_servers:\n  github:\n    command: npx\nz: 0\n",
            "# top comment\nmodel: x\n\n# servers\nmcp_servers:\n  a:\n    url: b\n",
        ] {
            let installed = upsert_entry(input, "mcp_servers", "mneme", &body()).unwrap();
            let removed = remove_entry(&installed, "mcp_servers", "mneme").unwrap();
            assert_eq!(removed, input, "round trip of {input:?} via {installed:?}");
        }
    }

    #[test]
    fn remove_last_entry_with_comments_left_becomes_empty_mapping() {
        let input = format!("mcp_servers:\n  # my notes\n{}", entry(2));
        let out = remove_entry(&input, "mcp_servers", "mneme").unwrap();
        assert_eq!(out, "mcp_servers: {}\n  # my notes\n");
    }

    #[test]
    fn remove_is_a_noop_when_absent() {
        for input in ["", "a: 1\n", "mcp_servers:\n  x:\n    url: y\n"] {
            assert_eq!(remove_entry(input, "mcp_servers", "mneme").unwrap(), input);
        }
    }

    #[test]
    fn has_entry_reports_presence() {
        let with = upsert_entry("", "mcp_servers", "mneme", &body()).unwrap();
        assert!(has_entry(&with, "mcp_servers", "mneme").unwrap());
        assert!(!has_entry("mcp_servers:\n  x:\n    url: y\n", "mcp_servers", "mneme").unwrap());
    }

    #[test]
    fn quote_escapes() {
        assert_eq!(quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(quote("Bearer ${T}"), "\"Bearer ${T}\"");
    }

    #[test]
    fn hash_inside_quotes_is_not_a_comment() {
        assert_eq!(split_comment(" \"a # b\" # c"), ("\"a # b\"", Some("# c")));
        assert_eq!(split_comment(" a#b"), ("a#b", None));
    }
}
