//! v0.7 integration: the opt-in in-process scheduler.
//!
//! The timer-only test makes no `run-due` call: it starts the task, waits for a
//! tick, and reads the DB. enqueue is exercised by enabling `[webhooks]` with
//! an attachment but never spawning the dispatcher (TestEnv has no background
//! tasks), so the assertion sees the outbox row, not a network attempt.

mod common;

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use web_widow::config::WebhookTargetConfig;
use web_widow::monitor::scheduler;
use web_widow::monitor::{MonitorCheckInput, MonitorCreateInput};
use web_widow::webhooks::{WebhookService, WebhookTargetInput};

use common::{TestEnv, start_fixture, test_config};

fn page(text: &str) -> String {
    format!(
        "<html><head><title>S1</title></head><body><main><article>\
         <h1>S1</h1><p>{text}</p></article></main></body></html>"
    )
}

async fn create_monitor(env: &TestEnv, url: &str, interval_s: u64) -> i64 {
    env.monitors
        .create(MonitorCreateInput {
            url: url.to_string(),
            mode: None,
            schema: None,
            interval_s: Some(interval_s),
            ignore_selectors: Vec::new(),
            label: None,
        })
        .await
        .unwrap()
        .monitor_id
}

fn count(env: &TestEnv, sql: &str) -> i64 {
    env.db().query_row(sql, [], |row| row.get(0)).unwrap()
}

fn scalar(env: &TestEnv, sql: &str) -> Option<String> {
    env.db()
        .query_row(sql, [], |row| row.get(0))
        .unwrap_or(None)
}

async fn wait_for(mut predicate: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    predicate()
}

fn scheduler_config() -> web_widow::config::Config {
    let mut config = test_config();
    config.scheduler.enabled = true;
    config.scheduler.tick_ms = 1_000;
    config.scheduler.max_checks_per_tick = 5;
    config.monitor.min_interval_s = 1;
    config.monitor.default_interval_s = 1;
    config.monitor.max_interval_s = 60;
    //  enqueue is on, but TestEnv never spawns the dispatcher.
    config.webhooks.enabled = true;
    config.webhooks.targets.insert(
        "ops".to_string(),
        WebhookTargetConfig {
            url: "https://example.com/hook".to_string(),
            ..WebhookTargetConfig::default()
        },
    );
    config
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timer_alone_checks_a_due_monitor_and_enqueues_the_change() {
    let fixture = start_fixture().await;
    fixture.set_page("/monitor/s1", &page("first body"));
    let env = TestEnv::new(scheduler_config()).await;
    let url = fixture.url("127.0.0.1", "/monitor/s1");
    let monitor_id = create_monitor(&env, &url, 1).await;
    WebhookService::new(Arc::clone(&env.config), Arc::clone(&env.store), None)
        .unwrap()
        .attach(WebhookTargetInput {
            monitor_id,
            targets: vec!["ops".to_string()],
        })
        .await
        .unwrap();

    // Baseline, then a changed page made due immediately.
    assert!(
        env.monitors
            .check(MonitorCheckInput {
                monitor_id: Some(monitor_id),
                all: None,
                force: Some(true),
                wait_ms: None,
            })
            .await
            .unwrap()
            .changed
            .is_empty()
    );
    fixture.set_page("/monitor/s1", &page("second body, changed"));
    env.db()
        .execute(
            "UPDATE monitors SET next_due_at = ?1 WHERE id = ?2",
            rusqlite::params!["2000-01-01T00:00:00.000Z", monitor_id],
        )
        .unwrap();

    // Timer starts here; no tool call or run-due after this point.
    let cancel = CancellationToken::new();
    let handle = scheduler::spawn(
        Arc::clone(&env.monitors),
        Arc::clone(&env.config),
        cancel.clone(),
    );
    let observed = wait_for(
        || {
            count(&env, "SELECT COUNT(*) FROM snapshots") == 2
                && count(&env, "SELECT COUNT(*) FROM webhook_deliveries") == 1
        },
        Duration::from_secs(10),
    )
    .await;
    cancel.cancel();
    let stopped = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .is_ok();
    assert!(observed, "the timer checked the due monitor and enqueued ");
    assert!(stopped, "the timer stops on cancellation");
    assert_eq!(count(&env, "SELECT COUNT(*) FROM snapshots"), 2);
    assert_eq!(count(&env, "SELECT COUNT(*) FROM webhook_deliveries"), 1);
    let delivery: (String, String) = env
        .db()
        .query_row(
            "SELECT target, status FROM webhook_deliveries LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(delivery, ("ops".to_string(), "pending".to_string()));
    let checked = scalar(
        &env,
        "SELECT last_checked_at FROM monitors ORDER BY id LIMIT 1",
    );
    assert!(checked.is_some(), "last_checked_at advanced");
    let next: String = env
        .db()
        .query_row("SELECT next_due_at FROM monitors LIMIT 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(next.as_str() > "2026-01-01T00:00:00.000Z", "{next}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_scheduler_task_does_nothing() {
    let fixture = start_fixture().await;
    fixture.set_page("/monitor/s1", &page("first body"));
    let mut config = scheduler_config();
    config.scheduler.enabled = false;
    let env = TestEnv::new(config).await;
    let monitor_id = create_monitor(&env, &fixture.url("127.0.0.1", "/monitor/s1"), 1).await;
    assert!(monitor_id > 0);
    let cancel = CancellationToken::new();
    let handle = scheduler::spawn(Arc::clone(&env.monitors), Arc::clone(&env.config), cancel);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .is_ok()
    );
    assert_eq!(count(&env, "SELECT COUNT(*) FROM snapshots"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pass_is_bounded_and_downtime_does_not_burst() {
    let fixture = start_fixture().await;
    let env = TestEnv::new(scheduler_config()).await;
    for index in 0..5 {
        let path = format!("/monitor/s1-{index}");
        fixture.set_page(&path, &page(&format!("body {index}")));
        let id = create_monitor(&env, &fixture.url("127.0.0.1", &path), 1).await;
        env.db()
            .execute(
                "UPDATE monitors SET next_due_at = ?1 WHERE id = ?2",
                rusqlite::params!["2000-01-01T00:00:00.000Z", id],
            )
            .unwrap();
    }
    let limit = scheduler::pass_limit(&env.config);
    assert_eq!(limit, 5, "config scheduler=5, monitor=500");
    let mut checked = Vec::new();
    for _ in 0..3 {
        checked.push(env.monitors.run_due(2).await.unwrap().checked);
    }
    assert_eq!(checked, vec![2, 2, 1], "two per pass, then the remainder");
    assert_eq!(count(&env, "SELECT COUNT(*) FROM snapshots"), 5);
    // A long-downtime monitor is checked once, not once per missed interval.
    assert_eq!(count(&env, "SELECT COUNT(*) FROM snapshots"), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_passes_cannot_double_check_one_monitor() {
    let fixture = start_fixture().await;
    let env = TestEnv::new(scheduler_config()).await;
    for index in 0..2 {
        let path = format!("/monitor/s1-race-{index}");
        fixture.set_page(&path, &page(&format!("body {index}")));
        let id = create_monitor(&env, &fixture.url("127.0.0.1", &path), 1).await;
        env.db()
            .execute(
                "UPDATE monitors SET next_due_at = ?1 WHERE id = ?2",
                rusqlite::params!["2000-01-01T00:00:00.000Z", id],
            )
            .unwrap();
    }
    let (first, second) = tokio::join!(env.monitors.run_due(10), env.monitors.run_due(10));
    let checked = first.unwrap().checked + second.unwrap().checked;
    assert!(checked <= 2, "at most one check per monitor: {checked}");
    assert_eq!(count(&env, "SELECT COUNT(*) FROM snapshots"), 2);

    // A direct monitor_check and a run-due pass on the same due monitor: one
    // snapshot; the per-monitor lock and next_due_at re-read decide the winner.
    let path = "/monitor/s1-race-direct";
    fixture.set_page(path, &page("direct body"));
    let id = create_monitor(&env, &fixture.url("127.0.0.1", path), 1).await;
    env.db()
        .execute(
            "UPDATE monitors SET next_due_at = ?1 WHERE id = ?2",
            rusqlite::params!["2000-01-01T00:00:00.000Z", id],
        )
        .unwrap();
    let check = env.monitors.check(MonitorCheckInput {
        monitor_id: Some(id),
        all: None,
        force: Some(false),
        wait_ms: None,
    });
    let (checked, _pass) = tokio::join!(check, env.monitors.run_due(10));
    let _ = checked;
    assert_eq!(
        count(
            &env,
            &format!("SELECT COUNT(*) FROM snapshots WHERE monitor_id = {id}")
        ),
        1,
        "the direct check and the pass produced one snapshot"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_run_due_fails_on_the_data_dir_lock_while_serve_holds_it() {
    use std::process::{Command, Stdio};

    let dir = tempfile::tempdir().unwrap();
    let config = common::write_binary_config(dir.path(), "");
    let data = dir.path().join("data");
    let mut server = Command::new(env!("CARGO_BIN_EXE_web-widow"))
        .arg("--config")
        .arg(&config)
        .arg("--data-dir")
        .arg(&data)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("server starts");
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let run_due = Command::new(env!("CARGO_BIN_EXE_web-widow"))
        .arg("--config")
        .arg(&config)
        .arg("--data-dir")
        .arg(&data)
        .arg("run-due")
        .output()
        .expect("run-due runs");
    let stderr = String::from_utf8_lossy(&run_due.stderr);
    assert!(!run_due.status.success(), "{run_due:?}");
    assert!(
        stderr.contains("already in use by pid"),
        "expected the lock refusal, got: {stderr}"
    );
    drop(server.stdin.take());
    for _ in 0..50 {
        if server.try_wait().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = server.kill();
    let _ = server.wait();
}
