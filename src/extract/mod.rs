//! Extraction: HTML -> LLM-ready Markdown plus metadata and links.
//!
//! `main` uses readability (`dom_smoothie`) and falls back to the full body when
//! readability finds nothing usable; `full`, `text` and `raw` are the escape
//! hatches. Non-markup content types are delivered as normalized text.

use std::collections::HashSet;

use ego_tree::NodeRef;
use scraper::node::Node;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use url::Url;

pub mod headings;
pub mod spec;
pub mod spec_eval;

use crate::core::error::{Result, WidowError};
use crate::core::truncate::{normalize_plain_text, truncate_chars, word_count};

/// Test-only counter proving one readability pass per acquisition (B4-4).
/// Thread-local so parallel tests do not share the count.
#[cfg(test)]
mod pass_instrumentation {
    use std::cell::Cell;

    thread_local! {
        static MAIN_ARTICLE_PASSES: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn record() {
        MAIN_ARTICLE_PASSES.with(|passes| passes.set(passes.get() + 1));
    }

    pub(super) fn reset() {
        MAIN_ARTICLE_PASSES.with(|passes| passes.set(0));
    }

    pub(super) fn count() -> usize {
        MAIN_ARTICLE_PASSES.with(Cell::get)
    }
}

/// Subtrees that never contribute text or Markdown (IMP-SAFE-001).
const SKIP_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "iframe", "object", "embed", "canvas", "head",
];

/// Tags that start a new line in plain text.
const BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "details",
    "div",
    "dl",
    "dt",
    "dd",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "tbody",
    "thead",
    "tfoot",
    "tr",
    "ul",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Main,
    Full,
    Text,
    Raw,
}

impl Mode {
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "main" => Ok(Self::Main),
            "full" => Ok(Self::Full),
            "text" => Ok(Self::Text),
            "raw" => Ok(Self::Raw),
            other => Err(WidowError::InvalidUrl(format!(
                "mode `{other}` is not one of main|full|text|raw"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Full => "full",
            Self::Text => "text",
            Self::Raw => "raw",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Link {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub offsite: bool,
}

#[derive(Debug, Clone)]
pub struct Extraction {
    pub markdown: String,
    pub truncated: bool,
    pub limit_hit: Option<String>,
    pub markdown_chars: usize,
    pub word_count: usize,
    pub title: Option<String>,
    pub byline: Option<String>,
    pub site_name: Option<String>,
    pub published: Option<String>,
    pub lang: Option<String>,
    pub links: Option<Vec<Link>>,
    pub links_truncated: bool,
}

#[derive(Debug, Clone)]
pub struct ExtractOptions {
    pub mode: Mode,
    pub max_chars: usize,
    pub max_links: usize,
    pub include_links: bool,
    /// Subtrees removed from the DOM **before** any conversion (B05). Compiled
    /// selectors, so a monitor check does not re-parse CSS on every fetch.
    pub ignore_selectors: Vec<Selector>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct Meta {
    title: Option<String>,
    byline: Option<String>,
    site_name: Option<String>,
    published: Option<String>,
    lang: Option<String>,
}

pub fn extract_html(html: &str, base: &Url, options: &ExtractOptions) -> Result<Extraction> {
    Ok(extract_page(html, base, options, None)?.main)
}

/// A page read once: the Markdown extraction plus, when a spec is supplied, the
/// records evaluated against the same parsed tree (IMP-B04: no second parse).
#[derive(Debug)]
pub struct PageExtraction {
    pub main: Extraction,
    pub records: Option<spec_eval::EvalOutcome>,
    /// DOM parses performed by this call (always 1).
    pub dom_parses: usize,
}

pub fn extract_page(
    html: &str,
    base: &Url,
    options: &ExtractOptions,
    spec: Option<(&spec::CompiledSpec, &spec_eval::EvalOptions)>,
) -> Result<PageExtraction> {
    let mut document = Html::parse_document(html);
    let removed = remove_ignored(&mut document, &options.ignore_selectors);
    // Only re-serialize when something was removed: the original string is the
    // faithful input for `raw` and for readability's own parse.
    let effective = if removed == 0 {
        html.to_string()
    } else {
        document.html()
    };
    // One readability pass serves both the `main` body and a `:main` spec scope.
    let needs_article = options.mode == Mode::Main
        || spec.is_some_and(|(compiled, _)| {
            matches!(&compiled.container, Some(spec::Container::Main))
        });
    let article = if needs_article {
        main_article(&document, &effective, base)
    } else {
        None
    };
    let main =
        extract_from_document_with_article(&document, &effective, base, options, article.as_ref())?;
    let mut dom_parses = 1;
    let mut records = match spec {
        Some((compiled, eval)) => {
            let article_document = if matches!(&compiled.container, Some(spec::Container::Main)) {
                article
                    .as_ref()
                    .map(|(content, _)| content.as_str())
                    .filter(|content| !content.trim().is_empty())
                    .map(Html::parse_document)
            } else {
                None
            };
            let outcome = match article_document.as_ref() {
                Some(article_document) => {
                    dom_parses = 2;
                    spec_eval::evaluate_document(article_document, base, compiled, eval)
                }
                None => spec_eval::evaluate_document(&document, base, compiled, eval),
            };
            Some(outcome)
        }
        None => None,
    };
    if let Some(records) = records.as_mut() {
        records.dom_parses = dom_parses;
    }
    Ok(PageExtraction {
        main,
        records,
        dom_parses,
    })
}

/// Remove every subtree matched by `selectors`. Returns how many nodes were
/// removed, so a caller can skip re-serialization when nothing matched.
pub fn remove_ignored(document: &mut Html, selectors: &[Selector]) -> usize {
    if selectors.is_empty() {
        return 0;
    }
    let mut ids: Vec<ego_tree::NodeId> = Vec::new();
    for selector in selectors {
        ids.extend(document.select(selector).map(|element| element.id()));
    }
    let mut removed = 0usize;
    for id in ids {
        if let Some(mut node) = document.tree.get_mut(id)
            && node.parent().is_some()
        {
            node.detach();
            removed += 1;
        }
    }
    removed
}

/// The extraction body, separated so a caller that already parsed the document
/// (the crawl path) can reuse that tree.
pub fn extract_from_document(
    document: &Html,
    html: &str,
    base: &Url,
    options: &ExtractOptions,
) -> Result<Extraction> {
    extract_from_document_with_article(document, html, base, options, None)
}

/// Same extraction, optionally reusing an already-computed readability article.
/// The pipeline computes it once when both the `main` body and a `:main` spec
/// need it.
fn extract_from_document_with_article(
    document: &Html,
    html: &str,
    base: &Url,
    options: &ExtractOptions,
    article: Option<&(String, Meta)>,
) -> Result<Extraction> {
    let links = if options.include_links {
        Some(collect_links(document, base, options.max_links))
    } else {
        None
    };
    let (links, links_truncated) = match links {
        Some((links, truncated)) => (Some(links), truncated),
        None => (None, false),
    };

    let (markdown, meta) = match options.mode {
        Mode::Raw => {
            let meta = meta_from_document(document);
            (html.to_string(), meta)
        }
        Mode::Text => {
            let meta = meta_from_document(document);
            (normalize_plain_text(&text_from_document(document)), meta)
        }
        Mode::Full => {
            let meta = meta_from_document(document);
            (markdown_from_body(document), meta)
        }
        Mode::Main => {
            let computed;
            let article = match article {
                Some(article) => Some(article),
                None => {
                    computed = main_article(document, html, base);
                    computed.as_ref()
                }
            };
            match article {
                Some((content, meta)) if !content.trim().is_empty() => {
                    let markdown = html_to_markdown(content)?;
                    if markdown.trim().is_empty() {
                        // Readability produced a subtree we cannot render: use the body.
                        (markdown_from_body(document), meta_from_document(document))
                    } else {
                        // Readability's assembly drops heading siblings; put the
                        // structural markers back inside the content it selected.
                        let markdown = headings::restore_section_headings(
                            document,
                            &markdown,
                            meta.title.as_deref(),
                        );
                        (markdown, meta.clone())
                    }
                }
                _ => (markdown_from_body(document), meta_from_document(document)),
            }
        }
    };

    let trimmed = markdown.trim().to_string();
    let (markdown, truncated) = truncate_chars(&trimmed, options.max_chars);
    Ok(Extraction {
        markdown_chars: markdown.chars().count(),
        word_count: word_count(&markdown),
        markdown,
        truncated,
        limit_hit: if truncated {
            Some("max_chars".to_string())
        } else {
            None
        },
        title: meta.title,
        byline: meta.byline,
        site_name: meta.site_name,
        published: meta.published,
        lang: meta.lang,
        links,
        links_truncated,
    })
}

/// Presentation-only truncation for `extract` (IMP-DEC-016). Cuts every string
/// value (and each string element of an `all` array) at `max_field_chars`, then
/// returns records in evaluation order until the serialized total would exceed
/// `max_chars`. The stored evaluation is never passed through this function.
#[derive(Debug, Clone)]
pub struct TruncatedRecords {
    pub records: Vec<Map<String, Value>>,
    /// String values actually returned after a cut.
    pub truncated_fields: usize,
    /// The record budget stopped before the next record.
    pub budget_truncated: bool,
}

pub fn truncate_records(
    records: Vec<Map<String, Value>>,
    max_field_chars: usize,
    max_chars: usize,
) -> TruncatedRecords {
    let mut returned = Vec::new();
    let mut used = 0usize;
    let mut truncated_fields = 0usize;
    let mut budget_truncated = false;
    for mut record in records {
        let mut cuts = 0usize;
        for value in record.values_mut() {
            cut_value(value, max_field_chars, &mut cuts);
        }
        let size = serde_json::to_string(&record)
            .map(|text| text.chars().count())
            .unwrap_or(0);
        if used.saturating_add(size) > max_chars {
            budget_truncated = true;
            break;
        }
        used += size;
        truncated_fields += cuts;
        returned.push(record);
    }
    TruncatedRecords {
        records: returned,
        truncated_fields,
        budget_truncated,
    }
}

fn cut_value(value: &mut Value, max_field_chars: usize, cuts: &mut usize) {
    match value {
        Value::String(text) => {
            if text.chars().count() > max_field_chars {
                *text = text.chars().take(max_field_chars).collect();
                *cuts += 1;
            }
        }
        Value::Array(items) => {
            for item in items {
                cut_value(item, max_field_chars, cuts);
            }
        }
        _ => {}
    }
}

/// Evaluate a spec with `:main` scoped to the shared article. A non-`:main` spec
/// keeps the existing single-parse `evaluate` path. For `:main` the caller
/// supplies the article obtained from [`main_article`] for this acquisition, so
/// no second readability pass happens here; the article is parsed once
/// (dom_parses = 2) and the raw document is the fallback when readability
/// yielded nothing usable.
pub fn evaluate_main_scoped(
    html: &str,
    base: &Url,
    article: Option<&str>,
    spec: &spec::CompiledSpec,
    options: &spec_eval::EvalOptions,
) -> spec_eval::EvalOutcome {
    if !matches!(&spec.container, Some(spec::Container::Main)) {
        return spec_eval::evaluate(html, base, spec, options);
    }
    match article.map(str::trim).filter(|content| !content.is_empty()) {
        Some(content) => {
            let article_document = Html::parse_document(content);
            let mut outcome = spec_eval::evaluate_document(&article_document, base, spec, options);
            outcome.dom_parses = 2;
            outcome
        }
        None => spec_eval::evaluate(html, base, spec, options),
    }
}

/// Content identity for the crawl's near-duplicate collapse: the Markdown with
/// link/image targets and bare URLs removed and whitespace collapsed.
///
/// A calendar or filter trap differs from its neighbour only in the URLs it
/// links to, so the delivered-text hash (`pages.text_hash`) would never match.
/// This key is not the delivered-content hash; it answers "same page, different
/// addresses".
pub fn near_duplicate_key(markdown: &str) -> String {
    let mut stripped = String::with_capacity(markdown.len());
    let chars: Vec<char> = markdown.chars().collect();
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] == ']' && chars.get(index + 1) == Some(&'(') {
            // Keep the link label, drop the target.
            stripped.push(']');
            index += 2;
            while index < chars.len() && chars[index] != ')' {
                index += 1;
            }
            index += 1;
            continue;
        }
        if (chars[index] == 'h' || chars[index] == 'H')
            && index + 4 <= chars.len()
            && chars[index..index + 4]
                .iter()
                .collect::<String>()
                .eq_ignore_ascii_case("http")
        {
            // Drop a bare URL up to the next whitespace.
            while index < chars.len() && !chars[index].is_whitespace() {
                index += 1;
            }
            continue;
        }
        stripped.push(chars[index]);
        index += 1;
    }
    let collapsed = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::core::url::content_hash(collapsed.as_bytes())
}

/// `text/plain`, JSON and XML: delivered as normalized text, no parsing.
pub fn extract_plain(text: &str, options: &ExtractOptions, include_links: bool) -> Extraction {
    let normalized = normalize_plain_text(text);
    let (markdown, truncated) = truncate_chars(&normalized, options.max_chars);
    Extraction {
        markdown_chars: markdown.chars().count(),
        word_count: word_count(&markdown),
        markdown,
        truncated,
        limit_hit: if truncated {
            Some("max_chars".to_string())
        } else {
            None
        },
        title: None,
        byline: None,
        site_name: None,
        published: None,
        lang: None,
        links: if include_links {
            Some(Vec::new())
        } else {
            None
        },
        links_truncated: false,
    }
}

/// The single readability pass for one acquisition: the article that the
/// delivered `main` body is built from and the scope `:main` evaluates inside.
///
/// Every consumer (the `main` Markdown, `:main` records in `extract_page`, and
/// the `extract` tool path) goes through this helper, so one acquisition costs
/// one readability pass and both views describe the same string (IMP-DEC-020).
pub(crate) fn main_article(document: &Html, html: &str, base: &Url) -> Option<(String, Meta)> {
    #[cfg(test)]
    pass_instrumentation::record();
    let mut readability = dom_smoothie::Readability::new(html, Some(base.as_str()), None).ok()?;
    let article = readability.parse().ok()?;
    let meta = Meta {
        title: non_empty(article.title.clone()),
        byline: article.byline.clone().and_then(non_empty),
        site_name: article.site_name.clone().and_then(non_empty),
        published: article.published_time.clone().and_then(non_empty),
        lang: article.lang.clone().and_then(non_empty),
    };
    let content = article.content.to_string();
    // Structural-gap repair runs before Markdown conversion, so both the
    // delivered body and `:main` see the restored blocks (IMP-DEC-022).
    let content = headings::repair_structural_gaps(document, &content).unwrap_or(content);
    Some((content, meta))
}

fn html_to_markdown(html: &str) -> Result<String> {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(SKIP_TAGS.to_vec())
        .build();
    converter
        .convert(html)
        .map_err(|error| WidowError::Extraction(format!("markdown conversion failed: {error}")))
}

fn markdown_from_body(document: &Html) -> String {
    let body = Selector::parse("body")
        .ok()
        .and_then(|selector| document.select(&selector).next())
        .map(|element| element.inner_html())
        .unwrap_or_else(|| document.root_element().inner_html());
    html_to_markdown(&body).unwrap_or_default()
}

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn meta_from_document(document: &Html) -> Meta {
    let mut meta = Meta::default();
    if let Some(title) = Selector::parse("title")
        .ok()
        .and_then(|selector| document.select(&selector).next())
    {
        meta.title = non_empty(title.text().collect::<String>());
    }
    if meta.title.is_none() {
        meta.title = meta_content(document, "meta[property='og:title']");
    }
    meta.byline = meta_content(document, "meta[name='author']");
    meta.site_name = meta_content(document, "meta[property='og:site_name']");
    meta.published = meta_content(document, "meta[property='article:published_time']")
        .or_else(|| meta_content(document, "meta[name='date']"))
        .or_else(|| meta_content(document, "meta[itemprop='datePublished']"));
    meta.lang = Selector::parse("html[lang]")
        .ok()
        .and_then(|selector| document.select(&selector).next())
        .and_then(|element| element.value().attr("lang"))
        .and_then(|value| non_empty(value.to_string()))
        .or_else(|| meta_content(document, "meta[http-equiv='content-language']"));
    meta
}

fn meta_content(document: &Html, selector: &str) -> Option<String> {
    let selector = Selector::parse(selector).ok()?;
    document
        .select(&selector)
        .next()
        .and_then(|element| element.value().attr("content"))
        .and_then(|value| non_empty(value.to_string()))
}

fn collect_links(document: &Html, base: &Url, max_links: usize) -> (Vec<Link>, bool) {
    let selector = match Selector::parse("a[href]") {
        Ok(selector) => selector,
        Err(_) => return (Vec::new(), false),
    };
    let base_host = base.host_str().unwrap_or_default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut links: Vec<Link> = Vec::new();
    let mut truncated = false;

    for element in document.select(&selector) {
        let href = match element.value().attr("href") {
            Some(href) => href.trim(),
            None => continue,
        };
        if href.is_empty() || href.starts_with('#') {
            continue;
        }
        let lower = href.to_ascii_lowercase();
        if lower.starts_with("javascript:")
            || lower.starts_with("mailto:")
            || lower.starts_with("tel:")
            || lower.starts_with("data:")
        {
            continue;
        }
        let resolved = match base.join(href) {
            Ok(resolved) => resolved,
            Err(_) => continue,
        };
        if !matches!(resolved.scheme(), "http" | "https") {
            continue;
        }
        let mut key = resolved.to_string();
        if let Some((without_fragment, _)) = key.split_once('#') {
            key = without_fragment.to_string();
        }
        if !seen.insert(key.clone()) {
            continue;
        }
        if links.len() >= max_links {
            truncated = true;
            break;
        }
        let text = element.text().collect::<String>();
        let text = text.trim();
        links.push(Link {
            offsite: resolved.host_str().unwrap_or_default() != base_host,
            url: key,
            text: if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            },
        });
    }
    (links, truncated)
}

fn text_from_document(document: &Html) -> String {
    let mut out = String::new();
    walk(document.tree.root(), &mut out);
    out
}

fn walk(node: NodeRef<'_, Node>, out: &mut String) {
    match node.value() {
        Node::Text(text) => out.push_str(&text.text),
        Node::Element(element) => {
            let name = element.name();
            if SKIP_TAGS.contains(&name) {
                return;
            }
            let block = BLOCK_TAGS.contains(&name);
            if block {
                push_newline(out);
            }
            for child in node.children() {
                walk(child, out);
            }
            if block {
                push_newline(out);
            }
            match name {
                "br" => push_newline(out),
                "td" | "th" => out.push('\t'),
                _ => {}
            }
        }
        // Document and fragment roots carry no text of their own.
        Node::Document | Node::Fragment => {
            for child in node.children() {
                walk(child, out);
            }
        }
        _ => {}
    }
}

fn push_newline(out: &mut String) {
    if !out.ends_with('\n') {
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(mode: Mode, max_chars: usize) -> ExtractOptions {
        ExtractOptions {
            mode,
            max_chars,
            max_links: 50,
            include_links: false,
            ignore_selectors: Vec::new(),
        }
    }

    fn base() -> Url {
        Url::parse("https://example.com/posts/one").unwrap()
    }

    const PAGE: &str = r#"<!doctype html>
<html lang="en"><head>
  <meta charset="utf-8">
  <title>The Article Title</title>
  <meta name="author" content="Ada Lovelace">
  <meta property="og:site_name" content="Example Journal">
  <meta property="article:published_time" content="2026-01-02T03:04:05Z">
  <style>body { color: red }</style>
  <script>window.analytics = "should not appear"</script>
</head>
<body>
  <nav><a href="/">Home</a><a href="https://other.example/x">Elsewhere</a></nav>
  <article>
    <h1>The Article Title</h1>
    <p>First paragraph with a distinctive sentence: the harbour lights were blue.</p>
    <p>Second paragraph with <em>emphasis</em> and <a href="/relative/path">a relative link</a>.</p>
  </article>
  <footer>Sign up for our newsletter</footer>
</body></html>"#;

    #[test]
    fn main_mode_keeps_article_text_and_drops_scripts() {
        let extraction = extract_html(PAGE, &base(), &options(Mode::Main, 200_000)).unwrap();
        assert!(extraction.markdown.contains("harbour lights were blue"));
        assert!(!extraction.markdown.contains("should not appear"));
        assert!(!extraction.markdown.contains("color: red"));
        assert_eq!(extraction.title.as_deref(), Some("The Article Title"));
        assert_eq!(extraction.byline.as_deref(), Some("Ada Lovelace"));
        assert_eq!(extraction.site_name.as_deref(), Some("Example Journal"));
        assert_eq!(
            extraction.published.as_deref(),
            Some("2026-01-02T03:04:05Z")
        );
        assert_eq!(extraction.lang.as_deref(), Some("en"));
        assert!(!extraction.truncated);
        assert!(extraction.word_count > 5);
        assert_eq!(
            extraction.markdown_chars,
            extraction.markdown.chars().count()
        );
    }

    #[test]
    fn full_mode_includes_boilerplate_but_never_scripts() {
        let extraction = extract_html(PAGE, &base(), &options(Mode::Full, 200_000)).unwrap();
        assert!(extraction.markdown.contains("newsletter"));
        assert!(extraction.markdown.contains("harbour lights"));
        assert!(!extraction.markdown.contains("should not appear"));
    }

    #[test]
    fn text_mode_is_plain_and_block_separated() {
        let extraction = extract_html(PAGE, &base(), &options(Mode::Text, 200_000)).unwrap();
        assert!(extraction.markdown.contains("harbour lights were blue"));
        assert!(!extraction.markdown.contains('<'));
        assert!(!extraction.markdown.contains("should not appear"));
        assert!(
            extraction.markdown.contains("\n"),
            "block elements should produce line breaks: {}",
            extraction.markdown
        );
    }

    #[test]
    fn raw_mode_returns_the_document_and_is_capped() {
        let extraction = extract_html(PAGE, &base(), &options(Mode::Raw, 80)).unwrap();
        assert!(extraction.truncated);
        assert_eq!(extraction.limit_hit.as_deref(), Some("max_chars"));
        assert_eq!(extraction.markdown.chars().count(), 80);
        let full = extract_html(PAGE, &base(), &options(Mode::Raw, 1_000_000)).unwrap();
        assert!(full.markdown.contains("<script>"));
    }

    #[test]
    fn truncation_is_explicit_and_never_exceeds_the_cap() {
        for cap in [0usize, 1, 10, 100, 1000] {
            let extraction = extract_html(PAGE, &base(), &options(Mode::Main, cap)).unwrap();
            assert!(extraction.markdown.chars().count() <= cap);
            if extraction.markdown_chars == cap && extraction.truncated {
                assert_eq!(extraction.limit_hit.as_deref(), Some("max_chars"));
            }
        }
    }

    #[test]
    fn links_are_absolute_deduplicated_and_marked_offsite() {
        let mut options = options(Mode::Main, 200_000);
        options.include_links = true;
        let extraction = extract_html(PAGE, &base(), &options).unwrap();
        let links = extraction.links.unwrap();
        assert!(links.iter().any(|link| link.url == "https://example.com/"));
        assert!(
            links
                .iter()
                .any(|link| link.url == "https://example.com/relative/path")
        );
        let offsite = links
            .iter()
            .find(|link| link.url == "https://other.example/x")
            .unwrap();
        assert!(offsite.offsite);
        assert_eq!(
            links
                .iter()
                .filter(|link| link.url == "https://example.com/")
                .count(),
            1
        );
    }

    #[test]
    fn link_count_cap_is_reported() {
        let html = format!(
            "<html><body>{}</body></html>",
            (0..10)
                .map(|index| format!("<a href=\"/p/{index}\">p</a>"))
                .collect::<String>()
        );
        let mut options = options(Mode::Text, 1000);
        options.include_links = true;
        options.max_links = 3;
        let extraction = extract_html(&html, &base(), &options).unwrap();
        assert_eq!(extraction.links.unwrap().len(), 3);
        assert!(extraction.links_truncated);
    }

    #[test]
    fn extract_truncation_cuts_strings_and_honours_the_record_budget() {
        let records = vec![
            serde_json::json!({"body": "x".repeat(20), "n": 7, "tags": ["y".repeat(9), "ok"]})
                .as_object()
                .unwrap()
                .clone(),
            serde_json::json!({"body": "second record"})
                .as_object()
                .unwrap()
                .clone(),
        ];
        let cut = truncate_records(records.clone(), 5, 10_000);
        assert_eq!(
            cut.truncated_fields, 3,
            "body, array element and second body were cut"
        );
        assert_eq!(cut.records[0]["body"], serde_json::json!("xxxxx"));
        assert_eq!(cut.records[0]["tags"], serde_json::json!(["yyyyy", "ok"]));
        assert_eq!(cut.records[0]["n"], serde_json::json!(7));
        assert!(!cut.budget_truncated);

        let budget = truncate_records(records, 100, 5);
        assert!(
            budget.records.is_empty(),
            "the first record exceeds the budget"
        );
        assert!(budget.budget_truncated);
        // Every returned record is well-formed JSON and the dropped one is not
        // silently half-serialized.
        let serialized = serde_json::to_string(&budget.records).unwrap();
        assert_eq!(serialized, "[]");
    }

    #[test]
    fn main_container_scopes_records_to_the_readability_article() {
        let raw = serde_json::json!({
            "container": ":main",
            "fields": { "body": { "selector": "p", "all": true } }
        });
        let validated = spec::validate(&raw).unwrap();
        let compiled = spec::compile(&validated.spec).unwrap();
        let eval = spec_eval::EvalOptions { max_records: 5_000 };
        let page = extract_page(
            PAGE,
            &base(),
            &options(Mode::Full, 200_000),
            Some((&compiled, &eval)),
        )
        .unwrap();
        assert_eq!(page.dom_parses, 2, "the article fragment is a second parse");
        let records = page.records.unwrap().records;
        let text = serde_json::to_string(&records).unwrap();
        assert!(text.contains("harbour lights"));
        assert!(
            !text.contains("newsletter"),
            "footer is outside the article"
        );
        assert!(!text.contains("Home"), "nav is outside the article");
        assert!(!text.contains("Elsewhere"), "nav is outside the article");
    }

    #[test]
    fn main_container_falls_back_to_the_document_when_readability_is_empty() {
        let raw = serde_json::json!({
            "container": ":main",
            "fields": { "only": { "selector": ".only" } }
        });
        let validated = spec::validate(&raw).unwrap();
        let compiled = spec::compile(&validated.spec).unwrap();
        let eval = spec_eval::EvalOptions { max_records: 5_000 };
        let page = extract_page(
            "<html><body><div class=\"only\"></div></body></html>",
            &base(),
            &options(Mode::Text, 1_000),
            Some((&compiled, &eval)),
        )
        .unwrap();
        let records = page.records.unwrap().records;
        // Readability finds no usable article text, so the scope falls back to
        // the document; the empty `.only` element is still selectable.
        assert_eq!(records[0]["only"], serde_json::json!(""));
    }

    #[test]
    fn near_duplicate_key_ignores_link_targets() {
        let first = "[next month](https://example.com/trap/1) and more prose";
        let second = "[next month](https://example.com/trap/999) and more prose";
        assert_eq!(
            near_duplicate_key(first),
            near_duplicate_key(second),
            "trap pages differ only by their link target"
        );
        let different = "[next month](https://example.com/trap/1) and other prose";
        assert_ne!(near_duplicate_key(first), near_duplicate_key(different));
        assert_eq!(
            near_duplicate_key("see https://example.com/a for details"),
            near_duplicate_key("see https://example.com/b for details")
        );
        assert_eq!(
            near_duplicate_key("plain   words\n\nhere"),
            near_duplicate_key("plain words here")
        );
    }

    #[test]
    fn plain_content_is_normalized_text() {
        let extraction = extract_plain("  a \n\n\n\n b \n", &options(Mode::Text, 100), false);
        assert_eq!(extraction.markdown, "a\n\nb");
        assert!(!extraction.truncated);
        assert!(extraction.links.is_none());
    }

    #[test]
    fn unreadable_markup_still_yields_text() {
        let html = "<html><body><p>no closing tags <b>bold";
        let extraction = extract_html(html, &base(), &options(Mode::Main, 1000)).unwrap();
        assert!(extraction.markdown.contains("no closing tags"));
    }

    #[test]
    fn mode_parsing_is_closed() {
        for mode in ["main", "full", "text", "raw"] {
            assert_eq!(Mode::parse(mode).unwrap().as_str(), mode);
        }
        let error = Mode::parse("everything").unwrap_err();
        assert_eq!(error.code(), crate::core::error::ErrorCode::InvalidUrl);
    }

    // -----------------------------------------------------------------------
    // B01 — one main-article source (IMP-DEC-020), one readability pass
    // -----------------------------------------------------------------------

    const SCOPE_PAGE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Scope fixture</title></head>
<body>
  <header><nav><a href="/">NAV_DECOY_HOME</a> <a href="/pricing">NAV_DECOY_PRICING</a></nav></header>
  <main><article>
    <h1>The harbour ledger</h1>
    <p>The harbourmaster logged forty-two identical crates on the pier.</p>
    <p>Second paragraph keeps the article substantial for readability.</p>
    <p>Third paragraph adds ordinary prose so the selection is not thin.</p>
    <p>Fourth paragraph continues with details about the tide tables.</p>
    <p>Fifth paragraph closes the record with the inspector's remarks.</p>
  </article></main>
  <footer><p>FOOTER_DECOY_NEWSLETTER</p></footer>
</body></html>"#;

    const SCOPE_PROSE: &str = "The harbourmaster logged forty-two identical crates";

    fn compiled(raw: serde_json::Value) -> (spec::ValidatedSpec, spec::CompiledSpec) {
        let validated = spec::validate(&raw).unwrap();
        let compiled = spec::compile(&validated.spec).unwrap();
        (validated, compiled)
    }

    fn eval_options() -> spec_eval::EvalOptions {
        spec_eval::EvalOptions { max_records: 100 }
    }

    #[test]
    fn main_body_and_main_scope_share_one_readability_pass() {
        pass_instrumentation::reset();
        let (_, compiled) = compiled(serde_json::json!({
            "container": ":main",
            "fields": { "body": { "selector": "*" } }
        }));

        let page = extract_page(
            SCOPE_PAGE,
            &base(),
            &options(Mode::Main, 100_000),
            Some((&compiled, &eval_options())),
        )
        .unwrap();

        assert_eq!(
            pass_instrumentation::count(),
            1,
            "one acquisition must cost exactly one readability pass"
        );
        assert_eq!(page.dom_parses, 2, "the article document is parsed once");
        assert!(page.main.markdown.contains(SCOPE_PROSE));

        let records = page.records.expect("records");
        assert_eq!(records.record_count, 1);
        let body = records.records[0]["body"].as_str().unwrap_or_default();
        assert!(body.contains(SCOPE_PROSE), "scope text: {body}");
        assert!(!body.contains("NAV_DECOY"), "chrome leaked: {body}");
        assert!(!body.contains("FOOTER_DECOY"), "footer leaked: {body}");
    }

    #[test]
    fn a_page_read_without_a_main_scope_skips_readability() {
        pass_instrumentation::reset();
        let (_, compiled) = compiled(serde_json::json!({
            "fields": { "text": { "selector": "p", "all": true } }
        }));
        let page = extract_page(
            SCOPE_PAGE,
            &base(),
            &options(Mode::Full, 100_000),
            Some((&compiled, &eval_options())),
        )
        .unwrap();
        assert_eq!(pass_instrumentation::count(), 0);
        assert_eq!(page.records.expect("records").record_count, 1);
    }
}
