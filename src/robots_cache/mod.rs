//! `robots_cache` — the read-only robots cache surface.
//!
//! This module never fetches: it reads the `hosts` cache and parses the stored
//! body with the same `texting_robots` parser enforcement uses. There is no
//! `HttpClient` in the service, so a network request cannot happen by
//! construction; the integration test also asserts a fixture server sees zero
//! requests.

use std::sync::Arc;

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use texting_robots::Robot;

use crate::core::error::{Result, WidowError};
use crate::store::Store;
use crate::store::db::HostRow;

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;
const BODY_PREVIEW_CHARS: usize = 200;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RobotsCacheInput {
    /// Restrict the host list and probe this host (bare host or host:port).
    #[serde(default)]
    pub host: Option<String>,
    /// Path to probe with the cached rules (default `/`).
    #[serde(default)]
    pub path: Option<String>,
    /// Hosts per page (1..=200, default 50).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Opaque cursor from the previous page (the last host).
    #[serde(default)]
    pub cursor: Option<String>,
    /// Include a 200-character body preview (never the full body).
    #[serde(default)]
    pub include_body: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RobotsCacheHost {
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub robots_fetched_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub robots_status: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub expired: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_allowed_at: Option<String>,
    pub consecutive_failures: i64,
    /// The cached file's `Crawl-delay`, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crawl_delay_s: Option<f32>,
    pub body_chars: usize,
    pub body_present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_preview: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RobotsCacheDecision {
    pub host: String,
    pub path: String,
    /// `null` when there is no usable cached body (never a fabricated allow).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crawl_delay_s: Option<f32>,
    /// `cache`, `expired` or `unavailable`.
    pub source: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RobotsCachePayload {
    pub hosts: Vec<RobotsCacheHost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<RobotsCacheDecision>,
    pub count: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug)]
pub struct RobotsCacheService {
    store: Arc<Store>,
    user_agent: String,
}

impl RobotsCacheService {
    pub fn new(store: Arc<Store>, user_agent: String) -> Self {
        Self { store, user_agent }
    }

    pub async fn cache(&self, input: RobotsCacheInput) -> Result<RobotsCachePayload> {
        let limit = match input.limit {
            Some(0) => {
                return Err(WidowError::SchemaInvalid {
                    pointer: "/limit".to_string(),
                    message: "limit must be >= 1".to_string(),
                });
            }
            Some(limit) => limit.min(MAX_LIMIT),
            None => DEFAULT_LIMIT,
        };
        let cursor = input.cursor.clone();
        let rows = self
            .store
            .db
            .hosts_page(cursor.clone(), limit.saturating_add(1))
            .await?;
        let truncated = rows.len() > limit;
        let page: Vec<HostRow> = rows.into_iter().take(limit).collect();
        let next_cursor = if truncated {
            page.last().map(|row| row.host.clone())
        } else {
            None
        };

        let now = Utc::now();
        let include_body = input.include_body.unwrap_or(false);
        let hosts: Vec<RobotsCacheHost> = page
            .iter()
            .map(|row| describe_host(row, now, &self.user_agent, include_body))
            .collect();

        let decision = match input.host.as_deref() {
            Some(host) => Some(
                self.decide(host, input.path.as_deref().unwrap_or("/"))
                    .await?,
            ),
            None => None,
        };
        Ok(RobotsCachePayload {
            count: hosts.len(),
            hosts,
            decision,
            truncated,
            next_cursor,
        })
    }

    async fn decide(&self, host: &str, path: &str) -> Result<RobotsCacheDecision> {
        let path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        let Some(row) = self.store.db.host(host).await? else {
            return Ok(RobotsCacheDecision {
                host: host.to_string(),
                path,
                allowed: None,
                matched_rule: None,
                crawl_delay_s: None,
                source: "unavailable".to_string(),
            });
        };
        let Some(body) = row.robots_body.as_deref() else {
            return Ok(RobotsCacheDecision {
                host: row.host.clone(),
                path,
                allowed: None,
                matched_rule: None,
                crawl_delay_s: None,
                source: "unavailable".to_string(),
            });
        };
        let robot = match Robot::new(&self.user_agent, body) {
            Ok(robot) => robot,
            Err(_) => {
                return Ok(RobotsCacheDecision {
                    host: row.host.clone(),
                    path,
                    allowed: None,
                    matched_rule: None,
                    crawl_delay_s: None,
                    source: "unavailable".to_string(),
                });
            }
        };
        let expired = row
            .robots_expires_at
            .is_none_or(|expires| Utc::now() >= expires);
        let url = format!("http://{}{path}", row.host);
        Ok(RobotsCacheDecision {
            host: row.host.clone(),
            path: path.clone(),
            allowed: Some(robot.allowed(&url)),
            matched_rule: matched_rule(body, &self.user_agent, &path),
            crawl_delay_s: robot.delay.filter(|delay| *delay > 0.0),
            source: if expired { "expired" } else { "cache" }.to_string(),
        })
    }
}

fn describe_host(
    row: &HostRow,
    now: chrono::DateTime<Utc>,
    user_agent: &str,
    include_body: bool,
) -> RobotsCacheHost {
    let body_text = row
        .robots_body
        .as_deref()
        .map(|body| String::from_utf8_lossy(body).to_string());
    let body_chars = body_text
        .as_deref()
        .map(|text| text.chars().count())
        .unwrap_or(0);
    let crawl_delay_s = row
        .robots_body
        .as_deref()
        .and_then(|body| Robot::new(user_agent, body).ok())
        .and_then(|robot| robot.delay)
        .filter(|delay| *delay > 0.0);
    RobotsCacheHost {
        host: row.host.clone(),
        robots_fetched_at: row.robots_fetched_at.map(rfc3339),
        robots_status: row.robots_status,
        expires_at: row.robots_expires_at.map(rfc3339),
        expired: row.robots_expires_at.is_none_or(|expires| now >= expires),
        next_allowed_at: row.next_allowed_at.map(rfc3339),
        consecutive_failures: row.consecutive_failures,
        crawl_delay_s,
        body_chars,
        body_present: row.robots_body.is_some(),
        body_preview: include_body.then(|| {
            body_text
                .unwrap_or_default()
                .chars()
                .take(BODY_PREVIEW_CHARS)
                .collect()
        }),
    }
}

fn rfc3339(value: chrono::DateTime<Utc>) -> String {
    value.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The decisive `Allow:`/`Disallow:` line for `path`, as documentation of the
/// decision. RFC 9309 semantics: the longest matching prefix wins, `Allow`
/// wins a tie, our exact product token wins over `*`, and no match means the
/// default allow. The *decision* itself still comes from `texting_robots`
/// (which matches a User-agent line by exact, case-insensitive equality, as
/// this helper does); the rule is explanatory only and never changes it.
pub fn matched_rule(body: &[u8], user_agent: &str, path: &str) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let agent = user_agent.to_ascii_lowercase();
    type Group = (Vec<String>, Vec<(bool, String)>);
    let mut groups: Vec<Group> = Vec::new();
    let mut current: Option<Group> = None;

    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            "user-agent" => {
                let value = value.to_ascii_lowercase();
                if current.as_ref().is_some_and(|(_, rules)| !rules.is_empty()) {
                    if let Some(group) = current.take() {
                        groups.push(group);
                    }
                    current = Some((vec![value], Vec::new()));
                } else if let Some((agents, _)) = current.as_mut() {
                    agents.push(value);
                } else {
                    current = Some((vec![value], Vec::new()));
                }
            }
            "allow" | "disallow" => {
                if let Some((_, rules)) = current.as_mut() {
                    rules.push((key == "allow", value.to_string()));
                }
            }
            _ => {}
        }
    }
    if let Some(group) = current {
        groups.push(group);
    }

    let ours = groups.iter().any(|(agents, _)| agents.contains(&agent));
    let mut best: Option<(usize, bool, String)> = None;
    for (agents, rules) in &groups {
        let selected = if ours {
            agents.contains(&agent)
        } else {
            agents.iter().any(|candidate| candidate == "*")
        };
        if !selected {
            continue;
        }
        for (allow, pattern) in rules {
            if pattern.is_empty() || !prefix_matches(pattern, path) {
                continue;
            }
            let length = pattern.len();
            let better = match &best {
                None => true,
                Some((best_len, best_allow, _)) => {
                    length > *best_len || (length == *best_len && *allow && !*best_allow)
                }
            };
            if better {
                let key = if *allow { "Allow" } else { "Disallow" };
                best = Some((length, *allow, format!("{key}: {pattern}")));
            }
        }
    }
    best.map(|(_, _, line)| line)
}

/// RFC 9309 prefix match with `*` and a trailing `$` anchor.
fn prefix_matches(pattern: &str, path: &str) -> bool {
    if pattern == "/" {
        return true;
    }
    let anchored = pattern.strip_suffix('$');
    let pattern = anchored.unwrap_or(pattern);
    if !pattern.contains('*') {
        return match anchored {
            Some(pattern) => path == pattern,
            None => path.starts_with(pattern),
        };
    }
    let mut remainder = path;
    for (index, part) in pattern.split('*').enumerate() {
        if part.is_empty() {
            continue;
        }
        if index == 0 {
            if let Some(stripped) = remainder.strip_prefix(part) {
                remainder = stripped;
                continue;
            }
            return false;
        }
        match remainder.find(part) {
            Some(position) => remainder = &remainder[position + part.len()..],
            None => return false,
        }
    }
    match anchored {
        Some(_) => remainder.is_empty(),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &[u8] = b"User-agent: *\nDisallow: /private\nAllow: /private/public\n";

    #[test]
    fn matched_rule_uses_the_longest_prefix_and_ties_allow() {
        assert_eq!(
            matched_rule(BODY, "web-widow", "/private/x").as_deref(),
            Some("Disallow: /private")
        );
        assert_eq!(
            matched_rule(BODY, "web-widow", "/private/public/x").as_deref(),
            Some("Allow: /private/public")
        );
        assert_eq!(matched_rule(BODY, "web-widow", "/open"), None);
    }

    #[test]
    fn matched_rule_prefers_our_agent_group() {
        let body = b"User-agent: *\nDisallow: /\n\nUser-agent: web-widow\nAllow: /\n";
        assert_eq!(
            matched_rule(body, "web-widow", "/anything").as_deref(),
            Some("Allow: /")
        );
    }

    #[test]
    fn wildcard_and_anchor_patterns_match() {
        assert!(prefix_matches("/a/*/c", "/a/b/c"));
        assert!(!prefix_matches("/a/*/c", "/a/b/d"));
        assert!(prefix_matches("/*.pdf$", "/docs/report.pdf"));
        assert!(!prefix_matches("/*.pdf$", "/docs/report.pdf?x=1"));
        assert!(prefix_matches("/", "/anything"));
    }
}
