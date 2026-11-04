//! v0.4 — structural-gap repair neutrality harness.
//!
//! The corpus is fixed here: the committed fixtures, a synthetic boilerplate
//! page, and the operator source captures when they are present. The captures
//! are not shipped with the repository, so `WEB_WIDOW_CORPUS_DIR` (default
//! `<repo>/corpus`) points at them and a missing capture is skipped rather
//! than failed.
//! `tests/fixtures/m04/corpus-baseline/` holds the **pre-repair** Markdown for
//! every page, produced by the candidate on the same bytes; this test diffs
//! the current output against that retained baseline.
//!
//! Rules asserted:
//! - every no-gap page is **byte-identical**;
//! - every diff has **zero removed lines** (additions only);
//! - only the sqlite.org page (and pages with a demonstrable gap) changes;
//! - the sqlite section 2 is restored in position within the 25 % bound;
//! - the boilerplate page adds nothing;
//! - monitor `normalized_hash` is unchanged for every no-gap page.

use std::path::PathBuf;

use web_widow::extract::{ExtractOptions, Mode, extract_html};
use web_widow::fetch::charset;
use web_widow::monitor::normalize;

const FIXTURES: &[&str] = &[
    "article-long",
    "cjk",
    "docs-page",
    "forum-thread",
    "huge-under-cap",
    "injection",
    "json-ld-article",
    "known-hard",
    "latin1-page",
    "malformed",
    "noscript-shell",
    "plain-adjacent",
    "product-listing",
    "scripts-and-styles",
    "section-headings",
    "table-heavy",
];

const REAL_PAGES: &[&str] = &[
    "example-com",
    "gnu-free-sw",
    "iana-example-domains",
    "mdn-http-overview",
    "python-about",
    "rust-book-ch01",
    "rust-lang",
    "sqlite-whentouse",
    "utctime",
    "w3c-accessibility",
];

/// Pages whose pre-repair output has a demonstrable structural gap. Every other
/// corpus page must be byte-identical.
const GAP_PAGES: &[&str] = &["real_sqlite-whentouse"];

struct Page {
    key: String,
    path: PathBuf,
    base: &'static str,
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Operator captures that are not part of the committed fixture set.
fn corpus_dir() -> PathBuf {
    std::env::var("WEB_WIDOW_CORPUS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| root().join("corpus"))
}

/// The gap page's source capture, when the operator corpus is present. Two
/// assertions below describe that page only, so they skip without it.
fn sqlite_capture() -> Option<PathBuf> {
    let path = corpus_dir()
        .join("real-site/source")
        .join("sqlite-whentouse.html");
    path.is_file().then_some(path)
}

fn pages() -> Vec<Page> {
    let root = root();
    let mut pages = Vec::new();
    for name in FIXTURES {
        pages.push(Page {
            key: format!("fixture_{name}"),
            path: root.join("tests/fixtures").join(format!("{name}.html")),
            base: "https://example.com/posts/one",
        });
    }
    for name in REAL_PAGES {
        let path = corpus_dir()
            .join("real-site/source")
            .join(format!("{name}.html"));
        if !path.is_file() {
            eprintln!("SKIP: no operator capture for {name}");
            continue;
        }
        pages.push(Page {
            key: format!("real_{name}"),
            path,
            base: "https://example.com/",
        });
    }
    let chainalysis = corpus_dir().join("extraction-fidelity/chainalysis-source.html");
    if chainalysis.is_file() {
        pages.push(Page {
            key: "chainalysis".to_string(),
            path: chainalysis,
            base: "https://www.chainalysis.com/",
        });
    }
    pages.push(Page {
        key: "boilerplate".to_string(),
        path: root.join("tests/fixtures/m04/boilerplate.html"),
        base: "https://example.com/posts/one",
    });
    pages
}

fn options() -> ExtractOptions {
    ExtractOptions {
        mode: Mode::Main,
        max_chars: 10_000_000,
        max_links: 0,
        include_links: false,
        ignore_selectors: Vec::new(),
    }
}

fn baseline(key: &str) -> String {
    std::fs::read_to_string(root().join(format!("tests/fixtures/m04/corpus-baseline/{key}.md")))
        .unwrap_or_else(|error| panic!("missing baseline for {key}: {error}"))
}

/// Evidence hook: `WEB_WIDOW_CORPUS_DUMP_DIR=<dir>` writes every post-repair
/// page there so the corpus diff can be retained next to the run.
fn maybe_dump(key: &str, text: &str) {
    let Ok(dir) = std::env::var("WEB_WIDOW_CORPUS_DUMP_DIR") else {
        return;
    };
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(PathBuf::from(dir).join(format!("{key}.md")), text).unwrap();
}

fn extract(path: &PathBuf, base: &str) -> String {
    let bytes = std::fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let html = charset::decode(&bytes, Some("text/html")).text;
    let base = url::Url::parse(base).unwrap();
    let markdown = extract_html(&html, &base, &options())
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .markdown;
    maybe_dump(
        path.file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_default()
            .as_str(),
        &markdown,
    );
    markdown
}

/// `haystack` contains every line of `needle`, in order (zero removed lines).
fn is_supersequence(needle: &str, haystack: &str) -> bool {
    let mut lines = haystack.lines();
    needle
        .lines()
        .all(|wanted| lines.any(|candidate| candidate == wanted))
}

#[test]
fn every_page_is_unchanged_or_adds_lines_only() {
    if sqlite_capture().is_none() {
        eprintln!("SKIP: no operator capture for sqlite-whentouse");
        return;
    }
    let mut changed = Vec::new();
    for page in pages() {
        let before = baseline(&page.key);
        let after = extract(&page.path, page.base);
        assert!(
            is_supersequence(&before, &after),
            "{} removed lines from {}\n=== before ===\n{before}\n=== after ===\n{after}",
            page.key,
            page.key
        );
        if before != after {
            changed.push(page.key.clone());
        }
    }
    assert_eq!(
        changed,
        GAP_PAGES
            .iter()
            .map(|key| key.to_string())
            .collect::<Vec<_>>(),
        "only pages with a demonstrable structural gap may change"
    );
}

#[test]
fn sqlite_section_two_is_restored_in_position_and_within_the_bound() {
    if sqlite_capture().is_none() {
        eprintln!("SKIP: no operator capture for sqlite-whentouse");
        return;
    }
    let page = pages()
        .into_iter()
        .find(|page| page.key == "real_sqlite-whentouse")
        .expect("sqlite page");
    let before = baseline("real_sqlite-whentouse");
    let after = extract(&page.path, page.base);

    let section_two = "## 2. Situations Where A Client/Server RDBMS May Work Better";
    assert!(
        after.contains(section_two),
        "section 2 heading missing:\n{after}"
    );
    assert!(
        after.contains("If there are many client programs sending SQL to the same"),
        "first restored sentence missing"
    );
    assert!(
        after.contains("A good rule of thumb is to avoid using SQLite"),
        "second restored sentence missing"
    );
    assert_eq!(
        after.matches(section_two).count(),
        1,
        "the heading must be restored exactly once"
    );

    let section_one = after
        .find("## 1. Situations Where SQLite Works Well")
        .unwrap();
    let restored = after.find(section_two).unwrap();
    let section_three = after
        .find("## 3. Checklist For Choosing The Right Database Engine")
        .unwrap();
    assert!(
        section_one < restored && restored < section_three,
        "section 2 must sit between sections 1 and 3"
    );

    let added = after.chars().count() - before.chars().count();
    let delivered = before.chars().count();
    assert!(added > 0, "the page must gain the dropped section");
    assert!(
        added * 100 <= delivered * 25,
        "inserted {added} chars exceeds 25 % of {delivered}"
    );
}

#[test]
fn the_boilerplate_page_adds_nothing() {
    let page = pages()
        .into_iter()
        .find(|page| page.key == "boilerplate")
        .expect("boilerplate page");
    assert_eq!(
        extract(&page.path, page.base),
        baseline("boilerplate"),
        "nav/footer/subscribe/teaser blocks must not be admitted"
    );
}

#[test]
fn no_gap_pages_keep_their_monitor_normalized_hash() {
    for page in pages() {
        if GAP_PAGES.contains(&page.key.as_str()) {
            continue;
        }
        let before = normalize::normalized_hash(&normalize::normalize(&baseline(&page.key)));
        let after_text = extract(&page.path, page.base);
        let after = normalize::normalized_hash(&normalize::normalize(&after_text));
        assert_eq!(
            before, after,
            "{} monitor text hash changed on a no-gap page",
            page.key
        );
    }
}
