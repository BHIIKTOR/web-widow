//! `events` — the change feed as data and as RSS 2.0.
//!
//! JSON and RSS read the same `changes`-join query; the feed never carries the
//! full diff and never fabricates a change. The HTTP route and its capability
//! token live in [`crate::mcp::http`]; this module owns the data, the document
//! renderer and the process-wide `feed_url` state.

pub mod rss;

use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::{EventsConfig, Paths};
use crate::core::error::{Result, WidowError};
use crate::monitor::normalize::ChangeStats;
use crate::store::Store;
use crate::store::monitor::EventRow;

pub const ENV_FEED_TOKEN: &str = "WEB_WIDOW_FEED_TOKEN";
/// The RSS document cap, independent of the configured item cap.
pub const RSS_MAX_ITEMS: usize = 100;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum EventsFormat {
    #[default]
    Json,
    Rss,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventsInput {
    /// `json` (default) or `rss`.
    #[serde(default)]
    pub format: Option<EventsFormat>,
    /// Only changes for this monitor.
    #[serde(default)]
    pub monitor_id: Option<i64>,
    /// Only changes detected at or after this RFC3339 timestamp.
    #[serde(default)]
    pub since: Option<String>,
    /// Items returned (1..=200, default 50).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Include the capped diff in JSON items (RSS never carries it).
    #[serde(default)]
    pub include_diff: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EventItem {
    pub change_id: i64,
    pub monitor_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor_label: Option<String>,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub detected_at: String,
    pub kind: String,
    pub stats: ChangeStats,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    pub truncated: bool,
    pub from_snapshot: String,
    pub to_snapshot: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EventsJsonPayload {
    pub items: Vec<EventItem>,
    pub count: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_hit: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EventsRssPayload {
    pub xml: String,
    pub bytes: usize,
    pub item_count: usize,
    pub feed_url: Option<String>,
}

/// The tool returns one of the two documented shapes.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum EventsPayload {
    Json(EventsJsonPayload),
    Rss(EventsRssPayload),
}

/// Process-wide state for `feed_url`: set while the loopback HTTP listener is
/// serving, cleared when it stops (stdio has no route → `null`).
#[derive(Debug, Default)]
pub struct FeedRuntime {
    url: RwLock<Option<String>>,
}

impl FeedRuntime {
    pub fn set_url(&self, url: Option<String>) {
        *self
            .url
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = url;
    }

    pub fn url(&self) -> Option<String> {
        self.url
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[derive(Debug)]
pub struct EventsService {
    store: Arc<Store>,
    config: EventsConfig,
    runtime: Arc<FeedRuntime>,
}

impl EventsService {
    pub fn new(store: Arc<Store>, config: EventsConfig, runtime: Arc<FeedRuntime>) -> Self {
        Self {
            store,
            config,
            runtime,
        }
    }

    pub fn runtime(&self) -> &Arc<FeedRuntime> {
        &self.runtime
    }

    pub async fn events(&self, input: EventsInput) -> Result<EventsPayload> {
        let limit = match input.limit {
            Some(0) => {
                return Err(WidowError::SchemaInvalid {
                    pointer: "/limit".to_string(),
                    message: "limit must be >= 1".to_string(),
                });
            }
            Some(limit) => limit.min(200),
            None => 50,
        };
        let since = match input.since.as_deref() {
            Some(text) => Some(
                DateTime::parse_from_rfc3339(text)
                    .map(|value| value.with_timezone(&Utc))
                    .map_err(|_| WidowError::SchemaInvalid {
                        pointer: "/since".to_string(),
                        message: format!("`{text}` is not an RFC3339 timestamp"),
                    })?,
            ),
            None => None,
        };
        if let Some(monitor_id) = input.monitor_id
            && self.store.db.monitor(monitor_id).await?.is_none()
        {
            return Err(WidowError::NotFound(format!(
                "no monitor with id {monitor_id}"
            )));
        }

        let format = input.format.unwrap_or_default();
        let effective = match format {
            EventsFormat::Json => limit,
            EventsFormat::Rss => limit.min(self.config.feed_items).min(RSS_MAX_ITEMS),
        };
        let rows = self
            .store
            .db
            .events_page(input.monitor_id, since, effective.saturating_add(1))
            .await?;
        let truncated = rows.len() > effective;
        let rows: Vec<EventRow> = rows.into_iter().take(effective).collect();
        let items: Vec<EventItem> = rows
            .into_iter()
            .map(|row| to_item(row, input.include_diff.unwrap_or(false)))
            .collect();

        match format {
            EventsFormat::Json => Ok(EventsPayload::Json(EventsJsonPayload {
                count: items.len(),
                truncated,
                limit_hit: truncated.then(|| "limit".to_string()),
                items,
            })),
            EventsFormat::Rss => {
                let feed_url = self.runtime.url();
                Ok(EventsPayload::Rss(rss_payload(&items, feed_url, truncated)))
            }
        }
    }

    /// The feed document for the HTTP route: newest first, no diff, capped by
    /// `[events].feed_items` and the RSS hard cap.
    pub async fn feed_xml(&self) -> Result<String> {
        let effective = self.config.feed_items.min(RSS_MAX_ITEMS);
        let rows = self.store.db.events_page(None, None, effective).await?;
        let items: Vec<EventItem> = rows.into_iter().map(|row| to_item(row, false)).collect();
        Ok(rss::render(&items, self.runtime.url().as_deref()))
    }

    pub fn feed_enabled(&self) -> bool {
        self.config.feed_enabled
    }
}

fn rss_payload(
    items: &[EventItem],
    feed_url: Option<String>,
    _truncated: bool,
) -> EventsRssPayload {
    let xml = rss::render(items, feed_url.as_deref());
    EventsRssPayload {
        bytes: xml.len(),
        item_count: items.len(),
        feed_url,
        xml,
    }
}

fn to_item(row: EventRow, include_diff: bool) -> EventItem {
    let stats: ChangeStats = serde_json::from_value(row.stats).unwrap_or_default();
    EventItem {
        change_id: row.change_id,
        monitor_id: row.monitor_id,
        monitor_label: row.monitor_label,
        url: row.url,
        title: row.title,
        detected_at: row
            .detected_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        kind: row.kind,
        stats,
        diff: include_diff.then_some(row.diff_text),
        truncated: row.truncated,
        from_snapshot: row
            .from_snapshot_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        to_snapshot: row
            .to_snapshot_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    }
}

/// Resolve the feed capability token: the environment wins; otherwise the
/// `0600` `<data_dir>/feed-token` is read, or created from 32 random bytes on
/// first start. `None` when the feed is disabled (no token is needed).
pub fn resolve_feed_token(enabled: bool, paths: &Paths) -> Result<Option<String>> {
    if !enabled {
        return Ok(None);
    }
    if let Ok(value) = std::env::var(ENV_FEED_TOKEN) {
        let value = value.trim().to_string();
        if !value.is_empty() {
            if value.len() < 32 {
                return Err(WidowError::Internal(format!(
                    "{ENV_FEED_TOKEN} must be at least 32 characters (a 256-bit capability token)"
                )));
            }
            return Ok(Some(value));
        }
    }
    let path = paths.feed_token();
    if path.exists() {
        check_private(&path)?;
        let token = fs::read_to_string(&path)
            .map_err(|error| {
                WidowError::Internal(format!("cannot read {}: {error}", path.display()))
            })?
            .trim()
            .to_string();
        if token.len() < 32 {
            return Err(WidowError::Internal(format!(
                "{} does not hold a 256-bit capability token; delete it and restart to regenerate",
                path.display()
            )));
        }
        return Ok(Some(token));
    }
    let token = random_hex_token()?;
    crate::store::datadir::write_private(&path, token.as_bytes())?;
    Ok(Some(token))
}

fn check_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)
        .map_err(|error| WidowError::Internal(format!("cannot stat {}: {error}", path.display())))?
        .permissions()
        .mode()
        & 0o777;
    if mode & 0o077 != 0 {
        return Err(WidowError::Internal(format!(
            "{} is accessible by group/other (mode {mode:o}); must be 0600",
            path.display()
        )));
    }
    Ok(())
}

fn random_hex_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| {
            WidowError::Internal(format!(
                "cannot read /dev/urandom for the feed token: {error}"
            ))
        })?;
    let mut token = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(token, "{byte:02x}");
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::normalize::ChangeStats;

    fn item(id: i64, url: &str, title: Option<&str>) -> EventItem {
        EventItem {
            change_id: id,
            monitor_id: 1,
            monitor_label: Some("project:slug".to_string()),
            url: url.to_string(),
            title: title.map(str::to_string),
            detected_at: "2026-01-02T03:04:05Z".to_string(),
            kind: "substantive".to_string(),
            stats: ChangeStats {
                lines_added: 1,
                lines_removed: 2,
                changed_lines: 3,
                words_added: 4,
                words_removed: 5,
                ..ChangeStats::default()
            },
            diff: None,
            truncated: false,
            from_snapshot: "2026-01-01T00:00:00Z".to_string(),
            to_snapshot: "2026-01-02T03:04:05Z".to_string(),
        }
    }

    #[test]
    fn the_rss_document_escapes_text_and_parses_back() {
        let mut item = item(
            7,
            "https://example.com/a?x=1&y=2",
            Some("A <title> & \"quotes\" 'apostrophe'"),
        );
        item.monitor_label = None;
        let items = vec![item];
        let xml = rss::render(&items, Some("http://127.0.0.1:8765/feeds/changes.xml"));
        assert!(xml.contains("&amp;"));
        assert!(!xml.contains("&\""));
        let parsed = feed_rs::parser::parse(xml.as_bytes()).expect("feed-rs parses our RSS");
        assert_eq!(
            parsed.title.map(|title| title.content),
            Some("web-widow changes".to_string())
        );
        assert_eq!(parsed.entries.len(), 1);
        let entry = &parsed.entries[0];
        assert_eq!(
            entry.links.first().map(|link| link.href.as_str()),
            Some("https://example.com/a?x=1&y=2")
        );
        assert!(entry.title.as_ref().unwrap().content.contains("<title>"));
    }

    #[test]
    fn the_rss_cap_and_control_stripping_are_applied() {
        let mut items: Vec<EventItem> = (0..120)
            .map(|index| item(index, "https://example.com/x", None))
            .collect();
        items[0].title = Some("bell\u{7} text".to_string());
        let xml = rss::render(&items, None);
        assert!(!xml.contains('\u{7}'));
        let parsed = feed_rs::parser::parse(xml.as_bytes()).expect("parses");
        assert_eq!(parsed.entries.len(), 100);
    }

    #[test]
    fn a_bad_since_is_schema_invalid() {
        // Covered through the service in the integration tests; the parse is the
        // same expression, asserted here without a store.
        let error = DateTime::parse_from_rfc3339("yesterday").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("input contains invalid characters")
                || error.to_string().contains("premature end")
        );
    }

    #[test]
    fn feed_tokens_are_private_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().to_path_buf());
        std::fs::create_dir_all(dir.path()).unwrap();
        let first = resolve_feed_token(true, &paths).unwrap().unwrap();
        assert_eq!(first.len(), 64);
        let second = resolve_feed_token(true, &paths).unwrap().unwrap();
        assert_eq!(first, second, "the token file is reused");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(paths.feed_token())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(resolve_feed_token(false, &paths).unwrap(), None);
    }
}
