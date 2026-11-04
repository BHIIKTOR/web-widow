//! v0.6 — `robots_cache` (C3-1, C3-2): cache listing, path probe and the
//! zero-network proof.

mod common;

use chrono::{Duration, Utc};
use common::{TestEnv, start_fixture, test_config};
use web_widow::robots_cache::{RobotsCacheInput, RobotsCacheService};
use web_widow::store::HostRow;

fn body(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}

async fn seed(env: &TestEnv, host: &str, robots: Option<&str>, expires_in: i64, failures: i64) {
    let now = Utc::now();
    env.store
        .db
        .save_host(HostRow {
            host: host.to_string(),
            robots_fetched_at: Some(now),
            robots_status: Some(if robots.is_some() { 200 } else { 500 }),
            robots_hash: robots.map(|text| web_widow::core::url::content_hash(text.as_bytes())),
            robots_body: robots.map(body),
            robots_expires_at: Some(now + Duration::hours(expires_in)),
            next_allowed_at: Some(now),
            consecutive_failures: failures,
        })
        .await
        .expect("host saved");
}

fn service(env: &TestEnv) -> RobotsCacheService {
    RobotsCacheService::new(env.store.clone(), env.config.robots.user_agent.clone())
}

#[tokio::test]
async fn the_cache_lists_in_host_order_with_status_expiry_and_pagination() {
    let (_fixture, env) = (start_fixture().await, TestEnv::new(test_config()).await);
    seed(
        &env,
        "alpha.example",
        Some("User-agent: *\nDisallow: /private\nCrawl-delay: 2\n"),
        24,
        0,
    )
    .await;
    seed(&env, "beta.example", None, -1, 3).await;
    seed(
        &env,
        "gamma.example",
        Some("User-agent: *\nAllow: /\n"),
        1,
        0,
    )
    .await;

    let page = service(&env)
        .cache(RobotsCacheInput {
            host: None,
            path: None,
            limit: Some(2),
            cursor: None,
            include_body: Some(true),
        })
        .await
        .expect("lists");
    assert_eq!(page.count, 2);
    assert!(page.truncated);
    assert_eq!(page.next_cursor.as_deref(), Some("beta.example"));

    let alpha = &page.hosts[0];
    assert_eq!(alpha.host, "alpha.example");
    assert!(!alpha.expired);
    assert!(alpha.body_present);
    assert!(alpha.body_chars > 0);
    assert_eq!(alpha.crawl_delay_s, Some(2.0));
    assert!(alpha.body_preview.as_deref().unwrap_or("").len() <= 200);
    assert_eq!(alpha.robots_status, Some(200));

    let beta = &page.hosts[1];
    assert!(beta.expired, "a past expiry is marked");
    assert!(!beta.body_present);
    assert_eq!(beta.consecutive_failures, 3);
    assert_eq!(beta.body_chars, 0);
    assert_eq!(beta.body_preview.as_deref(), Some(""));

    let next = service(&env)
        .cache(RobotsCacheInput {
            host: None,
            path: None,
            limit: Some(2),
            cursor: page.next_cursor.clone(),
            include_body: None,
        })
        .await
        .expect("second page");
    assert_eq!(next.count, 1);
    assert!(!next.truncated);
    assert_eq!(next.hosts[0].host, "gamma.example");
}

#[tokio::test]
async fn the_path_probe_reads_the_cached_body_and_reports_the_rule() {
    let (_fixture, env) = (start_fixture().await, TestEnv::new(test_config()).await);
    seed(
        &env,
        "example.test",
        Some("User-agent: *\nDisallow: /private\nAllow: /private/public\nCrawl-delay: 3\n"),
        24,
        0,
    )
    .await;

    let blocked = service(&env)
        .cache(RobotsCacheInput {
            host: Some("example.test".to_string()),
            path: Some("/private/page".to_string()),
            limit: None,
            cursor: None,
            include_body: None,
        })
        .await
        .expect("probe");
    let decision = blocked.decision.expect("decision");
    assert_eq!(decision.allowed, Some(false));
    assert_eq!(decision.matched_rule.as_deref(), Some("Disallow: /private"));
    assert_eq!(decision.crawl_delay_s, Some(3.0));
    assert_eq!(decision.source, "cache");

    let allowed = service(&env)
        .cache(RobotsCacheInput {
            host: Some("example.test".to_string()),
            path: Some("private/public/x".to_string()),
            limit: None,
            cursor: None,
            include_body: None,
        })
        .await
        .expect("probe");
    let decision = allowed.decision.expect("decision");
    assert_eq!(decision.allowed, Some(true));
    assert_eq!(
        decision.matched_rule.as_deref(),
        Some("Allow: /private/public")
    );
    assert_eq!(decision.path, "/private/public/x");
}

#[tokio::test]
async fn expired_cache_is_marked_and_still_parsed_and_missing_body_is_unavailable() {
    let (_fixture, env) = (start_fixture().await, TestEnv::new(test_config()).await);
    seed(
        &env,
        "expired.test",
        Some("User-agent: *\nDisallow: /x\n"),
        -1,
        0,
    )
    .await;
    seed(&env, "missing.test", None, 24, 1).await;

    let expired = service(&env)
        .cache(RobotsCacheInput {
            host: Some("expired.test".to_string()),
            path: Some("/x".to_string()),
            limit: None,
            cursor: None,
            include_body: None,
        })
        .await
        .expect("probe");
    let decision = expired.decision.expect("decision");
    assert_eq!(decision.source, "expired");
    assert_eq!(
        decision.allowed,
        Some(false),
        "the cached body is still parsed"
    );
    assert!(expired.hosts[0].expired);

    let missing = service(&env)
        .cache(RobotsCacheInput {
            host: Some("missing.test".to_string()),
            path: Some("/x".to_string()),
            limit: None,
            cursor: None,
            include_body: None,
        })
        .await
        .expect("probe");
    let decision = missing.decision.expect("decision");
    assert_eq!(decision.source, "unavailable");
    assert_eq!(decision.allowed, None, "never a fabricated allow");
    assert_eq!(decision.matched_rule, None);

    let unknown = service(&env)
        .cache(RobotsCacheInput {
            host: Some("unknown.test".to_string()),
            path: None,
            limit: None,
            cursor: None,
            include_body: None,
        })
        .await
        .expect("probe");
    assert_eq!(unknown.decision.expect("decision").source, "unavailable");
}

#[tokio::test]
async fn the_tool_makes_zero_network_requests() {
    let fixture = start_fixture().await;
    let env = TestEnv::new(test_config()).await;
    // Seed a host whose *real* robots.txt exists on the fixture: a fetching
    // implementation would contact it.
    seed(
        &env,
        &format!("127.0.0.1:{}", fixture.port),
        Some("User-agent: *\nDisallow: /private\n"),
        24,
        0,
    )
    .await;

    let payload = service(&env)
        .cache(RobotsCacheInput {
            host: Some(format!("127.0.0.1:{}", fixture.port)),
            path: Some("/private".to_string()),
            limit: None,
            cursor: None,
            include_body: None,
        })
        .await
        .expect("probe");
    assert_eq!(payload.decision.expect("decision").allowed, Some(false));
    assert!(
        fixture.log.entries().is_empty(),
        "robots_cache must not fetch: {:?}",
        fixture.log.entries()
    );
    assert_eq!(fixture.log.count_matching(""), 0);
}
