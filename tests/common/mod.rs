//! Shared test scaffolding: a scripted fixture HTTP server on loopback and a
//! pipeline builder pointing at a temporary data directory.
//!
//! The fixture server keys its behaviour on the `Host` header so one listener
//! can serve several "sites" (127.0.0.1 .. 127.0.0.9) with different robots.txt
//! policies, and records every request so a test can assert what was *not*
//! fetched.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use web_widow::config::{Config, Paths};
use web_widow::crawl::CrawlManager;
use web_widow::fetch::gate::{HostGate, PolitenessConfig};
use web_widow::fetch::http::HttpClient;
use web_widow::fetch::robots::RobotsService;
use web_widow::fetch::ssrf::{AllowList, SsrfGate};
use web_widow::fts::ContentIndex;
use web_widow::monitor::MonitorManager;
use web_widow::pipeline::Pipeline;
use web_widow::render::{self, Renderer};
use web_widow::store::Store;

pub const ARTICLE_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Fixture Article</title>
  <meta name="author" content="Test Author">
  <meta property="og:site_name" content="Fixture Site">
  <meta property="article:published_time" content="2026-02-03T04:05:06Z">
  <style>.x{color:red}</style>
  <script>window.secret = "script-should-not-appear";</script>
</head>
<body>
  <header><nav><a href="/">Home</a></nav></header>
  <main>
    <article>
      <h1>Fixture Article</h1>
      <p>The distinctive sentence is that the lighthouse keeper counted forty-one gulls.</p>
      <p>Second paragraph with a <a href="/relative">relative link</a> and more prose.</p>
    </article>
  </main>
  <footer>Subscribe to our newsletter</footer>
</body>
</html>"#;

/// extraction page: two cards and relative links.
pub const EXTRACT_ITEMS_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Items</title></head>
<body>
  <div class="item"><h2>First item</h2><span class="price">€10.00</span>
    <a href="../next">next</a></div>
  <div class="item"><h2>Second item</h2><span class="price">€20.50</span>
    <a href="/absolute">absolute</a></div>
</body></html>"#;

/// SERP fixtures: the real 2026-10-30 captures plus the synthetic ad and
/// anomaly pages. Expectations live in the fixture tests, not here.
pub const SERP_HTML_RESULTS: &str = include_str!("../fixtures/serp/html-results.html");
pub const SERP_LITE_RESULTS: &str = include_str!("../fixtures/serp/lite-results.html");
pub const SERP_HTML_EMPTY: &str = include_str!("../fixtures/serp/html-empty.html");
pub const SERP_HTML_ANOMALY: &str = include_str!("../fixtures/serp/html-anomaly.html");
pub const SERP_HTML_AD: &str = include_str!("../fixtures/serp/html-ad.html");
pub const SERP_LITE_AD: &str = include_str!("../fixtures/serp/lite-ad.html");

/// v0.4 feed fixtures (RSS 2.0, Atom, JSON Feed, duplicate ids, many items
/// with long summaries, malformed XML and an entity bomb).
pub const FEED_RSS: &str = include_str!("../fixtures/m04/feed/rss.xml");
pub const FEED_ATOM: &str = include_str!("../fixtures/m04/feed/atom.xml");
pub const FEED_JSON: &str = include_str!("../fixtures/m04/feed/json-feed.json");
pub const FEED_DUPES: &str = include_str!("../fixtures/m04/feed/dupes.xml");
pub const FEED_MANY: &str = include_str!("../fixtures/m04/feed/many.xml");
pub const FEED_MALFORMED: &str = include_str!("../fixtures/m04/feed/malformed.xml");
pub const FEED_BOMB: &str = include_str!("../fixtures/m04/feed/bomb.xml");

/// v0.4 fixture: nav/aside/footer decoys around one article, for the shared
/// `:main` source and `selector:"*"` record-scope semantics.
pub const ARTICLE_SCOPE: &str = include_str!("../fixtures/m04/article-scope.html");

/// v0.5 fixtures: the BUYALFA verbatim shape (redirect to
/// `/gz/account-verification`, 28 words), a direct login page (no drift) and a
/// short article (no verification segment), plus the feed identity fixtures.
pub const INTERSTITIAL_VERIFICATION: &str =
    include_str!("../fixtures/m05/interstitial-verification.html");
pub const SHORT_ARTICLE: &str = include_str!("../fixtures/m05/interstitial-short-article.html");
pub const FEED_CANONICAL: &str = include_str!("../fixtures/m05/feed-canonical.xml");

/// v0.3 fixtures: visible text, caps, `:main`, and a rendered thin shell.
pub const INLINE_SCRIPT: &str = include_str!("../fixtures/m03/article-with-inline-script.html");
pub const CNBC_BODY: &str = include_str!("../fixtures/m03/cnbc-class-body.html");
pub const NAV: &str = include_str!("../fixtures/m03/article-with-nav.html");
pub const THIN_SHELL: &str = include_str!("../fixtures/m03/thin-rendered-shell.html");
pub const LONG_FIELD: &str = include_str!("../fixtures/m03/long-field.html");
pub const NO_ARTICLE: &str = include_str!("../fixtures/m03/no-article.html");

///: `noscript`-only shell; the static DOM carries under 50 words while the
/// script injects a 60-word article.
pub const NOSCRIPT_APP_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>JS App</title></head>
<body>
  <noscript>This application requires JavaScript to display its content.</noscript>
  <div id="app">loading</div>
  <script>
    document.getElementById('app').innerHTML =
      '<article><h1>Rendered Article</h1><p>' +
      Array(60).fill('injectedword').join(' ') + '</p></article>';
  </script>
</body></html>"#;

///: the final marker is only written after four sequential fetches, so a
/// fixed 500 ms sleep after `load` would stop before it.
pub const ASYNC_APP_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Async App</title></head>
<body>
  <div id="app">loading</div>
  <script>
    (async () => {
      for (let i = 1; i <= 4; i++) {
        await new Promise(resolve => setTimeout(resolve, 150));
        const data = await fetch('/api/data?step=' + i).then(r => r.json());
        document.getElementById('app').textContent = data.value + ' ' + i;
      }
      const final = document.createElement('p');
      final.id = 'final';
      final.textContent = 'ASYNC_FINAL_MARKER';
      document.getElementById('app').appendChild(final);
    })();
  </script>
</body></html>"#;

/// resource-blocking fixture: image, font and stylesheet are blocked at the
/// CDP layer; the XHR must still be fetched.
pub const RESOURCES_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Resources</title>
  <link rel="stylesheet" href="/js/asset/style.css">
  <style>@font-face { font-family: blockedfont; src: url('/js/asset/font.woff2'); }
         body { font-family: blockedfont; }</style>
</head><body>
  <img src="/js/asset/image.png" alt="blocked">
  <p>resource text</p>
  <div id="data">waiting</div>
  <script>
    fetch('/api/data').then(r => r.json())
      .then(d => { document.getElementById('data').textContent = d.value; });
  </script>
</body></html>"#;

#[derive(Clone, Default)]
pub struct RequestLog {
    entries: Arc<Mutex<Vec<String>>>,
    /// (arrival time, entry) pairs for interval assertions.
    stamped: Arc<Mutex<Vec<(std::time::Instant, String)>>>,
}

impl RequestLog {
    fn record(&self, entry: String) {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(entry.clone());
        self.stamped
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((std::time::Instant::now(), entry));
    }

    /// Arrival times of requests whose log entry contains `needle`.
    pub fn stamps_matching(&self, needle: &str) -> Vec<std::time::Instant> {
        self.stamped
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(_, entry)| entry.contains(needle))
            .map(|(at, _)| *at)
            .collect()
    }

    pub fn entries(&self) -> Vec<String> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn clear(&self) {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.stamped
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    pub fn count_matching(&self, needle: &str) -> usize {
        self.entries()
            .iter()
            .filter(|entry| entry.contains(needle))
            .count()
    }

    pub fn any_matching(&self, needle: &str) -> bool {
        self.count_matching(needle) > 0
    }
}

#[derive(Clone)]
struct StoredPage {
    body: String,
    etag: String,
}

#[derive(Clone)]
struct FixtureState {
    log: RequestLog,
    retry_after_remaining: Arc<AtomicUsize>,
    flaky_5xx_remaining: Arc<AtomicUsize>,
    /// Mutable pages for monitor tests: `/monitor/*` and `/js/monitor`. A
    /// `set_page` call changes the ETag, so the next check is a 200 with a new
    /// body even when the text is identical.
    pages: Arc<Mutex<HashMap<String, StoredPage>>>,
    page_counter: Arc<AtomicUsize>,
    /// Mutable JSON value served by `/api/monitor-value`: the page shell keeps
    /// its ETag, so only a rendered monitor can see the change.
    api_value: Arc<Mutex<String>>,
}

pub struct Fixture {
    pub port: u16,
    pub log: RequestLog,
    state: FixtureState,
    _handle: tokio::task::JoinHandle<()>,
    _handle_v6: Option<tokio::task::JoinHandle<()>>,
}

impl Fixture {
    pub fn url(&self, host: &str, path: &str) -> String {
        format!("http://{host}:{}{path}", self.port)
    }

    /// Replace the value returned by `/api/monitor-value`.
    pub fn set_api_value(&self, value: &str) {
        *self.state.api_value.lock().unwrap() = value.to_string();
    }

    /// Replace a monitor page's body. The ETag changes, so a check sees a 200.
    pub fn set_page(&self, path: &str, body: &str) {
        self.state.pages.lock().unwrap().insert(
            path.to_string(),
            StoredPage {
                body: body.to_string(),
                etag: format!(
                    "\"p{}\"",
                    self.state.page_counter.fetch_add(1, Ordering::SeqCst)
                ),
            },
        );
    }
}

pub async fn start_fixture() -> Fixture {
    let state = FixtureState {
        log: RequestLog::default(),
        retry_after_remaining: Arc::new(AtomicUsize::new(2)),
        flaky_5xx_remaining: Arc::new(AtomicUsize::new(1)),
        pages: Arc::new(Mutex::new(HashMap::new())),
        page_counter: Arc::new(AtomicUsize::new(0)),
        api_value: Arc::new(Mutex::new("one".to_string())),
    };
    let app = Router::new()
        .fallback(any(handle))
        .with_state(state.clone());
    // 0.0.0.0 so 127.0.0.1 .. 127.0.0.9 all reach the same listener.
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app_v6 = app.clone();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // `localhost` commonly resolves to ::1 first; serve that family on the same
    // port too, so a hostname test does not pay a refused-connection round trip
    // (which can exceed a short total timeout on a loaded machine). Tolerate a
    // host without IPv6 loopback.
    let handle_v6 = match tokio::net::TcpListener::bind(("::1", port)).await {
        Ok(listener) => Some(tokio::spawn(async move {
            let _ = axum::serve(listener, app_v6).await;
        })),
        Err(_) => None,
    };
    // Give axum a moment to start accepting.
    tokio::time::sleep(Duration::from_millis(20)).await;
    Fixture {
        port,
        log: state.log.clone(),
        state,
        _handle: handle,
        _handle_v6: handle_v6,
    }
}

fn feed_response(body: &str, content_type: &str) -> Response {
    Response::builder()
        .header(
            header::CONTENT_TYPE,
            format!("{content_type}; charset=utf-8"),
        )
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn handle(State(state): State<FixtureState>, request: Request<Body>) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("127.0.0.1")
        .split(':')
        .next()
        .unwrap_or("127.0.0.1")
        .to_string();
    // Routing uses the path; the log keeps the query so variants are
    // distinguishable in assertions.
    let path = request.uri().path().to_string();
    let logged = match request.uri().query() {
        Some(query) => format!("{path}?{query}"),
        None => path.clone(),
    };
    state.log.record(format!("{host}{logged}"));
    let headers = request.headers().clone();

    if path == "/robots.txt" {
        return robots_for(&host);
    }

    // Monitor fixtures are mutable and ETag-aware; tests replace their body.
    if path.starts_with("/monitor/") || path == "/js/monitor" {
        let stored = state
            .pages
            .lock()
            .unwrap()
            .get(&path)
            .map(|page| (page.body.clone(), page.etag.clone()));
        if let Some((body, etag)) = stored {
            if headers
                .get(header::IF_NONE_MATCH)
                .and_then(|value| value.to_str().ok())
                == Some(etag.as_str())
            {
                return Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(header::ETAG, etag)
                    .body(Body::empty())
                    .unwrap();
            }
            return Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .header(header::ETAG, etag)
                .body(Body::from(body))
                .unwrap();
        }
    }

    match path.as_str() {
        "/article" => {
            let etag = "\"v1\"";
            if headers
                .get(header::IF_NONE_MATCH)
                .and_then(|value| value.to_str().ok())
                == Some(etag)
            {
                return Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(header::ETAG, etag)
                    .body(Body::empty())
                    .unwrap();
            }
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .header(header::ETAG, etag)
                .body(Body::from(ARTICLE_HTML))
                .unwrap()
        }
        // v0.5: the BUYALFA verbatim shape — a product path redirecting to
        // the whole verification segment, a direct login page (no drift), and a
        // short article with no verification path.
        "/p/MLM26030984" => Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, "/gz/account-verification")
            .body(Body::empty())
            .unwrap(),
        "/gz/account-verification" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(INTERSTITIAL_VERIFICATION))
            .unwrap(),
        "/login" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(INTERSTITIAL_VERIFICATION))
            .unwrap(),
        "/short-article" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(SHORT_ARTICLE))
            .unwrap(),
        "/plain" => Response::builder()
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from("plain text body\n\nsecond paragraph\n"))
            .unwrap(),
        "/json" => Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"answer":42,"nested":{"ok":true}}"#))
            .unwrap(),
        "/xml" => Response::builder()
            .header(header::CONTENT_TYPE, "application/xml")
            .body(Body::from(
                "<?xml version=\"1.0\"?><root><item>one</item></root>",
            ))
            .unwrap(),
        "/pdf" => Response::builder()
            .header(header::CONTENT_TYPE, "application/pdf")
            .body(Body::from("%PDF-1.7 not really a pdf"))
            .unwrap(),
        "/latin1" => {
            let mut body = Vec::new();
            body.extend_from_slice(br#"<html><head><meta charset="iso-8859-1"><title>Caf"#);
            body.extend_from_slice(&[0xE9]);
            body.extend_from_slice(br#"</title></head><body><p>na"#);
            body.extend_from_slice(&[0xEF]);
            body.extend_from_slice(br#"ve caf"#);
            body.extend_from_slice(&[0xE9]);
            body.extend_from_slice(b"</p></body></html>");
            Response::builder()
                .header(header::CONTENT_TYPE, "text/html")
                .body(Body::from(body))
                .unwrap()
        }
        "/medium" => {
            // ~200 KiB rendered page for the RSS check.
            let mut body = String::with_capacity(210_000);
            body.push_str("<html><head><title>Medium Page</title></head><body><h1>Medium</h1>");
            for index in 0..1_700 {
                body.push_str(&format!(
                    "<p>Paragraph {index}: the rendering stress fixture keeps a stable amount of prose so memory growth is visible.</p>"
                ));
            }
            body.push_str("</body></html>");
            Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Body::from(body))
                .unwrap()
        }
        "/large" => {
            // ~100 KiB page for the performance smoke run.
            let mut body = String::with_capacity(110_000);
            body.push_str(
                "<html><head><title>Large Page</title></head><body><article><h1>Large</h1>",
            );
            for index in 0..200 {
                body.push_str(&format!(
                    "<p>Paragraph {index}: the quick brown fox jumps over the lazy dog while the fixture server counts bytes.</p>"
                ));
            }
            body.push_str("</article></body></html>");
            Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Body::from(body))
                .unwrap()
        }
        "/oversize" => {
            let body = vec![b'x'; 11 * 1024 * 1024];
            Response::builder()
                .header(header::CONTENT_TYPE, "text/html")
                .body(Body::from(body))
                .unwrap()
        }
        "/many-headers" => {
            let mut builder = Response::builder().header(header::CONTENT_TYPE, "text/html");
            for index in 0..90 {
                builder = builder.header(format!("x-fixture-{index}"), "y".repeat(900));
            }
            builder
                .body(Body::from("<html><body>ok</body></html>"))
                .unwrap()
        }
        //: a small linked site, duplicate variants, redirect chains, a
        // 404, a 500-once page, a robots-disallowed subtree and a calendar trap.
        "/site/" => {
            let mut body = String::from(
                "<html><body><h1>Crawl fixture root</h1><p>The seeded page of the crawl test site with enough words to be readable.</p>",
            );
            for index in 1..=20 {
                body.push_str(&format!(
                    "<a href=\"/site/p{index}\">page {index}</a> "
                ));
            }
            body.push_str("<a href=\"/site/page/2\">more</a> ");
            body.push_str("<a href=\"/site/dup-a\">dup a</a> ");
            body.push_str("<a href=\"/site/dup-b\">dup b</a> ");
            body.push_str("<a href=\"/site/dup-c\">dup c</a> ");
            body.push_str("<a href=\"/site/redirect/1\">redirect chain</a> ");
            body.push_str("<a href=\"/site/404\">missing</a> ");
            body.push_str("<a href=\"/site/500-once\">flaky</a> ");
            body.push_str("<a href=\"/private/secret\">private</a> ");
            body.push_str("<a href=\"/site/redirect-private\">redirect to private</a> ");
            body.push_str("</body></html>");
            Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Body::from(body))
                .unwrap()
        }
        "/site/page/2" => {
            let mut body = String::from(
                "<html><body><h1>Second page of results</h1><p>Pagination page for the crawl fixture with distinct prose.</p>",
            );
            for index in 21..=40 {
                body.push_str(&format!(
                    "<a href=\"/site/p{index}\">page {index}</a> "
                ));
            }
            body.push_str("</body></html>");
            Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Body::from(body))
                .unwrap()
        }
        path if path.starts_with("/site/p") => {
            let index: u32 = path
                .trim_start_matches("/site/p")
                .parse()
                .unwrap_or_default();
            let body = format!(
                "<html><body><h1>Fixture page {index}</h1><p>Distinct body text for fixture page number {index} in the crawl site.</p><a href=\"/site/\">back</a></body></html>"
            );
            Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Body::from(body))
                .unwrap()
        }
        "/site/variant" | "/site/variant/" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(
                "<html><body><h1>Variant page</h1><p>One canonical resource reachable through tracking-parameter and query-order variants.</p></body></html>",
            ))
            .unwrap(),
        "/site/dup-a" | "/site/dup-b" | "/site/dup-c" => {
            let body = "<html><body><h1>Duplicate variant</h1><p>These three URLs serve byte-identical content so the near-duplicate rule can collapse them, and each links onward.</p><a href=\"/site/dup-next\">onward</a></body></html>";
            Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Body::from(body))
                .unwrap()
        }
        "/site/dup-next" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(
                "<html><body><h1>Onward page</h1><p>This page must not be fetched when the duplicate rule fires.</p></body></html>",
            ))
            .unwrap(),
        "/site/redirect/1" => Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, "/site/redirect/2")
            .body(Body::empty())
            .unwrap(),
        "/site/redirect/2" => Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, "/site/p1")
            .body(Body::empty())
            .unwrap(),
        "/site/redirect-private" => Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, "/private/secret")
            .body(Body::empty())
            .unwrap(),
        "/site/500-once" => {
            let remaining = state.flaky_5xx_remaining.load(Ordering::SeqCst);
            if remaining > 0 {
                state
                    .flaky_5xx_remaining
                    .store(remaining - 1, Ordering::SeqCst);
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::empty())
                    .unwrap()
            } else {
                Response::builder()
                    .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                    .body(Body::from(
                        "<html><body><h1>Recovered</h1><p>The flaky page answered on a later attempt.</p></body></html>",
                    ))
                    .unwrap()
            }
        }
        path if path.starts_with("/trap/") => {
            // Every trap URL serves the same content and links to the next one:
            // only the near-duplicate rule stops this, not the page budget.
            let index: u32 = path.trim_start_matches("/trap/").parse().unwrap_or_default();
            let body = format!(
                "<html><body><h1>Calendar trap</h1><p>Every one of these URLs has the same content, so following the next link would run forever.</p><a href=\"/trap/{}\">next month</a></body></html>",
                index + 1
            );
            Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Body::from(body))
                .unwrap()
        }
        "/bulk/" => {
            let mut body = String::from(
                "<html><body><h1>Bulk index</h1><p>Index of five hundred distinct pages for the latency fixture.</p>",
            );
            for index in 0..500 {
                body.push_str(&format!("<a href=\"/bulk/{index}\">item {index}</a> "));
            }
            body.push_str("</body></html>");
            Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Body::from(body))
                .unwrap()
        }
        path if path.starts_with("/bulk/") => {
            let index: u32 = path.trim_start_matches("/bulk/").parse().unwrap_or_default();
            let body = format!(
                "<html><body><h1>Bulk item {index}</h1><p>Unique paragraph for bulk item number {index}, used to measure crawl page latency.</p></body></html>"
            );
            Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Body::from(body))
                .unwrap()
        }
        "/crawl/slow" => {
            tokio::time::sleep(Duration::from_millis(250)).await;
            Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Body::from(
                    "<html><body><h1>Slow page</h1><p>This page sleeps so a cancellation can land while it is in flight.</p></body></html>",
                ))
                .unwrap()
        }
        //: a page whose links are relative, reached through a redirect, so
        // absolute-URL resolution must use the final URL.
        // ---- rendering fixtures -------------------------------------
        "/js/noscript" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(NOSCRIPT_APP_HTML))
            .unwrap(),
        "/js/four-scripts" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(
                "<html><body><div id='content'>loading</div>\
                 <script>window.a=1</script><script>window.b=2</script>\
                 <script>window.c=3</script>\
                 <script>document.getElementById('content').textContent='FOUR_SCRIPTS_RENDERED'</script>\
                 </body></html>",
            ))
            .unwrap(),
        "/js/async" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(ASYNC_APP_HTML))
            .unwrap(),
        "/js/cookie/set" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(
                "<html><body><script>document.cookie='leak=1; path=/'</script><p>cookie-set</p></body></html>",
            ))
            .unwrap(),
        "/js/cookie/read" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(
                "<html><body><div id='out'>unset</div><script>document.getElementById('out').textContent='COOKIE['+document.cookie+']'</script></body></html>",
            ))
            .unwrap(),
        "/js/resources" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(RESOURCES_HTML))
            .unwrap(),
        "/js/asset/style.css" => Response::builder()
            .header(header::CONTENT_TYPE, "text/css")
            .body(Body::from("body { color: red; }"))
            .unwrap(),
        "/js/asset/font.woff2" => Response::builder()
            .header(header::CONTENT_TYPE, "font/woff2")
            .body(Body::from(vec![0u8; 64]))
            .unwrap(),
        "/js/asset/image.png" => Response::builder()
            .header(header::CONTENT_TYPE, "image/png")
            .body(Body::from(vec![0u8; 64]))
            .unwrap(),
        "/js/big" => {
            let filler = "x".repeat(5 * 1024 * 1024 + 512 * 1024);
            let body = format!(
                "<html><body><div id='app'>start</div>\
                 <script>document.getElementById('app').textContent='BIG_START'</script>\
                 <pre>{filler}</pre>\
                 <script>setTimeout(()=>{{document.body.dataset.complete='yes'}},3000)</script>\
                 </body></html>"
            );
            Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Body::from(body))
                .unwrap()
        }
        "/js/many-requests" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(
                "<html><body><div id='app'>start</div><script>\
                 (async()=>{ for(let i=0;i<260;i++){ try{ await fetch('/api/tick?i='+i); }catch(e){ break; } } \
                 await new Promise(r=>setTimeout(r,3000)); document.body.dataset.complete='yes'; })();\
                 </script></body></html>",
            ))
            .unwrap(),
        "/js/slow" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(
                "<html><body><div id='app'>slow</div><script>\
                 (async()=>{ for(let i=0;i<40;i++){ await new Promise(r=>setTimeout(r,250)); \
                 try{ await fetch('/api/tick?slow='+i); }catch(e){} } \
                 document.getElementById('app').textContent='SLOW_DONE'; })();\
                 </script></body></html>",
            ))
            .unwrap(),
        "/api/monitor-value" => {
            let value = state.api_value.lock().unwrap().clone();
            Response::builder()
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!("{{\"value\":\"{value}\"}}")))
                .unwrap()
        }
        "/api/data" => Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"value":"API_VALUE","article":"API_ARTICLE"}"#))
            .unwrap(),
        path if path.starts_with("/api/tick") => Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"ok":true}"#))
            .unwrap(),
        "/extract/go" => Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, "/extract/items/")
            .body(Body::empty())
            .unwrap(),
        "/extract/items/" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(EXTRACT_ITEMS_HTML))
            .unwrap(),
        "/extract/items" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(EXTRACT_ITEMS_HTML))
            .unwrap(),
        "/redirect-disallowed-path" => Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, "/private/secret")
            .body(Body::empty())
            .unwrap(),
        "/redirect-other-host" => Response::builder()
            .status(StatusCode::FOUND)
            .header(
                header::LOCATION,
                format!("http://127.0.0.5:{}/article", port_of(&headers)),
            )
            .body(Body::empty())
            .unwrap(),
        "/redirect-private" => Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, "http://169.254.169.254/latest/meta-data/")
            .body(Body::empty())
            .unwrap(),
        "/redirect-file" => Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, "file:///etc/passwd")
            .body(Body::empty())
            .unwrap(),
        // ---- search (SERP) fixtures -----------------------------------
        "/serp/html" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(SERP_HTML_RESULTS))
            .unwrap(),
        "/serp/lite" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(SERP_LITE_RESULTS))
            .unwrap(),
        "/serp/empty" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(SERP_HTML_EMPTY))
            .unwrap(),
        "/serp/anomaly" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(SERP_HTML_ANOMALY))
            .unwrap(),
        "/serp/html-ad" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(SERP_HTML_AD))
            .unwrap(),
        "/serp/lite-ad" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(SERP_LITE_AD))
            .unwrap(),
        "/serp/redirect-private" => Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, "http://10.0.0.1/")
            .body(Body::empty())
            .unwrap(),
        // ---- v0.3 fixtures ----------------------------------------------
        "/fixtures/inline-script" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(INLINE_SCRIPT))
            .unwrap(),
        "/fixtures/cnbc" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(CNBC_BODY))
            .unwrap(),
        "/fixtures/nav" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(NAV))
            .unwrap(),
        "/fixtures/thin-shell" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(THIN_SHELL))
            .unwrap(),
        "/fixtures/long-field" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(LONG_FIELD))
            .unwrap(),
        "/fixtures/no-article" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(NO_ARTICLE))
            .unwrap(),
        // ---- v0.4 feed fixtures ----------------------------------------
        "/feed/rss" => feed_response(FEED_RSS, "application/rss+xml"),
        "/feed/atom" => feed_response(FEED_ATOM, "application/atom+xml"),
        "/feed/json" => feed_response(FEED_JSON, "application/feed+json"),
        "/feed/dupes" => feed_response(FEED_DUPES, "application/rss+xml"),
        "/feed/many" => feed_response(FEED_MANY, "application/rss+xml"),
        "/feed/malformed" => feed_response(FEED_MALFORMED, "application/rss+xml"),
        "/feed/bomb" => feed_response(FEED_BOMB, "application/rss+xml"),
        "/feed/canonical" => feed_response(FEED_CANONICAL, "application/rss+xml"),
        "/feed/html" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(ARTICLE_HTML))
            .unwrap(),
        "/feed/plain" => Response::builder()
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from("not a feed"))
            .unwrap(),
        "/fixtures/m04-scope" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(ARTICLE_SCOPE))
            .unwrap(),
        "/chain/0" => Response::builder()
            .header(header::CONTENT_TYPE, "text/html")
            .body(Body::from("<html><body><p>chain end</p></body></html>"))
            .unwrap(),
        path if path.starts_with("/chain/") => {
            let hop: u32 = path.trim_start_matches("/chain/").parse().unwrap_or(0);
            Response::builder()
                .status(StatusCode::FOUND)
                .header(header::LOCATION, format!("/chain/{}", hop - 1))
                .body(Body::empty())
                .unwrap()
        }
        "/retry-after" => {
            let remaining = state.retry_after_remaining.load(Ordering::SeqCst);
            if remaining > 0 {
                state
                    .retry_after_remaining
                    .store(remaining - 1, Ordering::SeqCst);
                Response::builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .header(header::RETRY_AFTER, "0")
                    .header(header::CONTENT_TYPE, "text/html")
                    .body(Body::empty())
                    .unwrap()
            } else {
                Response::builder()
                    .header(header::CONTENT_TYPE, "text/html")
                    .body(Body::from("<html><body><p>after retry</p></body></html>"))
                    .unwrap()
            }
        }
        "/flaky-5xx" => {
            let remaining = state.flaky_5xx_remaining.load(Ordering::SeqCst);
            if remaining > 0 {
                state
                    .flaky_5xx_remaining
                    .store(remaining - 1, Ordering::SeqCst);
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::empty())
                    .unwrap()
            } else {
                Response::builder()
                    .header(header::CONTENT_TYPE, "text/html")
                    .body(Body::from("<html><body><p>recovered</p></body></html>"))
                    .unwrap()
            }
        }
        path if path.starts_with("/private") => Response::builder()
            .header(header::CONTENT_TYPE, "text/html")
            .body(Body::from(
                "<html><body><p>private content</p></body></html>",
            ))
            .unwrap(),
        "/empty" => StatusCode::NO_CONTENT.into_response(),
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from("not found"))
            .unwrap(),
    }
}

fn port_of(headers: &axum::http::HeaderMap) -> String {
    headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(':').nth(1))
        .unwrap_or("80")
        .to_string()
}

fn robots_for(host: &str) -> Response {
    let body = match host {
        // 127.0.0.1: allow everything.
        "127.0.0.1" => "User-agent: *\nAllow: /\n",
        // 127.0.0.2: one path disallowed for everyone.
        "127.0.0.2" => "User-agent: *\nDisallow: /private\n",
        // 127.0.0.3: our user-agent group allows, wildcard disallows.
        "127.0.0.3" => "User-agent: *\nDisallow: /\n\nUser-agent: web-widow\nAllow: /\n",
        // 127.0.0.4: robots unavailable.
        "127.0.0.4" => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("boom"))
                .unwrap();
        }
        // 127.0.0.5: our user-agent is disallowed outright.
        "127.0.0.5" => "User-agent: web-widow\nDisallow: /\n",
        // 127.0.0.6: crawl-delay raises our interval to one second.
        "127.0.0.6" => "User-agent: *\nAllow: /\nCrawl-delay: 1\n",
        // 127.0.0.7: absent robots.txt (404).
        "127.0.0.7" => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        }
        _ => "User-agent: *\nAllow: /\n",
    };
    Response::builder()
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(body))
        .unwrap()
}

/// Config tuned for tests: loopback allowlisted, short politeness interval and
/// tight timeouts so cap/timeout tests stay fast.
pub fn test_config() -> Config {
    let mut config = Config::default();
    config.fetch.allow_hosts = vec!["127.0.0.0/8".to_string()];
    config.fetch.min_interval_ms = 10;
    config.fetch.jitter_ms = 0;
    config.fetch.connect_timeout_ms = 1_000;
    config.fetch.read_timeout_ms = 700;
    config.fetch.total_timeout_ms = 2_000;
    config.robots.failure_ttl_s = 60;
    config
}

pub struct TestEnv {
    pub pipeline: Arc<Pipeline>,
    pub crawler: Arc<CrawlManager>,
    pub monitors: Arc<MonitorManager>,
    pub content: Arc<ContentIndex>,
    pub store: Arc<Store>,
    pub config: Arc<Config>,
    pub renderer: Arc<dyn Renderer>,
    pub dir: tempfile::TempDir,
}

impl TestEnv {
    /// Static-only test environment. Rendering tests opt in explicitly so no
    /// browser is resolved (or launched) by the static suite.
    pub async fn new(config: Config) -> Self {
        Self::with_renderer(
            config,
            Arc::new(render::UnavailableRenderer::new("static test environment")),
        )
        .await
    }

    /// Environment whose renderer is built from the configuration, including
    /// `render.chrome_path`. No browser launches until the first render call.
    pub async fn new_rendering(config: Config) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let renderer = render::build_renderer(&config, dir.path()).unwrap();
        Self::with_renderer_in(config, renderer, dir).await
    }

    pub async fn with_renderer(config: Config, renderer: Arc<dyn Renderer>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self::with_renderer_in(config, renderer, dir).await
    }

    async fn with_renderer_in(
        config: Config,
        renderer: Arc<dyn Renderer>,
        dir: tempfile::TempDir,
    ) -> Self {
        let paths = Paths::new(dir.path().to_path_buf());
        std::fs::create_dir_all(paths.blobs()).unwrap();
        let config = Arc::new(config);
        let store =
            Arc::new(Store::open(&paths, &config.fts, config.auth.index_authenticated).unwrap());
        let allow = AllowList::parse(&config.fetch.allow_hosts).unwrap();
        let ssrf = Arc::new(SsrfGate::new(allow));
        let gate = Arc::new(HostGate::new(PolitenessConfig {
            min_interval: Duration::from_millis(config.fetch.min_interval_ms),
            jitter: Duration::from_millis(config.fetch.jitter_ms),
            max_concurrent: config.fetch.max_concurrent_per_host,
        }));
        let http = Arc::new(
            HttpClient::new(
                &config.fetch,
                Arc::clone(&ssrf),
                Arc::clone(&gate),
                config.user_agent(),
            )
            .unwrap(),
        );
        let robots = Arc::new(RobotsService::new(
            Arc::clone(&http),
            Arc::clone(&store.db),
            config.robots.clone(),
        ));
        let pipeline = Arc::new(Pipeline::new(
            Arc::clone(&config),
            http,
            ssrf,
            gate,
            robots,
            Arc::clone(&store),
            Arc::clone(&renderer),
        ));
        let crawler = CrawlManager::new(
            Arc::clone(&config),
            Arc::clone(&pipeline),
            Arc::clone(&store),
        );
        let monitors = MonitorManager::new(
            Arc::clone(&config),
            Arc::clone(&pipeline),
            Arc::clone(&store),
        );
        let content = Arc::new(ContentIndex::new(Arc::clone(&config), Arc::clone(&store)));
        Self {
            pipeline,
            crawler,
            monitors,
            content,
            store,
            config,
            renderer,
            dir,
        }
    }

    pub fn data_dir(&self) -> &std::path::Path {
        self.dir.path()
    }

    /// Open the store database directly to assert storage effects.
    pub fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.data_dir().join("web-widow.db")).unwrap()
    }

    pub fn blob_count(&self) -> usize {
        fn walk(path: &std::path::Path, count: &mut usize) {
            let Ok(entries) = std::fs::read_dir(path) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, count);
                } else {
                    *count += 1;
                }
            }
        }
        let mut count = 0;
        walk(&self.data_dir().join("blobs"), &mut count);
        count
    }

    pub async fn fetch(
        &self,
        url: &str,
    ) -> Result<web_widow::pipeline::FetchPageResult, web_widow::WidowError> {
        self.pipeline
            .fetch_page(web_widow::FetchPageInput {
                url: url.to_string(),
                mode: None,
                render: None,
                freshness_s: None,
                max_chars: None,
                include_links: None,
                include_content: None,
                profile: None,
            })
            .await
    }
}

/// A raw TCP listener that accepts a connection, waits, then answers. Used for
/// the slow-headers timeout case axum cannot script.
pub async fn start_slow_server(delay: Duration) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let delay = delay;
            tokio::spawn(async move {
                let mut buffer = [0u8; 2048];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                // robots.txt is answered at once so the test isolates the
                // page-fetch timeout rather than the robots decision.
                if request.contains("/robots.txt") {
                    let _ = socket
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    let _ = socket.shutdown().await;
                    return;
                }
                tokio::time::sleep(delay).await;
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 2\r\n\r\nhi",
                    )
                    .await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (address, handle)
}

/// Write a minimal config file for a spawned binary and return its path.
pub fn write_binary_config(dir: &std::path::Path, extra: &str) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    let text = format!(
        "[fetch]\nallow_hosts = [\"127.0.0.0/8\"]\nmin_interval_ms = 10\njitter_ms = 0\n\
         connect_timeout_ms = 1000\nread_timeout_ms = 700\ntotal_timeout_ms = 2000\n\
         contact = \"tests@example.invalid\"\n\
         [render]\ndiscovery = false\n{extra}\n[logging]\nlevel = \"info\"\n"
    );
    std::fs::write(&path, text).unwrap();
    path
}

/// Locate the Chrome for Testing binary used by the rendering tests. An
/// explicit `WEB_WIDOW_CHROME` wins; otherwise the Playwright cache is scanned.
/// Tests that need a browser skip with an explicit marker when absent.
pub fn chrome_for_tests() -> Option<std::path::PathBuf> {
    if let Ok(path) = std::env::var("WEB_WIDOW_CHROME") {
        let path = std::path::PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let cache = home.join(".cache/ms-playwright");
    let mut builds: Vec<std::path::PathBuf> = std::fs::read_dir(cache)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("chromium-"))
        })
        .collect();
    builds.sort();
    builds.reverse();
    for build in builds {
        for relative in ["chrome-linux64/chrome", "chrome-linux/chrome"] {
            let candidate = build.join(relative);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

pub fn skip_without_chrome(test: &str) -> Option<std::path::PathBuf> {
    match chrome_for_tests() {
        Some(chrome) => Some(chrome),
        None => {
            eprintln!(
                "SKIP {test}: no Chrome/Chromium found (set WEB_WIDOW_CHROME to run this test)"
            );
            None
        }
    }
}

pub fn rendering_test_config(chrome_path: &std::path::Path) -> Config {
    let mut config = test_config();
    config.render.chrome_path = Some(chrome_path.to_path_buf());
    config.render.discovery = false;
    config
}

/// Full binary config for a rendering test: the render section points at the
/// test Chrome and discovery stays off so the host cannot change the result.
pub fn write_rendering_binary_config(
    dir: &std::path::Path,
    chrome_path: &std::path::Path,
) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    let text = format!(
        "[fetch]\nallow_hosts = [\"127.0.0.0/8\"]\nmin_interval_ms = 10\njitter_ms = 0\n\
         connect_timeout_ms = 1000\nread_timeout_ms = 700\ntotal_timeout_ms = 2000\n\
         contact = \"tests@example.invalid\"\n\
         [render]\ndiscovery = false\nchrome_path = \"{}\"\n\
         [logging]\nlevel = \"info\"\n",
        chrome_path.display()
    );
    std::fs::write(&path, text).unwrap();
    path
}

/// Initialize a stderr tracing subscriber once for an evidence-oriented test.
/// The test harness shows these lines only with `--nocapture`.
pub fn init_test_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init();
}

pub fn request_counts(log: &RequestLog) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for entry in log.entries() {
        *counts.entry(entry).or_insert(0) += 1;
    }
    counts
}

/// Open a data directory's SQLite database directly (tests that spawn the real
/// binary rather than using `TestEnv`).
pub fn db_at(data_dir: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(data_dir.join("web-widow.db")).unwrap()
}

/// Minimal MCP-over-stdio client for tests that need a real process (crash and
/// resume). One JSON-RPC frame per line; ids are matched, notifications skipped.
pub struct McpChild {
    pub child: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
    stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
    next_id: u64,
}

impl McpChild {
    pub async fn spawn(config: &std::path::Path, data_dir: &std::path::Path) -> Self {
        Self::spawn_with_env(config, data_dir, &[]).await
    }

    /// Spawn with extra environment variables on the *server* process (used to
    /// prove what the manager child does and does not inherit).
    pub async fn spawn_with_env(
        config: &std::path::Path,
        data_dir: &std::path::Path,
        env: &[(&str, &str)],
    ) -> Self {
        Self::spawn_with_env_and_stderr(config, data_dir, env, None).await
    }

    /// Spawn with an optional stderr log file. The v0.8 log-scan tests need
    /// the server's own stderr to assert that no credential or provider output
    /// reaches it; every other test keeps stderr null.
    pub async fn spawn_with_env_and_stderr(
        config: &std::path::Path,
        data_dir: &std::path::Path,
        env: &[(&str, &str)],
        stderr_path: Option<&std::path::Path>,
    ) -> Self {
        use std::process::Stdio;
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_web-widow"));
        for (name, value) in env {
            command.env(name, value);
        }
        let stderr = match stderr_path {
            Some(path) => {
                Stdio::from(std::fs::File::create(path).expect("the stderr log is writable"))
            }
            None => Stdio::null(),
        };
        let mut child = command
            .arg("--config")
            .arg(config)
            .arg("--data-dir")
            .arg(data_dir)
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .kill_on_drop(true)
            .spawn()
            .expect("binary starts");
        let stdin = child.stdin.take();
        let stdout = tokio::io::BufReader::new(child.stdout.take().expect("stdout"));
        let mut client = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        client
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2026-07-28",
                    "capabilities": {},
                    "clientInfo": { "name": "crawl-resume-test", "version": "0.1.0" }
                }),
            )
            .await;
        client
            .notify("notifications/initialized", serde_json::json!({}))
            .await;
        client
    }

    async fn notify(&mut self, method: &str, params: serde_json::Value) {
        use tokio::io::AsyncWriteExt;
        let frame = serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params});
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .expect("write frame");
        stdin.flush().await.expect("flush frame");
    }

    pub async fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let id = self.next_id;
        self.next_id += 1;
        let frame =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        {
            let stdin = self.stdin.as_mut().expect("stdin open");
            stdin
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .expect("write frame");
            stdin.flush().await.expect("flush frame");
        }
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.stdout.read_line(&mut line),
            )
            .await
            .expect("frame timeout")
            .expect("read frame");
            assert!(read > 0, "server closed stdout");
            let value: serde_json::Value = serde_json::from_str(line.trim()).expect("json frame");
            if value.get("id") == Some(&serde_json::json!(id)) {
                return value;
            }
        }
    }

    pub async fn call(&mut self, tool: &str, arguments: serde_json::Value) -> serde_json::Value {
        self.request(
            "tools/call",
            serde_json::json!({"name": tool, "arguments": arguments}),
        )
        .await
    }

    /// SIGKILL: no graceful shutdown, exactly the crash the resume path must
    /// survive.
    pub async fn kill9(&mut self) {
        let id = self.child.id().expect("child pid");
        let status = tokio::process::Command::new("kill")
            .arg("-9")
            .arg(id.to_string())
            .status()
            .await
            .expect("kill");
        assert!(status.success(), "kill -9 failed");
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), self.child.wait()).await;
    }

    pub async fn shutdown(mut self) {
        drop(self.stdin.take());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), self.child.wait()).await;
    }
}
