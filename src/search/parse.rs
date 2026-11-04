//! Pure parsers for the two DuckDuckGo result pages (§1.3).
//!
//! No I/O lives here: the pipeline hands in a decoded body and the engine name.
//! The selectors mirror the captured markup; a page that does not match is an
//! `extraction_failed` (never a silent empty result).

use scraper::{ElementRef, Html, Selector};
use url::Url;

pub const ENGINE_HTML: &str = "duckduckgo_html";
pub const ENGINE_LITE: &str = "duckduckgo_lite";

/// Which DDG endpoint produced the page. The engine name is part of the output
/// contract, so it is a closed enum, not a caller argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Html,
    Lite,
}

impl Engine {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Html => ENGINE_HTML,
            Self::Lite => ENGINE_LITE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// A real zero-result page and a parse failure must stay distinguishable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseOutcome {
    Results(Vec<ParsedResult>),
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Parse one SERP body. `Err` means "not a recognisable results page"; the
/// pipeline treats that as a retryable parse failure for the fallback endpoint.
pub fn parse(html: &str, engine: Engine) -> Result<ParseOutcome, ParseError> {
    let document = Html::parse_document(html);
    if has_empty_marker(&document) {
        return Ok(ParseOutcome::Empty);
    }
    match engine {
        Engine::Html => parse_html_results(&document),
        Engine::Lite => parse_lite_results(&document),
    }
}

fn has_empty_marker(document: &Html) -> bool {
    let selectors = [".no-results", ".result--no-result"];
    selectors.iter().any(|raw| {
        let selector = Selector::parse(raw).expect("static empty-result selector");
        document.select(&selector).next().is_some()
    })
}

fn parse_html_results(document: &Html) -> Result<ParseOutcome, ParseError> {
    let block_selector = Selector::parse("div.result").expect("static html result selector");
    let anchor_selector = Selector::parse("a.result__a").expect("static html anchor selector");
    let snippet_selector =
        Selector::parse(".result__snippet").expect("static html snippet selector");

    let mut blocks = 0usize;
    let mut results = Vec::new();
    for block in document.select(&block_selector) {
        blocks += 1;
        if is_sponsored(block) {
            continue;
        }
        let Some(anchor) = block.select(&anchor_selector).next() else {
            continue;
        };
        let title = normalized_text(anchor);
        let Some(href) = anchor.value().attr("href") else {
            continue;
        };
        let Some(url) = unwrap_redirect(href) else {
            continue;
        };
        if title.is_empty() {
            continue;
        }
        let snippet = block
            .select(&snippet_selector)
            .next()
            .map(normalized_text)
            .unwrap_or_default();
        results.push(ParsedResult {
            title,
            url,
            snippet,
        });
    }
    if blocks == 0 {
        return Err(ParseError(
            "html page contains no result block and no empty-result marker".to_string(),
        ));
    }
    Ok(ParseOutcome::Results(results))
}

fn parse_lite_results(document: &Html) -> Result<ParseOutcome, ParseError> {
    let row_selector = Selector::parse("tr").expect("static lite row selector");
    let anchor_selector = Selector::parse("a.result-link").expect("static lite anchor selector");
    let snippet_selector =
        Selector::parse("td.result-snippet").expect("static lite snippet selector");

    let mut blocks = 0usize;
    let mut results: Vec<ParsedResult> = Vec::new();
    let mut current: Option<usize> = None;
    for row in document.select(&row_selector) {
        if is_sponsored(row) {
            current = None;
            continue;
        }
        if let Some(anchor) = row.select(&anchor_selector).next() {
            blocks += 1;
            current = None;
            let title = normalized_text(anchor);
            let Some(href) = anchor.value().attr("href") else {
                continue;
            };
            let Some(url) = unwrap_redirect(href) else {
                continue;
            };
            if title.is_empty() {
                continue;
            }
            results.push(ParsedResult {
                title,
                url,
                snippet: String::new(),
            });
            current = Some(results.len() - 1);
            continue;
        }
        if let Some(index) = current
            && results[index].snippet.is_empty()
            && let Some(snippet) = row.select(&snippet_selector).next()
        {
            results[index].snippet = normalized_text(snippet);
        }
    }
    if blocks == 0 {
        return Err(ParseError(
            "lite page contains no result link and no empty-result marker".to_string(),
        ));
    }
    Ok(ParseOutcome::Results(results))
}

fn is_sponsored(element: ElementRef<'_>) -> bool {
    element.value().classes().any(|class| {
        class.eq_ignore_ascii_case("result--ad")
            || class.eq_ignore_ascii_case("result--sponsored")
            || class.eq_ignore_ascii_case("result-sponsored")
    }) || element
        .select(&Selector::parse(".result-sponsored, .result--ad").expect("static ad selector"))
        .next()
        .is_some()
}

fn normalized_text(element: ElementRef<'_>) -> String {
    element
        .text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Resolve a result href to an absolute http(s) URL.
///
/// A DuckDuckGo `/l/?uddg=` redirect is percent-decoded **once** (the URL query
/// parser does that); `rut` and every other wrapper parameter is dropped. A
/// protocol-relative href gets `https:`. Non-http(s) targets (`mailto:`,
/// `javascript:`) and an unwrappable redirect yield `None`.
pub fn unwrap_redirect(href: &str) -> Option<String> {
    let trimmed = href.trim();
    if trimmed.is_empty() {
        return None;
    }
    let absolute = if trimmed.starts_with("//") {
        format!("https:{trimmed}")
    } else {
        trimmed.to_string()
    };
    let url = Url::parse(&absolute).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let is_duckduckgo = url
        .host_str()
        .is_some_and(|host| host == "duckduckgo.com" || host.ends_with(".duckduckgo.com"));
    if is_duckduckgo && matches!(url.path(), "/l/" | "/l") {
        let target = url
            .query_pairs()
            .find(|(name, _)| name == "uddg")
            .map(|(_, value)| value.into_owned())?;
        let target = Url::parse(&target).ok()?;
        if !matches!(target.scheme(), "http" | "https") {
            return None;
        }
        return Some(target.to_string());
    }
    Some(url.to_string())
}

/// Normalize the optional `site` argument to a bare lowercase host.
///
/// A bare host, `www.example.com` or an `http(s)://host` URL are accepted. A
/// path, query, fragment, port or userinfo is rejected: `site` filters a host,
/// it is not a second URL to fetch.
pub fn normalize_site(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("site must not be empty".to_string());
    }
    let lower = trimmed.to_ascii_lowercase();
    let host = if let Some((scheme, _)) = lower.split_once("://") {
        if !matches!(scheme, "http" | "https") {
            return Err("site must be an http(s) URL or a bare host".to_string());
        }
        let url = Url::parse(trimmed).map_err(|error| format!("invalid site URL: {error}"))?;
        if url.port().is_some() {
            return Err("site must not carry a port".to_string());
        }
        if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
            return Err("site must be a bare host, not a URL with a path".to_string());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("site must not carry userinfo".to_string());
        }
        url.host_str()
            .ok_or_else(|| "site URL has no host".to_string())?
            .to_ascii_lowercase()
    } else {
        if lower.contains(['/', ':', '@', '?', '#']) {
            return Err("site must be a bare host without a path or port".to_string());
        }
        lower
    };
    validate_host(&host)?;
    Ok(host)
}

fn validate_host(host: &str) -> Result<(), String> {
    if host.is_empty() || host.len() > 253 {
        return Err("site host is empty or too long".to_string());
    }
    if host.starts_with('.') || host.ends_with('.') {
        return Err("site host must not start or end with a dot".to_string());
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err("site host has an empty or over-long label".to_string());
        }
        if !label
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
        {
            return Err("site host contains an unsupported character".to_string());
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err("site host label must not start or end with a hyphen".to_string());
        }
    }
    Ok(())
}

/// The result host is on the requested site, or is a subdomain of it.
pub fn site_matches(host: &str, site: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == site || host.ends_with(&format!(".{site}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTML_RESULTS: &str = include_str!("../../tests/fixtures/serp/html-results.html");
    const LITE_RESULTS: &str = include_str!("../../tests/fixtures/serp/lite-results.html");
    const HTML_EMPTY: &str = include_str!("../../tests/fixtures/serp/html-empty.html");
    const HTML_ANOMALY: &str = include_str!("../../tests/fixtures/serp/html-anomaly.html");
    const HTML_AD: &str = include_str!("../../tests/fixtures/serp/html-ad.html");
    const LITE_AD: &str = include_str!("../../tests/fixtures/serp/lite-ad.html");

    fn results(outcome: ParseOutcome) -> Vec<ParsedResult> {
        match outcome {
            ParseOutcome::Results(results) => results,
            ParseOutcome::Empty => panic!("expected results, got empty"),
        }
    }

    #[test]
    fn html_results_are_read_from_the_captured_anchors() {
        let parsed = results(parse(HTML_RESULTS, Engine::Html).unwrap());
        assert_eq!(parsed.len(), 10);
        assert_eq!(
            parsed[0].title,
            "Rust Async Runtimes: Tokio vs async-std vs smol 2026"
        );
        assert_eq!(
            parsed[0].url,
            "https://rustify.rs/articles/rust-async-runtimes-tokio-vs-async-std-2026"
        );
        assert!(parsed[0].snippet.starts_with("Tokio dominates async Rust"));
        assert!(
            parsed[1]
                .snippet
                .starts_with("Compare Rust async runtimes: Tokio, async-std, smol, and glommio.")
        );
        assert_eq!(
            parsed[2].url,
            "https://www.nxrates.com/blog/rust-async-runtimes/"
        );
        assert!(parsed.iter().all(|result| !result.title.is_empty()));
        assert!(
            parsed
                .iter()
                .all(|result| result.url.starts_with("https://"))
        );
    }

    #[test]
    fn lite_results_are_read_from_the_captured_rows() {
        let parsed = results(parse(LITE_RESULTS, Engine::Lite).unwrap());
        assert_eq!(parsed.len(), 10);
        assert_eq!(
            parsed[0].title,
            "Async in depth | Tokio - An asynchronous Rust runtime"
        );
        assert_eq!(parsed[0].url, "https://tokio.rs/tokio/tutorial/async");
        assert!(
            parsed[0]
                .snippet
                .starts_with("Tokio is a runtime for writing reliable asynchronous applications")
        );
        assert!(parsed.iter().all(|result| !result.snippet.is_empty()));
    }

    #[test]
    fn a_real_zero_result_page_is_an_empty_success() {
        assert_eq!(
            parse(HTML_EMPTY, Engine::Html).unwrap(),
            ParseOutcome::Empty
        );
    }

    #[test]
    fn an_unrecognised_page_is_a_typed_failure() {
        let error = parse(HTML_ANOMALY, Engine::Html).unwrap_err();
        assert!(error.0.contains("no result block"));
        assert!(parse(HTML_ANOMALY, Engine::Lite).is_err());
    }

    #[test]
    fn sponsored_blocks_are_excluded_and_neighbours_survive() {
        let parsed = results(parse(HTML_AD, Engine::Html).unwrap());
        assert_eq!(parsed.len(), 2);
        assert!(
            parsed
                .iter()
                .any(|result| result.url.ends_with("/organic-before"))
        );
        assert!(
            parsed
                .iter()
                .any(|result| result.url.ends_with("/organic-after"))
        );
        assert!(
            !parsed
                .iter()
                .any(|result| result.url.contains("sponsor.example"))
        );
        assert!(
            !parsed
                .iter()
                .any(|result| result.snippet.contains("Paid placement"))
        );
    }

    #[test]
    fn lite_sponsored_rows_are_excluded_and_neighbours_survive() {
        let parsed = results(parse(LITE_AD, Engine::Lite).unwrap());
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].url, "https://org.example/lite-organic-one");
        assert_eq!(parsed[1].url, "https://org.example/lite-organic-two");
        assert!(
            !parsed
                .iter()
                .any(|result| result.snippet.contains("Paid lite snippet"))
        );
    }

    #[test]
    fn unwrap_covers_the_href_case_table() {
        // Protocol-relative wrapper.
        assert_eq!(
            unwrap_redirect("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa&rut=deadbeef"),
            Some("https://example.com/a".to_string())
        );
        // Absolute wrapper.
        assert_eq!(
            unwrap_redirect(
                "https://duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fb&rut=deadbeef"
            ),
            Some("https://example.com/b".to_string())
        );
        // A plain http(s) href is used as-is (normalized by the URL parser).
        assert_eq!(
            unwrap_redirect("https://example.com/plain"),
            Some("https://example.com/plain".to_string())
        );
        assert_eq!(
            unwrap_redirect("http://example.com/plain"),
            Some("http://example.com/plain".to_string())
        );
        // Double-encoded uddg is decoded exactly once, so it is not absolute.
        assert_eq!(
            unwrap_redirect("//duckduckgo.com/l/?uddg=https%253A%252F%252Fexample.com%252Fdouble"),
            None
        );
        // A wrapper without uddg is not a result target.
        assert_eq!(unwrap_redirect("//duckduckgo.com/l/?rut=deadbeef"), None);
        // Non-http(s) schemes are dropped, never resolved.
        assert_eq!(unwrap_redirect("mailto:someone@example.com"), None);
        assert_eq!(unwrap_redirect("javascript:alert(1)"), None);
        assert_eq!(unwrap_redirect(""), None);
        // A non-/l/ DuckDuckGo URL is an ordinary URL, not a wrapper.
        assert_eq!(
            unwrap_redirect("https://duckduckgo.com/y.js?ad=1"),
            Some("https://duckduckgo.com/y.js?ad=1".to_string())
        );
    }

    #[test]
    fn site_normalization_accepts_hosts_and_http_urls() {
        assert_eq!(normalize_site("Example.COM").unwrap(), "example.com");
        assert_eq!(
            normalize_site("www.example.com").unwrap(),
            "www.example.com"
        );
        assert_eq!(
            normalize_site("https://Example.com/").unwrap(),
            "example.com"
        );
        assert_eq!(
            normalize_site("http://www.example.com").unwrap(),
            "www.example.com"
        );
        for rejected in [
            "",
            "example.com/path",
            "example.com:443",
            "https://example.com/path",
            "https://example.com:8443/",
            "ftp://example.com/",
            "user@example.com",
            "exa mple.com",
        ] {
            assert!(normalize_site(rejected).is_err(), "{rejected:?}");
        }
    }

    #[test]
    fn site_filter_matches_equal_hosts_and_subdomains_only() {
        assert!(site_matches("example.com", "example.com"));
        assert!(site_matches("docs.example.com", "example.com"));
        assert!(!site_matches("notexample.com", "example.com"));
        assert!(!site_matches("example.com.evil.test", "example.com"));
        assert!(!site_matches("example.org", "example.com"));
    }
}
