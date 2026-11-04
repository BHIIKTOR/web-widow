//! Sitemap discovery (`sitemap` and `crawl_start.sitemap_urls`).
//!
//! Streaming parse only: `quick-xml` for `<urlset>`/`<sitemapindex>` and a line
//! reader for the plain-text format. A document type declaration is skipped and
//! never resolved, so no entity or external access is possible. A gzip body is
//! inflated with a hard `4 × max_bytes` ceiling before parsing.
//!
//! The parser is pure: it takes bytes and limits and returns a bounded document
//! model or a typed error. Fetching (SSRF, robots, politeness, caps) stays in
//! [`crate::pipeline`], so the tool and the crawl seed path cannot drift.

pub mod parse;

use std::io::Read;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::core::error::{Result, WidowError};
use crate::core::url::PageUrl;
use crate::fetch::charset;

/// The accept header for a sitemap request: XML first, gzip tolerated, HTML
/// last (an HTML error page fails as a non-sitemap root).
pub const ACCEPT_SITEMAP: &str =
    "application/xml,text/xml,text/plain;q=0.9,application/x-gzip;q=0.8,*/*;q=0.1";

/// Ceilings for one sitemap document. `max_bytes` bounds the fetched body and,
/// at 4×, the decompressed size; the entry caps bound the returned model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SitemapLimits {
    pub max_bytes: u64,
    pub max_urls: usize,
    pub max_children: usize,
}

impl SitemapLimits {
    /// The decompression ceiling: a gzip bomb hits `too_large` here.
    pub fn max_decompressed_bytes(self) -> u64 {
        self.max_bytes.saturating_mul(4)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SitemapKind {
    Urlset,
    Index,
    Text,
}

impl SitemapKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Urlset => "urlset",
            Self::Index => "index",
            Self::Text => "text",
        }
    }
}

/// One `<url>` entry (`<sitemap>` entries become [`ChildSitemap`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SitemapUrl {
    pub loc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lastmod: Option<String>,
}

/// One `<sitemap>` entry of a `<sitemapindex>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSitemap {
    pub loc: String,
}

/// A parsed, bounded sitemap document. `urls` and `children` are mutually
/// exclusive by document type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedSitemap {
    pub kind: Option<SitemapKind>,
    pub urls: Vec<SitemapUrl>,
    pub children: Vec<ChildSitemap>,
    /// Entries whose `loc` was absent or did not resolve to http(s).
    pub skipped_scheme: usize,
    /// Present `lastmod` values that are neither RFC3339 nor `YYYY-MM-DD`.
    pub invalid_lastmod: usize,
    pub truncated: bool,
    pub limit_hit: Option<&'static str>,
}

/// `sitemap` tool input. Discovery only: nothing is stored.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SitemapInput {
    /// Absolute http(s) URL of the sitemap.
    pub url: String,
    /// Maximum URL entries returned (1..=`[sitemap].max_urls`).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Include `lastmod` in each entry (data only; parsed but never scheduled).
    #[serde(default)]
    pub include_lastmod: Option<bool>,
    /// Fetch up to `[sitemap].max_children` child sitemaps of an index.
    #[serde(default)]
    pub follow_index: Option<bool>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SitemapChildReport {
    pub loc: String,
    pub url_count: usize,
    /// Typed error code when this child could not be fetched or parsed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SitemapPayload {
    pub url: String,
    pub final_url: String,
    pub status: u16,
    pub sitemap_type: SitemapKind,
    pub urls: Vec<SitemapUrl>,
    pub url_count: usize,
    pub skipped_scheme: usize,
    pub invalid_lastmod: usize,
    /// One row per child fetched (`follow_index`); errors are never dropped.
    pub sitemaps: Vec<SitemapChildReport>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_hit: Option<String>,
    pub fetched_at: String,
}

/// Decode a fetched sitemap body: bounded gzip inflate (magic bytes, not the
/// file name) then charset decode. `content_type` only supplies a charset hint.
pub fn decode_body(
    body: &[u8],
    content_type: Option<&str>,
    limits: SitemapLimits,
) -> Result<String> {
    let decoded = if is_gzip(body) {
        let inflated = inflate(body, limits.max_decompressed_bytes())?;
        charset::decode(&inflated, content_type)
    } else {
        charset::decode(body, content_type)
    };
    Ok(decoded.text)
}

fn is_gzip(body: &[u8]) -> bool {
    body.len() >= 2 && body[0] == 0x1f && body[1] == 0x8b
}

/// Inflate a gzip member (or members) with a hard output ceiling.
fn inflate(body: &[u8], limit: u64) -> Result<Vec<u8>> {
    let mut decoder = flate2::read::MultiGzDecoder::new(body);
    let mut out = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let read = decoder.read(&mut chunk).map_err(|error| {
            WidowError::Extraction(format!("gzip decompression failed: {error}"))
        })?;
        if read == 0 {
            break;
        }
        if out.len() as u64 + read as u64 > limit {
            return Err(WidowError::TooLarge { limit });
        }
        out.extend_from_slice(&chunk[..read]);
    }
    Ok(out)
}

/// Parse a fetched sitemap body. Malformed XML is `extraction_failed` with the
/// parser line/column; a gzip or entry-cap breach is the typed error/cap.
pub fn parse_body(
    body: &[u8],
    content_type: Option<&str>,
    limits: SitemapLimits,
) -> Result<ParsedSitemap> {
    let text = decode_body(body, content_type, limits)?;
    parse::parse_text(&text, limits)
}

/// Validate one `loc` as an http(s) URL. The returned string is the trimmed
/// original (what the document said), never a canonical rewrite.
pub fn valid_loc(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    PageUrl::parse(trimmed).ok()?;
    Some(trimmed.to_string())
}

/// `lastmod` is data only: RFC3339 or `YYYY-MM-DD` are accepted, anything else
/// is `None` and counted by the parser. The value is not used for scheduling.
pub fn normalize_lastmod(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if chrono::DateTime::parse_from_rfc3339(trimmed).is_ok()
        || NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").is_ok()
    {
        return Some(trimmed.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: SitemapLimits = SitemapLimits {
        max_bytes: 1024 * 1024,
        max_urls: 10,
        max_children: 2,
    };

    fn urlset(locs: &[&str]) -> String {
        let mut body = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n",
        );
        for loc in locs {
            body.push_str(&format!("  <url><loc>{loc}</loc></url>\n"));
        }
        body.push_str("</urlset>\n");
        body
    }

    #[test]
    fn lastmod_accepts_both_documented_forms_and_rejects_the_rest() {
        assert_eq!(
            normalize_lastmod("2026-01-02T03:04:05Z").as_deref(),
            Some("2026-01-02T03:04:05Z")
        );
        assert_eq!(
            normalize_lastmod("2026-01-02T03:04:05+02:00").as_deref(),
            Some("2026-01-02T03:04:05+02:00")
        );
        assert_eq!(
            normalize_lastmod("2026-01-02").as_deref(),
            Some("2026-01-02")
        );
        for invalid in ["", "  ", "yesterday", "2026-13-40", "01/02/2026"] {
            assert_eq!(normalize_lastmod(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn a_loc_must_resolve_to_http_or_https() {
        assert!(valid_loc("https://example.com/a").is_some());
        assert!(valid_loc(" http://example.com/a ").is_some());
        for invalid in [
            "ftp://example.com/a",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "/relative",
            "",
        ] {
            assert_eq!(valid_loc(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn gzip_is_detected_by_magic_bytes_not_the_name() {
        assert!(is_gzip(&[0x1f, 0x8b, 0x08]));
        assert!(!is_gzip(b"<urlset></urlset>"));
        assert!(!is_gzip(&[]));
    }

    #[test]
    fn gzip_body_parses_after_a_bounded_inflate() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        let body = urlset(&["https://example.com/a", "https://example.com/b"]);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(body.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();

        let parsed = parse_body(&compressed, Some("application/octet-stream"), LIMITS).unwrap();
        assert_eq!(parsed.kind, Some(SitemapKind::Urlset));
        assert_eq!(parsed.urls.len(), 2);
        assert_eq!(parsed.urls[0].loc, "https://example.com/a");
    }

    #[test]
    fn a_gzip_bomb_hits_the_decompression_cap() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        // 2 MiB of one byte, deflated; the 1 MiB cap allows 4 MiB, so a larger
        // body is required: repeat until the inflated size passes 4 MiB.
        let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
        let block = vec![b'A'; 64 * 1024];
        for _ in 0..96 {
            encoder.write_all(&block).unwrap();
        }
        let compressed = encoder.finish().unwrap();
        let error = parse_body(&compressed, None, LIMITS).unwrap_err();
        assert_eq!(error.code(), crate::core::error::ErrorCode::TooLarge);
    }
}
