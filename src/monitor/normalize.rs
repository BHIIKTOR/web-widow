//! Change normalization and classification.
//!
//! Pure functions: the decision that a page "changed" must be testable without
//! a server or a database. The rules, in order:
//!
//! 1. Normalize the extracted Markdown (LF, trailing whitespace, runs of more
//!    than two blank lines, zero-width characters).
//! 2. Equal normalized text means **no change**: markup churn and line endings
//!    are not content.
//! 3. Different text with an identical word sequence is **cosmetic** (whitespace
//!    that survived normalization) when it touches at most the configured number
//!    of lines.
//! 4. Anything else — a changed word, or a whitespace change spanning more lines
//!    than the threshold — is **substantive**. A watched field change is
//!    substantive regardless of the text diff (the caller adds that rule).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use similar::TextDiff;

use crate::core::url::content_hash;

/// Characters that never carry meaning in extracted prose.
const ZERO_WIDTH: [char; 5] = ['\u{200B}', '\u{200C}', '\u{200D}', '\u{FEFF}', '\u{2060}'];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// Normalized text and watched fields are identical.
    #[default]
    None,
    /// Whitespace-only difference within the threshold.
    Cosmetic,
    /// A changed word, a watched field, or a larger structural change.
    Substantive,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Cosmetic => "cosmetic",
            Self::Substantive => "substantive",
        }
    }
}

/// Full change counts. The diff may be capped; these never are.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChangeStats {
    pub lines_added: usize,
    pub lines_removed: usize,
    pub changed_lines: usize,
    pub words_added: usize,
    pub words_removed: usize,
    /// Schema field names whose value differs between the two snapshots.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_fields: Vec<String>,
    /// True when the previous snapshot's text was missing from the CAS, so the
    /// text diff could not be computed. Never silently reported as "no change".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub previous_unavailable: bool,
}

/// Normalize extracted Markdown for change detection and as the CAS identity.
pub fn normalize(markdown: &str) -> String {
    let unified = markdown.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<String> = Vec::new();
    for raw in unified.split('\n') {
        let cleaned: String = raw.chars().filter(|c| !ZERO_WIDTH.contains(c)).collect();
        let line = cleaned.trim_end();
        // "runs of >2 blank lines collapsed": at most two empty lines survive.
        if line.is_empty()
            && lines.len() >= 2
            && lines[lines.len() - 1].is_empty()
            && lines[lines.len() - 2].is_empty()
        {
            continue;
        }
        lines.push(line.to_string());
    }
    while lines.first().is_some_and(|line| line.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// BLAKE3 of the normalized text; the `snapshots.normalized_hash` and the CAS
/// key of the stored Markdown.
pub fn normalized_hash(normalized: &str) -> String {
    content_hash(normalized.as_bytes())
}

fn word_sequence(text: &str) -> impl Iterator<Item = &str> {
    text.split_whitespace()
}

/// Count inserted/deleted units of a line- or word-level diff.
fn diff_counts(diff: &TextDiff<'_, '_, str>) -> (usize, usize) {
    let mut added = 0usize;
    let mut removed = 0usize;
    for op in diff.ops() {
        match op {
            similar::DiffOp::Equal { .. } => {}
            similar::DiffOp::Delete { old_len, .. } => removed += old_len,
            similar::DiffOp::Insert { new_len, .. } => added += new_len,
            similar::DiffOp::Replace {
                old_len, new_len, ..
            } => {
                removed += old_len;
                added += new_len;
            }
        }
    }
    (added, removed)
}

/// Full (uncapped) diff counts and unified diff for two texts, without
/// normalization and without the cosmetic/substantive decision.
///
/// `snapshot_compare` uses this directly: a page compare must compare the
/// delivered text as stored, and a monitor compare already hands it normalized
/// text. `classify` is the same machinery plus the change kind.
pub fn diff_stats(previous: &str, current: &str) -> (ChangeStats, String) {
    if previous == current {
        return (ChangeStats::default(), String::new());
    }
    let line_diff = TextDiff::from_lines(previous, current);
    let (lines_added, lines_removed) = diff_counts(&line_diff);
    let word_diff = TextDiff::from_words(previous, current);
    let (words_added, words_removed) = diff_counts(&word_diff);
    let stats = ChangeStats {
        lines_added,
        lines_removed,
        changed_lines: lines_added + lines_removed,
        words_added,
        words_removed,
        changed_fields: Vec::new(),
        previous_unavailable: false,
    };
    let diff = line_diff
        .unified_diff()
        .context_radius(3)
        .header("previous", "current")
        .to_string();
    (stats, diff)
}

/// Classify two normalized texts and return the full (uncapped) unified diff.
pub fn classify(
    previous: &str,
    current: &str,
    cosmetic_max_changed_lines: usize,
) -> (ChangeKind, ChangeStats, String) {
    if previous == current {
        return (ChangeKind::None, ChangeStats::default(), String::new());
    }
    let (stats, diff) = diff_stats(previous, current);
    let words_identical = word_sequence(previous).eq(word_sequence(current));
    let kind = if words_identical && stats.changed_lines <= cosmetic_max_changed_lines {
        ChangeKind::Cosmetic
    } else {
        ChangeKind::Substantive
    };
    (kind, stats, diff)
}

/// Cap a unified diff at `max_bytes` on a UTF-8 character boundary.
/// Returns the kept prefix and whether it was cut.
pub fn cap_diff(diff: &str, max_bytes: usize) -> (String, bool) {
    if diff.len() <= max_bytes {
        return (diff.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !diff.is_char_boundary(end) {
        end -= 1;
    }
    (diff[..end].to_string(), true)
}

/// One value per field across every record the spec matched. A single-record spec
/// yields that record unchanged; a list spec whose values differ per record
/// yields an array, so one changed row is still a field change.
pub fn aggregate_fields(
    records: &[serde_json::Map<String, serde_json::Value>],
) -> serde_json::Map<String, serde_json::Value> {
    if let [record] = records {
        return record.clone();
    }
    let mut keys: BTreeSet<&str> = BTreeSet::new();
    for record in records {
        keys.extend(record.keys().map(String::as_str));
    }
    let mut aggregated = serde_json::Map::new();
    for key in keys {
        let values: Vec<serde_json::Value> = records
            .iter()
            .map(|record| record.get(key).cloned().unwrap_or(serde_json::Value::Null))
            .collect();
        let distinct: BTreeSet<String> = values.iter().map(|value| value.to_string()).collect();
        let value = if distinct.len() == 1 {
            values.into_iter().next().unwrap_or(serde_json::Value::Null)
        } else {
            serde_json::Value::Array(values)
        };
        aggregated.insert(key.to_string(), value);
    }
    aggregated
}

/// Changed schema fields: union of the two records compared per field. A missing
/// record on either side is a change (null is a value).
pub fn changed_fields(
    previous: Option<&serde_json::Map<String, serde_json::Value>>,
    current: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Vec<String> {
    let mut names: BTreeSet<&str> = BTreeSet::new();
    if let Some(previous) = previous {
        names.extend(previous.keys().map(String::as_str));
    }
    if let Some(current) = current {
        names.extend(current.keys().map(String::as_str));
    }
    let previous_value = |name: &str| previous.and_then(|record| record.get(name));
    let current_value = |name: &str| current.and_then(|record| record.get(name));
    names
        .into_iter()
        .filter(|name| previous_value(name) != current_value(name))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalization_removes_line_ending_and_whitespace_noise() {
        let crlf = "first\r\nsecond\r\n\r\n\r\n\r\nthird   \r\n";
        assert_eq!(normalize(crlf), "first\nsecond\n\n\nthird");
        let zero = "a\u{200B}b\u{FEFF}c";
        assert_eq!(normalize(zero), "abc");
        assert_eq!(normalize("  leading kept\n"), "  leading kept");
    }

    #[test]
    fn identical_text_after_normalization_is_no_change() {
        let previous = normalize("one two\nthree");
        let current = normalize("one two\r\nthree  \n");
        let (kind, stats, diff) = classify(&previous, &current, 2);
        assert_eq!(kind, ChangeKind::None);
        assert_eq!(stats.changed_lines, 0);
        assert!(diff.is_empty());
    }

    #[test]
    fn a_surviving_whitespace_change_is_cosmetic() {
        let (kind, stats, diff) = classify("one two three\nfour", "one  two three\nfour", 2);
        assert_eq!(kind, ChangeKind::Cosmetic, "{stats:?}");
        assert_eq!(stats.changed_lines, 2);
        assert!(diff.contains("-one two three"));
        assert!(diff.contains("+one  two three"));
    }

    #[test]
    fn a_single_word_change_is_substantive_and_appears_in_the_diff() {
        let (kind, stats, diff) = classify(
            "the lighthouse keeper counted forty gulls",
            "the lighthouse keeper counted forty-one gulls",
            2,
        );
        assert_eq!(kind, ChangeKind::Substantive);
        assert_eq!(stats.changed_lines, 2);
        assert!(diff.contains("forty-one"), "{diff}");
    }

    #[test]
    fn a_wide_whitespace_change_exceeds_the_cosmetic_threshold() {
        let previous = "a b\nc d\ne f\ng h";
        let current = "a  b\nc  d\ne  f\ng  h";
        let (kind, stats, _) = classify(previous, current, 2);
        assert_eq!(kind, ChangeKind::Substantive);
        assert_eq!(stats.changed_lines, 8);
    }

    #[test]
    fn the_diff_cap_respects_utf8_boundaries() {
        let diff = "ééééé";
        let (kept, truncated) = cap_diff(diff, 3);
        assert!(truncated);
        assert_eq!(kept, "é");
        let (kept, truncated) = cap_diff(diff, 64);
        assert!(!truncated);
        assert_eq!(kept, diff);
    }

    #[test]
    fn aggregate_fields_keeps_a_single_record_and_flags_list_changes() {
        let records = vec![json!({ "title": "A" }).as_object().unwrap().clone()];
        assert_eq!(aggregate_fields(&records).get("title"), Some(&json!("A")));
        let rows: Vec<_> = vec![json!({ "price": "10" }), json!({ "price": "11" })]
            .into_iter()
            .map(|value| value.as_object().unwrap().clone())
            .collect();
        assert_eq!(
            aggregate_fields(&rows).get("price"),
            Some(&json!(["10", "11"]))
        );
    }

    #[test]
    fn changed_fields_are_compared_per_name_and_null_counts() {
        let previous = json!({ "title": "A", "price": "10", "body": "x" });
        let current = json!({ "title": "A", "price": "11", "body": "x", "extra": "y" });
        let previous = previous.as_object().unwrap();
        let current = current.as_object().unwrap();
        assert_eq!(
            changed_fields(Some(previous), Some(current)),
            vec!["extra", "price"]
        );
        assert_eq!(
            changed_fields(Some(previous), None),
            vec!["body", "price", "title"]
        );
        assert!(changed_fields(Some(previous), Some(previous)).is_empty());
    }
}
