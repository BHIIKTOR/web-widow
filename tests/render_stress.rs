//! B03 B3-5 — 200 sequential renders of a ~200 KiB page, reporting RSS for the
//! Chrome process tree. Ignored by default because it is a multi-minute memory
//! probe, not an ordinary regression test; the evidence run executes it with
//! `--ignored`.
//!
//! The band is stated in advance: the average RSS at renders 190–200 must not
//! exceed the average at renders 20–30 by more than 15%.

#![cfg(feature = "render")]

mod common;

use std::collections::HashMap;
use std::time::Duration;

use url::Url;

fn process_tree_rss_kb(root: u32) -> u64 {
    let mut parents: HashMap<u32, u32> = HashMap::new();
    let mut rss: HashMap<u32, u64> = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let rest = &stat[close + 1..];
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let Some(ppid) = fields.get(1).and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        parents.insert(pid, ppid);
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb = rest
                    .trim()
                    .trim_end_matches(" kB")
                    .trim()
                    .parse::<u64>()
                    .unwrap_or(0);
                rss.insert(pid, kb);
            }
        }
    }
    let mut total = 0;
    for pid in parents.keys().copied().collect::<Vec<_>>() {
        let mut cursor = pid;
        let mut steps = 0;
        while steps < 64 {
            if cursor == root {
                total += rss.get(&pid).copied().unwrap_or(0);
                break;
            }
            match parents.get(&cursor) {
                Some(parent) if *parent != cursor => cursor = *parent,
                _ => break,
            }
            steps += 1;
        }
    }
    total + rss.get(&root).copied().unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "200-render RSS stress; run explicitly"]
async fn two_hundred_renders_do_not_grow_browser_rss_beyond_the_band() {
    let Some(chrome) =
        common::skip_without_chrome("two_hundred_renders_do_not_grow_browser_rss_beyond_the_band")
    else {
        return;
    };
    let fixture = common::start_fixture().await;
    let env = common::TestEnv::new_rendering(common::rendering_test_config(&chrome)).await;
    let url = Url::parse(&fixture.url("127.0.0.1", "/medium")).unwrap();

    // First render launches the browser.
    let first = env.renderer.render(&url).await.expect("first render");
    assert!(first.html.contains("Medium"));

    let renders: usize = std::env::var("WEB_WIDOW_STRESS_RENDERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200);
    let mut samples: Vec<(usize, u64)> = Vec::new();
    for render in 1..=renders {
        env.renderer
            .render(&url)
            .await
            .unwrap_or_else(|error| panic!("render {render} failed: {error}"));
        if render % 10 == 0 {
            let pid = env
                .renderer
                .browser_pid()
                .expect("browser pid during the stress run");
            samples.push((render, process_tree_rss_kb(pid)));
        }
    }

    let early: Vec<u64> = samples
        .iter()
        .filter(|(render, _)| (20..=30).contains(render))
        .map(|(_, rss)| *rss)
        .collect();
    let late: Vec<u64> = samples
        .iter()
        .filter(|(render, _)| (190..=200).contains(render))
        .map(|(_, rss)| *rss)
        .collect();
    let average = |values: &[u64]| {
        if values.is_empty() {
            0
        } else {
            values.iter().sum::<u64>() / values.len() as u64
        }
    };
    let early_avg = average(&early);
    let late_avg = average(&late);
    let peak = samples.iter().map(|(_, rss)| *rss).max().unwrap_or(0);
    for (render, rss) in &samples {
        println!("RSS render={render} tree_rss_kb={rss}");
    }
    println!(
        "RSS_SUMMARY renders={renders} page_kib=200 early_avg_kb={early_avg} late_avg_kb={late_avg} peak_kb={peak} growth_pct={:.1}",
        if early_avg == 0 {
            0.0
        } else {
            (late_avg as f64 / early_avg as f64 - 1.0) * 100.0
        }
    );

    assert!(early_avg > 0, "could not read browser RSS");
    assert!(
        late_avg <= early_avg * 115 / 100,
        "late average RSS {late_avg} kB exceeded the +15% band over early {early_avg} kB"
    );

    env.renderer.shutdown().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
}
