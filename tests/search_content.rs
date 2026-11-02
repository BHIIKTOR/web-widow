//! M05 Lane B — the local full-text index and `search_content` over the real
//! store and pipeline paths.
//!
//! B4-3 is driven through `fetch_page`, `extract`, `crawl_page` and
//! `monitor_fetch`, never by inserting an FTS row: the claim under test is that
//! every acquisition path maintains the index. B4-4/B4-5 check the query
//! contract on the store's own write API, where the fixture data is precise.

mod common;

use common::{Fixture, TestEnv, start_fixture, test_config};
use serde_json::json;
use web_widow::ErrorCode;
use web_widow::core::url::PageUrl;
use web_widow::extract::Mode;
use web_widow::fts::{SearchContentInput, SearchContentPayload};
use web_widow::pipeline::RenderPolicy;

async fn env_with(patch: impl FnOnce(&mut web_widow::config::Config)) -> (Fixture, TestEnv) {
    let fixture = start_fixture().await;
    let mut config = test_config();
    patch(&mut config);
    (fixture, TestEnv::new(config).await)
}

async fn search(env: &TestEnv, query: &str, limit: Option<usize>) -> SearchContentPayload {
    env.content
        .search(SearchContentInput {
            query: query.to_string(),
            limit,
        })
        .await
        .expect("search_content succeeds")
}

/// `(pages, pages_fts)` as the database holds them.
fn counts(env: &TestEnv) -> (i64, i64) {
    let connection = env.db();
    let pages = connection
        .query_row("SELECT COUNT(*) FROM pages", [], |row| row.get(0))
        .unwrap();
    let indexed = connection
        .query_row("SELECT COUNT(*) FROM pages_fts", [], |row| row.get(0))
        .unwrap();
    (pages, indexed)
}

fn indexed_body(env: &TestEnv, url: &str) -> String {
    env.db()
        .query_row(
            "SELECT f.body FROM pages_fts f JOIN pages p ON p.id = f.rowid WHERE p.url = ?1",
            [url],
            |row| row.get(0),
        )
        .unwrap()
}

#[tokio::test]
async fn every_acquisition_path_indexes_what_it_stores() {
    let (fixture, env) = env_with(|_| {}).await;
    fixture.set_page(
        "/monitor/fts",
        "<html><head><title>Monitor FTS</title></head><body><main><article>\
         <h1>Monitor FTS</h1><p>a kangaroo watches the monitored page</p>\
         </article></main></body></html>",
    );

    // 1. fetch_page — the delivered Markdown.
    let article = env
        .fetch(&fixture.url("127.0.0.1", "/article"))
        .await
        .expect("fetch_page");
    // 2. extract — the returned record string values.
    env.pipeline
        .extract(web_widow::pipeline::ExtractInput {
            url: fixture.url("127.0.0.1", "/extract/items/"),
            schema: json!({
                "container": ".item",
                "fields": { "name": { "selector": "h2" }, "href": { "selector": "a" } }
            }),
            render: None,
            use_cache: None,
            max_field_chars: None,
            max_chars: None,
        })
        .await
        .expect("extract");
    // 3. crawl — the delivered main Markdown.
    let crawl_url = fixture.url("127.0.0.1", "/site/variant");
    env.pipeline
        .crawl_page(
            &PageUrl::parse(&crawl_url).unwrap(),
            None,
            RenderPolicy::Never,
            None,
            "crawl",
        )
        .await
        .expect("crawl_page");
    // 4. monitor check — the delivered main Markdown.
    env.pipeline
        .monitor_fetch(
            &fixture.url("127.0.0.1", "/monitor/fts"),
            Mode::Main,
            &[],
            None,
            None,
            false,
        )
        .await
        .expect("monitor_fetch");

    let (pages, indexed) = counts(&env);
    assert_eq!(pages, 4, "four acquisitions, one page row each");
    assert_eq!(indexed, pages, "every acquisition indexed its page");

    // Each stored body is the text that acquisition delivered.
    assert!(indexed_body(&env, &article.payload.url).contains("lighthouse keeper"));

    let hits = search(&env, "lighthouse", None).await;
    assert_eq!(hits.result_count, 1);
    assert_eq!(hits.results[0].url, article.payload.url);
    assert_eq!(hits.results[0].title, "Fixture Article");
    assert_eq!(hits.results[0].render_mode.as_deref(), Some("static"));
    assert!(hits.results[0].last_fetched.is_some());

    let hits = search(&env, "Second item", None).await;
    assert_eq!(hits.result_count, 1, "extract records are indexed");
    assert!(hits.results[0].url.ends_with("/extract/items/"));

    let hits = search(&env, "canonical resource", None).await;
    assert_eq!(hits.result_count, 1, "crawl Markdown is indexed");
    assert!(hits.results[0].url.ends_with("/site/variant"));

    let hits = search(&env, "kangaroo", None).await;
    assert_eq!(hits.result_count, 1, "monitor Markdown is indexed");
    assert!(hits.results[0].url.ends_with("/monitor/fts"));
}

#[tokio::test]
async fn a_304_keeps_the_index_row_and_a_failed_fetch_indexes_nothing() {
    let (fixture, env) = env_with(|_| {}).await;
    let url = fixture.url("127.0.0.1", "/article");
    env.fetch(&url).await.expect("first fetch");

    // The fixture answers 304 to the stored ETag: the page row changes, the
    // index row must not.
    let second = env.fetch(&url).await.expect("conditional fetch");
    assert_eq!(second.payload.status, 304);
    assert_eq!(counts(&env).1, 1);
    assert!(indexed_body(&env, &url).contains("lighthouse keeper"));
    assert_eq!(search(&env, "lighthouse", None).await.result_count, 1);

    // A failed fetch records the page and the failure event but no index text.
    let missing = fixture.url("127.0.0.1", "/missing-page");
    assert!(env.fetch(&missing).await.is_err());
    let connection = env.db();
    let failed_page: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pages WHERE url = ?1",
            [&missing],
            |row| row.get(0),
        )
        .unwrap();
    let failed_index: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pages_fts f JOIN pages p ON p.id = f.rowid WHERE p.url = ?1",
            [&missing],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(failed_page, 1, "the failure is still recorded as a page");
    assert_eq!(failed_index, 0, "a failure writes no index text");
}

#[tokio::test]
async fn ranking_weights_the_title_and_snippets_are_plain_and_bounded() {
    let (_fixture, env) = env_with(|_| {}).await;
    let connection = env.db();
    let insert = |url: &str, title: &str, body: &str| {
        connection
            .execute(
                "INSERT INTO pages (url, canonical_url, url_hash, first_seen, outcome, title)
                 VALUES (?1, ?1, ?2, '2026-11-01T00:00:00Z', 'ok', ?3)",
                rusqlite::params![url, url, title],
            )
            .unwrap();
        let id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO pages_fts (rowid, title, body) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, title, body],
            )
            .unwrap();
    };
    insert(
        "https://example.com/title",
        "Zebra guide",
        "an unrelated page about other animals",
    );
    insert(
        "https://example.com/body",
        "Body page",
        "zebra zebra zebra appear only in this body",
    );
    insert(
        "https://example.com/other",
        "Other page",
        "nothing relevant is written here",
    );
    drop(connection);

    let hits = search(&env, "zebra", None).await;
    assert_eq!(hits.result_count, 2);
    assert_eq!(hits.results[0].url, "https://example.com/title");
    assert_eq!(hits.results[1].url, "https://example.com/body");
    assert!(
        hits.results[0].score > hits.results[1].score,
        "the 4x title weight outranks a repeated body term: {:?}",
        hits.results
    );
    assert!(hits.results[0].score > 0.0, "score is -bm25, so positive");

    // A body-only term returns a plain, non-empty snippet.
    let body_only = search(&env, "appear", None).await;
    assert_eq!(body_only.results.len(), 1);
    let snippet = &body_only.results[0].snippet;
    assert!(
        snippet.contains("zebra") || snippet.contains("appear"),
        "{snippet:?}"
    );
    assert!(
        !snippet.contains('<') && !snippet.contains('>'),
        "{snippet:?}"
    );

    // The count is the full match count; the page reports the limit.
    let bounded = search(&env, "zebra", Some(1)).await;
    assert_eq!(bounded.result_count, 2);
    assert_eq!(bounded.results.len(), 1);
    assert!(bounded.truncated);
    assert_eq!(bounded.limit_hit.as_deref(), Some("limit"));

    // No limit hit when the page holds everything.
    let whole = search(&env, "zebra", Some(10)).await;
    assert!(!whole.truncated);
    assert_eq!(whole.limit_hit, None);
}

#[tokio::test]
async fn fts_operators_in_a_query_are_literal_text() {
    let (_fixture, env) = env_with(|_| {}).await;
    let connection = env.db();
    connection
        .execute(
            "INSERT INTO pages (url, canonical_url, url_hash, first_seen, outcome, title)
             VALUES ('https://example.com/ops', 'https://example.com/ops', 'h1',
                     '2026-11-01T00:00:00Z', 'ok', 'Operators')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO pages_fts (rowid, title, body)
             VALUES (1, 'Operators', 'alpha OR beta NEAR gamma delta')",
            [],
        )
        .unwrap();
    drop(connection);

    // Each of these would be syntax or an operator under FTS5; here they are
    // words, and `OR`/`NEAR` really are in the stored text.
    assert_eq!(search(&env, "OR", None).await.result_count, 1);
    assert_eq!(search(&env, "NEAR", None).await.result_count, 1);
    assert_eq!(search(&env, "alpha OR beta", None).await.result_count, 1);
    // A term that is not in the page still matches nothing — no error.
    assert_eq!(search(&env, "*", None).await.result_count, 0);
    assert_eq!(search(&env, "-exclude", None).await.result_count, 0);
    // Every term must appear (AND, not OR).
    assert_eq!(search(&env, "alpha zeta", None).await.result_count, 0);
}

#[tokio::test]
async fn caps_truncate_the_indexed_body_and_the_snippet_but_keep_the_title() {
    let (fixture, env) = env_with(|config| {
        config.fts.max_index_chars = 32;
        config.fts.max_snippet_chars = 12;
    })
    .await;
    let url = fixture.url("127.0.0.1", "/article");
    let fetched = env.fetch(&url).await.expect("fetch_page");

    let stored = indexed_body(&env, &url);
    assert_eq!(stored.chars().count(), 32, "body capped at max_index_chars");
    assert_eq!(
        stored,
        fetched.markdown.chars().take(32).collect::<String>(),
        "the cap keeps a prefix of the delivered Markdown, on a char boundary"
    );
    // The tail of the body is not indexed.
    assert_eq!(search(&env, "forty-one", None).await.result_count, 0);
    // The title is always indexed, whole.
    let by_title = search(&env, "Fixture", None).await;
    assert_eq!(
        by_title.result_count, 1,
        "the title is indexed despite the cap"
    );
    assert_eq!(by_title.results[0].title, "Fixture Article");

    // The snippet is capped, and still names the matched term.
    let hits = search(&env, "Fixture", None).await;
    assert!(hits.results[0].snippet.chars().count() <= 12);
}

#[tokio::test]
async fn empty_blank_and_overlong_queries_are_schema_invalid_at_the_query_pointer() {
    let (_fixture, env) = env_with(|config| config.fts.max_query_chars = 16).await;

    for query in ["", "   ", &"x".repeat(17)] {
        let error = env
            .content
            .search(SearchContentInput {
                query: query.to_string(),
                limit: None,
            })
            .await
            .expect_err("invalid query");
        assert_eq!(error.code(), ErrorCode::SchemaInvalid, "query {query:?}");
        assert!(
            error
                .detail()
                .as_deref()
                .unwrap_or_default()
                .contains("/query"),
            "the error points at /query: {error}"
        );
    }

    // An out-of-range limit is schema_invalid at /limit, never a silent clamp.
    let error = env
        .content
        .search(SearchContentInput {
            query: "anything".to_string(),
            limit: Some(101),
        })
        .await
        .expect_err("invalid limit");
    assert_eq!(error.code(), ErrorCode::SchemaInvalid);
    assert!(
        error
            .detail()
            .as_deref()
            .unwrap_or_default()
            .contains("/limit")
    );
}

/// The delivered page text, as the index holds it: `None` when the page has no
/// row at all.
fn indexed_body_len(env: &TestEnv, url: &str) -> Option<i64> {
    env.db()
        .query_row(
            "SELECT length(f.body) FROM pages_fts f JOIN pages p ON p.id = f.rowid
             WHERE p.url = ?1",
            [url],
            |row| row.get(0),
        )
        .ok()
}

fn nav_extract(url: &str) -> web_widow::pipeline::ExtractInput {
    web_widow::pipeline::ExtractInput {
        url: url.to_string(),
        // Only the page's nav anchor: a projection that must never replace the
        // delivered article text (M05 B01b).
        schema: json!({
            "container": "nav a",
            "fields": { "label": { "selector": "*", "transform": "text" } }
        }),
        render: None,
        use_cache: None,
        max_field_chars: None,
        max_chars: None,
    }
}

#[tokio::test]
async fn an_extract_of_the_same_url_keeps_the_delivered_page_text_indexed() {
    let (fixture, env) = env_with(|_| {}).await;
    let url = fixture.url("127.0.0.1", "/article");
    let fetched = env.fetch(&url).await.expect("fetch_page");

    let before = indexed_body_len(&env, &url).expect("the fetch indexed the page");
    assert_eq!(
        before as usize,
        fetched.markdown.chars().count(),
        "the index holds the delivered Markdown"
    );
    assert_eq!(search(&env, "lighthouse", None).await.result_count, 1);

    // The same URL through `extract`. Its records are a projection of the page.
    let extracted = env
        .pipeline
        .extract(nav_extract(&url))
        .await
        .expect("extract");
    assert_eq!(extracted.records.len(), 1, "{extracted:?}");
    assert_eq!(extracted.records[0]["label"], json!("Home"));

    let after = indexed_body_len(&env, &url).expect("the row still exists");
    assert_eq!(
        after, before,
        "an extract must not change the indexed body of the page it projects"
    );
    assert_eq!(
        search(&env, "lighthouse", None).await.result_count,
        1,
        "a body-only term still finds the page after the extract"
    );
    assert_eq!(
        search(&env, "Home", None).await.result_count,
        0,
        "the projection did not replace the delivered text"
    );
    let (pages, indexed) = counts(&env);
    assert_eq!(indexed, pages, "still one index row per page");
}

#[tokio::test]
async fn an_extract_first_page_is_filled_then_replaced_by_the_page_acquisition() {
    let (fixture, env) = env_with(|_| {}).await;
    let url = fixture.url("127.0.0.1", "/extract/items/");

    // Extract first: the page has no row, so the record text fills it.
    let filled = env
        .pipeline
        .extract(web_widow::pipeline::ExtractInput {
            url: url.clone(),
            schema: json!({
                "container": ".item",
                "fields": { "name": { "selector": "h2" } }
            }),
            render: None,
            use_cache: None,
            max_field_chars: None,
            max_chars: None,
        })
        .await
        .expect("extract with a matching spec");
    assert_eq!(filled.records.len(), 2);
    let filled_len = indexed_body_len(&env, &url).expect("the extract filled the row");
    assert!(filled_len > 0, "the record text is indexed");
    assert_eq!(search(&env, "First item", None).await.result_count, 1);

    // Then the page itself: the delivered Markdown replaces the projection.
    let fetched = env.fetch(&url).await.expect("fetch_page");
    let replaced_len = indexed_body_len(&env, &url).expect("the row still exists");
    assert_eq!(
        replaced_len as usize,
        fetched.markdown.chars().count(),
        "the page acquisition replaced the projected text with the delivered Markdown"
    );
    let (pages, indexed) = counts(&env);
    assert_eq!(indexed, pages, "one index row for the page");
}
