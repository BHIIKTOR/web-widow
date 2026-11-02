//! JSON rows (IMP-DEC-033): an RFC6901 pointer selects the record set.
//!
//! An array of objects is one record per element; a single object is one record;
//! an array with no object element becomes `{"value": …}` per element, keeping
//! the JSON type. Everything else is `schema_invalid` at `/json_pointer` with the
//! observed shape, so a caller learns what the document is instead of receiving
//! an empty result.

use serde_json::{Map, Value};

use crate::core::error::WidowError;

use super::ParsedRows;

/// Parse `text` and select the record set named by `pointer` (empty = root).
pub fn parse(text: &str, pointer: &str, limit: usize) -> Result<ParsedRows, WidowError> {
    let document: Value = serde_json::from_str(text).map_err(|error| {
        WidowError::Extraction(format!(
            "invalid JSON at line {} column {}: {error}",
            error.line().max(1),
            error.column().max(1)
        ))
    })?;
    let target = resolve(&document, pointer)?;
    let mut parsed = ParsedRows::default();
    match target {
        Value::Object(map) => {
            parsed.records.push(map.clone());
        }
        Value::Array(items) => {
            let objects = items.iter().filter(|item| item.is_object()).count();
            if objects == items.len() {
                parsed
                    .records
                    .extend(items.iter().filter_map(|item| item.as_object().cloned()));
            } else if objects == 0 {
                parsed.records.extend(items.iter().map(|item| {
                    let mut record = Map::new();
                    record.insert("value".to_string(), item.clone());
                    record
                }));
            } else {
                return Err(schema_invalid(format!(
                    "the array at `{}` mixes object and non-object elements; keep one record shape",
                    display_pointer(pointer)
                )));
            }
        }
        other => {
            return Err(schema_invalid(format!(
                "expected an object or an array at `{}` but found a {}",
                display_pointer(pointer),
                type_name(other)
            )));
        }
    }
    parsed.records.truncate(limit.saturating_add(1));
    parsed.row_count = parsed.records.len();
    Ok(parsed)
}

fn schema_invalid(message: String) -> WidowError {
    WidowError::SchemaInvalid {
        pointer: "/json_pointer".to_string(),
        message,
    }
}

/// Fail-fast pointer check before any network request: empty, or `/`-rooted with
/// valid `~0`/`~1` escapes.
pub fn validate_pointer(pointer: &str) -> Result<(), WidowError> {
    if pointer.is_empty() {
        return Ok(());
    }
    if !pointer.starts_with('/') {
        return Err(schema_invalid(format!(
            "`{pointer}` is not a JSON pointer: it must be empty or start with `/`"
        )));
    }
    for raw in pointer.split('/').skip(1) {
        if unescape(raw).is_none() {
            return Err(schema_invalid(format!(
                "invalid `~` escape in pointer token `{raw}`"
            )));
        }
    }
    Ok(())
}

fn display_pointer(pointer: &str) -> &str {
    if pointer.is_empty() { "/" } else { pointer }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// RFC6901 evaluation with the strict array-index form (no leading zeros, no
/// `-`), because a read has no "append" position.
fn resolve<'a>(document: &'a Value, pointer: &str) -> Result<&'a Value, WidowError> {
    if pointer.is_empty() {
        return Ok(document);
    }
    if !pointer.starts_with('/') {
        return Err(schema_invalid(format!(
            "`{pointer}` is not a JSON pointer: it must be empty or start with `/`"
        )));
    }
    let mut current = document;
    // The pointer of `current`, so an error names where the walk stopped.
    let mut walked = String::new();
    for raw in pointer.split('/').skip(1) {
        let token = unescape(raw).ok_or_else(|| {
            schema_invalid(format!("invalid `~` escape in pointer token `{raw}`"))
        })?;
        let next = format!("{walked}/{token}");
        current = match current {
            Value::Object(map) => map.get(&token).ok_or_else(|| {
                schema_invalid(format!("no value at `{next}` in the JSON document"))
            })?,
            Value::Array(items) => {
                let index = array_index(&token).ok_or_else(|| {
                    schema_invalid(format!(
                        "`{token}` is not an array index at `{}`",
                        display_pointer(&walked)
                    ))
                })?;
                items.get(index).ok_or_else(|| {
                    schema_invalid(format!("no element {index} at `{next}` in the JSON array"))
                })?
            }
            other => {
                return Err(schema_invalid(format!(
                    "cannot descend into a {} at `{}`",
                    type_name(other),
                    display_pointer(&walked)
                )));
            }
        };
        walked = next;
    }
    Ok(current)
}

fn unescape(token: &str) -> Option<String> {
    let mut out = String::with_capacity(token.len());
    let mut chars = token.chars();
    while let Some(c) = chars.next() {
        if c != '~' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('0') => out.push('~'),
            Some('1') => out.push('/'),
            _ => return None,
        }
    }
    Some(out)
}

fn array_index(token: &str) -> Option<usize> {
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if token.len() > 1 && token.starts_with('0') {
        return None;
    }
    token.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DOCUMENT: &str = r#"{
        "items": [
            {"name": "Widget", "price": 10.0, "qty": 3},
            {"name": "Gadget", "price": 20.5, "qty": 7}
        ],
        "single": {"name": "One"},
        "labels": ["alpha", "beta"],
        "meta": {"count": 2, "nested": {"ok": true}}
    }"#;

    #[test]
    fn a_pointer_selects_an_array_of_objects() {
        let parsed = parse(DOCUMENT, "/items", 10).unwrap();
        assert_eq!(parsed.row_count, 2);
        assert_eq!(parsed.records[0]["name"], json!("Widget"));
        // Numbers keep their JSON type.
        assert_eq!(parsed.records[1]["price"], json!(20.5));
        assert!(parsed.records[1]["price"].is_number());
    }

    #[test]
    fn the_root_object_and_a_nested_object_both_become_one_record() {
        let parsed = parse(DOCUMENT, "", 10).unwrap();
        assert_eq!(parsed.row_count, 1);
        assert!(parsed.records[0]["items"].is_array());
        let single = parse(DOCUMENT, "/single", 10).unwrap();
        assert_eq!(single.records[0]["name"], json!("One"));
    }

    #[test]
    fn an_array_of_scalars_becomes_value_records_with_types_kept() {
        let parsed = parse(DOCUMENT, "/labels", 10).unwrap();
        assert_eq!(
            parsed.records[0],
            json!({"value": "alpha"}).as_object().cloned().unwrap()
        );
        let numbers = parse("[1, true, null]", "", 10).unwrap();
        assert_eq!(numbers.records[0]["value"], json!(1));
        assert_eq!(numbers.records[1]["value"], json!(true));
        assert_eq!(numbers.records[2]["value"], json!(null));
    }

    #[test]
    fn nested_objects_and_arrays_survive_uncollapsed() {
        let parsed = parse(DOCUMENT, "/meta", 10).unwrap();
        assert_eq!(parsed.records[0]["nested"], json!({"ok": true}));
    }

    #[test]
    fn a_bad_pointer_is_typed_at_json_pointer() {
        for pointer in [
            "/missing",
            "items",
            "/items/9",
            "/items/01",
            "/items/x",
            "/meta/count/deep",
        ] {
            let error = parse(DOCUMENT, pointer, 10).unwrap_err();
            let tool = error.to_tool_error();
            assert_eq!(tool.code.as_str(), "schema_invalid", "{pointer}: {tool:?}");
            assert_eq!(tool.detail.as_deref(), Some("pointer=/json_pointer"));
        }
    }

    #[test]
    fn a_pointer_with_escapes_reads_the_escaped_key() {
        let document = r#"{"a/b": {"x~y": [1, 2]}}"#;
        let parsed = parse(document, "/a~1b/x~0y", 10).unwrap();
        assert_eq!(parsed.records[0]["value"], json!(1));
        assert_eq!(parsed.records[1]["value"], json!(2));
        assert!(parse(document, "/a~2b", 10).is_err());
    }

    #[test]
    fn a_scalar_target_and_a_mixed_array_are_typed_errors_naming_the_shape() {
        let scalar = parse(DOCUMENT, "/meta/count", 10).unwrap_err();
        assert!(scalar.to_string().contains("number"), "{scalar}");
        let mixed = parse(r#"{"rows": [{"a": 1}, 2]}"#, "/rows", 10).unwrap_err();
        assert!(mixed.to_string().contains("mixes"), "{mixed}");
    }

    #[test]
    fn invalid_json_names_a_position() {
        let error = parse("{not json", "", 10).unwrap_err();
        assert_eq!(error.code().as_str(), "extraction_failed");
        assert!(error.to_string().contains("line 1"), "{error}");
    }

    #[test]
    fn the_limit_keeps_one_record_extra_for_truncation_detection() {
        let document = json!({"rows": [{"i": 1}, {"i": 2}, {"i": 3}]}).to_string();
        let mut parsed = parse(&document, "/rows", 2).unwrap();
        assert_eq!(parsed.row_count, 3);
        parsed.apply_limit(2);
        assert_eq!(parsed.records.len(), 2);
        assert_eq!(parsed.row_count, 3);
        assert!(parsed.truncated);
    }
}
