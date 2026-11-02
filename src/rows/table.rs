//! HTML table rows (IMP-DEC-033).
//!
//! The table is chosen by `selector` (the first `<table>` when omitted). Headers
//! come from the first `<thead>` row when present, otherwise the first row.
//! `colspan`/`rowspan` are **not expanded** in v1: the cell text appears once and
//! the table carries one bounded `span_ignored` note, so the limit is documented
//! rather than guessed. A selector that matches no table is not an error: it is a
//! `table_not_found` note with zero records.

use ego_tree::NodeRef;
use scraper::node::Node;
use scraper::{ElementRef, Html, Selector};

use crate::core::error::WidowError;

use super::{ParsedRows, RowFieldError, names_from_cells, positional_names, record_from_cells};

/// Elements whose text is not visible content.
const SKIP_TAGS: [&str; 4] = ["script", "style", "template", "noscript"];

/// Fail-fast selector check before any network request.
pub fn validate_selector(raw: &str) -> Result<(), WidowError> {
    Selector::parse(raw)
        .map(|_| ())
        .map_err(|error| WidowError::SchemaInvalid {
            pointer: "/selector".to_string(),
            message: format!("invalid CSS selector `{raw}`: {error}"),
        })
}

/// Parse the table selected from `html`.
pub fn parse(
    html: &str,
    selector: Option<&str>,
    header_row: bool,
    limit: usize,
) -> Result<ParsedRows, WidowError> {
    let document = Html::parse_document(html);
    let table_selector = Selector::parse("table").expect("static selector");
    let table = match selector {
        Some(raw) => {
            let parsed = Selector::parse(raw).map_err(|error| WidowError::SchemaInvalid {
                pointer: "/selector".to_string(),
                message: format!("invalid CSS selector `{raw}`: {error}"),
            })?;
            document
                .select(&parsed)
                .next()
                .and_then(|element| first_table(&element, &table_selector))
        }
        None => document.select(&table_selector).next(),
    };
    let Some(table) = table else {
        let mut parsed = ParsedRows::default();
        parsed.push_error(RowFieldError {
            row: 0,
            field: None,
            reason: "table_not_found".to_string(),
        });
        return Ok(parsed);
    };

    let tr_selector = Selector::parse("tr").expect("static selector");
    let rows: Vec<ElementRef<'_>> = table
        .select(&tr_selector)
        .filter(|row| !is_nested(row, &table))
        .collect();
    let thead_rows: Vec<ElementRef<'_>> = rows
        .iter()
        .filter(|row| in_thead(row, &table))
        .cloned()
        .collect();
    let body_rows: Vec<ElementRef<'_>> = rows
        .iter()
        .filter(|row| !in_thead(row, &table))
        .cloned()
        .collect();

    // One note for the whole table, at the first row that carries a span.
    let mut span_row: Option<usize> = None;
    let mut header_cells: Vec<ElementRef<'_>> = Vec::new();
    let mut data_rows: Vec<ElementRef<'_>> = Vec::new();
    if header_row {
        let header = thead_rows.first().or_else(|| body_rows.first());
        if let Some(header) = header {
            header_cells = cells_of(header);
            note_spans(&header_cells, 0, &mut span_row);
            data_rows = body_rows
                .iter()
                .filter(|row| row.id() != header.id())
                .cloned()
                .collect();
        }
    } else {
        data_rows = rows.clone();
    }
    // Bounded: one row beyond `limit` proves truncation, nothing more is read.
    data_rows.truncate(limit.saturating_add(1));
    let row_cells: Vec<Vec<ElementRef<'_>>> = data_rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let cells = cells_of(row);
            note_spans(&cells, index + 1, &mut span_row);
            cells
        })
        .collect();

    let header = if header_row {
        names_from_cells(&header_cells.iter().map(cell_text).collect::<Vec<_>>())
    } else {
        positional_names(row_cells.first().map(Vec::len).unwrap_or(0))
    };

    let mut parsed = ParsedRows::default();
    if let Some(row) = span_row {
        parsed.push_error(RowFieldError {
            row,
            field: None,
            reason: "span_ignored".to_string(),
        });
    }
    for (index, cells) in row_cells.iter().enumerate() {
        let values: Vec<String> = cells.iter().map(cell_text).collect();
        let record = record_from_cells(&header, &values, index + 1, &mut parsed);
        parsed.records.push(record);
    }
    parsed.row_count = parsed.records.len();
    Ok(parsed)
}

/// The selected element when it is a table, else its first table descendant.
fn first_table<'a>(element: &ElementRef<'a>, table: &Selector) -> Option<ElementRef<'a>> {
    if element.value().name() == "table" {
        return Some(*element);
    }
    element.select(table).next()
}

/// Direct `th`/`td` children of a row, in document order.
fn cells_of<'a>(row: &ElementRef<'a>) -> Vec<ElementRef<'a>> {
    row.children()
        .filter_map(ElementRef::wrap)
        .filter(|child| matches!(child.value().name(), "td" | "th"))
        .collect()
}

fn note_spans(cells: &[ElementRef<'_>], row: usize, span_row: &mut Option<usize>) {
    for cell in cells {
        let spans = ["colspan", "rowspan"].iter().any(|attribute| {
            cell.attr(attribute)
                .and_then(|value| value.trim().parse::<usize>().ok())
                .is_some_and(|span| span > 1)
        });
        if spans && span_row.is_none() {
            *span_row = Some(row);
        }
    }
}

/// A row belongs to this table only when no other table sits between it and the
/// chosen one; nested tables are their own tables.
fn is_nested(row: &ElementRef<'_>, table: &ElementRef<'_>) -> bool {
    for ancestor in row.ancestors() {
        if ancestor.id() == table.id() {
            return false;
        }
        if let Some(element) = ElementRef::wrap(ancestor)
            && element.value().name() == "table"
        {
            return true;
        }
    }
    false
}

fn in_thead(row: &ElementRef<'_>, table: &ElementRef<'_>) -> bool {
    for ancestor in row.ancestors() {
        if ancestor.id() == table.id() {
            return false;
        }
        if let Some(element) = ElementRef::wrap(ancestor)
            && element.value().name() == "thead"
        {
            return true;
        }
    }
    false
}

/// Visible text of a cell with runs of whitespace collapsed to one space.
fn cell_text(cell: &ElementRef<'_>) -> String {
    let mut out = String::new();
    collect_text(**cell, &mut out);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn collect_text(node: NodeRef<'_, Node>, out: &mut String) {
    for child in node.children() {
        match child.value() {
            Node::Text(text) => {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&text.text);
            }
            Node::Element(element) => {
                if SKIP_TAGS.contains(&element.name()) {
                    continue;
                }
                collect_text(child, out);
            }
            Node::Document | Node::Fragment => collect_text(child, out),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const THEAD_TABLE: &str = r#"<!doctype html><html><body>
        <table class="results">
          <thead><tr><th>Name</th><th>Price</th></tr></thead>
          <tbody>
            <tr><td>Widget</td><td>10.00</td></tr>
            <tr><td>Gadget</td><td>20.50</td></tr>
          </tbody>
        </table>
    </body></html>"#;

    const FIRST_ROW_TABLE: &str = r#"<table>
        <tr><td>Code</td><td>Value</td></tr>
        <tr><td>A</td><td>1</td></tr>
    </table>"#;

    #[test]
    fn thead_wins_over_the_first_row_and_cells_are_visible_text() {
        let parsed = parse(THEAD_TABLE, None, true, 10).unwrap();
        assert_eq!(parsed.row_count, 2);
        assert_eq!(
            parsed.records[0],
            json!({"Name": "Widget", "Price": "10.00"})
                .as_object()
                .cloned()
                .unwrap()
        );
        assert!(parsed.field_errors.is_empty());
    }

    #[test]
    fn without_a_thead_the_first_row_is_the_header() {
        let parsed = parse(FIRST_ROW_TABLE, None, true, 10).unwrap();
        assert_eq!(parsed.row_count, 1);
        assert_eq!(parsed.records[0]["Code"], json!("A"));
    }

    #[test]
    fn a_selector_picks_the_table_and_a_miss_is_a_bounded_note() {
        let parsed = parse(THEAD_TABLE, Some("table.results"), true, 10).unwrap();
        assert_eq!(parsed.records[0]["Name"], json!("Widget"));
        let miss = parse(THEAD_TABLE, Some("#nope"), true, 10).unwrap();
        assert_eq!(miss.row_count, 0);
        assert_eq!(miss.field_errors.len(), 1);
        assert_eq!(miss.field_errors[0].reason, "table_not_found");
        let error = parse(THEAD_TABLE, Some("::not-css"), true, 10).unwrap_err();
        assert_eq!(error.code().as_str(), "schema_invalid");
    }

    #[test]
    fn a_page_without_a_table_is_a_note_not_an_error() {
        let parsed = parse("<html><body><p>no table</p></body></html>", None, true, 10).unwrap();
        assert_eq!(parsed.row_count, 0);
        assert_eq!(parsed.field_errors[0].reason, "table_not_found");
    }

    #[test]
    fn spans_are_not_expanded_and_the_table_says_so_once() {
        let html = r#"<table>
            <tr><th>Name</th><th colspan="2">Range</th></tr>
            <tr><td>Widget</td><td>1</td><td>5</td></tr>
        </table>"#;
        let parsed = parse(html, None, true, 10).unwrap();
        assert_eq!(parsed.records[0]["Range"], json!("1"));
        assert_eq!(parsed.records[0]["column_3"], json!("5"));
        let spans: Vec<_> = parsed
            .field_errors
            .iter()
            .filter(|error| error.reason == "span_ignored")
            .collect();
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn nested_tables_do_not_leak_rows() {
        let html = r#"<table id="outer">
            <tr><th>Name</th></tr>
            <tr><td>Widget<table><tr><td>inner</td></tr></table></td></tr>
        </table>"#;
        let parsed = parse(html, None, true, 10).unwrap();
        assert_eq!(parsed.row_count, 1);
        assert_eq!(parsed.records[0]["Name"], json!("Widget inner"));
    }

    #[test]
    fn header_row_false_makes_every_row_data() {
        let parsed = parse(FIRST_ROW_TABLE, None, false, 10).unwrap();
        assert_eq!(parsed.row_count, 2);
        assert_eq!(parsed.records[0]["column_1"], json!("Code"));
        assert_eq!(parsed.records[1]["column_2"], json!("1"));
    }

    #[test]
    fn a_short_row_nulls_the_missing_cell() {
        let html = "<table><tr><th>a</th><th>b</th></tr><tr><td>1</td></tr></table>";
        let parsed = parse(html, None, true, 10).unwrap();
        assert_eq!(parsed.records[0]["b"], json!(null));
        assert_eq!(parsed.field_errors[0].reason, "ragged_short_row");
    }
}
