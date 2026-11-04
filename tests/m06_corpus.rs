//! v0.6 — `corpus_export` (JSONL + markdown) and `snapshot_compare`
//! (monitor snapshots and page fetch versions).
//!
//! Lane-local fixture server and helpers; `tests/common/mod.rs` is untouched.
//! The live store is never opened here: the live check is a copy drill run
//! against a copied data directory.

mod common;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use axum::routing::any;
use serde_json::{Value, json};
use web_widow::ErrorCode;
use web_widow::compare::SnapshotCompareInput;
use web_widow::export::CorpusExportInput;
use web_widow::monitor::{MonitorCheckInput, MonitorCreateInput};
use web_widow::pipeline::{ExtractInput, RenderPolicy};
use web_widow::recipes::RecipeSaveInput;

#[derive(Clone)]
struct Page {
    body: String,
    etag: String,
}

#[derive(Clone, Default)]
struct FixtureState {
    pages: Arc<Mutex<HashMap<String, Page>>>,
    counter: Arc<Mutex<usize>>,
}

impl FixtureState {
    fn set(&self, path: &str, body: &str) {
        let mut counter = self.counter.lock().unwrap();
        *counter += 1;
        let etag = format!("\"v{}\"", *counter);
        self.pages.lock().unwrap().insert(
            path.to_string(),
            Page {
                body: body.to_string(),
                etag,
            },
        );
    }

    fn get(&self, path: &str) -> Option<Page> {
        self.pages.lock().unwrap().get(path).cloned()
    }
}

struct Fixture {
    port: u16,
    state: FixtureState,
    _handle: tokio::task::JoinHandle<()>,
}

impl Fixture {
    fn url(&self, host: &str, path: &str) -> String {
        format!("http://{host}:{}{path}", self.port)
    }
}

fn article(title: &str, word: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head>\
         <body><main><article><h1>{title}</h1>\
         <p>The {word} page mentions the lighthouse keeper and forty gulls.</p>\
         <p>Second paragraph for {word}.</p></article></main></body></html>"
    )
}

const ITEMS_HTML: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Items</title></head>
<body>
  <div class="item"><h2>First item</h2><span class="price">10.00</span></div>
  <div class="item"><h2>Second item</h2><span class="price">20.50</span></div>
</body></html>"#;

fn long_html() -> String {
    let filler = "lorem ipsum dolor sit amet ".repeat(10_000); // ~270k characters
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Long</title></head>\
         <body><main><article><h1>Long</h1><div id=\"blob\">{filler}</div>\
         </article></main></body></html>"
    )
}

fn schema_html(rows: usize, changed: bool) -> String {
    let mut body = String::from(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Schema</title></head><body>",
    );
    for index in 0..rows {
        let price = if changed && index == 0 {
            "99.00"
        } else {
            "10.00"
        };
        body.push_str(&format!(
            "<div class=\"item\"><h2>Item {index}</h2><span class=\"price\">{price}</span></div>"
        ));
    }
    body.push_str("</body></html>");
    body
}

async fn handle(State(state): State<FixtureState>, request: Request<Body>) -> Response {
    let path = request.uri().path().to_string();
    let conditional = request
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    if path == "/robots.txt" {
        return typed("User-agent: *\nAllow: /\n", "text/plain");
    }
    let stored = state.get(&path);
    if let Some(page) = stored {
        if conditional.as_deref() == Some(page.etag.as_str()) {
            return Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, page.etag)
                .body(Body::empty())
                .unwrap();
        }
        return Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::ETAG, page.etag)
            .body(Body::from(page.body))
            .unwrap();
    }
    match path.as_str() {
        "/a" => typed(article("Page A", "alpha"), "text/html; charset=utf-8"),
        "/b" => typed(article("Page B", "beta"), "text/html; charset=utf-8"),
        "/items" => typed(ITEMS_HTML, "text/html; charset=utf-8"),
        "/long" => typed(long_html(), "text/html; charset=utf-8"),
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("not found"))
            .unwrap(),
    }
}

fn typed(body: impl Into<Body>, content_type: &str) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .body(body.into())
        .unwrap()
}

async fn start_fixture() -> Fixture {
    let state = FixtureState::default();
    let app = Router::new()
        .fallback(any(handle))
        .with_state(state.clone());
    // 0.0.0.0 so 127.0.0.1 and 127.0.0.2 both reach it (the site filter).
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    Fixture {
        port,
        state,
        _handle: handle,
    }
}

async fn env() -> common::TestEnv {
    common::TestEnv::new(common::test_config()).await
}

fn export_input(format: &str, name: &str) -> CorpusExportInput {
    CorpusExportInput {
        format: format.to_string(),
        name: name.to_string(),
        site: None,
        since: None,
        limit: None,
        include_text: None,
        include_extraction: None,
        max_bytes: None,
    }
}

fn extract_input(url: &str, schema: Value) -> ExtractInput {
    ExtractInput {
        url: url.to_string(),
        schema,
        render: Some(RenderPolicy::Never),
        use_cache: None,
        max_field_chars: None,
        max_chars: None,
    }
}

fn items_spec() -> Value {
    json!({
        "container": ".item",
        "fields": {
            "name": {"selector": "h2"},
            "price": {"selector": ".price", "transform": "number"}
        }
    })
}

fn read_jsonl(path: &str) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("export file")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("one JSON object per line"))
        .collect()
}

fn snapshot_ids(env: &common::TestEnv) -> Vec<i64> {
    let db = env.db();
    let mut statement = db.prepare("SELECT id FROM snapshots ORDER BY id").unwrap();
    statement
        .query_map([], |row| row.get::<_, i64>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn fetch_event_ids(env: &common::TestEnv, url: &str) -> Vec<i64> {
    let db = env.db();
    let mut statement = db
        .prepare(
            "SELECT e.id FROM fetch_events e JOIN pages p ON p.id = e.page_id
             WHERE p.url = ?1 ORDER BY e.id",
        )
        .unwrap();
    statement
        .query_map([url], |row| row.get::<_, i64>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

async fn check_monitor(env: &common::TestEnv, monitor_id: i64) {
    env.monitors
        .check(MonitorCheckInput {
            monitor_id: Some(monitor_id),
            all: None,
            force: Some(true),
            wait_ms: None,
        })
        .await
        .expect("monitor check");
}

fn compare_input(kind: &str, from: i64, to: i64) -> SnapshotCompareInput {
    SnapshotCompareInput {
        kind: kind.to_string(),
        from,
        to,
        include_diff: None,
        max_chars: None,
    }
}

async fn fetch_page(env: &common::TestEnv, url: &str) {
    env.fetch(url).await.expect("page fetch");
}

// ---------------------------------------------------------------------------
// JSONL shape, required keys, and the corpus boundary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn jsonl_export_has_one_valid_object_per_line_with_the_documented_keys() {
    let fixture = start_fixture().await;
    let env = env().await;
    fetch_page(&env, &fixture.url("127.0.0.1", "/a")).await;
    fetch_page(&env, &fixture.url("127.0.0.1", "/b")).await;
    env.pipeline
        .extract(extract_input(
            &fixture.url("127.0.0.1", "/items"),
            items_spec(),
        ))
        .await
        .expect("extract");

    // A recipe and a monitor are configuration/history, not corpus rows.
    env.pipeline
        .recipe_save(RecipeSaveInput {
            name: "fixture:items".to_string(),
            schema: items_spec(),
            description: None,
        })
        .await
        .unwrap();
    env.monitors
        .create(MonitorCreateInput {
            url: fixture.url("127.0.0.1", "/a"),
            mode: None,
            schema: None,
            interval_s: None,
            ignore_selectors: Vec::new(),
            label: None,
        })
        .await
        .unwrap();

    let payload = env
        .pipeline
        .corpus_export(export_input("jsonl", "corpus"))
        .await
        .expect("export");
    assert_eq!(payload.records, 3, "three pages, no monitor/recipe rows");
    assert_eq!(payload.skipped_no_text, 0);
    assert!(!payload.truncated);
    assert_eq!(payload.format, "jsonl");
    assert!(payload.bytes > 0);
    assert!(
        Path::new(&payload.path).starts_with(env.data_dir().join("exports")),
        "{}",
        payload.path
    );

    let lines = read_jsonl(&payload.path);
    assert_eq!(lines.len(), 3);
    for line in &lines {
        for key in [
            "url",
            "canonical_url",
            "title",
            "lang",
            "site",
            "first_seen",
            "last_fetched",
            "status",
            "content_type",
            "render_mode",
            "text_hash",
            "text",
            "text_truncated",
            "extraction",
            "extraction_truncated",
        ] {
            assert!(line.get(key).is_some(), "missing {key}: {line}");
        }
        assert_eq!(line["text_truncated"], json!(false));
    }
    let items = lines
        .iter()
        .find(|line| line["url"].as_str().unwrap_or_default().ends_with("/items"))
        .expect("items row");
    assert!(
        items["extraction"].is_object(),
        "the extraction is embedded"
    );
    assert_eq!(items["extraction"]["record_count"], json!(2));
    let markdown = lines
        .iter()
        .find(|line| line["url"].as_str().unwrap_or_default().ends_with("/a"))
        .expect("page a row");
    assert!(
        markdown["text"]
            .as_str()
            .unwrap_or_default()
            .contains("lighthouse keeper"),
        "{markdown}"
    );
    assert!(markdown["extraction"].is_object());
}

#[tokio::test]
async fn site_since_and_limit_filters_select_the_rows() {
    let fixture = start_fixture().await;
    let env = env().await;
    fetch_page(&env, &fixture.url("127.0.0.1", "/a")).await;
    fetch_page(&env, &fixture.url("127.0.0.2", "/b")).await;

    let mut input = export_input("jsonl", "site");
    input.site = Some("127.0.0.2".to_string());
    let by_site = env.pipeline.corpus_export(input).await.unwrap();
    assert_eq!(by_site.records, 1);
    assert_eq!(read_jsonl(&by_site.path)[0]["site"], json!("127.0.0.2"));

    let mut input = export_input("jsonl", "since");
    input.since = Some("2999-01-01T00:00:00Z".to_string());
    let future = env.pipeline.corpus_export(input).await.unwrap();
    assert_eq!(future.records, 0);
    assert_eq!(read_jsonl(&future.path).len(), 0);

    let mut input = export_input("jsonl", "limit");
    input.limit = Some(1);
    let limited = env.pipeline.corpus_export(input).await.unwrap();
    assert_eq!(limited.records, 1);
    assert!(limited.truncated);
    assert_eq!(limited.limit_hit.as_deref(), Some("limit"));
    assert_eq!(read_jsonl(&limited.path).len(), 1);

    // An invalid `since` and an invalid limit are typed at their pointer.
    let mut input = export_input("jsonl", "bad");
    input.since = Some("yesterday".to_string());
    let error = env.pipeline.corpus_export(input).await.unwrap_err();
    assert_eq!(
        error.to_tool_error().detail.as_deref(),
        Some("pointer=/since")
    );
    let mut input = export_input("jsonl", "bad");
    input.limit = Some(10_001);
    let error = env.pipeline.corpus_export(input).await.unwrap_err();
    assert_eq!(
        error.to_tool_error().detail.as_deref(),
        Some("pointer=/limit")
    );
}

// ---------------------------------------------------------------------------
// markdown export
// ---------------------------------------------------------------------------

#[tokio::test]
async fn markdown_export_has_index_and_one_file_per_page_matching_the_jsonl_text() {
    let fixture = start_fixture().await;
    let env = env().await;
    fetch_page(&env, &fixture.url("127.0.0.1", "/a")).await;
    fetch_page(&env, &fixture.url("127.0.0.1", "/b")).await;

    let jsonl = env
        .pipeline
        .corpus_export(export_input("jsonl", "both"))
        .await
        .unwrap();
    let lines = read_jsonl(&jsonl.path);

    let markdown = env
        .pipeline
        .corpus_export(export_input("markdown", "both"))
        .await
        .unwrap();
    assert_eq!(markdown.format, "markdown");
    assert_eq!(markdown.records, 2);
    let root = PathBuf::from(&markdown.path);
    assert!(root.is_dir(), "{}", markdown.path);
    let index = std::fs::read_to_string(root.join("index.md")).expect("index.md");
    assert!(index.contains("generated_at:"), "{index}");
    assert!(index.contains("records: 2"), "{index}");
    assert!(index.contains("site:"), "{index}");

    let mut names: Vec<String> = std::fs::read_dir(root.join("pages"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    assert_eq!(names.len(), 2, "{names:?}");

    for line in &lines {
        let url = line["url"].as_str().unwrap();
        let url_hash = db_url_hash(&env, url);
        let body = std::fs::read_to_string(root.join("pages").join(format!("{url_hash}.md")))
            .unwrap_or_else(|error| panic!("{url_hash}.md: {error}"));
        assert!(body.starts_with("---\nurl: "), "{body}");
        assert!(body.contains(&format!("title: {}", line["title"].as_str().unwrap_or(""))));
        assert!(body.contains("fetched_at: "));
        assert!(
            body.trim_end().ends_with(line["text"].as_str().unwrap()),
            "the markdown body equals the JSONL text"
        );
    }
}

fn db_url_hash(env: &common::TestEnv, url: &str) -> String {
    env.db()
        .query_row("SELECT url_hash FROM pages WHERE url = ?1", [url], |row| {
            row.get(0)
        })
        .unwrap()
}

// ---------------------------------------------------------------------------
// bounds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_byte_budget_and_the_row_cap_are_reported() {
    let fixture = start_fixture().await;
    let env = env().await;
    // Six long pages: each row carries ~270k characters of record text, so the
    // 1 MiB floor cuts the export and every long row is text-capped.
    for index in 0..6 {
        let url = fixture.url("127.0.0.1", &format!("/long?v={index}"));
        env.pipeline
            .extract(extract_input(
                &url,
                json!({"container": "article", "fields": {"blob": {"selector": "#blob"}}}),
            ))
            .await
            .expect("long extract");
    }

    let mut input = export_input("jsonl", "budget");
    input.max_bytes = Some(1 << 20);
    let payload = env.pipeline.corpus_export(input).await.unwrap();
    assert!(payload.records >= 1);
    assert!(payload.records < 6, "the byte budget stopped the export");
    assert!(payload.truncated);
    assert_eq!(payload.limit_hit.as_deref(), Some("max_bytes"));
    assert!(payload.bytes <= 1 << 20);

    let lines = read_jsonl(&payload.path);
    assert_eq!(lines.len(), payload.records);
    assert_eq!(lines[0]["text_truncated"], json!(true));
    assert_eq!(lines[0]["extraction_truncated"], json!(true));
    assert_eq!(
        lines[0]["text"].as_str().unwrap().chars().count(),
        200_000,
        "the row text is capped at exactly the documented limit"
    );
}

// ---------------------------------------------------------------------------
// path safety
// ---------------------------------------------------------------------------

#[tokio::test]
async fn exports_land_under_a_private_directory_and_are_never_overwritten() {
    let fixture = start_fixture().await;
    let env = env().await;
    fetch_page(&env, &fixture.url("127.0.0.1", "/a")).await;

    let first = env
        .pipeline
        .corpus_export(export_input("jsonl", "safe"))
        .await
        .unwrap();
    let second = env
        .pipeline
        .corpus_export(export_input("jsonl", "safe"))
        .await
        .unwrap();
    assert_ne!(first.path, second.path, "the timestamp keeps names unique");
    let original = std::fs::read_to_string(&first.path).unwrap();
    assert_eq!(original, std::fs::read_to_string(&second.path).unwrap());
    assert_eq!(original.lines().count(), 1);

    let exports = env.data_dir().join("exports");
    let mode = std::fs::metadata(&exports).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "the exports directory is private");

    for name in ["../evil", "bad name", "UPPER", ""] {
        let error = env
            .pipeline
            .corpus_export(export_input("jsonl", name))
            .await
            .expect_err("invalid name");
        assert_eq!(error.code(), ErrorCode::SchemaInvalid, "{name:?}");
        assert_eq!(
            error.to_tool_error().detail.as_deref(),
            Some("pointer=/name")
        );
    }

    let error = env
        .pipeline
        .corpus_export(export_input("parquet", "safe"))
        .await
        .expect_err("invalid format");
    assert_eq!(
        error.to_tool_error().detail.as_deref(),
        Some("pointer=/format")
    );
}

// ---------------------------------------------------------------------------
// monitor pairs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_monitor_pair_matches_monitor_changes_and_cross_monitor_is_typed() {
    let fixture = start_fixture().await;
    let env = env().await;
    let url = fixture.url("127.0.0.1", "/monitor-page");
    fixture.state.set("/monitor-page", &article("Watch", "one"));
    let monitor = env
        .monitors
        .create(MonitorCreateInput {
            url: url.clone(),
            mode: None,
            schema: None,
            interval_s: None,
            ignore_selectors: Vec::new(),
            label: None,
        })
        .await
        .unwrap();

    check_monitor(&env, monitor.monitor_id).await;
    fixture.state.set("/monitor-page", &article("Watch", "two"));
    check_monitor(&env, monitor.monitor_id).await;

    let ids = snapshot_ids(&env);
    assert_eq!(ids.len(), 2, "{ids:?}");
    let pair = env
        .pipeline
        .snapshot_compare(compare_input("monitor", ids[0], ids[1]))
        .await
        .expect("compare");
    assert_eq!(pair.kind, "monitor");
    assert_eq!(pair.id.from, ids[0]);
    assert_eq!(pair.id.to, ids[1]);
    assert!(pair.stats.added > 0 && pair.stats.removed > 0, "{pair:?}");
    assert!(pair.diff.as_deref().unwrap_or_default().contains("two"));

    // The stored change row for the same adjacent pair has the same stats.
    let changes = env
        .monitors
        .changes(web_widow::monitor::MonitorChangesInput {
            monitor_id: monitor.monitor_id,
            limit: None,
            cursor: None,
            include_diff: None,
        })
        .await
        .unwrap();
    let stored = &changes.changes[0].stats;
    assert_eq!(pair.stats.added, stored.lines_added);
    assert_eq!(pair.stats.removed, stored.lines_removed);
    assert_eq!(pair.stats.words_added, stored.words_added);
    assert_eq!(pair.stats.words_removed, stored.words_removed);

    // A non-adjacent pair (1 -> 3) works; the diff still reports the text change.
    fixture
        .state
        .set("/monitor-page", &article("Watch", "three"));
    check_monitor(&env, monitor.monitor_id).await;
    let ids = snapshot_ids(&env);
    let far = env
        .pipeline
        .snapshot_compare(compare_input("monitor", ids[0], ids[2]))
        .await
        .expect("non-adjacent compare");
    assert!(far.stats.added > 0);

    // A different monitor's snapshot is a typed mismatch.
    let other_url = fixture.url("127.0.0.1", "/a");
    let other = env
        .monitors
        .create(MonitorCreateInput {
            url: other_url,
            mode: None,
            schema: None,
            interval_s: None,
            ignore_selectors: Vec::new(),
            label: None,
        })
        .await
        .unwrap();
    check_monitor(&env, other.monitor_id).await;
    let other_snapshot = *snapshot_ids(&env).last().unwrap();
    let error = env
        .pipeline
        .snapshot_compare(compare_input("monitor", ids[0], other_snapshot))
        .await
        .expect_err("cross-monitor");
    assert_eq!(error.code(), ErrorCode::SchemaInvalid);
    assert!(error.to_string().contains("mismatch"), "{error}");
    assert_eq!(error.to_tool_error().detail.as_deref(), Some("pointer=/to"));

    // A missing id is not_found.
    let error = env
        .pipeline
        .snapshot_compare(compare_input("monitor", 9_999_999, ids[0]))
        .await
        .expect_err("missing snapshot");
    assert_eq!(error.code(), ErrorCode::NotFound);

    // The diff cap sets truncated + limit_hit.
    let ids = snapshot_ids(&env);
    let mut capped = compare_input("monitor", ids[0], ids[1]);
    capped.max_chars = Some(10);
    let capped = env.pipeline.snapshot_compare(capped).await.unwrap();
    assert!(capped.truncated);
    assert_eq!(capped.limit_hit.as_deref(), Some("max_chars"));
    assert!(capped.diff.as_deref().unwrap_or_default().chars().count() <= 10);
}

#[tokio::test]
async fn a_schema_monitor_also_reports_record_changes() {
    let fixture = start_fixture().await;
    let env = env().await;
    let url = fixture.url("127.0.0.1", "/monitor-schema");
    fixture.state.set("/monitor-schema", &schema_html(2, false));
    let monitor = env
        .monitors
        .create(MonitorCreateInput {
            url,
            mode: None,
            schema: Some(items_spec()),
            interval_s: None,
            ignore_selectors: Vec::new(),
            label: None,
        })
        .await
        .unwrap();
    check_monitor(&env, monitor.monitor_id).await;
    fixture.state.set("/monitor-schema", &schema_html(3, true));
    check_monitor(&env, monitor.monitor_id).await;

    let ids = snapshot_ids(&env);
    let pair = env
        .pipeline
        .snapshot_compare(compare_input("monitor", ids[0], ids[1]))
        .await
        .unwrap();
    assert_eq!(pair.stats.records_added, 1, "{pair:?}");
    assert_eq!(pair.stats.records_removed, 0);
    assert_eq!(pair.stats.records_changed, 1);
    assert_eq!(pair.record_changes, vec![0]);
}

// ---------------------------------------------------------------------------
// page fetch versions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn page_versions_compare_as_stored_and_a_missing_extraction_is_typed() {
    let fixture = start_fixture().await;
    let env = env().await;
    let url = fixture.url("127.0.0.1", "/monitor-page");
    fixture.state.set("/monitor-page", &article("Watch", "one"));
    fetch_page(&env, &url).await;
    fixture.state.set("/monitor-page", &article("Watch", "two"));
    fetch_page(&env, &url).await;

    let events = fetch_event_ids(&env, &url);
    assert_eq!(events.len(), 2, "{events:?}");
    let pair = env
        .pipeline
        .snapshot_compare(compare_input("page", events[0], events[1]))
        .await
        .expect("page compare");
    assert_eq!(pair.kind, "page");
    assert!(pair.stats.words_added > 0 && pair.stats.words_removed > 0);
    assert!(pair.diff.as_deref().unwrap_or_default().contains("two"));

    let mut no_diff = compare_input("page", events[0], events[1]);
    no_diff.include_diff = Some(false);
    let no_diff = env.pipeline.snapshot_compare(no_diff).await.unwrap();
    assert!(no_diff.diff.is_none());
    assert!(!no_diff.truncated);

    // Deleting the newer extraction (retention would) makes the version
    // unveilable: not_found naming the constraint, never an empty text.
    env.db()
        .execute(
            "DELETE FROM extractions WHERE fetch_event_id = ?1",
            [events[1]],
        )
        .unwrap();
    let error = env
        .pipeline
        .snapshot_compare(compare_input("page", events[0], events[1]))
        .await
        .expect_err("missing extraction");
    assert_eq!(error.code(), ErrorCode::NotFound);
    assert!(
        error.to_string().contains("no stored extraction"),
        "{error}"
    );

    // Two different URLs are a typed mismatch before any extraction check.
    fixture.state.set("/a", &article("Page A", "alpha"));
    let other = fixture.url("127.0.0.1", "/a");
    fetch_page(&env, &other).await;
    let other_events = fetch_event_ids(&env, &other);
    let error = env
        .pipeline
        .snapshot_compare(compare_input("page", events[0], other_events[0]))
        .await
        .expect_err("cross-url");
    assert_eq!(error.code(), ErrorCode::SchemaInvalid);
    assert!(error.to_string().contains("mismatch"), "{error}");
}

// ---------------------------------------------------------------------------
// the corpus boundary and the content scope stay intact
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pages_without_stored_text_are_skipped_and_counted() {
    let fixture = start_fixture().await;
    let env = env().await;
    fetch_page(&env, &fixture.url("127.0.0.1", "/a")).await;
    let pruned = fixture.url("127.0.0.1", "/b");
    fetch_page(&env, &pruned).await;

    // Simulate retention pruning the extraction of a successful page.
    env.db()
        .execute(
            "DELETE FROM extractions WHERE page_id = (SELECT id FROM pages WHERE url = ?1)",
            [&pruned],
        )
        .unwrap();

    let payload = env
        .pipeline
        .corpus_export(export_input("jsonl", "skip"))
        .await
        .unwrap();
    assert_eq!(payload.records, 1, "the pruned page has no text");
    assert_eq!(payload.skipped_no_text, 1);
    assert!(!payload.truncated);
    let lines = read_jsonl(&payload.path);
    assert_eq!(lines.len(), 1);
    assert!(
        !lines[0]["url"].as_str().unwrap_or_default().ends_with("/b"),
        "a page with no text is never emitted"
    );
}
