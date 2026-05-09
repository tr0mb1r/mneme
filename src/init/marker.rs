//! Marker-block parser for the mneme-managed section of a user's
//! primary instructions file (release-planning v2.1 §4.3).
//!
//! Every per-agent integration that touches a CLAUDE.md / .cursorrules /
//! .clinerules / AGENTS.md / etc. inserts its reference inside a
//! pair of HTML-comment sentinels:
//!
//! ```markdown
//! <!-- mneme:begin -->
//! See @MNEME.md for memory instructions (managed by mneme).
//! <!-- mneme:end -->
//! ```
//!
//! [`upsert_block`] replaces the body between the markers (or appends
//! a fresh block if none exists). [`remove_block`] takes the block
//! out — surrounding content is preserved verbatim in both cases.
//! User content outside the block is never touched: §4.3 makes mneme
//! the owner of `MNEME.md` and the marker block; everything else in
//! the user's instructions file is theirs.
//!
//! Sentinel rules:
//!
//! - Exact match on the literal lines `<!-- mneme:begin -->` and
//!   `<!-- mneme:end -->`. Whitespace inside the comment must match
//!   exactly (no `<!--mneme:begin-->`, no `<!-- mneme:begin   -->`).
//!   Strict matching avoids false positives in user content that
//!   happens to mention the markers.
//! - Markers must be on their own lines (anchored by `\n` boundaries
//!   or document edges). A marker buried inside a paragraph isn't
//!   recognised — same false-positive avoidance rationale.
//! - Exactly one BEGIN/END pair is supported. Two or more pairs is
//!   a [`MarkerError::Malformed`] — mneme owns the block; multiple
//!   blocks indicate user confusion or external tooling we don't
//!   want to fight.

use thiserror::Error;

/// Sentinel that opens an mneme-managed block.
pub const BEGIN_MARKER: &str = "<!-- mneme:begin -->";

/// Sentinel that closes an mneme-managed block.
pub const END_MARKER: &str = "<!-- mneme:end -->";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MarkerError {
    /// One or both sentinels are present but the structure is wrong:
    /// BEGIN without END, END without BEGIN, END before BEGIN, or
    /// multiple BEGIN/END pairs.
    #[error("malformed marker block: {0}")]
    Malformed(String),
}

/// Slices of the input around an mneme-managed block. `body` is the
/// text strictly between the two marker lines (markers themselves
/// excluded). `before` ends at the newline that precedes BEGIN;
/// `after` starts at the newline that follows END. Reassembling
/// `before` + `\n` + BEGIN + `\n` + body + `\n` + END + `\n` +
/// `after` reproduces the input exactly when there's no leading or
/// trailing whitespace weirdness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerBlock<'a> {
    pub before: &'a str,
    pub body: &'a str,
    pub after: &'a str,
}

/// Locate the mneme-managed block in the input. Returns:
///
/// - `Ok(None)` if neither sentinel is present.
/// - `Ok(Some(block))` if exactly one BEGIN/END pair is present in
///   the right order.
/// - `Err(Malformed)` for any other shape (orphan marker, swapped
///   order, duplicates).
pub fn find_block(input: &str) -> Result<Option<MarkerBlock<'_>>, MarkerError> {
    let begins: Vec<usize> = on_own_line_indices(input, BEGIN_MARKER);
    let ends: Vec<usize> = on_own_line_indices(input, END_MARKER);

    match (begins.len(), ends.len()) {
        (0, 0) => Ok(None),
        (1, 0) => Err(MarkerError::Malformed(
            "found BEGIN marker without matching END".into(),
        )),
        (0, 1) => Err(MarkerError::Malformed(
            "found END marker without matching BEGIN".into(),
        )),
        (1, 1) => {
            let begin = begins[0];
            let end = ends[0];
            if end < begin {
                return Err(MarkerError::Malformed(
                    "END marker appears before BEGIN".into(),
                ));
            }
            // `body_start` skips the BEGIN line + its trailing
            // newline; `body_end` is the newline before END so the
            // body slice doesn't include that final blank line.
            let begin_end = begin + BEGIN_MARKER.len();
            let body_start = match input[begin_end..].find('\n') {
                // +1 to skip past the newline itself.
                Some(rel) => begin_end + rel + 1,
                // BEGIN at end of file with no body or END — the
                // earlier ends-count check handles the END-missing
                // case, so this branch only fires on degenerate
                // input we'd reject elsewhere. Kept as a defensive
                // fall-through.
                None => begin_end,
            };
            let body_end = end.saturating_sub(1).min(input.len());
            // Trim a trailing newline just before END so the body
            // doesn't carry a phantom blank line.
            let body = strip_trailing_newline(&input[body_start..=body_end.min(end - 1)]);

            // `before` is everything up to (but not including) the
            // BEGIN line. We strip the trailing newline that
            // precedes BEGIN so `before` doesn't end with the
            // separator we'll re-insert on reassembly.
            let before = strip_trailing_newline(&input[..begin]);

            // `after` is everything from the newline after END
            // (skipped) onward.
            let end_end = end + END_MARKER.len();
            let after = match input[end_end..].find('\n') {
                Some(rel) => &input[end_end + rel + 1..],
                None => "",
            };

            Ok(Some(MarkerBlock {
                before,
                body,
                after,
            }))
        }
        _ => Err(MarkerError::Malformed(format!(
            "expected exactly one BEGIN/END pair, found {} BEGINs and {} ENDs",
            begins.len(),
            ends.len()
        ))),
    }
}

/// Replace the body of an existing block, or append a fresh block
/// to the end of `input` if none exists. Idempotent — calling with
/// the same `body` produces the same output. Reassembly preserves
/// surrounding content verbatim.
///
/// When appending, a single blank line separates the user's existing
/// content from the new block, and a trailing newline closes the
/// block — so the output ends with `<END>\n` regardless of whether
/// the input had a trailing newline.
pub fn upsert_block(input: &str, new_body: &str) -> Result<String, MarkerError> {
    let body_normalised = strip_trailing_newline(new_body);
    match find_block(input)? {
        Some(block) => {
            let mut out = String::with_capacity(input.len() + new_body.len());
            if !block.before.is_empty() {
                out.push_str(block.before);
                out.push('\n');
            }
            out.push_str(BEGIN_MARKER);
            out.push('\n');
            if !body_normalised.is_empty() {
                out.push_str(body_normalised);
                out.push('\n');
            }
            out.push_str(END_MARKER);
            out.push('\n');
            if !block.after.is_empty() {
                out.push_str(block.after);
            }
            Ok(out)
        }
        None => {
            let mut out = String::with_capacity(input.len() + new_body.len() + 64);
            out.push_str(input);
            // Ensure separation between the existing content and
            // the new block. If the input is empty, no separator
            // needed; if it ends with one or more newlines, leave
            // them alone but add another blank-line gap.
            if !input.is_empty() {
                if !input.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
            }
            out.push_str(BEGIN_MARKER);
            out.push('\n');
            if !body_normalised.is_empty() {
                out.push_str(body_normalised);
                out.push('\n');
            }
            out.push_str(END_MARKER);
            out.push('\n');
            Ok(out)
        }
    }
}

/// Remove the marker block entirely. No-op when no block exists
/// (returns the input unchanged).
///
/// `before` already carries its own trailing newline (preserved by
/// [`find_block`] which strips only one — the separator between the
/// last content line and BEGIN). `after` similarly starts at the
/// character following the END line's terminator. Reassembly is
/// just concatenation; no inserted separator. The blank-line
/// separator that the user typically had between their content and
/// the block collapses naturally — appropriate for "the section is
/// gone."
pub fn remove_block(input: &str) -> Result<String, MarkerError> {
    match find_block(input)? {
        Some(block) => {
            let mut out = String::with_capacity(block.before.len() + block.after.len());
            out.push_str(block.before);
            out.push_str(block.after);
            Ok(out)
        }
        None => Ok(input.to_owned()),
    }
}

/// Find every byte-offset at which `needle` appears at the start of
/// a line (preceded by `\n` or the document start) AND is followed
/// by either `\n`, EOF, or end-of-line whitespace. Strict line-
/// anchoring avoids false positives on inline mentions like
/// "the marker `<!-- mneme:begin -->` indicates …" inside a
/// paragraph.
fn on_own_line_indices(haystack: &str, needle: &str) -> Vec<usize> {
    let mut hits = Vec::new();
    let mut search_from = 0;
    while let Some(rel) = haystack[search_from..].find(needle) {
        let abs = search_from + rel;
        let preceding_newline = abs == 0 || haystack.as_bytes()[abs - 1] == b'\n';
        let following = haystack.as_bytes().get(abs + needle.len()).copied();
        let line_terminated = matches!(following, None | Some(b'\n') | Some(b'\r'));
        if preceding_newline && line_terminated {
            hits.push(abs);
        }
        search_from = abs + needle.len();
    }
    hits
}

fn strip_trailing_newline(s: &str) -> &str {
    s.strip_suffix("\r\n")
        .or_else(|| s.strip_suffix('\n'))
        .unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_block_returns_none_on_empty_input() {
        assert_eq!(find_block(""), Ok(None));
    }

    #[test]
    fn find_block_returns_none_when_no_markers_present() {
        let input = "Just some user text\nwith multiple lines\nand no markers at all\n";
        assert_eq!(find_block(input), Ok(None));
    }

    #[test]
    fn find_block_returns_none_for_inline_marker_mention() {
        // The marker text appears inside a paragraph but not on its
        // own line — strict anchoring rejects it as a false positive.
        let input = "Talk about `<!-- mneme:begin -->` and `<!-- mneme:end -->` in prose\n";
        assert_eq!(find_block(input), Ok(None));
    }

    #[test]
    fn find_block_simple_three_line() {
        let input = "<!-- mneme:begin -->\nbody\n<!-- mneme:end -->\n";
        let block = find_block(input).unwrap().expect("block present");
        assert_eq!(block.before, "");
        assert_eq!(block.body, "body");
        assert_eq!(block.after, "");
    }

    #[test]
    fn find_block_preserves_before_and_after() {
        let input = "Top user content\n\
                     <!-- mneme:begin -->\n\
                     mneme guidance\n\
                     <!-- mneme:end -->\n\
                     Bottom user content\n";
        let block = find_block(input).unwrap().expect("block present");
        assert_eq!(block.before, "Top user content");
        assert_eq!(block.body, "mneme guidance");
        assert_eq!(block.after, "Bottom user content\n");
    }

    #[test]
    fn find_block_handles_multiline_body() {
        let input = "<!-- mneme:begin -->\n\
                     line one\n\
                     line two\n\
                     line three\n\
                     <!-- mneme:end -->\n";
        let block = find_block(input).unwrap().expect("block present");
        assert_eq!(block.body, "line one\nline two\nline three");
    }

    #[test]
    fn find_block_errors_on_begin_without_end() {
        let input = "<!-- mneme:begin -->\nbody\nno close\n";
        assert!(matches!(find_block(input), Err(MarkerError::Malformed(_))));
    }

    #[test]
    fn find_block_errors_on_end_without_begin() {
        let input = "body\n<!-- mneme:end -->\n";
        assert!(matches!(find_block(input), Err(MarkerError::Malformed(_))));
    }

    #[test]
    fn find_block_errors_on_swapped_order() {
        let input = "<!-- mneme:end -->\nbody\n<!-- mneme:begin -->\n";
        assert!(matches!(find_block(input), Err(MarkerError::Malformed(_))));
    }

    #[test]
    fn find_block_errors_on_duplicate_begin() {
        let input = "<!-- mneme:begin -->\na\n<!-- mneme:begin -->\nb\n<!-- mneme:end -->\n";
        assert!(matches!(find_block(input), Err(MarkerError::Malformed(_))));
    }

    #[test]
    fn find_block_errors_on_duplicate_pairs() {
        let input = "<!-- mneme:begin -->\na\n<!-- mneme:end -->\n\
                     <!-- mneme:begin -->\nb\n<!-- mneme:end -->\n";
        assert!(matches!(find_block(input), Err(MarkerError::Malformed(_))));
    }

    #[test]
    fn upsert_appends_when_no_block_exists() {
        let input = "User content\n";
        let result = upsert_block(input, "mneme says hi").unwrap();
        // Blank-line gap between user content and the new block.
        assert_eq!(
            result,
            "User content\n\n<!-- mneme:begin -->\nmneme says hi\n<!-- mneme:end -->\n"
        );
    }

    #[test]
    fn upsert_appends_to_empty_input_without_separator() {
        let result = upsert_block("", "first time").unwrap();
        assert_eq!(
            result,
            "<!-- mneme:begin -->\nfirst time\n<!-- mneme:end -->\n"
        );
    }

    #[test]
    fn upsert_replaces_existing_body() {
        let input = "header\n\n\
                     <!-- mneme:begin -->\n\
                     old body\n\
                     <!-- mneme:end -->\n\
                     footer\n";
        let result = upsert_block(input, "new body").unwrap();
        assert_eq!(
            result,
            "header\n\n\
             <!-- mneme:begin -->\n\
             new body\n\
             <!-- mneme:end -->\n\
             footer\n"
        );
    }

    #[test]
    fn upsert_is_idempotent() {
        let input = "user header\n";
        let body = "@MNEME.md reference";
        let once = upsert_block(input, body).unwrap();
        let twice = upsert_block(&once, body).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn upsert_preserves_user_content_around_block() {
        let input = "alpha\nbravo\n\n\
                     <!-- mneme:begin -->\n\
                     middle\n\
                     <!-- mneme:end -->\n\
                     charlie\n\
                     delta\n";
        let result = upsert_block(input, "fresh").unwrap();
        assert!(result.starts_with("alpha\nbravo\n\n"));
        assert!(result.contains("<!-- mneme:begin -->\nfresh\n<!-- mneme:end -->\n"));
        assert!(result.ends_with("charlie\ndelta\n"));
    }

    #[test]
    fn upsert_handles_empty_body() {
        // An empty body collapses to no body lines, just the
        // markers back-to-back.
        let result = upsert_block("", "").unwrap();
        assert_eq!(result, "<!-- mneme:begin -->\n<!-- mneme:end -->\n");
    }

    #[test]
    fn upsert_strips_trailing_newline_from_caller_body() {
        // Callers that hand in a body with its own trailing
        // newline don't end up with two blank lines before END.
        let result = upsert_block("", "with newline\n").unwrap();
        assert_eq!(
            result,
            "<!-- mneme:begin -->\nwith newline\n<!-- mneme:end -->\n"
        );
    }

    #[test]
    fn remove_is_noop_when_no_block() {
        let input = "Just user content\nNo markers anywhere\n";
        let result = remove_block(input).unwrap();
        assert_eq!(result, input);
    }

    #[test]
    fn remove_strips_block_and_collapses_separator() {
        // The blank-line separator before the block (which a fresh
        // `upsert_block` inserts) collapses on removal — "remove"
        // implies the section is gone, including its visual
        // surround. Round-trip behaviour with `upsert_block` is the
        // canonical test (`upsert_then_remove_round_trips_to_original`
        // below) — this test pins the explicit before/after shape.
        let input = "header\n\n\
                     <!-- mneme:begin -->\n\
                     guidance\n\
                     <!-- mneme:end -->\n\
                     footer\n";
        let result = remove_block(input).unwrap();
        assert_eq!(result, "header\nfooter\n");
    }

    #[test]
    fn remove_handles_block_at_end_of_file() {
        // Block was appended after a blank-line separator; removing
        // it leaves the user's prior content with its own trailing
        // newline only — the separator is gone.
        let input = "header\n\n\
                     <!-- mneme:begin -->\n\
                     guidance\n\
                     <!-- mneme:end -->\n";
        let result = remove_block(input).unwrap();
        assert_eq!(result, "header\n");
    }

    #[test]
    fn remove_handles_block_at_start_of_file() {
        let input = "<!-- mneme:begin -->\nguidance\n<!-- mneme:end -->\nfooter\n";
        let result = remove_block(input).unwrap();
        assert_eq!(result, "footer\n");
    }

    #[test]
    fn remove_handles_lonely_block() {
        let input = "<!-- mneme:begin -->\nguidance\n<!-- mneme:end -->\n";
        let result = remove_block(input).unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn upsert_then_remove_round_trips_to_original() {
        let original = "user content here\nmore lines\n";
        let added = upsert_block(original, "block body").unwrap();
        let removed = remove_block(&added).unwrap();
        assert_eq!(removed, original);
    }

    #[test]
    fn upsert_with_existing_block_preserves_round_trip() {
        let original = "before\n\n\
                        <!-- mneme:begin -->\n\
                        body\n\
                        <!-- mneme:end -->\n\
                        after\n";
        let upserted = upsert_block(original, "body").unwrap();
        // No-op upsert must equal the original.
        assert_eq!(upserted, original);
    }
}
