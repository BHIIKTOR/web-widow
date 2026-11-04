//! v0.6 — `extract_rows`: CSV/TSV, JSON and HTML-table extraction.
//!
//! Lane-local by rule: the fixture server and every helper live here, and
//! `tests/common/mod.rs` is not edited. The server controls the `Content-Type`
//! header per route (including two lying ones), so auto-detection and the content
//! scope are tested against exact bytes.

mod common;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use axum::routing::any;
use serde_json::json;
use web_widow::ErrorCode;
use web_widow::fts::SearchContentInput;
use web_widow::pipeline::{ExtractInput, ExtractRowsInput, ExtractRowsPayload};

const SIMPLE_CSV: &str = include_str!("fixtures/m06/data/simple.csv");
const QUOTED_CSV: &str = include_str!("fixtures/m06/data/quoted.csv");
const CRLF_BOM_CSV: &[u8] = include_bytes!("fixtures/m06/data/crlf-bom.csv");
const SEMICOLON_CSV: &str = include_str!("fixtures/m06/data/semicolon.csv");
const RAGGED_CSV: &str = include_str!("fixtures/m06/data/ragged.csv");
const UNCLOSED_CSV: &str = include_str!("fixtures/m06/data/unclosed.csv");
const SIMPLE_TSV: &str = include_str!("fixtures/m06/data/simple.tsv");
const ITEMS_JSON: &str = include_str!("fixtures/m06/data/items.json");
const NESTED_JSON: &str = include_str!("fixtures/m06/data/nested.json");
const TABLE_HTML: &str = include_str!("fixtures/m06/data/table.html");
const TABLE_SPANS_HTML: &str = include_str!("fixtures/m06/data/table-spans.html");
const LATIN1_CSV: &[u8] = include_bytes!("fixtures/m06/data/latin1.csv");

#[derive(Clone, Default)]
struct RequestLog {
    entries: Arc<Mutex<Vec<String>>>,
    /// (path, Accept header) in arrival order.
    accepts: Arc<Mutex<Vec<(String, String)>>>,
}

impl RequestLog {
    fn record(&self, entry: String) {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(entry);
    }

    fn record_accept(&self, path: &str, accept: &str) {
        self.accepts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((path.to_string(), accept.to_string()));
    }

    fn entries(&self) -> Vec<String> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn accepts_for(&self, path: &str) -> Vec<String> {
        self.accepts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(entry, _)| entry == path)
            .map(|(_, accept)| accept.clone())
            .collect()
    }
}

struct DataFixture {
    port: u16,
    log: RequestLog,
    _handle: tokio::task::JoinHandle<()>,
}

impl DataFixture {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

fn typed(body: impl Into<Body>, content_type: &str) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .body(body.into())
        .unwrap()
}

async fn handle(State(log): State<RequestLog>, request: Request<Body>) -> Response {
    let path = request.uri().path().to_string();
    log.record(path.clone());
    let accept = request
        .headers()
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    log.record_accept(&path, &accept);
    match path.as_str() {
        "/robots.txt" => typed("User-agent: *\nAllow: /\n", "text/plain"),
        "/data/simple.csv" => typed(SIMPLE_CSV, "text/csv; charset=utf-8"),
        // The header lies: the bytes are CSV, served as `text/plain`.
        "/data/lying.csv" => typed(SIMPLE_CSV, "text/plain; charset=utf-8"),
        // Never sniffed: `application/octet-stream` is refused, not guessed.
        "/data/octet.csv" => typed(SIMPLE_CSV, "application/octet-stream"),
        "/data/quoted.csv" => typed(QUOTED_CSV, "text/csv; charset=utf-8"),
        "/data/crlf-bom.csv" => typed(CRLF_BOM_CSV.to_vec(), "text/csv"),
        "/data/semicolon.csv" => typed(SEMICOLON_CSV, "text/csv"),
        "/data/ragged.csv" => typed(RAGGED_CSV, "text/csv"),
        "/data/unclosed.csv" => typed(UNCLOSED_CSV, "text/csv"),
        "/data/simple.tsv" => typed(SIMPLE_TSV, "text/tab-separated-values"),
        "/data/items.json" => typed(ITEMS_JSON, "application/json"),
        "/data/nested.json" => typed(NESTED_JSON, "application/json"),
        "/data/table.html" => typed(TABLE_HTML, "text/html; charset=utf-8"),
        "/data/table-spans.html" => typed(TABLE_SPANS_HTML, "text/html; charset=utf-8"),
        "/data/plain.html" => typed(
            "<html><body><p>no table here</p></body></html>",
            "text/html; charset=utf-8",
        ),
        "/data/latin1.csv" => typed(LATIN1_CSV.to_vec(), "text/csv; charset=iso-8859-1"),
        // Content negotiation: the table only appears when the request named CSV.
        "/data/negotiate" => {
            if accept.contains("text/csv") {
                typed(SIMPLE_CSV, "text/csv; charset=utf-8")
            } else {
                typed(
                    "<html><body><p>HTML variant with no table</p></body></html>",
                    "text/html; charset=utf-8",
                )
            }
        }
        // Dynamic: 250 rows for the limit/truncation checks.
        "/data/many.csv" => {
            let mut body = String::from("name,price\n");
            for index in 1..=250 {
                body.push_str(&format!("Item {index},{index}.00\n"));
            }
            typed(body, "text/csv; charset=utf-8")
        }
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("not found"))
            .unwrap(),
    }
}

async fn start_fixture() -> DataFixture {
    let log = RequestLog::default();
    let app = Router::new().fallback(any(handle)).with_state(log.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    DataFixture {
        port,
        log,
        _handle: handle,
    }
}

async fn env() -> (DataFixture, common::TestEnv) {
    let fixture = start_fixture().await;
    let environment = common::TestEnv::new(common::test_config()).await;
    (fixture, environment)
}

/// All fields default, so each test states only what it exercises.
fn input(url: String) -> ExtractRowsInput {
    ExtractRowsInput {
        url,
        format: None,
        selector: None,
        delimiter: None,
        header_row: None,
        json_pointer: None,
        limit: None,
        max_field_chars: None,
        render: None,
        use_cache: None,
    }
}

fn extract_input(url: String, schema: serde_json::Value) -> ExtractInput {
    ExtractInput {
        url,
        schema,
        render: None,
        use_cache: None,
        max_field_chars: None,
        max_chars: None,
    }
}

fn error_code<T: std::fmt::Debug>(result: Result<T, web_widow::WidowError>) -> ErrorCode {
    result.expect_err("expected a typed failure").code()
}

async fn rows(env: &common::TestEnv, mut request: ExtractRowsInput) -> ExtractRowsPayload {
    request.render = Some(web_widow::pipeline::RenderPolicy::Never);
    env.pipeline
        .extract_rows(request)
        .await
        .unwrap_or_else(|error| panic!("extract_rows failed: {error}"))
}

// ---------------------------------------------------------------------------
// the deliberate content scope, and the tabular sibling that widens it
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extract_refuses_csv_and_extract_rows_returns_typed_records() {
    let (fixture, env) = env().await;
    let url = fixture.url("/data/simple.csv");

    // Red on the pre-change surface: `extract`/`fetch_page` keep the page content
    // scope and refuse `text/csv` before the body is used.
    let css = json!({"container": "table", "fields": {"name": {"selector": "td"}}});
    assert_eq!(
        error_code(
            env.pipeline
                .extract(extract_input(url.clone(), css.clone()))
                .await
        ),
        ErrorCode::UnsupportedContentType
    );
    assert_eq!(
        error_code(env.fetch(&url).await),
        ErrorCode::UnsupportedContentType
    );

    let payload = rows(&env, input(url)).await;
    assert_eq!(payload.format, "csv");
    assert_eq!(payload.content_type, "text/csv");
    assert_eq!(payload.status, 200);
    assert_eq!(payload.row_count, 3);
    assert_eq!(payload.returned, 3);
    assert!(!payload.truncated);
    assert_eq!(
        payload.records[0],
        json!({"name": "Widget", "price": "10.00", "qty": "3"})
            .as_object()
            .cloned()
            .unwrap()
    );
    assert_eq!(payload.records[2]["name"], json!("Doohickey"));
}

#[tokio::test]
async fn the_tabular_fetch_uses_the_shared_robots_and_gate_path() {
    let (fixture, env) = env().await;
    let payload = rows(&env, input(fixture.url("/data/simple.csv"))).await;
    assert_eq!(payload.row_count, 3);

    // One robots.txt request and one page request: the tabular acquisition is the
    // same SSRF → robots → host-gate → caps path, not a private fetch.
    let entries = fixture.log.entries();
    assert_eq!(
        entries
            .iter()
            .filter(|entry| *entry == "/data/simple.csv")
            .count(),
        1,
        "{entries:?}"
    );
    assert!(
        entries.iter().any(|entry| entry == "/robots.txt"),
        "{entries:?}"
    );
}

#[tokio::test]
async fn the_tabular_fetch_names_csv_and_tsv_before_the_wildcard() {
    let (fixture, env) = env().await;
    let payload = rows(&env, input(fixture.url("/data/negotiate"))).await;
    assert_eq!(payload.format, "csv");
    assert_eq!(payload.row_count, 3);

    let accepts = fixture.log.accepts_for("/data/negotiate");
    let accept = accepts.first().expect("one tabular request");
    let csv = accept.find("text/csv").expect("csv named");
    let tsv = accept.find("text/tab-separated-values").expect("tsv named");
    let wildcard = accept.find("*/*").expect("wildcard present");
    assert!(csv < wildcard, "{accept}");
    assert!(tsv < wildcard, "{accept}");

    // The page tools keep the HTML-first header on the same URL.
    let page = env.fetch(&fixture.url("/data/negotiate")).await;
    assert!(page.is_ok(), "the HTML variant still fetches: {page:?}");
    let accepts = fixture.log.accepts_for("/data/negotiate");
    let page_accept = accepts.last().expect("page request");
    assert!(page_accept.starts_with("text/html"), "{page_accept}");
}

// ---------------------------------------------------------------------------
// the CSV edge matrix
// ---------------------------------------------------------------------------

#[tokio::test]
async fn csv_edge_matrix_quotes_newlines_crlf_bom_semicolons_and_ragged_rows() {
    let (fixture, env) = env().await;

    // Quoted delimiters, escaped quotes and an embedded newline stay one field.
    let quoted = rows(&env, input(fixture.url("/data/quoted.csv"))).await;
    assert_eq!(quoted.records[0]["name"], json!("Widget, large"));
    assert_eq!(
        quoted.records[0]["note"],
        json!("contains \"quotes\" and\na newline")
    );

    // BOM stripped, CRLF rows, no phantom trailing record.
    let crlf = rows(&env, input(fixture.url("/data/crlf-bom.csv"))).await;
    assert_eq!(crlf.row_count, 2);
    assert_eq!(crlf.records[0]["id"], json!("1"));
    assert_eq!(crlf.records[1]["name"], json!("Beta"));

    // Semicolon sniffing and a TSV response.
    let semicolon = rows(&env, input(fixture.url("/data/semicolon.csv"))).await;
    assert_eq!(semicolon.records[1]["city"], json!("Paris"));
    let tsv = rows(&env, input(fixture.url("/data/simple.tsv"))).await;
    assert_eq!(tsv.format, "tsv");
    assert_eq!(tsv.records[1]["price"], json!("20.50"));

    // Ragged rows: null + a bounded error per side.
    let ragged = rows(&env, input(fixture.url("/data/ragged.csv"))).await;
    assert_eq!(ragged.records[1]["qty"], json!(null));
    assert_eq!(ragged.records[2]["column_4"], json!("extra"));
    assert_eq!(
        ragged
            .field_errors
            .iter()
            .map(|error| (error.row, error.reason.as_str()))
            .collect::<Vec<_>>(),
        vec![(2, "ragged_short_row"), (3, "ragged_long_row")]
    );

    // An unclosed quote is a typed extraction failure naming its line.
    let error = env
        .pipeline
        .extract_rows(input(fixture.url("/data/unclosed.csv")))
        .await
        .expect_err("unclosed quote");
    assert_eq!(error.code(), ErrorCode::ExtractionFailed);
    assert!(error.to_string().contains("line 2"), "{error}");
}

#[tokio::test]
async fn an_explicit_delimiter_and_header_row_false_are_honoured() {
    let (fixture, env) = env().await;
    let mut request = input(fixture.url("/data/simple.csv"));
    request.delimiter = Some(";".to_string());
    let payload = rows(&env, request).await;
    // One column named after the whole header line, then the rows.
    assert_eq!(payload.records[0].keys().collect::<Vec<_>>().len(), 1);

    // A tab delimiter is data, not padding: it is not trimmed away into a sniff.
    let mut request = input(fixture.url("/data/simple.csv"));
    request.delimiter = Some("\t".to_string());
    let payload = rows(&env, request).await;
    assert_eq!(
        payload.records[0].keys().collect::<Vec<_>>(),
        vec!["name,price,qty"],
        "the explicit tab delimiter must be honoured"
    );
    assert_eq!(
        payload.records[0]["name,price,qty"],
        json!("Widget,10.00,3")
    );

    let mut request = input(fixture.url("/data/simple.csv"));
    request.header_row = Some(false);
    let payload = rows(&env, request).await;
    assert_eq!(payload.row_count, 4);
    assert_eq!(payload.records[0]["column_1"], json!("name"));
}

// ---------------------------------------------------------------------------
// JSON shapes and pointers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn json_pointers_shapes_and_types() {
    let (fixture, env) = env().await;

    let mut request = input(fixture.url("/data/items.json"));
    request.json_pointer = Some("/items".to_string());
    let items = rows(&env, request).await;
    assert_eq!(items.format, "json");
    assert_eq!(items.row_count, 2);
    // Numbers keep their JSON type instead of being stringified.
    assert_eq!(items.records[0]["price"], json!(10.0));
    assert!(items.records[0]["price"].is_number());

    // A nested pointer keeps nested values and nulls intact.
    let mut request = input(fixture.url("/data/nested.json"));
    request.json_pointer = Some("/rows".to_string());
    let nested = rows(&env, request).await;
    assert_eq!(nested.records[0]["tags"], json!(["a", "b"]));
    assert_eq!(nested.records[0]["details"], json!({"ok": true}));
    assert_eq!(nested.records[1]["details"], json!(null));

    // An array of scalars becomes `{"value": …}` records.
    let mut request = input(fixture.url("/data/nested.json"));
    request.json_pointer = Some("/labels".to_string());
    let labels = rows(&env, request).await;
    assert_eq!(labels.records[0]["value"], json!("alpha"));

    // The document root is one record when it is an object.
    let root = rows(&env, input(fixture.url("/data/nested.json"))).await;
    assert_eq!(root.row_count, 1);
    assert!(root.records[0]["rows"].is_array());

    // A syntactically valid pointer with no value is typed at /json_pointer.
    let mut request = input(fixture.url("/data/items.json"));
    request.json_pointer = Some("/missing".to_string());
    let error = env
        .pipeline
        .extract_rows(request)
        .await
        .expect_err("missing pointer");
    assert_eq!(error.code(), ErrorCode::SchemaInvalid);
    let tool = error.to_tool_error();
    assert_eq!(tool.detail.as_deref(), Some("pointer=/json_pointer"));
}

// ---------------------------------------------------------------------------
// HTML tables
// ---------------------------------------------------------------------------

#[tokio::test]
async fn html_tables_use_thead_first_row_and_report_spans_and_misses() {
    let (fixture, env) = env().await;

    let table = rows(&env, input(fixture.url("/data/table.html"))).await;
    assert_eq!(table.format, "html_table");
    assert_eq!(table.row_count, 2);
    // The first `<table>` uses its `<thead>` headers.
    assert_eq!(
        table.records[0],
        json!({"Name": "Widget", "Price": "10.00"})
            .as_object()
            .cloned()
            .unwrap()
    );

    // A selector picks the second table, whose first row is the header.
    let mut request = input(fixture.url("/data/table.html"));
    request.selector = Some("table.first-row".to_string());
    let second = rows(&env, request).await;
    assert_eq!(second.records[0]["Code"], json!("A"));

    // A selector that matches nothing is a note with zero records, not an error.
    let mut request = input(fixture.url("/data/plain.html"));
    request.selector = Some("#absent".to_string());
    let miss = rows(&env, request).await;
    assert_eq!(miss.row_count, 0);
    assert_eq!(miss.field_errors[0].reason, "table_not_found");

    // Spans are not expanded; the table says so once.
    let spans = rows(&env, input(fixture.url("/data/table-spans.html"))).await;
    assert_eq!(spans.records[0]["Range"], json!("1"));
    assert_eq!(
        spans
            .field_errors
            .iter()
            .filter(|error| error.reason == "span_ignored")
            .count(),
        1
    );
}

// ---------------------------------------------------------------------------
// auto-detection and charset
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auto_detection_sniffs_a_lying_header_and_an_explicit_format_wins() {
    let (fixture, env) = env().await;

    // `text/plain` names no format: the first 4 KiB are sniffed as CSV.
    let lying = rows(&env, input(fixture.url("/data/lying.csv"))).await;
    assert_eq!(lying.format, "csv");
    assert_eq!(lying.content_type, "text/plain");
    assert_eq!(lying.records[0]["name"], json!("Widget"));

    // An explicit format is never overridden by the response or the sniff.
    let mut request = input(fixture.url("/data/lying.csv"));
    request.format = Some("html_table".to_string());
    let explicit = rows(&env, request).await;
    assert_eq!(explicit.format, "html_table");
    assert_eq!(explicit.row_count, 0);
    assert_eq!(explicit.field_errors[0].reason, "table_not_found");

    // Octet-stream is refused, never guessed from the bytes.
    let error = env
        .pipeline
        .extract_rows(input(fixture.url("/data/octet.csv")))
        .await
        .expect_err("octet-stream");
    assert_eq!(error.code(), ErrorCode::UnsupportedContentType);

    // The HTTP charset parameter drives decoding: iso-8859-1 bytes become UTF-8.
    let latin1 = rows(&env, input(fixture.url("/data/latin1.csv"))).await;
    assert_eq!(latin1.records[0]["name"], json!("Café"));
    assert_eq!(latin1.records[0]["city"], json!("Zürich"));
}

// ---------------------------------------------------------------------------
// bounds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn limit_row_count_and_field_caps_are_reported() {
    let (fixture, env) = env().await;

    let mut request = input(fixture.url("/data/many.csv"));
    request.limit = Some(3);
    let limited = rows(&env, request).await;
    assert_eq!(limited.returned, 3);
    // Parsed up to `limit + 1` to prove more rows existed.
    assert_eq!(limited.row_count, 4);
    assert!(limited.truncated);
    assert_eq!(limited.limit_hit.as_deref(), Some("limit"));

    let mut request = input(fixture.url("/data/simple.csv"));
    request.max_field_chars = Some(4);
    let capped = rows(&env, request).await;
    assert_eq!(capped.records[0]["name"], json!("Widg"));
    assert_eq!(capped.records[0]["price"], json!("10.0"));
    assert!(capped.truncated_fields >= 5);
    assert!(capped.truncated);
    assert_eq!(capped.limit_hit.as_deref(), Some("max_field_chars"));

    for (limit, max_field_chars) in [(Some(0), None), (Some(1001), None), (None, Some(0))] {
        let mut request = input(fixture.url("/data/simple.csv"));
        request.limit = limit;
        request.max_field_chars = max_field_chars;
        let error = env
            .pipeline
            .extract_rows(request)
            .await
            .expect_err("out of range");
        assert_eq!(error.code(), ErrorCode::SchemaInvalid);
    }
}

#[tokio::test]
async fn a_cached_row_set_is_capped_to_the_call_and_refetched_when_it_is_short() {
    let (fixture, env) = env().await;
    let url = fixture.url("/data/many.csv");

    let mut request = input(url.clone());
    request.limit = Some(2);
    let first = rows(&env, request).await;
    assert_eq!(first.returned, 2);
    assert!(first.truncated);
    assert!(!first.cached);

    // Same limit: the stored row set answers it without a fetch.
    let mut request = input(url.clone());
    request.limit = Some(2);
    request.use_cache = Some(true);
    let cached = rows(&env, request).await;
    assert!(cached.cached, "second call should reuse the stored row set");
    assert_eq!(cached.returned, 2);
    assert_eq!(cached.limit_hit.as_deref(), Some("limit"));

    // Larger limit: the cached table stopped short, so it is refetched rather
    // than served as if it were the whole page.
    let mut request = input(url);
    request.limit = Some(5);
    request.use_cache = Some(true);
    let widened = rows(&env, request).await;
    assert!(
        !widened.cached,
        "a short cache must not answer a larger limit"
    );
    assert_eq!(widened.returned, 5);
    assert!(widened.truncated);
}

// ---------------------------------------------------------------------------
// store and FTS (fill-empty)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn one_extraction_row_a_profile_row_and_row_text_findable_locally() {
    let (fixture, env) = env().await;
    let url = fixture.url("/data/table.html");
    let mut request = input(url.clone());
    request.format = Some("html_table".to_string());
    request.selector = Some("table.results".to_string());
    let payload = rows(&env, request).await;
    assert_eq!(payload.row_count, 2);

    let db = env.db();
    let extractions: i64 = db
        .query_row("SELECT COUNT(*) FROM extractions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(extractions, 1);
    let (spec_json, record_count): (String, i64) = db
        .query_row(
            "SELECT p.spec_json, e.record_count FROM profiles p
             JOIN extractions e ON e.profile_hash = p.hash",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        spec_json,
        r#"{"format":"html_table","selector":"table.results","header_row":true}"#
    );
    assert_eq!(record_count, payload.returned as i64);

    let indexed: String = db
        .query_row(
            "SELECT body FROM pages_fts WHERE rowid = (SELECT id FROM pages LIMIT 1)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(indexed.contains("Widget"), "{indexed}");

    let hits = env
        .content
        .search(SearchContentInput {
            query: "Gadget Widget".to_string(),
            limit: None,
        })
        .await
        .unwrap();
    // Every term must appear: the row text carries both names.
    assert_eq!(hits.results.len(), 1, "{hits:?}");
    assert_eq!(hits.results[0].url, url);
}

#[tokio::test]
async fn a_later_extraction_never_replaces_the_page_body_it_did_not_deliver() {
    let (fixture, env) = env().await;
    let url = fixture.url("/data/table.html");

    // `fetch_page` delivers and indexes the page body.
    let page = env.fetch(&url).await.expect("page fetch");
    assert_eq!(page.payload.status, 200);
    let db = env.db();
    let before: String = db
        .query_row("SELECT body FROM pages_fts LIMIT 1", [], |row| row.get(0))
        .unwrap();
    assert!(before.contains("Fixture table page"), "{before}");

    // A tabular projection that follows may only fill an empty row.
    let payload = rows(&env, input(url)).await;
    assert_eq!(payload.row_count, 2);
    let after: String = env
        .db()
        .query_row("SELECT body FROM pages_fts LIMIT 1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(after, before, "fill-empty replaced the delivered page body");
}

// ---------------------------------------------------------------------------
// no regression on the neighbouring tools
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extract_and_fetch_page_keep_their_payloads_and_content_scope() {
    let (fixture, env) = env().await;
    let table = fixture.url("/data/table.html");

    // `extract` still evaluates CSS-over-HTML and returns its own envelope.
    let spec = json!({
        "container": "table.results tbody tr",
        "fields": {
            "name": {"selector": "td"},
            "price": {"selector": "td:nth-of-type(2)"}
        }
    });
    let extracted = env
        .pipeline
        .extract(extract_input(table.clone(), spec))
        .await
        .unwrap();
    assert_eq!(extracted.records.len(), 2);
    assert_eq!(extracted.records[0]["name"], json!("Widget"));
    assert_eq!(extracted.content_type, "text/html");

    // `fetch_page` still delivers Markdown with its unchanged envelope.
    let page = env.fetch(&table).await.expect("fetch_page must still work");
    assert_eq!(page.payload.status, 200);
    assert_eq!(page.payload.content_type, "text/html");
    assert!(
        page.markdown.contains("Fixture table page"),
        "{}",
        page.markdown
    );
    assert!(page.payload.links.is_empty());
    assert!(!page.payload.cached);
}
