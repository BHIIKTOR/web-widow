//! Evaluate a compiled extraction spec over the parsed DOM.
//!
//! Deliberately independent of `mode=main` Markdown: the spec is evaluated
//! against the page's own tree, so a page whose readability selection omits a
//! block still yields records for it (separation). One parse per page,
//! recorded in [`EvalOutcome::dom_parses`] so the property is testable without
//! timing.

use ego_tree::NodeRef;
use scraper::node::Node;
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use url::Url;

use crate::extract::spec::{CompiledField, CompiledSpec, Container, Transform};

/// Per-record/per-field failure codes. These are field codes, not the
/// tool error taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FieldErrorCode {
    SelectorNoMatch,
    AttrMissing,
    UrlJoinFailed,
    NumberParseFailed,
}

impl FieldErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SelectorNoMatch => "selector_no_match",
            Self::AttrMissing => "attr_missing",
            Self::UrlJoinFailed => "url_join_failed",
            Self::NumberParseFailed => "number_parse_failed",
        }
    }
}

/// One field failure. `record`/`field` are `None` for a page-level failure (the
/// container matched nothing), which is reported instead of a fabricated record.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FieldError {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub code: FieldErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct EvalOptions {
    pub max_records: usize,
}

#[derive(Debug, Clone)]
pub struct EvalOutcome {
    pub records: Vec<Map<String, Value>>,
    pub field_errors: Vec<FieldError>,
    /// Total records the spec matched, before the cap.
    pub record_count: usize,
    pub truncated: bool,
    /// Instrumented DOM parse count: always 1 (no per-record or per-field parse).
    pub dom_parses: usize,
}

type FieldFailure = (FieldErrorCode, Option<String>);

pub fn evaluate(html: &str, base: &Url, spec: &CompiledSpec, options: &EvalOptions) -> EvalOutcome {
    let document = Html::parse_document(html);
    let mut outcome = evaluate_document(&document, base, spec, options);
    outcome.dom_parses = 1;
    outcome
}

/// Evaluate against an already parsed document. The caller owns the parse count:
/// `dom_parses` stays 0 so a combined markdown+spec path can report one parse.
pub fn evaluate_document(
    document: &Html,
    base: &Url,
    spec: &CompiledSpec,
    options: &EvalOptions,
) -> EvalOutcome {
    let dom_parses = 0;

    let roots: Vec<ElementRef<'_>> = match &spec.container {
        Some(Container::Css(selector)) => document.select(selector).collect(),
        // `:main` is evaluated inside the readability article's `body`:
        // the caller supplies that parsed article document, and the wrapper
        // `<html>`/`<head>` are never a field root. A document without a body
        // falls back to its root.
        Some(Container::Main) => document
            .select(&Selector::parse("body").expect("static body selector"))
            .next()
            .map_or_else(|| vec![document.root_element()], |body| vec![body]),
        None => vec![document.root_element()],
    };

    if roots.is_empty() {
        return EvalOutcome {
            records: Vec::new(),
            field_errors: vec![FieldError {
                record: None,
                field: None,
                code: FieldErrorCode::SelectorNoMatch,
                detail: Some("container matched no elements".to_string()),
            }],
            record_count: 0,
            truncated: false,
            dom_parses,
        };
    }

    let record_count = roots.len();
    let mut records = Vec::with_capacity(record_count.min(options.max_records));
    let mut field_errors = Vec::new();

    for (index, root) in roots.iter().take(options.max_records).enumerate() {
        let mut record: Map<String, Value> = Map::new();
        for (name, field) in &spec.fields {
            match evaluate_field(root, field, base) {
                Ok(value) => {
                    record.insert(name.clone(), value);
                }
                Err((code, detail)) => field_errors.push(FieldError {
                    record: Some(index),
                    field: Some(name.clone()),
                    code,
                    detail,
                }),
            }
        }
        records.push(record);
    }

    EvalOutcome {
        records,
        field_errors,
        record_count,
        truncated: record_count > options.max_records,
        dom_parses,
    }
}

fn evaluate_field(
    root: &ElementRef<'_>,
    field: &CompiledField,
    base: &Url,
) -> Result<Value, FieldFailure> {
    // `*` as the whole selector means the record scope itself,
    // not its first descendant: the only useful meaning of a universal selector
    // in a record-shaped spec, and the fix for the silent `<head>` match.
    if field.source.trim() == "*" {
        let value = transform_value(root, field, base)?;
        return Ok(if field.all {
            Value::Array(vec![value])
        } else {
            value
        });
    }
    if field.all {
        let mut values = Vec::new();
        for element in root.select(&field.selector) {
            values.push(transform_value(&element, field, base)?);
        }
        // A collection's empty state is data (`[]`), not a failure: the selector
        // ran and found nothing. Scalar fields keep `selector_no_match`, where
        // the absence of the one required value is the failure.
        return Ok(Value::Array(values));
    }
    let element = root
        .select(&field.selector)
        .next()
        .ok_or_else(|| no_match(field))?;
    transform_value(&element, field, base)
}

fn no_match(field: &CompiledField) -> FieldFailure {
    (
        FieldErrorCode::SelectorNoMatch,
        Some(format!("selector `{}` matched no elements", field.source)),
    )
}

fn transform_value(
    element: &ElementRef<'_>,
    field: &CompiledField,
    base: &Url,
) -> Result<Value, FieldFailure> {
    match field.transform {
        Transform::Text => Ok(Value::String(element_text(element))),
        Transform::Html => Ok(Value::String(element.inner_html())),
        Transform::Trim => Ok(Value::String(collapse(&raw_value(element, field)?))),
        Transform::Attr => {
            let name = field
                .attr
                .as_deref()
                .ok_or((FieldErrorCode::AttrMissing, None))?;
            element
                .value()
                .attr(name)
                .map(|value| Value::String(value.to_string()))
                .ok_or_else(|| {
                    (
                        FieldErrorCode::AttrMissing,
                        Some(format!("attribute `{name}` is absent")),
                    )
                })
        }
        Transform::AbsoluteUrl => {
            let raw = url_source(element, field)?;
            let joined = base.join(raw.trim()).map_err(|error| {
                (
                    FieldErrorCode::UrlJoinFailed,
                    Some(format!("cannot resolve `{raw}`: {error}")),
                )
            })?;
            Ok(Value::String(joined.to_string()))
        }
        Transform::Number => {
            let raw = raw_value(element, field)?;
            parse_number_json(&raw).ok_or_else(|| {
                (
                    FieldErrorCode::NumberParseFailed,
                    Some(format!("`{raw}` does not contain a number")),
                )
            })
        }
    }
}

/// Attribute value when the field names one, otherwise the element's text.
fn raw_value(element: &ElementRef<'_>, field: &CompiledField) -> Result<String, FieldFailure> {
    match field.attr.as_deref() {
        Some(name) => element
            .value()
            .attr(name)
            .map(str::to_string)
            .ok_or_else(|| {
                (
                    FieldErrorCode::AttrMissing,
                    Some(format!("attribute `{name}` is absent")),
                )
            }),
        None => Ok(element_text(element)),
    }
}

/// `absolute_url` reads the named attribute, else `href`, else `src`. There is no
/// text fallback: the transform resolves links and image sources, not prose.
fn url_source<'a>(
    element: &ElementRef<'a>,
    field: &CompiledField,
) -> Result<&'a str, FieldFailure> {
    if let Some(name) = field.attr.as_deref() {
        return element.value().attr(name).ok_or_else(|| {
            (
                FieldErrorCode::AttrMissing,
                Some(format!("attribute `{name}` is absent")),
            )
        });
    }
    for candidate in ["href", "src"] {
        if let Some(value) = element.value().attr(candidate) {
            return Ok(value);
        }
    }
    Err((
        FieldErrorCode::AttrMissing,
        Some("element has neither href nor src".to_string()),
    ))
}

/// Subtrees a field `text` value must never include. This is a
/// field-level list, deliberately separate from the page-flattening skip set in
/// `extract`: a field is prose, not a page.
const FIELD_SKIP_TAGS: &[&str] = &["script", "style", "template", "head"];

fn element_text(element: &ElementRef<'_>) -> String {
    if FIELD_SKIP_TAGS.contains(&element.value().name()) {
        return String::new();
    }
    let mut text = String::new();
    walk_field_text(**element, &mut text);
    text.trim().to_string()
}

fn walk_field_text(node: NodeRef<'_, Node>, out: &mut String) {
    match node.value() {
        Node::Text(content) => out.push_str(&content.text),
        Node::Element(element) => {
            if FIELD_SKIP_TAGS.contains(&element.name()) {
                return;
            }
            for child in node.children() {
                walk_field_text(child, out);
            }
        }
        Node::Document | Node::Fragment => {
            for child in node.children() {
                walk_field_text(child, out);
            }
        }
        _ => {}
    }
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse a number out of arbitrary cell text. Handles currency symbols, thousands
/// separators and both `1.234,56` and `1,234.56` conventions; rejects anything
/// that is not a finite number.
pub fn parse_number_json(raw: &str) -> Option<Value> {
    let number = parse_number(raw)?;
    if number.fract() == 0.0 && number.abs() < 9_007_199_254_740_992.0 {
        Some(Value::from(number as i64))
    } else {
        Some(Value::from(number))
    }
}

pub fn parse_number(raw: &str) -> Option<f64> {
    let token = numeric_token(raw)?;
    let mut normalized = String::with_capacity(token.len());
    let dots = token.matches('.').count();
    let commas = token.matches(',').count();

    let decimal_separator: Option<char> = if dots > 0 && commas > 0 {
        // The rightmost separator wins: "1.234,56" -> comma, "1,234.56" -> dot.
        match (token.rfind('.'), token.rfind(',')) {
            (Some(dot), Some(comma)) if comma > dot => Some(','),
            _ => Some('.'),
        }
    } else if commas > 0 {
        // One comma with 1-2 trailing digits is a decimal comma; otherwise
        // thousands separators.
        let trailing = token.len() - token.rfind(',')? - 1;
        if commas == 1 && trailing <= 2 && trailing > 0 {
            Some(',')
        } else {
            None
        }
    } else if dots > 1 {
        // "1.234.567": every dot is a thousands separator.
        None
    } else {
        Some('.')
    };

    for character in token.chars() {
        match character {
            '.' | ',' if Some(character) == decimal_separator => normalized.push('.'),
            '.' | ',' => {}
            other => normalized.push(other),
        }
    }
    let value: f64 = normalized.parse().ok()?;
    if value.is_finite() { Some(value) } else { None }
}

/// The first numeric run in the text: optional sign, digits, separators.
fn numeric_token(raw: &str) -> Option<&str> {
    let bytes = raw.as_bytes();
    let mut start = None;
    for (index, byte) in bytes.iter().enumerate() {
        let is_sign = *byte == b'-' || *byte == b'+';
        let next_is_digit = bytes.get(index + 1).is_some_and(u8::is_ascii_digit);
        if byte.is_ascii_digit() || (is_sign && next_is_digit) {
            start = Some(index);
            break;
        }
    }
    let start = start?;
    let mut end = start;
    for (offset, byte) in bytes[start..].iter().enumerate() {
        let absolute = start + offset;
        let keep = byte.is_ascii_digit()
            || *byte == b'.'
            || *byte == b','
            || (offset == 0 && (*byte == b'-' || *byte == b'+'))
            || (matches!(byte, b'e' | b'E')
                && absolute > start
                && bytes.get(absolute + 1).is_some_and(|next| {
                    next.is_ascii_digit()
                        || (matches!(next, b'-' | b'+')
                            && bytes.get(absolute + 2).is_some_and(u8::is_ascii_digit))
                }));
        if keep {
            end = absolute + 1;
        } else {
            break;
        }
    }
    if end <= start {
        return None;
    }
    raw.get(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::spec;
    use serde_json::json;

    fn compiled(raw: Value) -> (spec::ValidatedSpec, CompiledSpec) {
        let validated = spec::validate(&raw).unwrap();
        let compiled = spec::compile(&validated.spec).unwrap();
        (validated, compiled)
    }

    fn base() -> Url {
        Url::parse("https://example.com/shop/items/").unwrap()
    }

    fn run(html: &str, raw: Value) -> EvalOutcome {
        let (_, compiled) = compiled(raw);
        evaluate(
            html,
            &base(),
            &compiled,
            &EvalOptions { max_records: 5_000 },
        )
    }

    const CARDS: &str = r#"
    <div class="card"><h2>Lantern</h2><span class="price">€39.90</span>
      <a href="../lantern">details</a><span class="tag">light</span><span class="tag">camp</span></div>
    <div class="card"><h2>Stove</h2><span class="price">€1,299.00</span>
      <a href="/stove">details</a></div>
    "#;

    #[test]
    fn list_fields_are_extracted_in_order() {
        let outcome = run(
            CARDS,
            json!({
                "container": ".card",
                "fields": {
                    "name": { "selector": "h2" },
                    "price": { "selector": ".price", "transform": "number" },
                    "href": { "selector": "a", "transform": "absolute_url" },
                    "tags": { "selector": ".tag", "all": true }
                }
            }),
        );
        assert_eq!(outcome.record_count, 2);
        assert!(
            outcome.field_errors.is_empty(),
            "{:?}",
            outcome.field_errors
        );
        assert_eq!(outcome.records[0]["name"], json!("Lantern"));
        assert_eq!(outcome.records[0]["price"], json!(39.9));
        assert_eq!(
            outcome.records[0]["href"],
            json!("https://example.com/shop/lantern")
        );
        assert_eq!(outcome.records[0]["tags"], json!(["light", "camp"]));
        assert_eq!(outcome.records[1]["price"], json!(1299));
        assert_eq!(outcome.records[1]["tags"], json!([]));
    }

    #[test]
    fn attribute_reads_the_attribute_not_the_text() {
        let outcome = run(
            r#"<div class="card"><a href="/x" title="Link title">Visible text</a></div>"#,
            json!({
                "container": ".card",
                "fields": { "href": { "selector": "a", "attr": "href" } }
            }),
        );
        assert_eq!(outcome.records[0]["href"], json!("/x"));
    }

    #[test]
    fn missing_attribute_is_reported_not_defaulted() {
        let outcome = run(
            r#"<div class="card"><a>no href</a></div>"#,
            json!({
                "container": ".card",
                "fields": { "href": { "selector": "a", "attr": "href" } }
            }),
        );
        assert!(outcome.records[0].get("href").is_none());
        assert_eq!(outcome.field_errors[0].code, FieldErrorCode::AttrMissing);
    }

    #[test]
    fn single_record_without_a_container_uses_the_document_root() {
        let outcome = run(
            r#"<html><body><h1>Title</h1><p class="lead">Lead paragraph.</p></body></html>"#,
            json!({
                "fields": {
                    "title": { "selector": "h1" },
                    "lead": { "selector": ".lead", "transform": "trim" }
                }
            }),
        );
        assert_eq!(outcome.record_count, 1);
        assert_eq!(outcome.records[0]["title"], json!("Title"));
        assert_eq!(outcome.records[0]["lead"], json!("Lead paragraph."));
    }

    #[test]
    fn empty_container_match_is_a_page_level_error_and_no_record() {
        let outcome = run(
            r#"<div class="nothing">x</div>"#,
            json!({ "container": ".card", "fields": { "name": { "selector": "h2" } } }),
        );
        assert_eq!(outcome.record_count, 0);
        assert!(outcome.records.is_empty());
        assert_eq!(outcome.field_errors.len(), 1);
        let error = &outcome.field_errors[0];
        assert_eq!(error.record, None);
        assert_eq!(error.field, None);
        assert_eq!(error.code, FieldErrorCode::SelectorNoMatch);
    }

    #[test]
    fn one_parse_per_page_regardless_of_records_and_fields() {
        let mut html = String::from("<html><body>");
        for index in 0..50 {
            html.push_str(&format!(
                "<div class='card'><h2>n{index}</h2><p class='a'>a</p><p class='b'>b</p></div>"
            ));
        }
        html.push_str("</body></html>");
        let outcome = run(
            &html,
            json!({
                "container": ".card",
                "fields": {
                    "n": { "selector": "h2" },
                    "a": { "selector": ".a" },
                    "b": { "selector": ".b" }
                }
            }),
        );
        assert_eq!(outcome.record_count, 50);
        assert_eq!(outcome.dom_parses, 1);
    }

    #[test]
    fn records_are_capped_and_counted_in_full() {
        let mut html = String::from("<html><body>");
        for index in 0..7 {
            html.push_str(&format!("<div class='card'><h2>{index}</h2></div>"));
        }
        html.push_str("</body></html>");
        let (_, compiled) = compiled(json!({
            "container": ".card",
            "fields": { "n": { "selector": "h2", "transform": "number" } }
        }));
        let outcome = evaluate(&html, &base(), &compiled, &EvalOptions { max_records: 3 });
        assert_eq!(outcome.record_count, 7);
        assert_eq!(outcome.records.len(), 3);
        assert!(outcome.truncated);
    }

    #[test]
    fn nested_selectors_are_scoped_to_the_record_root() {
        let outcome = run(
            r#"<div class="card"><div class="meta"><span class="k">A</span></div></div>
               <div class="card"><div class="meta"><span class="k">B</span></div></div>"#,
            json!({
                "container": ".card",
                "fields": { "key": { "selector": ".meta .k" } }
            }),
        );
        assert_eq!(outcome.records[0]["key"], json!("A"));
        assert_eq!(outcome.records[1]["key"], json!("B"));
    }

    #[test]
    fn malformed_html_still_yields_deterministic_records() {
        let html = "<div class=card><h2>Broken<div class=card><h2>Second";
        let first = run(
            html,
            json!({ "container": ".card", "fields": { "n": { "selector": "h2" } } }),
        );
        let second = run(
            html,
            json!({ "container": ".card", "fields": { "n": { "selector": "h2" } } }),
        );
        assert_eq!(
            serde_json::to_string(&first.records).unwrap(),
            serde_json::to_string(&second.records).unwrap()
        );
        assert_eq!(first.record_count, second.record_count);
    }

    #[test]
    fn text_skips_script_style_template_and_head() {
        let html = r#"<div class="card">
            <p>Visible sentence only.</p>
            <script>var leaked = "SCRIPT_TOKEN_178K";</script>
            <style>.leak { color: red; }</style>
            <template><p>TEMPLATE_TOKEN</p></template>
            <div class="nested">Nested <b>visible</b> text <script>NESTED_SCRIPT</script></div>
        </div>"#;
        let outcome = run(
            html,
            json!({
                "container": ".card",
                "fields": {
                    "body": { "selector": "p", "transform": "text" },
                    "nested": { "selector": ".nested", "transform": "text" },
                    "trimmed": { "selector": ".nested", "transform": "trim" },
                    "script": { "selector": "script", "transform": "text" }
                }
            }),
        );
        assert_eq!(outcome.records[0]["body"], json!("Visible sentence only."));
        assert_eq!(outcome.records[0]["nested"], json!("Nested visible text"));
        assert_eq!(outcome.records[0]["trimmed"], json!("Nested visible text"));
        assert_eq!(outcome.records[0]["script"], json!(""));
        let serialized = serde_json::to_string(&outcome.records).unwrap();
        assert!(!serialized.contains("SCRIPT_TOKEN_178K"));
        assert!(!serialized.contains("NESTED_SCRIPT"));
        assert!(!serialized.contains("color: red"));
        assert!(!serialized.contains("TEMPLATE_TOKEN"));
    }

    #[test]
    fn numbers_cover_common_shapes() {
        for (text, expected) in [
            ("€39.90", 39.9),
            ("€1,299.00", 1299.0),
            ("1.234,56", 1234.56),
            ("1,234.56", 1234.56),
            ("1 234", 1.0),
            ("-12,5", -12.5),
            ("+3.5", 3.5),
            ("1.234.567", 1234567.0),
            ("12", 12.0),
        ] {
            assert_eq!(parse_number(text), Some(expected), "{text}");
        }
        for text in ["", "free", "€", "--", "1e999", "nan"] {
            assert_eq!(parse_number(text), None, "{text}");
        }
        assert_eq!(parse_number("1e3"), Some(1000.0));
        assert_eq!(parse_number_json("€1,299.00"), Some(json!(1299)));
        assert_eq!(parse_number_json("12"), Some(json!(12)));
    }

    #[test]
    fn url_resolution_uses_the_supplied_base() {
        let outcome = run(
            r#"<div class="card"><a href="../a/b">x</a><img src="/img.png"></div>"#,
            json!({
                "container": ".card",
                "fields": {
                    "link": { "selector": "a", "transform": "absolute_url" },
                    "image": { "selector": "img", "transform": "absolute_url" }
                }
            }),
        );
        assert_eq!(
            outcome.records[0]["link"],
            json!("https://example.com/shop/a/b")
        );
        assert_eq!(
            outcome.records[0]["image"],
            json!("https://example.com/img.png")
        );
    }

    #[test]
    fn unjoinable_url_is_reported() {
        let outcome = run(
            r#"<div class="card"><a href="http://[::1">x</a></div>"#,
            json!({
                "container": ".card",
                "fields": { "link": { "selector": "a", "transform": "absolute_url" } }
            }),
        );
        assert_eq!(outcome.field_errors[0].code, FieldErrorCode::UrlJoinFailed);
        assert!(outcome.records[0].get("link").is_none());
    }

    #[test]
    fn text_transform_trims_edges_but_keeps_inner_whitespace() {
        let outcome = run(
            r#"<div class="card"><span class="t">  a
            b  </span></div>"#,
            json!({
                "container": ".card",
                "fields": {
                    "raw": { "selector": ".t" },
                    "trimmed": { "selector": ".t", "transform": "trim" }
                }
            }),
        );
        assert_eq!(outcome.records[0]["raw"], json!("a\n            b"));
        assert_eq!(outcome.records[0]["trimmed"], json!("a b"));
    }

    #[test]
    fn html_transform_keeps_markup() {
        let outcome = run(
            r#"<div class="card"><div class="body"><b>bold</b> text</div></div>"#,
            json!({
                "container": ".card",
                "fields": { "body": { "selector": ".body", "transform": "html" } }
            }),
        );
        assert_eq!(outcome.records[0]["body"], json!("<b>bold</b> text"));
    }

    #[test]
    fn partial_array_failure_omits_the_field_rather_than_hiding_it() {
        let outcome = run(
            r#"<div class="card"><a href="/a">a</a><a>no href</a></div>"#,
            json!({
                "container": ".card",
                "fields": { "links": { "selector": "a", "all": true, "transform": "absolute_url" } }
            }),
        );
        assert!(outcome.records[0].get("links").is_none());
        assert_eq!(outcome.field_errors[0].code, FieldErrorCode::AttrMissing);
        assert_eq!(outcome.field_errors[0].field.as_deref(), Some("links"));
    }

    // -----------------------------------------------------------------------
    //  — `:main` roots at the article body and `*` is the record scope
    // -----------------------------------------------------------------------

    const ARTICLE_DOC: &str = r#"<!doctype html>
<html><head><title>HEAD_DECOY_TITLE</title><style>.x{}</style></head>
<body>
  <div id="readability-page-1" class="page">
    <div><h2>Article heading</h2><p>First article paragraph with prose.</p>
    <p>Second article paragraph with more prose.</p></div>
  </div>
</body></html>"#;

    fn main_document() -> Html {
        Html::parse_document(ARTICLE_DOC)
    }

    fn evaluate_main(raw: Value) -> EvalOutcome {
        let (_, compiled) = compiled(raw);
        evaluate_document(
            &main_document(),
            &base(),
            &compiled,
            &EvalOptions { max_records: 5_000 },
        )
    }

    #[test]
    fn main_scope_roots_at_the_article_body_not_the_wrapper_html() {
        let outcome = evaluate_main(json!({
            "container": ":main",
            "fields": { "text": { "selector": "*" } }
        }));
        assert_eq!(outcome.record_count, 1);
        assert!(
            outcome.field_errors.is_empty(),
            "{:?}",
            outcome.field_errors
        );
        let text = outcome.records[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("First article paragraph"), "{text}");
        assert!(!text.contains("HEAD_DECOY_TITLE"), "head reached: {text}");
        assert!(!text.contains(".x{}"), "style reached: {text}");
    }

    #[test]
    fn star_is_the_record_scope_itself_for_every_transform() {
        let outcome = evaluate_main(json!({
            "container": ":main",
            "fields": {
                "text": { "selector": "*" },
                "trimmed": { "selector": " * ", "transform": "trim" },
                "html": { "selector": "*", "transform": "html" },
                "one": { "selector": "*", "all": true }
            }
        }));
        let record = &outcome.records[0];
        let text = record["text"].as_str().unwrap_or_default();
        assert!(text.contains("First article paragraph"));
        let trimmed = record["trimmed"].as_str().unwrap_or_default();
        assert!(trimmed.contains("First article paragraph with prose."));
        assert!(trimmed.contains("Second article paragraph with more prose."));
        assert!(
            !trimmed.contains('\n'),
            "trim collapses whitespace: {trimmed:?}"
        );
        let html = record["html"].as_str().unwrap_or_default();
        assert!(html.contains("<h2>Article heading</h2>"), "{html}");
        assert!(!html.contains("<body"), "html must be inner HTML: {html}");
        let one = record["one"].as_array().expect("one-element array");
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].as_str().unwrap_or_default(), text);
    }

    #[test]
    fn other_selectors_keep_descendant_css_semantics() {
        let outcome = evaluate_main(json!({
            "container": ":main",
            "fields": {
                "first": { "selector": "p" },
                "all": { "selector": "p", "all": true },
                "nested": { "selector": "* p", "all": true }
            }
        }));
        let record = &outcome.records[0];
        assert_eq!(
            record["first"],
            json!("First article paragraph with prose.")
        );
        assert_eq!(
            record["all"],
            json!([
                "First article paragraph with prose.",
                "Second article paragraph with more prose."
            ])
        );
        assert_eq!(record["nested"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn star_without_a_container_is_the_whole_page_scope() {
        let outcome = run(
            r#"<html><head><title>HEAD_DECOY</title></head><body><p>page body text</p></body></html>"#,
            json!({ "fields": { "text": { "selector": "*" } } }),
        );
        let text = outcome.records[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("page body text"));
        assert!(!text.contains("HEAD_DECOY"));
    }
}
