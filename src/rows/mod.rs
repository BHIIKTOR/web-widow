//! Tabular extraction: the `extract_rows` data family.
//!
//! `extract` is CSS-over-HTML and its spec identity is `profiles.hash`; a CSV,
//! TSV, JSON or HTML-table document is a different family with its own spec
//! (`format`, `selector`, `delimiter`, `header_row`, `json_pointer`). Sibling
//! module, not a mode of `extract`, so neither contract has to widen.
//!
//! Everything here is pure: the pipeline acquires bytes (through the shared
//! SSRF → robots → host-gate path) and calls one of [`csv`], [`json`] or
//! [`table`]. Records are `serde_json::Map` values so a JSON document keeps its
//! number/boolean/null/nested types instead of being stringified.

pub mod csv;
pub mod json;
pub mod table;

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::core::error::WidowError;

/// Parsed rows kept when the caller is silent.
pub const DEFAULT_LIMIT: usize = 200;
/// Hard ceiling for `limit` (schema_invalid above it).
pub const MAX_LIMIT: usize = 1_000;
/// Per-string-value cap kept when the caller is silent.
pub const DEFAULT_MAX_FIELD_CHARS: usize = 8_000;
/// Hard ceiling for `max_field_chars` (schema_invalid above it).
pub const MAX_FIELD_CHARS: usize = 20_000;
/// Bounded `field_errors` list; past this `field_errors_truncated` is true.
pub const MAX_FIELD_ERRORS: usize = 20;
/// Auto-detection looks at the first 4 KiB of the decoded body.
pub const SNIFF_BYTES: usize = 4 * 1024;

/// The formats `extract_rows` parses. `Auto` is a request, never a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowFormat {
    Auto,
    Csv,
    Tsv,
    Json,
    HtmlTable,
}

impl RowFormat {
    pub fn parse(raw: &str) -> Result<Self, WidowError> {
        match raw {
            "auto" => Ok(Self::Auto),
            "csv" => Ok(Self::Csv),
            "tsv" => Ok(Self::Tsv),
            "json" => Ok(Self::Json),
            "html_table" => Ok(Self::HtmlTable),
            other => Err(WidowError::SchemaInvalid {
                pointer: "/format".to_string(),
                message: format!(
                    "unknown format `{other}`: expected auto, csv, tsv, json or html_table"
                ),
            }),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Csv => "csv",
            Self::Tsv => "tsv",
            Self::Json => "json",
            Self::HtmlTable => "html_table",
        }
    }

    /// A concrete format (everything but `Auto`).
    pub fn resolved(self) -> Option<Self> {
        (self != Self::Auto).then_some(self)
    }
}

/// The tabular spec as the caller stated it: this is the `profiles.spec_json`
/// payload and the `profiles.hash` input. It records the *request* (`format:
/// "auto"` included), not the response, so a cache lookup never needs a fetch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowsSpec {
    pub format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delimiter: Option<String>,
    pub header_row: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json_pointer: Option<String>,
}

/// One bounded anomaly in a parsed row set. `row` is the 1-based data row; `0`
/// is a table-level note (no `<table>` matched, ignored spans).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RowFieldError {
    pub row: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub reason: String,
}

/// What one parser produced, before the presentation caps.
#[derive(Debug, Default)]
pub struct ParsedRows {
    pub records: Vec<Map<String, Value>>,
    /// Rows parsed, stopping at `limit + 1` so truncation is detectable.
    pub row_count: usize,
    pub field_errors: Vec<RowFieldError>,
    pub field_errors_truncated: bool,
    /// Whether more rows were parsed than `limit` kept.
    pub truncated: bool,
}

impl ParsedRows {
    /// Keep the first `limit` records. Anomalies recorded for a row that is not
    /// returned are dropped with it, so `field_errors` always refers to a row the
    /// caller can see (`row: 0` is a table-level note).
    pub fn apply_limit(&mut self, limit: usize) {
        self.truncated = self.row_count > limit;
        self.records.truncate(limit);
        self.field_errors
            .retain(|error| error.row == 0 || error.row <= limit);
    }

    /// Record one anomaly, bounded by [`MAX_FIELD_ERRORS`].
    pub fn push_error(&mut self, error: RowFieldError) {
        if self.field_errors.len() < MAX_FIELD_ERRORS {
            self.field_errors.push(error);
        } else {
            self.field_errors_truncated = true;
        }
    }
}

/// The delimiters a CSV caller may name explicitly.
pub const SEPARATORS: [char; 4] = [',', ';', '\t', '|'];

/// One explicit `delimiter` value: exactly one separator character.
pub fn parse_delimiter(raw: &str) -> Result<char, WidowError> {
    let mut chars = raw.chars();
    match (chars.next(), chars.next()) {
        (Some(character), None) if SEPARATORS.contains(&character) => Ok(character),
        _ => Err(WidowError::SchemaInvalid {
            pointer: "/delimiter".to_string(),
            message: format!(
                "`{raw}` is not a delimiter: use exactly one of `,` `;` `\\t` `|`, or omit it to sniff"
            ),
        }),
    }
}

/// `limit` is 1..=1000; a per-call value never widens the ceiling.
pub fn check_limit(value: usize) -> Result<usize, WidowError> {
    if (1..=MAX_LIMIT).contains(&value) {
        Ok(value)
    } else {
        Err(WidowError::SchemaInvalid {
            pointer: "/limit".to_string(),
            message: format!("limit must be 1..={MAX_LIMIT}, got {value}"),
        })
    }
}

/// `max_field_chars` is 1..=20000.
pub fn check_max_field_chars(value: usize) -> Result<usize, WidowError> {
    if (1..=MAX_FIELD_CHARS).contains(&value) {
        Ok(value)
    } else {
        Err(WidowError::SchemaInvalid {
            pointer: "/max_field_chars".to_string(),
            message: format!("max_field_chars must be 1..={MAX_FIELD_CHARS}, got {value}"),
        })
    }
}

/// Column names for one header row: trimmed, blank → `column_{n}`, and a
/// repeated base name gets `_2`, `_3`, … (first occurrence keeps the bare name;
/// a generated suffix that would collide with an existing name keeps counting).
pub fn names_from_cells(cells: &[String]) -> Vec<String> {
    let mut used: HashSet<String> = HashSet::new();
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut names = Vec::with_capacity(cells.len());
    for (index, raw) in cells.iter().enumerate() {
        let trimmed = raw.trim();
        let base = if trimmed.is_empty() {
            format!("column_{}", index + 1)
        } else {
            trimmed.to_string()
        };
        let seen = counts.entry(base.clone()).or_insert(0);
        *seen += 1;
        let mut suffix = *seen;
        let mut name = if suffix == 1 {
            base.clone()
        } else {
            format!("{base}_{suffix}")
        };
        while used.contains(&name) {
            suffix += 1;
            name = format!("{base}_{suffix}");
        }
        used.insert(name.clone());
        names.push(name);
    }
    names
}

/// Positional names for `header_row: false`.
pub fn positional_names(width: usize) -> Vec<String> {
    (1..=width).map(|index| format!("column_{index}")).collect()
}

/// One data row against its header. Missing values become `null` plus a
/// `ragged_short_row` error; extra values land under `column_{n}` plus a
/// `ragged_long_row` error.
pub fn record_from_cells(
    header: &[String],
    cells: &[String],
    row: usize,
    parsed: &mut ParsedRows,
) -> Map<String, Value> {
    let mut record = Map::with_capacity(header.len().max(cells.len()));
    for (index, name) in header.iter().enumerate() {
        match cells.get(index) {
            Some(cell) => {
                record.insert(name.clone(), Value::String(cell.clone()));
            }
            None => {
                record.insert(name.clone(), Value::Null);
                parsed.push_error(RowFieldError {
                    row,
                    field: Some(name.clone()),
                    reason: "ragged_short_row".to_string(),
                });
            }
        }
    }
    for (index, cell) in cells.iter().enumerate().skip(header.len()) {
        let name = format!("column_{}", index + 1);
        record.insert(name.clone(), Value::String(cell.clone()));
        parsed.push_error(RowFieldError {
            row,
            field: Some(name),
            reason: "ragged_long_row".to_string(),
        });
    }
    record
}

/// `max_field_chars` over every string value of the returned records, counting
/// the cuts. The stored payload keeps the full values (same rule as `extract`).
pub fn cap_field_values(records: &mut [Map<String, Value>], max_field_chars: usize) -> usize {
    let mut cuts = 0usize;
    for record in records {
        for value in record.values_mut() {
            cut_value(value, max_field_chars, &mut cuts);
        }
    }
    cuts
}

fn cut_value(value: &mut Value, max_field_chars: usize, cuts: &mut usize) {
    match value {
        Value::String(text) => {
            if text.chars().count() > max_field_chars {
                *text = text.chars().take(max_field_chars).collect();
                *cuts += 1;
            }
        }
        Value::Array(items) => {
            for item in items {
                cut_value(item, max_field_chars, cuts);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                cut_value(item, max_field_chars, cuts);
            }
        }
        _ => {}
    }
}

/// Keep only the first [`SNIFF_BYTES`] bytes on a character boundary.
fn sniff_head(text: &str) -> &str {
    match text.char_indices().nth(SNIFF_BYTES) {
        Some((offset, _)) => &text[..offset],
        None => text,
    }
}

/// Auto-detection on the decoded body, used only when the response media type
/// does not name a format (for example a CSV served as `text/plain`). A JSON
/// document starts with `{`/`[`; otherwise the first line's unquoted delimiters
/// decide. `None` means "no tabular shape recognized".
pub fn sniff_format(text: &str) -> Option<RowFormat> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let head = sniff_head(text);
    let trimmed = head.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Some(RowFormat::Json);
    }
    let line = head.lines().next().unwrap_or_default();
    let counts = csv::count_unquoted_candidates(line);
    // A tab header is a TSV header; any other candidate is comma-separated.
    if counts[2] > 0 {
        return Some(RowFormat::Tsv);
    }
    if counts[0] > 0 || counts[1] > 0 || counts[3] > 0 {
        return Some(RowFormat::Csv);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_delimiter_must_be_exactly_one_separator() {
        for good in [",", ";", "\t", "|"] {
            assert_eq!(parse_delimiter(good).unwrap(), good.chars().next().unwrap());
        }
        for bad in ["", " ", ",,", "\\t", "ab"] {
            let error = parse_delimiter(bad).unwrap_err();
            assert_eq!(error.code().as_str(), "schema_invalid", "{bad:?}");
            assert_eq!(
                error.to_tool_error().detail.as_deref(),
                Some("pointer=/delimiter")
            );
        }
    }

    #[test]
    fn header_names_trim_fill_blanks_and_dedupe() {
        let names = names_from_cells(&[
            " name ".to_string(),
            String::new(),
            "price".to_string(),
            "price".to_string(),
            "price".to_string(),
            "column_2".to_string(),
        ]);
        assert_eq!(
            names,
            vec![
                "name".to_string(),
                "column_2".to_string(),
                "price".to_string(),
                "price_2".to_string(),
                "price_3".to_string(),
                "column_2_2".to_string(),
            ]
        );
    }

    #[test]
    fn ragged_rows_report_each_side_and_keep_both_shapes() {
        let header = vec!["a".to_string(), "b".to_string()];
        let mut parsed = ParsedRows::default();
        let short = record_from_cells(&header, &["only".to_string()], 1, &mut parsed);
        assert_eq!(short["a"], Value::String("only".into()));
        assert_eq!(short["b"], Value::Null);
        let long = record_from_cells(
            &header,
            &["x".to_string(), "y".to_string(), "z".to_string()],
            2,
            &mut parsed,
        );
        assert_eq!(long["column_3"], Value::String("z".into()));
        assert_eq!(
            parsed.field_errors,
            vec![
                RowFieldError {
                    row: 1,
                    field: Some("b".into()),
                    reason: "ragged_short_row".into()
                },
                RowFieldError {
                    row: 2,
                    field: Some("column_3".into()),
                    reason: "ragged_long_row".into()
                },
            ]
        );
    }

    #[test]
    fn field_errors_stop_at_the_bound_and_say_so() {
        let header = vec!["a".to_string(), "b".to_string()];
        let mut parsed = ParsedRows::default();
        for row in 1..=(MAX_FIELD_ERRORS + 5) {
            record_from_cells(&header, &[], row, &mut parsed);
        }
        assert_eq!(parsed.field_errors.len(), MAX_FIELD_ERRORS);
        assert!(parsed.field_errors_truncated);
    }

    #[test]
    fn the_field_cap_counts_nested_strings_and_leaves_numbers() {
        let mut records = vec![
            serde_json::json!({
                "name": "abcdef",
                "n": 42,
                "nested": { "deep": "uvwxyz" },
                "list": ["ok", "toolong"]
            })
            .as_object()
            .cloned()
            .unwrap(),
        ];
        let cuts = cap_field_values(&mut records, 3);
        assert_eq!(cuts, 3);
        assert_eq!(records[0]["name"], serde_json::json!("abc"));
        assert_eq!(records[0]["n"], serde_json::json!(42));
        assert_eq!(records[0]["nested"]["deep"], serde_json::json!("uvw"));
        assert_eq!(records[0]["list"][1], serde_json::json!("too"));
        assert_eq!(records[0]["list"][0], serde_json::json!("ok"));
    }

    #[test]
    fn sniffing_prefers_json_then_tab_then_comma() {
        assert_eq!(sniff_format("{\"a\":1}"), Some(RowFormat::Json));
        assert_eq!(sniff_format("\u{feff}[1,2]"), Some(RowFormat::Json));
        assert_eq!(sniff_format("a\tb\n1\t2\n"), Some(RowFormat::Tsv));
        assert_eq!(sniff_format("a;b\n1;2\n"), Some(RowFormat::Csv));
        assert_eq!(sniff_format("a,b\n1,2\n"), Some(RowFormat::Csv));
        assert_eq!(sniff_format("plain prose only"), None);
        // A comma inside a quoted header cell is not a delimiter.
        assert_eq!(sniff_format("\"a,b\"\n1\n"), None);
    }
}
