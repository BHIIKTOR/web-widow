//! `search`: server-side DuckDuckGo discovery through the shared fetch gate
//! (§1).
//!
//! Discovery only: results are URLs and snippets, never fetched content. The
//! module owns the request shape and the payload; the pipeline owns acquisition
//! so robots/SSRF/politeness are the same code as `fetch_page`.

pub mod parse;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::core::error::{Result, WidowError};

/// One SERP page holds ten results, so ten is the hard cap (no pagination).
pub const MAX_LIMIT: usize = 10;

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    #[default]
    Any,
    Day,
    Week,
    Month,
    Year,
}

impl Freshness {
    /// DuckDuckGo's `df` parameter; `any` sends none.
    pub fn df(self) -> Option<&'static str> {
        match self {
            Self::Any => None,
            Self::Day => Some("d"),
            Self::Week => Some("w"),
            Self::Month => Some("m"),
            Self::Year => Some("y"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Year => "year",
        }
    }
}

/// `search` input (§1.1). Every field except `query` has a
/// documented default; there is deliberately no capability-widening argument.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SearchInput {
    /// Search terms. Trimmed; must be non-empty and within `[search].max_query_chars`.
    pub query: String,
    /// Optional bare host or http(s) URL. Applied as `site:<host>` in the engine
    /// query and as a host filter on the returned results.
    #[serde(default)]
    pub site: Option<String>,
    /// `any` (default) | `day` | `week` | `month` | `year`.
    #[serde(default)]
    pub freshness: Option<Freshness>,
    /// 1..=10, default `[search].default_limit`.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub engine: String,
}

/// `search` output (§1.4).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
pub struct SearchPayload {
    pub query: String,
    pub engine: String,
    pub final_url: String,
    pub site: Option<String>,
    pub freshness: String,
    pub fetched_at: String,
    pub duration_ms: u64,
    pub results: Vec<SearchResult>,
    /// Organic results parsed (after unwrap and site filtering) before `limit`.
    pub result_count: usize,
    pub truncated: bool,
    pub limit_hit: Option<String>,
}

/// Validate and trim the query. Over-length and empty are `schema_invalid`.
pub fn validate_query(query: &str, max_chars: usize) -> Result<String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(WidowError::SchemaInvalid {
            pointer: "/query".to_string(),
            message: "query must not be empty".to_string(),
        });
    }
    if trimmed.chars().count() > max_chars {
        return Err(WidowError::SchemaInvalid {
            pointer: "/query".to_string(),
            message: format!("query must be at most {max_chars} characters"),
        });
    }
    Ok(trimmed.to_string())
}

/// `limit` is 1..=10; a larger value is rejected, never silently clamped.
pub fn validate_limit(limit: usize) -> Result<()> {
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(WidowError::SchemaInvalid {
            pointer: "/limit".to_string(),
            message: format!("limit must be 1..={MAX_LIMIT}"),
        });
    }
    Ok(())
}

/// Build the engine request URL: `q` (with the optional `site:` qualifier) and
/// `df` only. No `s`/pagination parameter is ever added.
pub fn request_url(
    endpoint: &str,
    query: &str,
    site: Option<&str>,
    freshness: Freshness,
) -> Result<Url> {
    let mut url = Url::parse(endpoint).map_err(|error| {
        WidowError::InvalidUrl(format!(
            "search endpoint `{endpoint}` is not a URL: {error}"
        ))
    })?;
    let engine_query = match site {
        Some(site) => format!("{query} site:{site}"),
        None => query.to_string(),
    };
    {
        let mut pairs = url.query_pairs_mut();
        pairs.clear();
        pairs.append_pair("q", &engine_query);
        if let Some(df) = freshness.df() {
            pairs.append_pair("df", df);
        }
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_validation_rejects_empty_and_over_long() {
        assert_eq!(validate_query("  rust  ", 512).unwrap(), "rust");
        assert!(validate_query("   ", 512).is_err());
        assert!(validate_query(&"x".repeat(513), 512).is_err());
        assert!(validate_query(&"x".repeat(512), 512).is_ok());
    }

    #[test]
    fn limit_is_bounded_without_clamping() {
        assert!(validate_limit(1).is_ok());
        assert!(validate_limit(10).is_ok());
        assert!(validate_limit(0).is_err());
        assert!(validate_limit(11).is_err());
    }

    #[test]
    fn request_url_carries_only_q_and_df() {
        let url = request_url(
            "https://html.duckduckgo.com/html/",
            "rust async",
            Some("tokio.rs"),
            Freshness::Month,
        )
        .unwrap();
        let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        assert_eq!(
            pairs,
            vec![
                ("q".to_string(), "rust async site:tokio.rs".to_string()),
                ("df".to_string(), "m".to_string()),
            ]
        );
        assert_eq!(url.path(), "/html/");
    }

    #[test]
    fn request_url_omits_df_for_any_freshness() {
        let url = request_url(
            "https://lite.duckduckgo.com/lite/",
            "rust",
            None,
            Freshness::Any,
        )
        .unwrap();
        let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs, vec![("q".to_string(), "rust".to_string())]);
    }

    #[test]
    fn freshness_maps_to_the_engine_values() {
        assert_eq!(Freshness::Any.df(), None);
        assert_eq!(Freshness::Day.df(), Some("d"));
        assert_eq!(Freshness::Week.df(), Some("w"));
        assert_eq!(Freshness::Month.df(), Some("m"));
        assert_eq!(Freshness::Year.df(), Some("y"));
    }
}
