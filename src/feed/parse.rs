//! Pure feed parsing: bytes -> normalized feed and items (IMP-DEC-019).
//!
//! `feed-rs` handles RSS 2.0/1.0/0.x, Atom and JSON Feed, including CDATA,
//! namespaces and the date grammars. This module owns only the normalization:
//! feed order, dedupe by id (fallback url), absolute URLs, RFC3339 UTC dates and
//! plain-text summaries. It never renders and never persists.

use scraper::Html;
use scraper::node::Node;
use url::Url;

use crate::core::error::{Result, WidowError};
use crate::core::truncate::{normalize_plain_text, truncate_chars};
use crate::core::url::PageUrl;
use crate::feed::FeedItem;

/// Subtrees that never contribute summary text.
const SKIP_TAGS: &[&str] = &[
    "head", "script", "style", "noscript", "template", "svg", "iframe", "object", "embed", "canvas",
];

/// Elements that separate text runs; a space is emitted around their content so
/// `</p><p>` and `<br>` cannot run words together.
const BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "br",
    "dd",
    "div",
    "dl",
    "dt",
    "figcaption",
    "figure",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "ul",
];

#[derive(Debug)]
pub struct ParsedFeed {
    pub feed_type: &'static str,
    pub title: Option<String>,
    pub site_url: Option<String>,
    pub language: Option<String>,
    pub description: String,
    /// Deduplicated items in feed order, before any `limit`.
    pub items: Vec<FeedItem>,
    pub item_count: usize,
    pub truncated_summaries: usize,
}

/// Parse a feed body. A parse failure is `extraction_failed`; the body size is
/// already bounded by the acquisition cap.
pub fn parse(bytes: &[u8], base: &Url, max_summary_chars: usize) -> Result<ParsedFeed> {
    // An internal DTD subset is never needed by a feed and is the classic
    // entity-expansion surface: refuse it before handing the bytes to the
    // parser (bounded, typed, and independent of parser tolerance).
    let head = &bytes[..bytes.len().min(16 * 1024)];
    let head = String::from_utf8_lossy(head).to_ascii_lowercase();
    if head.contains("<!entity") {
        return Err(WidowError::Extraction(
            "feed contains a DTD entity declaration".to_string(),
        ));
    }
    let feed = feed_rs::parser::Builder::new()
        .base_uri(Some(base.as_str()))
        // No `sanitize` feature: summaries become plain text below, never markup.
        .sanitize_content(false)
        .build()
        .parse(bytes)
        .map_err(|error| WidowError::Extraction(format!("feed parse failed: {error}")))?;

    let feed_type = match feed.feed_type {
        feed_rs::model::FeedType::Atom => "atom",
        feed_rs::model::FeedType::JSON => "json",
        feed_rs::model::FeedType::RSS0
        | feed_rs::model::FeedType::RSS1
        | feed_rs::model::FeedType::RSS2 => "rss",
    };
    let (description, description_cut) = capped_plain_text(
        feed.description
            .as_ref()
            .map(|text| text.content.as_str())
            .unwrap_or_default(),
        max_summary_chars,
    );
    let mut truncated_summaries = usize::from(description_cut);

    let mut items: Vec<FeedItem> = Vec::with_capacity(feed.entries.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_canonical: std::collections::HashSet<String> = std::collections::HashSet::new();
    let feed_author = author_of(&feed.authors);

    for entry in feed.entries {
        let url = entry
            .links
            .iter()
            .find_map(|link| absolute(&link.href, base));
        // Identity (crawl/monitor rule) beside the verbatim fetch target.
        // A non-http(s) link keeps its verbatim URL and has no page identity.
        let canonical_url = url
            .as_deref()
            .and_then(|raw| PageUrl::parse(raw).ok())
            .map(|page| page.canonical().to_string());
        let id = if entry.id.trim().is_empty() {
            url.clone().unwrap_or_default()
        } else {
            entry.id.trim().to_string()
        };
        let key = if id.is_empty() {
            format!("url:{}", url.clone().unwrap_or_default())
        } else {
            format!("id:{id}")
        };
        if !seen.insert(key) {
            continue;
        }
        // Tracking-parameter variants of one item collapse to the first one.
        if let Some(canonical) = &canonical_url
            && !seen_canonical.insert(canonical.clone())
        {
            continue;
        }
        let summary_source = entry
            .content
            .as_ref()
            .and_then(|content| content.body.clone())
            .filter(|body| !body.trim().is_empty())
            .or_else(|| {
                entry
                    .summary
                    .as_ref()
                    .map(|text| text.content.clone())
                    .filter(|body| !body.trim().is_empty())
            })
            .unwrap_or_default();
        let (summary, cut) = capped_plain_text(&summary_source, max_summary_chars);
        truncated_summaries += usize::from(cut);
        items.push(FeedItem {
            id,
            title: entry
                .title
                .as_ref()
                .map(|text| normalize_plain_text(&text.content))
                .filter(|title| !title.is_empty()),
            url,
            canonical_url,
            published: entry
                .published
                .or(entry.updated)
                .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            author: author_of(&entry.authors).or_else(|| feed_author.clone()),
            summary,
        });
    }

    let item_count = items.len();
    Ok(ParsedFeed {
        feed_type,
        title: feed
            .title
            .as_ref()
            .map(|text| normalize_plain_text(&text.content))
            .filter(|title| !title.is_empty()),
        site_url: feed
            .links
            .iter()
            .find(|link| link.rel.as_deref().is_none_or(|rel| rel == "alternate"))
            .and_then(|link| absolute(&link.href, base)),
        language: feed.language.filter(|language| !language.trim().is_empty()),
        description,
        items,
        item_count,
        truncated_summaries,
    })
}

fn author_of(authors: &[feed_rs::model::Person]) -> Option<String> {
    authors.iter().find_map(|person| {
        person
            .name
            .as_deref()
            .map(normalize_plain_text)
            .filter(|name| !name.is_empty())
            .or_else(|| {
                person
                    .email
                    .as_deref()
                    .map(normalize_plain_text)
                    .filter(|email| !email.is_empty())
            })
    })
}

/// Resolve a feed link against the response's final URL. A link that cannot be
/// made absolute stays `None` (never invented).
fn absolute(href: &str, base: &Url) -> Option<String> {
    let href = href.trim();
    if href.is_empty() {
        return None;
    }
    Url::parse(href)
        .or_else(|_| base.join(href))
        .ok()
        .map(|url| url.to_string())
}

/// Visible plain text of an HTML/text summary, capped. Returns `(text, cut)`.
fn capped_plain_text(markup: &str, max_chars: usize) -> (String, bool) {
    let plain = plain_text(markup);
    if max_chars == 0 {
        return (String::new(), !plain.is_empty());
    }
    truncate_chars(&plain, max_chars)
}

fn plain_text(markup: &str) -> String {
    if !markup.contains('<') {
        return collapse(markup);
    }
    let document = Html::parse_fragment(markup);
    let mut out = String::new();
    collect_text(document.tree.root(), &mut out);
    collapse(&out)
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn collect_text(node: ego_tree::NodeRef<'_, Node>, out: &mut String) {
    for child in node.children() {
        match child.value() {
            Node::Text(text) => out.push_str(&text.text),
            Node::Element(element) => {
                if SKIP_TAGS.contains(&element.name()) {
                    continue;
                }
                let block = BLOCK_TAGS.contains(&element.name());
                if block {
                    out.push(' ');
                }
                collect_text(child, out);
                if block {
                    out.push(' ');
                }
            }
            Node::Document | Node::Fragment => collect_text(child, out),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.com/feed.xml").unwrap()
    }

    const RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel>
  <title>Example news</title>
  <link>https://example.com/</link>
  <description>Example feed</description>
  <language>en</language>
  <item>
    <title>First story</title>
    <link>https://example.com/first</link>
    <guid>tag:example.com,2026:1</guid>
    <pubDate>Fri, 31 Oct 2026 06:00:00 +0000</pubDate>
    <description><![CDATA[<p>First <b>summary</b> text.</p><script>alert(1)</script>]]></description>
  </item>
  <item>
    <title>Second story</title>
    <link>/second</link>
    <pubDate>Fri, 31 Oct 2026 07:00:00 GMT</pubDate>
    <description>Plain second summary.</description>
  </item>
</channel></rss>"#;

    #[test]
    fn rss_items_keep_order_and_normalize_fields() {
        let parsed = parse(RSS.as_bytes(), &base(), 2_000).unwrap();
        assert_eq!(parsed.feed_type, "rss");
        assert_eq!(parsed.title.as_deref(), Some("Example news"));
        assert_eq!(parsed.site_url.as_deref(), Some("https://example.com/"));
        assert_eq!(parsed.language.as_deref(), Some("en"));
        assert_eq!(parsed.item_count, 2);
        assert_eq!(parsed.items[0].title.as_deref(), Some("First story"));
        assert_eq!(
            parsed.items[0].url.as_deref(),
            Some("https://example.com/first")
        );
        assert_eq!(parsed.items[0].id, "tag:example.com,2026:1");
        assert_eq!(
            parsed.items[0].published.as_deref(),
            Some("2026-10-31T06:00:00Z")
        );
        assert_eq!(parsed.items[0].summary, "First summary text.");
        assert_eq!(
            parsed.items[1].url.as_deref(),
            Some("https://example.com/second")
        );
        assert_eq!(parsed.truncated_summaries, 0);
    }

    #[test]
    fn summaries_are_plain_text_and_capped() {
        let parsed = parse(RSS.as_bytes(), &base(), 12).unwrap();
        assert_eq!(parsed.items[0].summary.chars().count(), 12);
        assert_eq!(parsed.truncated_summaries, 2);
        assert!(!parsed.items[0].summary.contains('<'));
    }

    #[test]
    fn duplicate_ids_or_urls_keep_the_first() {
        let duplicate = RSS.replace(
            "<title>Second story</title>",
            "<title>Second story</title><guid>tag:example.com,2026:1</guid>",
        );
        let parsed = parse(duplicate.as_bytes(), &base(), 2_000).unwrap();
        assert_eq!(parsed.item_count, 1);
        assert_eq!(parsed.items[0].title.as_deref(), Some("First story"));
    }

    #[test]
    fn malformed_xml_is_extraction_failed() {
        let error = parse(b"<rss><channel><title>broken", &base(), 2_000).unwrap_err();
        assert_eq!(
            error.code(),
            crate::core::error::ErrorCode::ExtractionFailed
        );
    }

    #[test]
    fn atom_and_json_feeds_are_recognized() {
        let atom = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom">
          <title>Atom feed</title><link href="https://example.com/"/>
          <entry><title>Atom item</title><id>urn:1</id>
          <link href="/atom-item"/><updated>2026-10-31T08:00:00Z</updated>
          <summary>Atom summary.</summary></entry></feed>"#;
        let parsed = parse(atom.as_bytes(), &base(), 2_000).unwrap();
        assert_eq!(parsed.feed_type, "atom");
        assert_eq!(
            parsed.items[0].url.as_deref(),
            Some("https://example.com/atom-item")
        );

        let json = r#"{"version":"https://jsonfeed.org/version/1.1","title":"JSON feed",
          "home_page_url":"https://example.com/","items":[
          {"id":"j1","title":"JSON item","url":"https://example.com/json-item",
           "date_published":"2026-10-31T09:00:00Z","content_text":"JSON summary."}]}"#;
        let parsed = parse(json.as_bytes(), &base(), 2_000).unwrap();
        assert_eq!(parsed.feed_type, "json");
        assert_eq!(parsed.items[0].summary, "JSON summary.");
    }
}
