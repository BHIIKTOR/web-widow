//! Feed ingestion: one `fetch_feed` call returns the item list of an RSS/Atom/
//! JSON feed.
//!
//! This is a **discovery primitive**, not page extraction: no readability, no
//! Markdown, no store write. Acquisition reuses the page client (robots, SSRF
//! gate, host floor, body cap) and parsing is delegated to `feed-rs`; this
//! module only normalizes the result into the tool payload.

use serde::{Deserialize, Serialize};

/// Accept header for a feed read: the feed media types first, then the generic
/// XML/JSON types a misconfigured server may use.
pub const ACCEPT_FEED: &str = "application/rss+xml, application/atom+xml, application/feed+json, \
application/json;q=0.9, application/xml;q=0.9, text/xml;q=0.8";

pub mod parse;

/// `fetch_feed` input.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct FeedInput {
    /// Absolute http(s) URL of the feed.
    pub url: String,
    /// 1..=[feed].max_limit, default [feed].default_limit.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One feed item. Keys are stable: an absent value is `null`, never invented.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
pub struct FeedItem {
    /// Feed-provided id; when the feed has none, the library's stable hash of
    /// the first link (or a generated id).
    pub id: String,
    pub title: Option<String>,
    /// Absolute item URL resolved against the response's final URL, exactly as
    /// the feed states it (verbatim; the fetch target).
    pub url: Option<String>,
    /// Canonical identity of `url`: the same rule as crawl/monitor (tracking
    /// parameters dropped, remaining query sorted). `null` when `url` is absent
    /// or is not an http(s) URL. Unknown parameters such as `syn` remain part of
    /// this identity.
    #[serde(default)]
    pub canonical_url: Option<String>,
    /// RFC3339 UTC.
    pub published: Option<String>,
    pub author: Option<String>,
    /// Plain visible text, capped at `[feed].max_summary_chars`.
    pub summary: String,
}

/// `fetch_feed` output (§1).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
pub struct FeedPayload {
    pub url: String,
    pub final_url: String,
    pub content_type: String,
    /// `rss` | `atom` | `json`.
    pub feed_type: String,
    pub title: Option<String>,
    pub site_url: Option<String>,
    pub language: Option<String>,
    pub description: String,
    pub fetched_at: String,
    pub duration_ms: u64,
    pub items: Vec<FeedItem>,
    /// Parsed (and deduplicated) items before `limit`.
    pub item_count: usize,
    pub truncated: bool,
    /// `"limit"` > `"max_summary_chars"`; `null` when nothing was cut.
    pub limit_hit: Option<String>,
    /// Items whose summary was cut at the character cap.
    pub truncated_summaries: usize,
}
