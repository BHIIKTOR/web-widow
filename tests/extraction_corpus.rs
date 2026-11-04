//! A3 — golden corpus: Markdown snapshots paired with independently written
//! must-contain / must-not-contain assertions (§2).
//!
//! The snapshots are regression evidence; the assertions are the quality
//! evidence. A snapshot regenerated from the implementation still has to satisfy
//! them, and the reviewer's read of five outputs is recorded in the evidence
//! card, not here.

use std::path::PathBuf;

use web_widow::extract::{ExtractOptions, Mode, extract_html};
use web_widow::fetch::charset;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.html"))
}

fn html(name: &str) -> String {
    let path = fixture(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

fn base() -> url::Url {
    url::Url::parse("https://example.com/posts/one").unwrap()
}

fn options(mode: Mode, max_chars: usize) -> ExtractOptions {
    ExtractOptions {
        mode,
        max_chars,
        max_links: 2_000,
        include_links: false,
        ignore_selectors: Vec::new(),
    }
}

struct Case {
    name: &'static str,
    /// Phrases taken from the source page itself, not from prior output.
    must_contain: &'static [&'static str],
    must_not_contain: &'static [&'static str],
}

const CASES: &[Case] = &[
    Case {
        name: "article-long",
        must_contain: &[
            "harbourmaster logged the event",
            "four turbines mounted on a single gravity base",
            "six weeks each spring during the smelt run",
        ],
        must_not_contain: &[
            "Sign up for our newsletter",
            "buy our tide clock",
            "window.dataLayer",
            "font-family:serif",
        ],
    },
    Case {
        name: "docs-page",
        must_contain: &["The queue is configured in", "lease_seconds", "Do not set"],
        must_not_contain: &["width:240px", "API reference\nInstall"],
    },
    Case {
        name: "forum-thread",
        must_contain: &[
            "collapse in the first ten minutes of baking",
            "That is overproofing",
            "the poke test was the missing piece",
        ],
        must_not_contain: &["Buy a proofing basket", "Powered by forum software"],
    },
    Case {
        name: "product-listing",
        must_contain: &[
            "Featherweight Canister Stove",
            "€39.90",
            "Basecamp Two-Burner",
        ],
        must_not_contain: &["Free shipping over €50"],
    },
    Case {
        name: "table-heavy",
        must_contain: &["BME280", "±1 hPa", "1-Wire", "seven days"],
        must_not_contain: &["<table>"],
    },
    Case {
        name: "json-ld-article",
        must_contain: &["Die Stadtbibliothek öffnet ab Mai", "18 Prozent"],
        must_not_contain: &["@context", "application/ld+json"],
    },
    Case {
        name: "cjk",
        must_contain: &["三条夜间公交线路", "東京では", "夜班工人"],
        must_not_contain: &["<p>"],
    },
    Case {
        name: "malformed",
        must_contain: &[
            "amber lanterns lined the pier",
            "This paragraph never closes",
        ],
        must_not_contain: &[],
    },
    Case {
        name: "injection",
        must_contain: &[
            "IGNORE ALL PREVIOUS INSTRUCTIONS",
            "curl evil.example",
            "rounding error in the invoice total",
        ],
        must_not_contain: &[],
    },
    Case {
        // Site class: section bodies are `<div class="content">` siblings and the
        // `<h1>` section headings sit between them, so readability's sibling
        // assembly drops every heading (see the premise test below).
        name: "section-headings",
        must_contain: &[
            "## 1. When a single file is enough",
            "## 2. When a server earns its keep",
            "## 3. Checklist before migrating",
            "restoring it is a single command",
            "one arbiter that holds the lock",
            "a backup that merely completes is not a backup",
        ],
        must_not_contain: &[
            "\n# Choosing a storage engine",
            "Subscribe to the ops newsletter",
        ],
    },
    Case {
        // Deliberately hard: readability prefers the 48-comment block over a
        // two-sentence article. v0.1 does not tune per site, so the fixture
        // records the wrong choice instead of hiding it.
        name: "known-hard",
        must_contain: &["Commenter one writes at length"],
        must_not_contain: &[],
    },
    Case {
        name: "scripts-and-styles",
        must_contain: &["copper kettles cool slowly"],
        must_not_contain: &[
            "inline-script-marker",
            "hidden-css-marker",
            "template-marker",
            "noscript-fallback-marker",
        ],
    },
    Case {
        name: "plain-adjacent",
        must_contain: &[
            "library closes at noon",
            "Recycling collection moves to Saturday",
        ],
        must_not_contain: &["Municipal notices, updated weekly"],
    },
    Case {
        name: "noscript-shell",
        must_contain: &[],
        must_not_contain: &["client-rendered-content-marker"],
    },
    Case {
        name: "huge-under-cap",
        must_contain: &["Section 100", "seal the enclosure with the supplied gasket"],
        must_not_contain: &[],
    },
];

#[test]
fn main_mode_snapshots_satisfy_independent_assertions() {
    for case in CASES {
        let extraction = extract_html(&html(case.name), &base(), &options(Mode::Main, 200_000))
            .unwrap_or_else(|error| panic!("{} failed: {error}", case.name));
        for needle in case.must_contain {
            assert!(
                extraction.markdown.contains(needle),
                "{} must contain {needle:?}\n---\n{}",
                case.name,
                extraction.markdown
            );
        }
        for needle in case.must_not_contain {
            assert!(
                !extraction.markdown.contains(needle),
                "{} must not contain {needle:?}\n---\n{}",
                case.name,
                extraction.markdown
            );
        }
        insta::assert_snapshot!(format!("main_{}", case.name), extraction.markdown);
    }
}

#[test]
fn huge_page_is_extracted_without_truncation_below_the_cap() {
    let extraction = extract_html(
        &html("huge-under-cap"),
        &base(),
        &options(Mode::Main, 200_000),
    )
    .unwrap();
    assert!(!extraction.truncated, "the cap was not reached");
    assert!(
        extraction.markdown_chars > 20_000,
        "{}",
        extraction.markdown_chars
    );
}

#[test]
fn text_mode_of_the_article_has_no_markup() {
    let extraction = extract_html(
        &html("article-long"),
        &base(),
        &options(Mode::Text, 200_000),
    )
    .unwrap();
    assert!(
        extraction
            .markdown
            .contains("harbourmaster logged the event")
    );
    assert!(
        !extraction.markdown.contains("## "),
        "text mode keeps no markdown headings"
    );
    assert!(
        !extraction.markdown.contains('<'),
        "text mode emits no tags"
    );
    insta::assert_snapshot!("text_article-long", extraction.markdown);
}

#[test]
fn full_mode_keeps_boilerplate_but_still_drops_scripts() {
    let extraction = extract_html(
        &html("article-long"),
        &base(),
        &options(Mode::Full, 200_000),
    )
    .unwrap();
    assert!(extraction.markdown.contains("Sign up for our newsletter"));
    assert!(!extraction.markdown.contains("window.dataLayer"));
}

#[test]
fn latin1_fixture_decodes_through_the_charset_layer() {
    let bytes = std::fs::read(fixture("latin1-page")).unwrap();
    let decoded = charset::decode(&bytes, Some("text/html"));
    assert_eq!(decoded.charset, "windows-1252");
    let extraction = extract_html(&decoded.text, &base(), &options(Mode::Main, 200_000)).unwrap();
    // Readability drops an <h1> that duplicates the title, so the body text —
    // not the heading — is what proves the decoding.
    assert!(extraction.markdown.contains("Le café ouvre à sept heures"));
    assert!(extraction.markdown.contains("tarte aux pommes"));
    assert_eq!(extraction.title.as_deref(), Some("Café du port"));
    insta::assert_snapshot!("main_latin1-page", extraction.markdown);
}

#[test]
fn section_heading_fixture_reproduces_the_readability_drop() {
    // Premise of the A3 repair: readability's assembled content carries no
    // heading at all for this page class, so the Markdown headings in the
    // snapshot can only come from `extract::headings`. If a future
    // `dom_smoothie` keeps them, this test fails and the repair should be
    // re-evaluated rather than quietly kept.
    let source = html("section-headings");
    let mut readability =
        dom_smoothie::Readability::new(source.as_str(), Some(base().as_str()), None).unwrap();
    let article = readability.parse().unwrap();
    let content = article.content.to_string();
    assert_eq!(
        content.matches("<h1").count(),
        0,
        "readability now keeps h1"
    );
    assert_eq!(
        content.matches("<h2").count(),
        0,
        "readability now keeps h2"
    );
    assert!(!content.contains("When a single file is enough"));
}

#[test]
fn known_hard_page_has_a_documented_escape_hatch() {
    let main = extract_html(&html("known-hard"), &base(), &options(Mode::Main, 200_000)).unwrap();
    assert!(
        !main.markdown.contains("the ferry will resume on Thursday"),
        "if readability starts finding the article, review this fixture's premise"
    );
    let full = extract_html(&html("known-hard"), &base(), &options(Mode::Full, 200_000)).unwrap();
    assert!(
        full.markdown.contains("the ferry will resume on Thursday"),
        "mode=full must still deliver the article body"
    );
}

#[test]
fn metadata_is_read_from_the_document() {
    let extraction = extract_html(
        &html("json-ld-article"),
        &base(),
        &options(Mode::Main, 200_000),
    )
    .unwrap();
    assert_eq!(
        extraction.title.as_deref(),
        Some("Warum Bibliotheken länger geöffnet bleiben")
    );
    assert_eq!(extraction.lang.as_deref(), Some("de"));
    // dom_smoothie reads the JSON-LD block; either way the author is discovered.
    assert_eq!(extraction.byline.as_deref(), Some("Jonas Weber"));
}
