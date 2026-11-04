//! v0.6 — `events` JSON/RSS over real monitor changes.

mod common;

use std::sync::Arc;

use common::{Fixture, TestEnv, start_fixture, test_config};
use web_widow::config::Config;
use web_widow::events::{EventsFormat, EventsInput, EventsPayload, EventsService, FeedRuntime};
use web_widow::extract::Mode;
use web_widow::monitor::{MonitorChangesInput, MonitorCheckInput, MonitorCreateInput};

fn page(body: &str) -> String {
    format!(
        "<!doctype html><html><head><title>Events page</title></head><body>\
         <main><article><h1>Events page</h1><p>{body}</p></article></main></body></html>"
    )
}

async fn env_with(patch: impl FnOnce(&mut Config)) -> (Fixture, TestEnv) {
    let fixture = start_fixture().await;
    let mut config = test_config();
    patch(&mut config);
    (fixture, TestEnv::new(config).await)
}

/// Create the monitor and take its baseline snapshot.
async fn baseline(fixture: &Fixture, env: &TestEnv, path: &str, body: &str) -> i64 {
    fixture.set_page(path, &page(body));
    let monitor = env
        .monitors
        .create(MonitorCreateInput {
            url: fixture.url("127.0.0.1", path),
            mode: Some(Mode::Full),
            schema: None,
            interval_s: Some(3600),
            ignore_selectors: Vec::new(),
            label: Some("project:events".to_string()),
        })
        .await
        .expect("monitor created")
        .monitor_id;
    check(env, monitor).await;
    monitor
}

async fn check(env: &TestEnv, monitor: i64) {
    env.monitors
        .check(MonitorCheckInput {
            monitor_id: Some(monitor),
            all: None,
            force: Some(true),
            wait_ms: None,
        })
        .await
        .expect("check");
}

async fn change(fixture: &Fixture, env: &TestEnv, monitor: i64, path: &str, body: &str) {
    fixture.set_page(path, &page(body));
    check(env, monitor).await;
}

fn service(env: &TestEnv) -> EventsService {
    EventsService::new(
        env.store.clone(),
        env.config.events.clone(),
        Arc::new(FeedRuntime::default()),
        false,
    )
}

fn input(format: EventsFormat) -> EventsInput {
    EventsInput {
        format: Some(format),
        monitor_id: None,
        since: None,
        limit: None,
        include_diff: None,
    }
}

#[tokio::test]
async fn json_items_carry_the_change_shape_stats_and_filters() {
    let (fixture, env) = env_with(|_| {}).await;
    let path = "/monitor/events";
    let monitor = baseline(&fixture, &env, path, "first version of the events body").await;
    change(
        &fixture,
        &env,
        monitor,
        path,
        "second changed version of the events body with more words",
    )
    .await;

    let changed = env
        .monitors
        .changes(MonitorChangesInput {
            monitor_id: monitor,
            limit: Some(10),
            cursor: None,
            include_diff: Some(false),
        })
        .await
        .expect("monitor changes");
    assert_eq!(changed.changes.len(), 1, "one real change");

    let payload = service(&env)
        .events(input(EventsFormat::Json))
        .await
        .expect("events");
    let EventsPayload::Json(json) = payload else {
        panic!("expected JSON");
    };
    assert_eq!(json.count, 1);
    assert!(!json.truncated);
    let item = &json.items[0];
    assert_eq!(item.monitor_id, monitor);
    assert_eq!(item.monitor_label.as_deref(), Some("project:events"));
    assert!(item.url.ends_with(path));
    assert_eq!(item.title.as_deref(), Some("Events page"));
    assert!(item.detected_at.ends_with('Z'));
    assert!(!item.kind.is_empty());
    assert!(item.diff.is_none(), "include_diff defaults to false");
    assert_eq!(
        serde_json::to_value(&item.stats).unwrap(),
        serde_json::to_value(&changed.changes[0].stats).unwrap(),
        "stats shape matches monitor_changes"
    );
    assert_eq!(item.from_snapshot, changed.changes[0].from_snapshot);
    assert_eq!(item.to_snapshot, changed.changes[0].to_snapshot);

    // include_diff returns the capped diff.
    let mut with_diff = input(EventsFormat::Json);
    with_diff.include_diff = Some(true);
    let EventsPayload::Json(json) = service(&env).events(with_diff).await.expect("events") else {
        panic!("expected JSON");
    };
    assert!(json.items[0].diff.is_some());

    // since filters inclusively; a future timestamp is empty.
    let detected = item.detected_at.clone();
    let mut since = input(EventsFormat::Json);
    since.since = Some(detected.clone());
    let EventsPayload::Json(json) = service(&env).events(since).await.expect("events") else {
        panic!("expected JSON");
    };
    assert_eq!(json.count, 1);
    let mut future = input(EventsFormat::Json);
    future.since = Some("2999-01-01T00:00:00Z".to_string());
    let EventsPayload::Json(json) = service(&env).events(future).await.expect("events") else {
        panic!("expected JSON");
    };
    assert_eq!(json.count, 0);

    let mut bad = input(EventsFormat::Json);
    bad.since = Some("yesterday".to_string());
    let error = service(&env).events(bad).await.unwrap_err();
    assert_eq!(error.code(), web_widow::ErrorCode::SchemaInvalid);

    let mut unknown = input(EventsFormat::Json);
    unknown.monitor_id = Some(999_999);
    let error = service(&env).events(unknown).await.unwrap_err();
    assert_eq!(error.code(), web_widow::ErrorCode::NotFound);

    let mut other = input(EventsFormat::Json);
    other.monitor_id = Some(monitor);
    let EventsPayload::Json(json) = service(&env).events(other).await.expect("events") else {
        panic!("expected JSON");
    };
    assert_eq!(json.count, 1);

    // limit=1 with a second monitor's change reports truncation.
    let second = baseline(&fixture, &env, "/monitor/events-2", "another body entirely").await;
    change(
        &fixture,
        &env,
        second,
        "/monitor/events-2",
        "another changed body entirely",
    )
    .await;
    let mut limited = input(EventsFormat::Json);
    limited.limit = Some(1);
    let EventsPayload::Json(json) = service(&env).events(limited).await.expect("events") else {
        panic!("expected JSON");
    };
    assert_eq!(json.count, 1);
    assert!(json.truncated);
    assert_eq!(json.limit_hit.as_deref(), Some("limit"));
}

#[tokio::test]
async fn the_rss_document_is_valid_escaped_capped_and_carries_no_diff() {
    let (fixture, env) = env_with(|_| {}).await;
    let path = "/monitor/events-rss";
    let monitor = baseline(&fixture, &env, path, "alpha version of the feed body").await;
    change(
        &fixture,
        &env,
        monitor,
        path,
        "beta & <changed> version of the feed body",
    )
    .await;

    let payload = service(&env)
        .events(input(EventsFormat::Rss))
        .await
        .expect("rss");
    let EventsPayload::Rss(rss) = payload else {
        panic!("expected RSS");
    };
    assert!(rss.bytes > 0);
    assert_eq!(rss.bytes, rss.xml.len());
    assert_eq!(rss.item_count, 1);
    assert!(rss.feed_url.is_none(), "stdio has no route");
    assert!(rss.xml.starts_with("<?xml"));
    assert!(!rss.xml.contains("--- "), "the feed never carries the diff");

    let parsed = feed_rs::parser::parse(rss.xml.as_bytes()).expect("feed-rs parses the RSS");
    assert_eq!(
        parsed.title.as_ref().map(|title| title.content.as_str()),
        Some("web-widow changes")
    );
    assert_eq!(parsed.entries.len(), 1);
    let entry = &parsed.entries[0];
    assert_eq!(
        entry.links.first().map(|link| link.href.as_str()),
        Some(fixture.url("127.0.0.1", path).as_str())
    );
    assert!(entry.published.is_some());
    let description = entry
        .summary
        .as_ref()
        .map(|summary| summary.content.clone())
        .unwrap_or_default();
    assert!(description.contains("lines"), "{description}");
}

#[tokio::test]
async fn the_rss_item_cap_is_one_hundred() {
    let (fixture, env) = env_with(|config| config.events.feed_items = 200).await;
    let path = "/monitor/events-many";
    let monitor = baseline(&fixture, &env, path, "seed body").await;
    for index in 0..105 {
        change(
            &fixture,
            &env,
            monitor,
            path,
            &format!("changed body number {index} with distinct words"),
        )
        .await;
    }
    let mut many = input(EventsFormat::Rss);
    many.limit = Some(200);
    let payload = service(&env).events(many).await.expect("rss");
    let EventsPayload::Rss(rss) = payload else {
        panic!("expected RSS");
    };
    assert_eq!(rss.item_count, 100, "RSS caps at 100 items");
    let parsed = feed_rs::parser::parse(rss.xml.as_bytes()).expect("parses");
    assert_eq!(parsed.entries.len(), 100);
}

#[test]
fn events_config_defaults_and_bounds_are_validated() {
    let config = web_widow::config::Config::default();
    assert!(config.events.feed_enabled);
    assert_eq!(config.events.feed_items, 100);

    let mut disabled = web_widow::config::Config::default();
    disabled.events.feed_enabled = false;
    disabled.validate().expect("disabled is valid");

    for items in [0usize, 201] {
        let mut bad = web_widow::config::Config::default();
        bad.events.feed_items = items;
        assert!(
            bad.validate().is_err(),
            "feed_items {items} must be refused"
        );
    }
}
