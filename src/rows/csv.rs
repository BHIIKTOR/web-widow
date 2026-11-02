//! RFC4180-style delimited text (IMP-DEC-033): quoted fields with `""` escapes,
//! embedded delimiters and newlines, CRLF or LF, and a UTF-8 BOM.
//!
//! Two deliberate leniencies, both documented rather than silent: a `"` that is
//! not at the start of a field is literal data, and a completely empty line is
//! skipped (it is never a one-empty-column record). An unclosed quote at end of
//! input is a typed `extraction_failed` naming the line and byte where it opened.

use crate::core::error::WidowError;

use super::{ParsedRows, names_from_cells, positional_names, record_from_cells};

/// Delimiter candidates, in tie-break order.
const CANDIDATES: [char; 4] = [',', ';', '\t', '|'];

/// Where an unterminated quoted field opened (line and byte offset).
#[derive(Debug, PartialEq, Eq)]
struct UnclosedQuote {
    line: usize,
    offset: usize,
}

/// Unquoted occurrences of each candidate, for delimiter sniffing.
pub fn count_unquoted_candidates(line: &str) -> [usize; 4] {
    let mut counts = [0usize; 4];
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                } else {
                    in_quotes = false;
                }
            }
            continue;
        }
        if c == '"' {
            in_quotes = true;
        } else if let Some(index) = CANDIDATES.iter().position(|candidate| *candidate == c) {
            counts[index] += 1;
        }
    }
    counts
}

/// Highest unquoted count on the header line; a tie keeps `,`.
pub fn sniff_delimiter(text: &str) -> char {
    let line = text.lines().next().unwrap_or_default();
    let counts = count_unquoted_candidates(line);
    let highest = counts.iter().copied().max().unwrap_or(0);
    if highest == 0 || counts[0] == highest {
        return ',';
    }
    CANDIDATES[counts
        .iter()
        .position(|count| *count == highest)
        .unwrap_or(0)]
}

/// Split the document into raw records of raw cells, stopping at `max_records`.
fn tokenize(
    text: &str,
    delimiter: char,
    max_records: usize,
) -> Result<Vec<Vec<String>>, UnclosedQuote> {
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut at_field_start = true;
    let mut quote_mark = (0usize, 0usize);
    let mut line = 1usize;
    let mut pending = false;
    let mut chars = text.char_indices().peekable();

    let push_record =
        |records: &mut Vec<Vec<String>>, record: &mut Vec<String>, field: &mut String| {
            record.push(std::mem::take(field));
            // A blank line is not a one-empty-column record.
            let blank = record.len() == 1 && record[0].is_empty();
            if !blank {
                records.push(std::mem::take(record));
            } else {
                record.clear();
            }
        };

    while let Some((offset, c)) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek().is_some_and(|(_, next)| *next == '"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                if c == '\n' {
                    line += 1;
                }
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if at_field_start => {
                in_quotes = true;
                quote_mark = (line, offset);
                at_field_start = false;
                pending = true;
            }
            '"' => {
                field.push('"');
                at_field_start = false;
                pending = true;
            }
            c if c == delimiter => {
                record.push(std::mem::take(&mut field));
                at_field_start = true;
                pending = true;
            }
            '\r' => {
                if chars.peek().is_some_and(|(_, next)| *next == '\n') {
                    chars.next();
                }
                push_record(&mut records, &mut record, &mut field);
                at_field_start = true;
                pending = false;
                line += 1;
                if records.len() >= max_records {
                    return Ok(records);
                }
            }
            '\n' => {
                push_record(&mut records, &mut record, &mut field);
                at_field_start = true;
                pending = false;
                line += 1;
                if records.len() >= max_records {
                    return Ok(records);
                }
            }
            other => {
                field.push(other);
                at_field_start = false;
                pending = true;
            }
        }
    }

    if in_quotes {
        return Err(UnclosedQuote {
            line: quote_mark.0,
            offset: quote_mark.1,
        });
    }
    if pending {
        push_record(&mut records, &mut record, &mut field);
    }
    Ok(records)
}

/// Parse a delimited document. `delimiter: None` sniffs the header line;
/// `limit` bounds the records kept (one extra is parsed to detect truncation).
pub fn parse(
    text: &str,
    delimiter: Option<char>,
    header_row: bool,
    limit: usize,
) -> Result<ParsedRows, WidowError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let delimiter = delimiter.unwrap_or_else(|| sniff_delimiter(text));
    // Header + `limit` rows + the row that proves there are more.
    let max_records = limit.saturating_add(2);
    let raw = tokenize(text, delimiter, max_records).map_err(|unclosed| {
        WidowError::Extraction(format!(
            "unclosed quote starting at line {} (byte {}): a quoted field must be closed before the end of the document",
            unclosed.line, unclosed.offset
        ))
    })?;

    let mut parsed = ParsedRows::default();
    let (header, rows) = if header_row {
        match raw.split_first() {
            Some((first, rest)) => (names_from_cells(first), rest),
            None => (Vec::new(), &[][..]),
        }
    } else {
        let width = raw.first().map(Vec::len).unwrap_or(0);
        (positional_names(width), &raw[..])
    };

    for (index, cells) in rows.iter().enumerate() {
        let record = record_from_cells(&header, cells, index + 1, &mut parsed);
        parsed.records.push(record);
    }
    parsed.records.truncate(limit.saturating_add(1));
    parsed.row_count = parsed.records.len();
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn quoted_delimiters_newlines_and_escaped_quotes_stay_in_one_field() {
        let text = "name,note\n\"Widget, large\",\"two\nlines and \"\"quotes\"\"\"\n";
        let parsed = parse(text, None, true, 10).unwrap();
        assert_eq!(parsed.row_count, 1);
        assert_eq!(
            parsed.records[0],
            json!({"name": "Widget, large", "note": "two\nlines and \"quotes\""})
                .as_object()
                .cloned()
                .unwrap()
        );
    }

    #[test]
    fn crlf_and_bom_and_trailing_newline_are_transparent() {
        let text = "\u{feff}id,name\r\n1,Alpha\r\n2,Beta\r\n";
        let parsed = parse(text, None, true, 10).unwrap();
        assert_eq!(parsed.row_count, 2);
        assert_eq!(parsed.records[1]["name"], json!("Beta"));
        assert_eq!(parsed.records[1]["id"], json!("2"));
    }

    #[test]
    fn sniffing_picks_semicolon_pipe_and_tab_and_ties_keep_comma() {
        assert_eq!(sniff_delimiter("a;b;c\n1;2;3\n"), ';');
        assert_eq!(sniff_delimiter("a|b|c\n1|2|3\n"), '|');
        assert_eq!(sniff_delimiter("a\tb\n1\t2\n"), '\t');
        assert_eq!(sniff_delimiter("a,b;c\n"), ',');
        assert_eq!(sniff_delimiter("\"a;b\",c\n"), ',');
        assert_eq!(sniff_delimiter("single\n"), ',');
    }

    #[test]
    fn an_explicit_delimiter_beats_the_sniff() {
        let parsed = parse("a;b\n1;2\n", Some(';'), true, 10).unwrap();
        assert_eq!(parsed.records[0]["b"], json!("2"));
    }

    #[test]
    fn blank_and_duplicate_headers_become_stable_names() {
        let parsed = parse(",price,price\nx,1,2\n", None, true, 10).unwrap();
        let record = &parsed.records[0];
        assert_eq!(record["column_1"], json!("x"));
        assert_eq!(record["price"], json!("1"));
        assert_eq!(record["price_2"], json!("2"));
    }

    #[test]
    fn header_row_false_uses_positional_names() {
        let parsed = parse("1,2\n3,4\n", None, false, 10).unwrap();
        assert_eq!(
            parsed.records[0],
            json!({"column_1": "1", "column_2": "2"})
                .as_object()
                .cloned()
                .unwrap()
        );
        assert_eq!(parsed.row_count, 2);
        assert!(parsed.field_errors.is_empty());
    }

    #[test]
    fn a_short_row_nulls_the_missing_field_and_a_long_row_adds_one() {
        let parsed = parse("a,b\n1\n2,3,4\n", None, true, 10).unwrap();
        assert_eq!(parsed.records[0]["b"], json!(null));
        assert_eq!(parsed.records[1]["column_3"], json!("4"));
        assert_eq!(parsed.field_errors.len(), 2);
        assert_eq!(parsed.field_errors[0].reason, "ragged_short_row");
        assert_eq!(parsed.field_errors[1].reason, "ragged_long_row");
    }

    #[test]
    fn an_unclosed_quote_is_typed_and_names_its_position() {
        let error = parse("a,b\n\"1,2\n", None, true, 10).unwrap_err();
        assert_eq!(error.code().as_str(), "extraction_failed");
        assert!(error.to_string().contains("line 2"), "{error}");
        assert!(error.to_string().contains("unclosed quote"), "{error}");
    }

    #[test]
    fn parsing_stops_at_the_limit_and_proves_more_rows_exist() {
        let text = "a\n1\n2\n3\n4\n";
        let mut parsed = parse(text, None, true, 2).unwrap();
        // The extra parsed row is kept for truncation detection and dropped by
        // the pipeline; `row_count` reports what was parsed.
        assert_eq!(parsed.records.len(), 3);
        assert_eq!(parsed.row_count, 3);
        parsed.apply_limit(2);
        assert_eq!(parsed.records.len(), 2);
        assert_eq!(parsed.row_count, 3);
        assert!(parsed.truncated);
    }

    #[test]
    fn a_stray_quote_inside_an_unquoted_field_is_literal() {
        let parsed = parse("a,b\n5\" screen,2\n", None, true, 10).unwrap();
        assert_eq!(parsed.records[0]["a"], json!("5\" screen"));
    }

    #[test]
    fn empty_input_is_zero_rows_not_an_error() {
        let parsed = parse("", None, true, 10).unwrap();
        assert_eq!(parsed.row_count, 0);
        assert!(parsed.records.is_empty());
    }
}
