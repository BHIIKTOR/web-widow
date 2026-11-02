//! M04 B03 — `fetch_feed` (IMP-DEC-019): happy paths, caps, errors, politeness,
//! persistence, and monitor composition.
//!
//! Every request goes to the loopback fixture server; the real-feed read is a
//! separate bounded operator transcript under `plan/artifacts/`.

mod common;

use common::{Fixture, TestEnv, start_fixture, test_config};
use web_widow::ErrorCode;
use web_widow::config::Config;
use web_widow::extract::Mode;
use web_widow::feed::FeedInput;
use web_widow::monitor::{MonitorChangesInput, MonitorCheckInput, MonitorCreateInput};

fn input(url: String, limit: Option<usize>) -> FeedInput {
    FeedInput { url, limit }
}

async fn env_with(patch: impl FnOnce(&mut Config)) -> (Fixture, TestEnv) {
    let fixture = start_fixture().await;
    let mut config = test_config();
    patch(&mut config);
    (fixture, TestEnv::new(config).await)
}

fn row_counts(env: &TestEnv) -> (i64, i64, i64, i64, i64) {
    let db = env.db();
    let count = |table: &str| {
        db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap()
    };
    (
        count("pages"),
        count("extractions"),
        count("profiles"),
        count("blobs"),
        count("jobs"),
    )
}

// B4-11 ----------------------------------------------------------------------
#[tokio::test]
async fn rss_items_keep_feed_order_with_normalized_fields() {
    let (fixture, env) = env_with(|_| {}).await;
    let payload = env
        .pipeline
        .fetch_feed(input(fixture.url("127.0.0.1", "/feed/rss"), None))
        .await
        .unwrap();

    assert_eq!(payload.feed_type, "rss");
    assert_eq!(payload.content_type, "application/rss+xml");
    assert_eq!(payload.title.as_deref(), Some("Fixture news"));
    assert_eq!(payload.site_url.as_deref(), Some("https://example.com/"));
    assert_eq!(payload.language.as_deref(), Some("en"));
    assert_eq!(payload.description, "The fixture feed description.");
    assert_eq!(payload.item_count, 2);
    assert_eq!(payload.items.len(), 2);
    assert!(!payload.truncated);
    assert_eq!(payload.limit_hit, None);

    assert_eq!(payload.items[0].id, "fixture:1");
    assert_eq!(
        payload.items[0].title.as_deref(),
        Some("First fixture story")
    );
    assert_eq!(
        payload.items[0].url.as_deref(),
        Some("https://example.com/first")
    );
    assert_eq!(
        payload.items[0].published.as_deref(),
        Some("2026-10-31T06:00:00Z")
    );
    assert_eq!(
        payload.items[0].summary,
        "First summary text with a script."
    );
    assert!(!payload.items[0].summary.contains("alert(1)"));

    // Feed order is preserved and a relative link resolves against the
    // response's final URL (the fixture host in this test).
    assert_eq!(
        payload.items[1].title.as_deref(),
        Some("Second fixture story")
    );
    assert_eq!(
        payload.items[1].url.as_deref(),
        Some(fixture.url("127.0.0.1", "/second").as_str())
    );
    assert_eq!(payload.items[1].summary, "Plain second summary.");
}

#[tokio::test]
async fn atom_and_json_feeds_are_recognized() {
    let (fixture, env) = env_with(|_| {}).await;

    let atom = env
        .pipeline
        .fetch_feed(input(fixture.url("127.0.0.1", "/feed/atom"), None))
        .await
        .unwrap();
    assert_eq!(atom.feed_type, "atom");
    assert_eq!(atom.items.len(), 1);
    // A relative item link resolves against the response's final URL, per the
    // contract (not against the feed's declared home page).
    assert_eq!(
        atom.items[0].url.as_deref(),
        Some(fixture.url("127.0.0.1", "/atom-item").as_str())
    );
    assert_eq!(atom.items[0].summary, "Atom fixture summary.");

    let json = env
        .pipeline
        .fetch_feed(input(fixture.url("127.0.0.1", "/feed/json"), None))
        .await
        .unwrap();
    assert_eq!(json.feed_type, "json");
    assert_eq!(json.item_count, 2);
    assert_eq!(json.items[0].summary, "JSON fixture summary.");
    assert_eq!(json.items[1].summary, "Second JSON summary.");
}

#[tokio::test]
async fn duplicate_items_keep_the_first_and_caps_are_reported() {
    let (fixture, env) = env_with(|_| {}).await;

    let dupes = env
        .pipeline
        .fetch_feed(input(fixture.url("127.0.0.1", "/feed/dupes"), None))
        .await
        .unwrap();
    assert_eq!(dupes.item_count, 1);
    assert_eq!(dupes.items[0].title.as_deref(), Some("Kept first"));

    let limited = env
        .pipeline
        .fetch_feed(input(fixture.url("127.0.0.1", "/feed/many"), Some(2)))
        .await
        .unwrap();
    assert_eq!(limited.item_count, 5);
    assert_eq!(limited.items.len(), 2);
    assert!(limited.truncated);
    assert_eq!(limited.limit_hit.as_deref(), Some("limit"));

    // Summary caps use their own hit only when the item list is complete.
    let (fixture, env) = env_with(|config| {
        config.feed.max_summary_chars = 10;
    })
    .await;
    let capped = env
        .pipeline
        .fetch_feed(input(fixture.url("127.0.0.1", "/feed/many"), None))
        .await
        .unwrap();
    assert_eq!(capped.items.len(), 5);
    assert_eq!(capped.truncated_summaries, 5);
    assert!(capped.truncated);
    assert_eq!(capped.limit_hit.as_deref(), Some("max_summary_chars"));
    assert!(
        capped
            .items
            .iter()
            .all(|item| item.summary.chars().count() <= 10)
    );

    // A zero limit is a schema error, not a silent empty list.
    let error = env
        .pipeline
        .fetch_feed(input(fixture.url("127.0.0.1", "/feed/rss"), Some(0)))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::SchemaInvalid);
}

// M05 A01 — feed item identity (IMP-DEC-026) --------------------------------
#[tokio::test]
async fn canonical_url_is_the_identity_and_tracking_variants_dedupe() {
    let (fixture, env) = env_with(|_| {}).await;
    let payload = env
        .pipeline
        .fetch_feed(input(fixture.url("127.0.0.1", "/feed/canonical"), None))
        .await
        .unwrap();

    // Three survive: the first tracking variant (verbatim URL kept), the `syn`
    // variant (unknown params remain identity), and the item with no link.
    assert_eq!(payload.item_count, 3, "{payload:#?}");
    assert_eq!(payload.items.len(), 3);

    let first = &payload.items[0];
    assert_eq!(first.title.as_deref(), Some("First tracking variant"));
    assert_eq!(
        first.url.as_deref(),
        Some("https://example.com/story?utm_source=newsletter&b=2&a=1"),
        "url stays verbatim, as the feed stated it"
    );
    assert_eq!(
        first.canonical_url.as_deref(),
        Some("https://example.com/story?a=1&b=2"),
        "canonical drops tracking params and sorts the rest"
    );

    let syn = &payload.items[1];
    assert_eq!(syn.title.as_deref(), Some("Unknown parameter variant"));
    assert_eq!(
        syn.url.as_deref(),
        Some("https://example.com/story?syn=abc")
    );
    assert_eq!(
        syn.canonical_url.as_deref(),
        Some("https://example.com/story?syn=abc"),
        "syn is not a tracking parameter: it remains identity"
    );

    let no_link = &payload.items[2];
    assert_eq!(no_link.url, None);
    assert_eq!(no_link.canonical_url, None);

    // The key is stable on the wire: null, never absent.
    let value = serde_json::to_value(&payload).unwrap();
    let item = &value["items"][2];
    assert!(item.get("canonical_url").is_some(), "{item}");
    assert!(item["canonical_url"].is_null(), "{item}");
    assert_eq!(
        value["items"][0]["canonical_url"],
        "https://example.com/story?a=1&b=2"
    );
}

// B4-12 ----------------------------------------------------------------------
#[tokio::test]
async fn non_feed_and_broken_bodies_are_typed_failures() {
    let (fixture, env) = env_with(|_| {}).await;
    for path in ["/feed/html", "/feed/plain"] {
        let error = env
            .pipeline
            .fetch_feed(input(fixture.url("127.0.0.1", path), None))
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            ErrorCode::UnsupportedContentType,
            "{path}: {error}"
        );
    }
    for path in ["/feed/malformed", "/feed/bomb"] {
        let error = env
            .pipeline
            .fetch_feed(input(fixture.url("127.0.0.1", path), None))
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::ExtractionFailed, "{path}: {error}");
    }
}

#[tokio::test]
async fn feed_reads_are_polite_and_write_no_content_rows() {
    let (fixture, env) = env_with(|config| {
        config.fetch.min_interval_ms = 300;
    })
    .await;
    let before = row_counts(&env);
    let url = fixture.url("127.0.0.1", "/feed/rss");
    env.pipeline
        .fetch_feed(input(url.clone(), None))
        .await
        .unwrap();
    env.pipeline.fetch_feed(input(url, None)).await.unwrap();

    let stamps = fixture.log.stamps_matching("/feed/rss");
    assert_eq!(stamps.len(), 2);
    let gap = stamps[1].duration_since(stamps[0]);
    assert!(
        gap >= std::time::Duration::from_millis(250),
        "two feed reads were {gap:?} apart"
    );
    assert_eq!(
        row_counts(&env),
        before,
        "fetch_feed writes no content rows"
    );
}

// B4-13 ----------------------------------------------------------------------
#[tokio::test]
async fn a_feed_body_can_be_watched_and_an_item_url_is_an_ordinary_monitor() {
    let (fixture, env) = env_with(|_| {}).await;
    let feed_url = fixture.url("127.0.0.1", "/monitor/feed");
    fixture.set_page("/monitor/feed", common::M04_FEED_RSS);

    let monitor = env
        .monitors
        .create(MonitorCreateInput {
            url: feed_url,
            mode: Some(Mode::Text),
            schema: None,
            interval_s: Some(3600),
            ignore_selectors: Vec::new(),
            label: Some("m04:feed".to_string()),
        })
        .await
        .unwrap();
    env.monitors
        .check(MonitorCheckInput {
            monitor_id: Some(monitor.monitor_id),
            all: None,
            force: Some(true),
            wait_ms: None,
        })
        .await
        .unwrap();

    fixture.set_page(
        "/monitor/feed",
        &common::M04_FEED_RSS.replace("First fixture story", "Renamed fixture story"),
    );
    let checked = env
        .monitors
        .check(MonitorCheckInput {
            monitor_id: Some(monitor.monitor_id),
            all: None,
            force: Some(true),
            wait_ms: None,
        })
        .await
        .unwrap();
    assert_eq!(checked.changed.len(), 1, "{checked:?}");

    let changes = env
        .monitors
        .changes(MonitorChangesInput {
            monitor_id: monitor.monitor_id,
            limit: None,
            cursor: None,
            include_diff: None,
        })
        .await
        .unwrap();
    assert_eq!(changes.changes.len(), 1);

    // An item URL is just a normal page monitor: no feed-specific kind exists.
    let item = env
        .monitors
        .create(MonitorCreateInput {
            url: fixture.url("127.0.0.1", "/article"),
            mode: None,
            schema: None,
            interval_s: Some(3600),
            ignore_selectors: Vec::new(),
            label: None,
        })
        .await
        .unwrap();
    let listed = env
        .monitors
        .list(web_widow::monitor::MonitorListInput {
            include_inactive: None,
            limit: None,
        })
        .await
        .unwrap();
    assert!(
        listed
            .monitors
            .iter()
            .any(|row| row.monitor_id == item.monitor_id)
    );
    // The feed URL is an ordinary text monitor; there is no feed-specific kind.
    let feed_row = listed
        .monitors
        .iter()
        .find(|row| row.monitor_id == monitor.monitor_id)
        .expect("feed monitor row");
    assert_eq!(feed_row.mode, "text");
    assert!(!feed_row.has_schema);
}
