//! Spec evaluation over the parsed DOM, plus the separation from
//! `mode=main` selection.
//!
//! No network here: these tests drive `extract::spec_eval` directly against fixture
//! bytes, so a failure is an evaluator failure and not a fetch flake.

use std::path::PathBuf;

use serde_json::{Value, json};
use url::Url;
use web_widow::config::Config;
use web_widow::extract::spec::{self, CompiledSpec};
use web_widow::extract::spec_eval::{EvalOptions, FieldErrorCode, evaluate};
use web_widow::extract::{ExtractOptions, Mode, extract_html};
use web_widow::fetch::charset;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn read(name: &str) -> String {
    let path = fixture(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

fn base() -> Url {
    Url::parse("https://example.com/shop/list.html").unwrap()
}

fn prepare(raw: Value) -> CompiledSpec {
    let validated = spec::validate(&raw).expect("spec should validate");
    spec::compile(&validated.spec).expect("spec should compile")
}

fn run(html: &str, raw: Value, max_records: usize) -> web_widow::extract::spec_eval::EvalOutcome {
    let compiled = prepare(raw);
    evaluate(html, &base(), &compiled, &EvalOptions { max_records })
}

// ---------------------------------------------------------------------------
// List and table fixtures with exact expected values
// ---------------------------------------------------------------------------

/// The source-of-truth table, copied from the fixture generator. Written out here
//  independently so a change in the evaluator cannot move both sides together.
const CARDS: &[(&str, f64, &[&str])] = &[
    ("Trail Lantern 01", 19.9, &["light"]),
    ("Trail Lantern 02", 24.0, &["light", "camp"]),
    ("Trail Lantern 03", 29.5, &[]),
    ("Trail Lantern 04", 34.0, &["sale"]),
    ("Trail Lantern 05", 39.0, &["light"]),
    ("Trail Lantern 06", 44.9, &["camp"]),
    ("Trail Lantern 07", 49.0, &["light", "sale"]),
    ("Trail Lantern 08", 54.0, &[]),
    ("Trail Lantern 09", 59.95, &["camp"]),
    ("Trail Lantern 10", 64.0, &["light"]),
    ("Trail Lantern 11", 69.0, &["sale"]),
    ("Trail Lantern 12", 74.5, &[]),
    ("Trail Lantern 13", 79.0, &["light", "camp"]),
    ("Trail Lantern 14", 84.0, &["camp"]),
    ("Trail Lantern 15", 89.9, &["sale"]),
    ("Trail Lantern 16", 94.0, &[]),
    ("Trail Lantern 17", 99.0, &["light"]),
    ("Trail Lantern 18", 104.0, &["camp", "sale"]),
    ("Trail Lantern 19", 109.0, &[]),
    ("Trail Lantern 20", 114.5, &["light"]),
    ("Trail Lantern 21", 1199.0, &["camp"]),
    ("Trail Lantern 22", 1249.0, &["sale"]),
    ("Trail Lantern 23", 1299.5, &[]),
    ("Trail Lantern 24", 1349.0, &["light", "camp", "sale"]),
];

#[test]
fn list_fixture_yields_24_records_with_exact_values() {
    let outcome = run(
        &read("extract/list-24-cards.html"),
        json!({
            "container": ".product-card",
            "fields": {
                "name": { "selector": "h2" },
                "price": { "selector": ".price", "transform": "number" },
                "href": { "selector": "a", "transform": "absolute_url" },
                "tags": { "selector": ".tag", "all": true }
            }
        }),
        5_000,
    );

    assert_eq!(outcome.record_count, 24);
    assert_eq!(outcome.records.len(), 24);
    assert!(
        outcome.field_errors.is_empty(),
        "{:?}",
        outcome.field_errors
    );
    assert!(!outcome.truncated);

    for (index, (name, price, tags)) in CARDS.iter().enumerate() {
        let record = &outcome.records[index];
        assert_eq!(record["name"], json!(name), "record {index}");
        assert_eq!(
            record["price"].as_f64(),
            Some(*price),
            "record {index} price {:?}",
            record["price"]
        );
        assert_eq!(
            record["href"],
            json!(format!(
                "https://example.com/items/trail-lantern-{:02}",
                index + 1
            )),
            "record {index}"
        );
        assert_eq!(
            record["tags"],
            json!(tags.iter().map(|tag| tag.to_string()).collect::<Vec<_>>()),
            "record {index} tags"
        );
    }

    // Full spot checks at both ends and the middle.
    assert_eq!(
        outcome.records[0],
        serde_json::from_value::<serde_json::Map<String, Value>>(json!({
            "name": "Trail Lantern 01",
            "price": 19.9,
            "href": "https://example.com/items/trail-lantern-01",
            "tags": ["light"]
        }))
        .unwrap()
    );
    assert_eq!(
        outcome.records[23],
        serde_json::from_value::<serde_json::Map<String, Value>>(json!({
            "name": "Trail Lantern 24",
            "price": 1349,
            "href": "https://example.com/items/trail-lantern-24",
            "tags": ["light", "camp", "sale"]
        }))
        .unwrap()
    );
    assert_eq!(outcome.records[11]["price"].as_f64(), Some(74.5));
}

#[test]
fn table_fixture_preserves_cell_text_and_row_order() {
    let outcome = run(
        &read("extract/table.html"),
        json!({
            "container": "#specs tbody tr",
            "fields": {
                "model": { "selector": "td:nth-child(1)", "transform": "trim" },
                "output": { "selector": "td:nth-child(2)", "transform": "trim" },
                "price": { "selector": "td:nth-child(3)", "transform": "trim" }
            }
        }),
        5_000,
    );
    assert_eq!(outcome.record_count, 4);
    let rows: Vec<(String, String, String)> = outcome
        .records
        .iter()
        .map(|record| {
            (
                record["model"].as_str().unwrap().to_string(),
                record["output"].as_str().unwrap().to_string(),
                record["price"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            (
                "TL-01".to_string(),
                "300 lm".to_string(),
                "€19.90".to_string()
            ),
            (
                "TL-02".to_string(),
                "450 lm".to_string(),
                "€24.00".to_string()
            ),
            (
                "TL-07".to_string(),
                "750 lm".to_string(),
                "€49.00".to_string()
            ),
            (
                "TL-21".to_string(),
                "1 500 lm".to_string(),
                "€1,199.00".to_string()
            ),
        ]
    );
}

// ---------------------------------------------------------------------------
// Nested selectors, all: true and attribute extraction
// ---------------------------------------------------------------------------

#[test]
fn detail_fixture_nested_and_attribute_fields() {
    let outcome = run(
        &read("extract/detail.html"),
        json!({
            "container": ".product",
            "fields": {
                "sku": { "selector": "h1", "attr": "data-sku" },
                "title": { "selector": "h1" },
                "output": { "selector": ".specs .spec .v" },
                "keys": { "selector": ".specs .spec .k", "all": true, "transform": "trim" },
                "price": { "selector": ".price", "transform": "number" },
                "manufacturer": { "selector": "a.manufacturer", "attr": "href" }
            }
        }),
        5_000,
    );
    assert_eq!(outcome.record_count, 1);
    // `h1` has no `data-sku`; the attribute lives on the container, which field
    // selectors do not include, so the field is reported instead of defaulted.
    assert_eq!(outcome.field_errors[0].field.as_deref(), Some("sku"));
    assert_eq!(outcome.field_errors[0].code, FieldErrorCode::AttrMissing);
    let record = &outcome.records[0];
    assert_eq!(record["title"], json!("Trail Lantern 07"));
    assert_eq!(record["output"], json!("750 lm"));
    assert_eq!(record["keys"], json!(["Output", "Weight", "Battery"]));
    assert_eq!(record["price"], json!(49));
    assert_eq!(
        record["manufacturer"],
        json!("https://example.org/lanterns/tl-07")
    );
}

#[test]
fn container_root_attribute_is_unreachable_from_fields() {
    // Field selectors are descendant-scoped: a field may not read an attribute of
    // the record root itself (here `.product[data-sku]`). The selector finds no
    // descendant, so the field is reported as a selector miss and stays absent
    // rather than being defaulted or silently resolved against the root.
    let outcome = run(
        &read("extract/detail.html"),
        json!({
            "container": ".product",
            "fields": {
                "sku": { "selector": ".product", "attr": "data-sku" },
                "lede": { "selector": ".lede", "transform": "trim" }
            }
        }),
        5_000,
    );
    assert_eq!(outcome.records[0].get("sku"), None);
    assert_eq!(outcome.field_errors.len(), 1);
    assert_eq!(outcome.field_errors[0].field.as_deref(), Some("sku"));
    assert_eq!(
        outcome.field_errors[0].code,
        FieldErrorCode::SelectorNoMatch
    );
    assert_eq!(
        outcome.records[0]["lede"],
        json!("A rechargeable lantern with a warm low setting and a one-week standby.")
    );
}

#[test]
fn attr_with_number_and_trim_reads_the_attribute_not_the_text() {
    // Positive counterpart to the `attr` + `text`/`html` rejection: these
    // combinations are supported and must take the attribute value. The element
    // text differs from the attribute so the two cannot be confused.
    let outcome = run(
        r#"<div class="card">
             <span class="p" data-price="42.50">€1,299.00</span>
             <span class="s" data-slug="  tl-07  ">not-the-slug</span>
           </div>"#,
        json!({
            "container": ".card",
            "fields": {
                "price": { "selector": ".p", "attr": "data-price", "transform": "number" },
                "slug": { "selector": ".s", "attr": "data-slug", "transform": "trim" }
            }
        }),
        5_000,
    );
    assert_eq!(outcome.record_count, 1);
    assert_eq!(outcome.records[0]["price"].as_f64(), Some(42.5));
    assert_eq!(outcome.records[0]["slug"], json!("tl-07"));
}

// ---------------------------------------------------------------------------
// Fail-fast spec and field contracts
// ---------------------------------------------------------------------------

#[test]
fn missing_scalar_selector_is_a_field_error_not_an_empty_string() {
    let outcome = run(
        &read("extract/list-24-cards.html"),
        json!({
            "container": ".product-card",
            "fields": {
                "name": { "selector": "h2" },
                "promo": { "selector": ".promo" }
            }
        }),
        5_000,
    );
    assert_eq!(outcome.records.len(), 24);
    assert_eq!(outcome.field_errors.len(), 24);
    assert!(outcome.field_errors.iter().all(|error| {
        error.field.as_deref() == Some("promo")
            && error.code == FieldErrorCode::SelectorNoMatch
            && error.record.is_some()
    }));
    for record in &outcome.records {
        assert_eq!(record.get("promo"), None, "absent, not null/empty");
    }
}

#[test]
fn invalid_specs_carry_the_json_pointer() {
    let cases: Vec<(Value, &str)> = vec![
        (
            json!({ "fields": { "name": { "selector": "h2", "regex": "^x" } } }),
            "/fields/name",
        ),
        (
            json!({ "fields": { "price": { "selector": ".p", "transform": "eval" } } }),
            "/fields/price/transform",
        ),
        (
            json!({ "fields": { "href": { "selector": "a", "transform": "attr" } } }),
            "/fields/href/attr",
        ),
        (
            json!({ "fields": { "n": { "selector": "h2", "attr": "href", "transform": "text" } } }),
            "/fields/n/attr",
        ),
        (
            json!({ "fields": { "n": { "selector": "a", "attr": "href", "transform": "html" } } }),
            "/fields/n/attr",
        ),
        (
            json!({ "fields": { "x": { "selector": "div[[" } } }),
            "/fields/x/selector",
        ),
        (json!({ "fields": {} }), "/fields"),
    ];
    for (raw, expected_pointer) in cases {
        let error = spec::validate(&raw).expect_err("spec must be rejected");
        let tool = error.to_tool_error();
        assert_eq!(tool.code.as_str(), "schema_invalid", "{raw}");
        let detail = tool.detail.unwrap_or_default();
        assert!(
            detail.contains(expected_pointer),
            "expected pointer {expected_pointer}, got {detail}"
        );
    }
}

// ---------------------------------------------------------------------------
// The 5 000-record cap at the shipped default
// ---------------------------------------------------------------------------

#[test]
fn record_cap_is_reachable_at_the_default_and_counted_in_full() {
    let mut html = String::with_capacity(5001 * 60);
    html.push_str("<html><body>");
    for index in 0..5_001 {
        html.push_str(&format!("<div class='card'><h2>Item {index}</h2></div>"));
    }
    html.push_str("</body></html>");

    let defaults = Config::default();
    assert_eq!(defaults.extract.max_records, 5_000);
    let outcome = run(
        &html,
        json!({ "container": ".card", "fields": { "n": { "selector": "h2" } } }),
        defaults.extract.max_records,
    );
    assert_eq!(outcome.record_count, 5_001, "full count is reported");
    assert_eq!(outcome.records.len(), 5_000, "records are capped");
    assert!(outcome.truncated);
    assert_eq!(outcome.records[0]["n"], json!("Item 0"));
    assert_eq!(outcome.records[4_999]["n"], json!("Item 4999"));
}

// ---------------------------------------------------------------------------
// Corpus robustness and determinism
// ---------------------------------------------------------------------------

#[test]
fn corpus_evaluation_is_panic_free_and_byte_deterministic() {
    let mut fixtures: Vec<PathBuf> = std::fs::read_dir(fixture(""))
        .expect("fixtures dir")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "html")
        })
        .collect();
    fixtures.sort();
    assert!(
        fixtures.len() >= 16,
        "corpus should be present: {fixtures:?}"
    );

    let compiled = prepare(json!({
        "fields": {
            "title": { "selector": "title" },
            "h1": { "selector": "h1", "all": true, "transform": "trim" },
            "first_paragraph": { "selector": "p" },
            "links": { "selector": "a[href]", "all": true, "attr": "href" }
        }
    }));

    for path in fixtures {
        let bytes = std::fs::read(&path).unwrap();
        let decoded = charset::decode(&bytes, None);
        let base = Url::parse("https://example.com/corpus/").unwrap();
        let first = evaluate(
            &decoded.text,
            &base,
            &compiled,
            &EvalOptions { max_records: 5_000 },
        );
        let second = evaluate(
            &decoded.text,
            &base,
            &compiled,
            &EvalOptions { max_records: 5_000 },
        );
        let first_json = serde_json::to_string(&first.records).unwrap();
        let second_json = serde_json::to_string(&second.records).unwrap();
        assert_eq!(
            first_json,
            second_json,
            "{} is not deterministic",
            path.display()
        );
        assert_eq!(first.dom_parses, 1, "{}", path.display());
        assert_eq!(first.record_count, 1, "no container means one record");
    }
}

// ---------------------------------------------------------------------------
// One parse per page, instrumented
// ---------------------------------------------------------------------------

#[test]
fn dom_parse_count_is_one_for_a_wide_spec() {
    let outcome = run(
        &read("extract/list-24-cards.html"),
        json!({
            "container": ".product-card",
            "fields": {
                "a": { "selector": "h2" },
                "b": { "selector": ".price" },
                "c": { "selector": "a", "transform": "absolute_url" },
                "d": { "selector": ".tag", "all": true },
                "e": { "selector": "h2", "transform": "trim" },
                "f": { "selector": "span", "all": true, "transform": "trim" }
            }
        }),
        5_000,
    );
    // 24 records x 6 fields = 144 selector evaluations, still one parse.
    assert_eq!(outcome.record_count, 24);
    assert_eq!(outcome.dom_parses, 1);
}

// ---------------------------------------------------------------------------
// Spec extraction must not depend on main-mode selection
// ---------------------------------------------------------------------------

#[test]
fn spec_extraction_sees_dom_content_that_main_mode_omits() {
    // `noscript-shell.html` has an empty client root; readability selects
    // nothing, so `mode=main` delivers no text. A spec over `noscript p` still
    // sees the DOM, which is the independence guarantee.
    let source = read("noscript-shell.html");
    let main = extract_html(
        &source,
        &base(),
        &ExtractOptions {
            mode: Mode::Main,
            max_chars: 200_000,
            max_links: 2_000,
            include_links: false,
            ignore_selectors: Vec::new(),
        },
    )
    .unwrap();
    assert!(
        !main.markdown.contains("javascript-required-marker"),
        "premise: main mode omits the noscript content: {:?}",
        main.markdown
    );

    let outcome = run(
        &source,
        json!({
            "fields": {
                "notice": { "selector": "noscript", "transform": "trim" },
                "empty_root": { "selector": "#root p" }
            }
        }),
        5_000,
    );
    assert_eq!(outcome.record_count, 1);
    let notice = outcome.records[0]["notice"].as_str().unwrap_or_default();
    // html5ever parses <noscript> content as text when scripting is enabled, so
    // the fallback notice is one text node rather than child elements.
    assert!(
        notice.contains("javascript-required-marker"),
        "spec must see the noscript text: {notice:?}"
    );
    assert_eq!(outcome.records[0].get("empty_root"), None);
}
