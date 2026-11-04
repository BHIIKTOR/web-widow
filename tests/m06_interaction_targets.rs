//! v0.6 safety probes — popup and child-target egress.
//!
//! Private listeners sit on loopback addresses **outside** the test SSRF
//! allowlist. Both TCP connections and HTTP request lines are counted, so a
//! popup preconnect cannot be mistaken for a refused request.

#![cfg(feature = "render")]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::State as AxumState;
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use tokio::io::AsyncReadExt;
use web_widow::core::ErrorCode;
use web_widow::interaction::{
    InteractActInput, InteractOpenInput, InteractStateInput, InteractionManager,
};

#[derive(Clone, Default)]
struct Log {
    entries: Arc<std::sync::Mutex<Vec<String>>>,
    stamped: Arc<std::sync::Mutex<Vec<(Instant, String)>>>,
}

impl Log {
    fn record(&self, entry: String) {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(entry.clone());
        self.stamped
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((Instant::now(), entry));
    }

    fn stamps_matching(&self, needle: &str) -> Vec<Instant> {
        self.stamped
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(_, entry)| entry.contains(needle))
            .map(|(at, _)| *at)
            .collect()
    }

    fn entries(&self) -> Vec<String> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[derive(Clone)]
struct State {
    log: Log,
    worker_probe: String,
    iframe_probe: String,
    child_origin: String,
    second_origin: String,
}

#[derive(Debug, Default)]
struct Probe {
    port: u16,
    connections: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    _handle: Option<tokio::task::JoinHandle<()>>,
}

impl Probe {
    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

async fn start_probe(host: &'static str) -> Probe {
    let listener = tokio::net::TcpListener::bind((host, 0))
        .await
        .expect("probe bind");
    let port = listener.local_addr().unwrap().port();
    let connections = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::clone(&connections);
    let requested = Arc::clone(&requests);
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            accepted.fetch_add(1, Ordering::SeqCst);
            let requested = Arc::clone(&requested);
            tokio::spawn(async move {
                let mut buffer = [0u8; 1024];
                if let Ok(Ok(read)) =
                    tokio::time::timeout(Duration::from_millis(800), socket.read(&mut buffer)).await
                    && read > 0
                    && String::from_utf8_lossy(&buffer[..read]).starts_with("GET ")
                {
                    requested.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });
    Probe {
        port,
        connections,
        requests,
        _handle: Some(handle),
    }
}

fn html(body: String) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}

fn js(body: String) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/javascript")
        .body(Body::from(body))
        .unwrap()
}

async fn handle(AxumState(state): AxumState<State>, request: Request<Body>) -> Response {
    let path = request.uri().path().to_string();
    let logged = match request.uri().query() {
        Some(query) => format!("{path}?{query}"),
        None => path.clone(),
    };
    state.log.record(logged);
    match path.as_str() {
        "/robots.txt" => Response::builder()
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from("User-agent: *\nAllow: /\n"))
            .unwrap(),
        "/images.html" => html(r#"<!doctype html><html><head><title>images</title></head><body>
<img src="/asset/img.png?i=1"><img src="/asset/img.png?i=2"><img src="/asset/img.png?i=3">
<img src="/asset/img.png?i=4"><img src="/asset/img.png?i=5"><img src="/asset/img.png?i=6">
</body></html>"#
            .to_string()),
        "/asset/img.png" => Response::builder()
            .header(header::CONTENT_TYPE, "image/png")
            .body(Body::from(
                include_bytes!("fixtures/m06/browser/image.png").to_vec(),
            ))
            .unwrap(),
        "/flood.html" => html(r#"<!doctype html><html><head><title>flood</title></head><body>
<script>
(async () => {
  for (let i = 0; i < 800; i += 1) {
    try { await fetch('http://127.0.0.2:1/x?i=' + i); } catch (_) {}
  }
})();
</script>
</body></html>"#
            .to_string()),
        "/origin.html" => html(format!(
            r#"<!doctype html><html><head><title>origin</title></head><body>
<a id="cross" href="{next}/other.html">cross origin</a>
</body></html>"#,
            next = state.second_origin
        )),
        "/other.html" => html("<!doctype html><html><head><title>other origin</title></head><body><h1>other</h1></body></html>".to_string()),
        "/longurl.html" => html(format!(
            r#"<!doctype html><html><head><title>short title</title></head><body>
<a id="big" href="/big?{}">big</a>
</body></html>"#,
            "q".repeat(5_000)
        )),
        "/big" => html("<!doctype html><html><head><title>big</title></head><body><h1>big</h1></body></html>".to_string()),
        "/longtitle.html" => html(format!(
            "<!doctype html><html><head><title>{}</title></head><body><h1>long title</h1></body></html>",
            "T".repeat(500)
        )),
        "/hostile.html" => html("<!doctype html><html><head><title>hostile</title></head><body><div id=\"target\">target</div></body></html>".to_string()),
        "/redirect-private" => Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, format!("{}/probe", state.worker_probe))
            .body(Body::empty())
            .unwrap(),
        "/next.html" => html("<!doctype html><html><head><title>next</title></head><body><h1>next</h1></body></html>".to_string()),
        "/nav-targets.html" => html(r#"<!doctype html><html><head><title>nav targets</title></head><body>
<a id="top" href="/next.html" target="_top">top</a>
<a id="parent" href="/next.html" target="_parent">parent</a>
<a id="named" href="/next.html" target="namedwin">named</a>
</body></html>"#
            .to_string()),
        "/nested.html" => html("<!doctype html><html><head><title>nested</title></head><body><script>new Worker('/nested.js');</script></body></html>".to_string()),
        "/shared.html" => html(
            r#"<!doctype html><html><head><title>shared</title></head><body>
<h1>shared worker</h1>
<script>
try {{ new SharedWorker('/shared.js'); }} catch (error) {{ fetch('/n2-error?shared=' + encodeURIComponent(String(error))); }}
try {{ new Worker('/nested.js'); }} catch (error) {{ fetch('/n2-error?nested=' + encodeURIComponent(String(error))); }}
</script>
</body></html>"#
            .to_string(),
        ),
        "/shared.js" => js(format!(
            "fetch('/shared-started').finally(() => fetch({}));",
            serde_json::json!(format!("{}/probe?shared=1", state.worker_probe))
        )),
        "/nested.js" => js(format!(
            "fetch('/nested-started'); try {{ importScripts({}); }} catch (_) {{}}",
            serde_json::json!(format!("{}/probe?import=1", state.worker_probe))
        )),
        "/nested-inner.js" => js(format!(
            "fetch('/nested-started').finally(() => fetch({}));",
            serde_json::json!(format!("{}/probe?nested=1", state.worker_probe))
        )),
        "/g8.html" => html(format!(
            r#"<!doctype html><html><head><title>g8</title><base target="_blank"></head><body>
<div id="g8a" style="width:220px;height:70px;background:#eee"><template shadowrootmode="closed"><a href="{probe}/probe?g8a=1" style="display:block;width:220px;height:70px">g8a</a></template></div>
<div id="g8b" style="width:220px;height:70px;background:#ddd"><template shadowrootmode="closed"><a href="{probe}/probe?g8b=1" target="" style="display:block;width:220px;height:70px">g8b</a></template></div>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g9.html" => {
            let mut decoys = String::new();
            for index in 0..500 {
                decoys.push_str(&format!(
                    "<a target=\"d{index}\" href=\"/next.html\">d{index}</a>"
                ));
            }
            html(format!(
                r#"<!doctype html><html><head><title>g9</title></head><body>{decoys}
<div id="g9" style="width:220px;height:70px;background:#eee"><template shadowrootmode="closed"><a target="_blank" href="{probe}/probe?g9=1" style="display:block;width:220px;height:70px">g9</a></template></div>
</body></html>"#,
                probe = state.worker_probe
            ))
        }
        "/g10.html" => html(format!(
            r#"<!doctype html><html><head><title>g10</title></head><body>
<div id="g10" style="width:220px;height:70px;background:#eee"><template shadowrootmode="closed"><a target="_blank" href="{probe}/probe?g10=1" style="display:contents"><span style="display:block;width:220px;height:70px;background:#ddd">g10</span></a></template></div>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g11.html" => html(format!(
            r#"<!doctype html><html><head><title>g11</title></head><body>
<form id="g11-form" action="{probe}/probe?g11=1" method="get"></form>
<button id="g11-run">run</button>
<script>
document.getElementById('g11-run').addEventListener('click', () => {{
  const form = document.getElementById('g11-form');
  const submitter = document.createElement('button');
  submitter.type = 'submit';
  submitter.setAttribute('formtarget', '_blank');
  form.appendChild(submitter);
  form.requestSubmit(submitter);
}});
</script>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g12.html" => html(format!(
            r#"<!doctype html><html><head><title>g12</title></head><body>
<div id="g12-host"></div><button id="g12-click">click</button><button id="g12-dispatch">dispatch</button>
<script>
const host = document.getElementById('g12-host');
const root = host.attachShadow({{mode: 'closed'}});
const anchor = document.createElement('a');
anchor.href = '{probe}/probe?g12=1';
anchor.target = '_blank';
anchor.style.display = 'block';
anchor.style.width = '220px';
anchor.style.height = '70px';
root.appendChild(anchor);
document.getElementById('g12-click').addEventListener('click', () => anchor.click());
document.getElementById('g12-dispatch').addEventListener('click', () => {{
  anchor.dispatchEvent(new MouseEvent('click', {{bubbles: false, composed: false}}));
}});
</script>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g13.html" => html(format!(
            r#"<!doctype html><html><head><title>g13</title></head><body>
<form id="g13-form" action="{probe}/probe?g13=1" target="_blank" method="get"></form>
<button id="g13-mutate">mutate</button><button id="g13-again">again</button>
<script>
const form = document.getElementById('g13-form');
const mutate = () => {{
  form.submit();
  const value = window.__wwCancelled;
  if (value) {{
    try {{ value.seq = 999999; value.kind = 'evil'; value.target = 'XX'; }} catch (_) {{}}
  }}
}};
document.getElementById('g13-mutate').addEventListener('click', mutate);
document.getElementById('g13-again').addEventListener('click', () => form.submit());
</script>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g14.html" => html(format!(
            r#"<!doctype html><html><head><title>g14</title>
<base href="/base-dir/">
<base target="_blank">
</head><body>
<div id="g14" style="width:220px;height:70px;background:#eee"><template shadowrootmode="closed"><a href="{probe}/probe?g14=1" style="display:block;width:220px;height:70px">g14</a></template></div>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g16.html" => html(format!(
            r#"<!doctype html><html><head><title>g16</title><base target="_blank"></head><body>
<a id="g16" href="{probe}/probe?g16=1" style="display:block;width:220px;height:70px;background:#eee">g16</a>
<script>
const nativeQuery = Document.prototype.querySelector;
Document.prototype.querySelector = function (selector) {{
  const text = String(selector);
  if (text.indexOf('base') >= 0) {{
    fetch('/g16-tamper?q=' + encodeURIComponent(text));
    return null;
  }}
  return nativeQuery.call(this, selector);
}};
const nativeAttr = Element.prototype.getAttribute;
Element.prototype.getAttribute = function (name) {{
  const key = String(name).toLowerCase();
  if (key === 'target' || key === 'href') {{
    fetch('/g16-tamper?a=' + encodeURIComponent(key));
    return null;
  }}
  return nativeAttr.call(this, name);
}};
// Prove the overrides are live: both lie to the page, so any policy read that
// went through them would see "no base" and "no target".
fetch('/g16-installed?base_null=' +
      (Document.prototype.querySelector.call(document, 'base[target]') === null ? 1 : 0) +
      '&attr_null=' +
      (Element.prototype.getAttribute.call(document.getElementById('g16'), 'target') === null ? 1 : 0));
</script>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g17.html" => html(format!(
            r#"<!doctype html><html><head><title>g17</title></head><body>
<div id="g17" style="margin-top:calc(100vh - 50px);width:220px;height:70px;background:#eee">g17</div>
<div style="height:1500px"></div>
<div id="g17-trap" style="position:fixed;left:0;top:0;width:220px;height:70px;visibility:hidden;z-index:2147483647">
  <template shadowrootmode="closed"><a target="_blank" href="{probe}/probe?g17=1" style="display:block;width:100%;height:100%">trap</a></template>
</div>
<script>
let scrollCalls = 0;
Element.prototype.scrollIntoView = function () {{
  scrollCalls += 1;
  fetch('/g17-scroll-called?n=' + scrollCalls);
  // Stay partially visible so the caller's second (page-visible) call happens.
  window.scrollBy(0, 5);
  if (scrollCalls >= 2) {{
    const rect = this.getBoundingClientRect();
    const trap = document.getElementById('g17-trap');
    trap.style.left = rect.left + 'px';
    trap.style.top = rect.top + 'px';
    trap.style.width = rect.width + 'px';
    trap.style.height = rect.height + 'px';
    trap.style.visibility = 'visible';
  }}
}};
</script>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g18.html" => html(format!(
            r#"<!doctype html><html><head><title>g18</title></head><body>
<div id="g18-trap" style="position:fixed;left:0;top:0;width:220px;height:400px;visibility:hidden;z-index:2147483647">
  <template shadowrootmode="closed"><a target="_blank" href="{probe}/probe?g18=1" style="display:block;width:100%;height:100%">trap</a></template>
</div>
<div id="g18" style="position:absolute;left:0;top:200px;width:220px;height:70px;background:#eee">g18</div>
<script>
const target = document.getElementById('g18');
target.addEventListener('mouseover', () => {{
  fetch('/g18-hover');
  document.getElementById('g18-trap').style.visibility = 'visible';
  target.style.top = '600px';
}});
</script>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g19.html" => html(format!(
            r#"<!doctype html><html><head><title>g19</title></head><body>
<div id="g19" style="width:220px;height:70px;background:#eee">g19</div>
<script>
Object.freeze = function (value) {{ return value; }};
document.getElementById('g19').addEventListener('click', () => {{
  let opened = 'none';
  try {{ opened = String(window.open('{probe}/probe?g19=1')); }} catch (error) {{ opened = 'threw'; }}
  let mutated = 'none';
  try {{
    const value = window.__wwCancelled;
    if (value) {{
      value.seq = 0;
      value.kind = 'evil';
      value.target = 'XV';
      mutated = JSON.stringify(value);
    }}
  }} catch (error) {{ mutated = 'threw'; }}
  fetch('/g19-state?opened=' + encodeURIComponent(opened) + '&mutated=' + encodeURIComponent(mutated));
}});
</script>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g20.html" => html(format!(
            r#"<!doctype html><html><head><title>g20</title></head><body>
<form id="f20" action="{probe}/probe?g20=1" method="get" target="_blank"></form>
<input id="g20" type="submit" form="f20" value="go" style="display:block;width:220px;height:70px">
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g21.html" => html(format!(
            r#"<!doctype html><html><head><title>g21</title></head><body>
<div id="g21a" style="width:220px;height:70px"><template shadowrootmode="open">
  <form id="f21a" action="{probe}/probe?g21a=1" method="get" target="_blank"></form>
  <input type="submit" form="f21a" value="go" style="display:block;width:220px;height:70px">
</template></div>
<div id="g21b" style="width:220px;height:70px"><template shadowrootmode="open">
  <form id="f21b" action="{probe}/probe?g21b=1" method="get" target="_blank"></form>
  <button form="f21b" style="display:block;width:220px;height:70px">go</button>
</template></div>
<div id="g21c" style="width:220px;height:70px"><template shadowrootmode="open">
  <form id="f21c" action="{probe}/probe?g21c=1" method="get" target="_blank"></form>
  <input type="image" form="f21c" alt="go" style="display:block;width:220px;height:70px">
</template></div>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g22.html" => html(format!(
            r#"<!doctype html><html><head><title>g22</title></head><body>
<div id="g22-boot">boot</div>
<script>
// document.open() re-creates the document and removes every listener on the
// Document and its Window; the guard's prototypes and closure survive. The
// rewritten page then flips the target after the pre-dispatch check.
document.open();
document.write('<!doctype html><html><head><title>g22</title></head><body>' +
  '<a id="g22" href="{probe}/probe?g22=1" target="_self" style="display:block;width:220px;height:70px;background:#eee">g22</a>' +
  '<script>document.getElementById("g22").addEventListener("mousedown", function () {{ document.getElementById("g22").setAttribute("target", "_blank"); }});<\/script>' +
  '</body></html>');
document.close();
</script>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g23.html" => html(format!(
            r#"<!doctype html><html><head><title>g23</title></head><body>
<div id="g23" style="position:relative;width:220px;height:70px">
  <template shadowrootmode="closed"><a target="_blank" href="{probe}/probe?g23=1" style="display:block;width:220px;height:70px">closed</a></template>
  <div style="position:absolute;left:0;top:0;width:220px;height:70px;pointer-events:none"></div>
</div>
<div id="g23o" style="position:relative;width:220px;height:70px">
  <template shadowrootmode="open"><a target="_blank" href="{probe}/probe?g23o=1" style="display:block;width:220px;height:70px">open</a></template>
  <div style="position:absolute;left:0;top:0;width:220px;height:70px;pointer-events:none"></div>
</div>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g24.html" => html(format!(
            r#"<!doctype html><html><head><title>g24</title></head><body>
<div id="g24-real" style="width:220px;height:70px"><template shadowrootmode="closed"><a target="_blank" href="{probe}/probe?g24=1" style="display:block;width:220px;height:70px">real</a></template></div>
<div id="g24-decoy" style="width:220px;height:70px;background:#ddd">decoy</div>
<script>
const nativeQuerySelectorAll = Document.prototype.querySelectorAll;
Document.prototype.querySelectorAll = function (selector) {{
  return nativeQuerySelectorAll.call(this, '#g24-decoy');
}};
const probed = Document.prototype.querySelectorAll.call(document, '#g24-real');
fetch('/g24-installed?decoy=' + (probed.length === 1 && probed[0].id === 'g24-decoy' ? 1 : 0));
</script>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g25.html" => html(r#"<!doctype html><html><head><title>g25</title></head><body>
<div id="g25">heartbeat forge</div>
<script>
const value = (typeof window.__wwSessionGuard === 'function') ? window.__wwSessionGuard() : 'none';
console.debug('WWIW2:' + value);
console.debug('WWIW:' + value);
console.debug('WWIW2:forged');
</script>
</body></html>"#
            .to_string()),
        "/g26.html" => html(r#"<!doctype html><html><head><title>g26</title></head><body>
<div id="g26" style="width:220px;height:70px;background:#eee">g26</div>
<script>
console.debug('WWIW:page-line');
console.debug('WWIW2:page-line');
</script>
</body></html>"#
            .to_string()),
        "/g27.html" => html(format!(
            r#"<!doctype html><html><head><title>g27</title></head><body>
<div id="g27a" style="width:260px;height:70px"><template shadowrootmode="closed">
  <form id="f27a" action="{probe}/probe?g27a=1" method="get"></form>
  <label for="s27a" style="display:block;width:260px;height:70px;background:#eee">Continue</label>
  <input id="s27a" type="submit" form="f27a" formtarget="_blank" value="go">
</template></div>
<div id="g27b" style="width:260px;height:70px"><template shadowrootmode="closed">
  <form id="f27b" action="{probe}/probe?g27b=1" method="get">
    <label style="display:block;width:260px;height:70px;background:#ddd">Continue <input type="submit" formtarget="_blank" value="go"></label>
  </form>
</template></div>
<div id="g27c" style="width:260px;height:70px"><template shadowrootmode="closed">
  <form id="f27c" action="{probe}/probe?g27c=1" method="get" target="_blank"></form>
  <label for="s27c" style="display:block;width:260px;height:70px;background:#ccc">Continue</label>
  <input id="s27c" type="submit" form="f27c" value="go">
</template></div>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g29.html" => html(format!(
            r#"<!doctype html><html><head><title>g29</title></head><body>
<form action="{probe}/probe?g29=1" method="get" target="_blank">
  <input id="g29" type="text" name="q" style="display:block;width:220px;height:40px">
</form>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g28.html" => html(format!(
            r#"<!doctype html><html><head><title>g28</title></head><body>
<form action="{origin}/next.html" method="get" target="_blank">
  <button id="g28" formtarget="_self" style="display:block;width:220px;height:70px">go</button>
</form>
</body></html>"#,
            origin = state.second_origin
        )),
        "/g30.html" => html(format!(
            r#"<!doctype html><html><head><title>g30</title></head><body>
<div id="g30a" style="width:260px;height:70px"><template shadowrootmode="closed">
  <a target=" " href="{probe}/probe?g30a=1" style="display:block;width:260px;height:70px;background:#eee">space target</a>
</template></div>
<div id="g30b" style="width:260px;height:70px"><template shadowrootmode="closed">
  <form id="f30b" action="{probe}/probe?g30b=1" method="get" target=" "></form>
  <input type="submit" form="f30b" value="go" style="display:block;width:260px;height:70px">
</template></div>
<div id="g30c" style="width:260px;height:70px"><template shadowrootmode="closed">
  <form id="f30c" action="{probe}/probe?g30c=1" method="get"></form>
  <input type="submit" form="f30c" formtarget=" " value="go" style="display:block;width:260px;height:70px">
</template></div>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g31.html" => html(format!(
            r#"<!doctype html><html><head><title>g31</title></head><body>
<div id="g31" style="width:260px;height:70px"><template shadowrootmode="closed">
  <form id="f31" action="{probe}/probe?g31=1" method="get"></form>
  <label for="s31" style="display:block;width:260px;height:70px">
    <a id="a31" href="{origin}/next.html" style="display:block;width:260px;height:70px;background:#eee">next</a>
  </label>
  <input id="s31" type="submit" form="f31" formtarget="_blank" value="go">
</template></div>
</body></html>"#,
            probe = state.worker_probe,
            origin = state.second_origin
        )),
        "/g32.html" => html(format!(
            r#"<!doctype html><html><head><title>g32</title></head><body>
<div id="g32" style="width:220px;height:70px"><template shadowrootmode="open">
  <form id="f32" action="{probe}/probe?g32=1" method="get"></form>
  <label id="lab32" for="s32" style="display:block;width:220px;height:70px;background:#eee">Continue</label>
  <input id="s32" type="submit" form="f32" formtarget="_blank" value="go">
</template></div>
<button id="g32-run" style="display:block;width:220px;height:40px">run</button>
<script>
document.getElementById('g32-run').addEventListener('click', () => {{
  const host = document.getElementById('g32');
  const label = host.shadowRoot.getElementById('lab32');
  label.click();
}});
</script>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g33.html" => html(format!(
            r#"<!doctype html><html><head><title>g33</title></head><body>
<div id="g33a" style="width:260px;height:70px"><template shadowrootmode="closed">
  <form id="f33a" action="{probe}/probe?g33a=1" method="get"></form>
  <label for="s33a" style="display:block;width:260px;height:70px">
    <span tabindex="0" style="display:block;width:260px;height:70px;background:#eee">focusable span</span>
  </label>
  <input id="s33a" type="submit" form="f33a" formtarget="_blank" value="go">
</template></div>
<div id="g33b" style="width:260px;height:70px"><template shadowrootmode="closed">
  <form id="f33b" action="{probe}/probe?g33b=1" method="get"></form>
  <label for="s33b" style="display:block;width:260px;height:70px">
    <input type="hidden" style="display:block !important;width:260px;height:70px;background:#ddd">
    <span style="display:block">Continue</span>
  </label>
  <input id="s33b" type="submit" form="f33b" formtarget="_blank" value="go">
</template></div>
<div id="g33c" style="width:260px;height:70px"><template shadowrootmode="open">
  <form id="f33c" action="{probe}/probe?g33c=1" method="get"></form>
  <label for="s33c" id="lab33c" style="display:block;width:260px;height:70px">
    <span id="span33c" tabindex="0" style="display:block;width:260px;height:70px;background:#ccc">focusable span</span>
  </label>
  <input id="s33c" type="submit" form="f33c" formtarget="_blank" value="go">
</template></div>
<button id="g33c-run" style="display:block;width:260px;height:40px">run</button>
<script>
document.getElementById('g33c-run').addEventListener('click', () => {{
  const host = document.getElementById('g33c');
  const span = host.shadowRoot.getElementById('span33c');
  span.dispatchEvent(new MouseEvent('click', {{bubbles: true, composed: true}}));
}});
</script>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g34.html" => html(format!(
            r#"<!doctype html><html><head><title>g34</title></head><body>
<a id="g34-space" href="{origin}/next.html" target=" " style="display:block;width:260px;height:40px;background:#eee">space</a>
<a id="g34-upper" href="{origin}/next.html" target="_SELF" style="display:block;width:260px;height:40px;background:#ddd">upper</a>
<button id="g34-run-space" style="display:block;width:260px;height:40px">space</button>
<button id="g34-run-upper" style="display:block;width:260px;height:40px">upper</button>
<script>
document.getElementById('g34-run-space').addEventListener('click', () => {{
  document.getElementById('g34-space').click();
}});
document.getElementById('g34-run-upper').addEventListener('click', () => {{
  document.getElementById('g34-upper').click();
}});
</script>
</body></html>"#,
            origin = state.second_origin
        )),
        "/g35.html" => html(format!(
            r#"<!doctype html><html><head><title>g35</title></head><body>
<form id="f35" action="{probe}/probe?g35=1" method="get" target="_blank">
  <button id="g35" formtarget="" style="display:block;width:260px;height:70px">go</button>
</form>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g33d.html" => html(format!(
            r#"<!doctype html><html><head><title>g33d</title></head><body>
<div id="g33d" style="width:260px;height:70px"><template shadowrootmode="closed">
  <form id="f33d" action="{probe}/probe?g33d=1" method="get"></form>
  <label for="s33d" style="display:block;width:260px;height:70px">
    <a style="display:block;width:260px;height:70px;background:#eee">href-less anchor</a>
  </label>
  <input id="s33d" type="submit" form="f33d" formtarget="_blank" value="go">
</template></div>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g36.html" => html(format!(
            r#"<!doctype html><html><head><title>g36</title>
<base target="_blank">
</head><body>
<form id="f36" action="{probe}/probe?g36=1" method="get">
  <button id="g36" formtarget="" style="display:block;width:260px;height:70px">go</button>
</form>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/g37.html" => html(format!(
            r#"<!doctype html><html><head><title>g37</title></head><body>
<form id="f37" action="{origin}/next.html" method="get" target="_blank"></form>
<button id="g37" type="button" style="display:block;width:260px;height:70px">run</button>
<script>
document.getElementById('g37').addEventListener('click', () => {{
  const form = document.getElementById('f37');
  const submitter = document.createElement('button');
  submitter.type = 'submit';
  submitter.setAttribute('formtarget', '');
  form.appendChild(submitter);
  form.requestSubmit(submitter);
}});
</script>
</body></html>"#,
            origin = state.second_origin
        )),
        "/g38.html" => html(
            r#"<!doctype html><html><head><title>g38</title></head><body>
<a id="g38" target="_blank" style="display:block;width:260px;height:70px;background:#eee">no href</a>
</body></html>"#
            .to_string()
        ),
        "/g5.html" => html(r#"<!doctype html><html><head><title>g5</title></head><body>
<button id="benign">benign</button>
<button id="forge">forge</button>
<button id="suppress">suppress</button>
<a id="named" href="/next.html" target="namedwin">named</a>
<script>
document.getElementById('forge').addEventListener('click', () => {
  window.__wwCancelled = { seq: 999, kind: 'evil', target: 'X'.repeat(100000) };
});
document.getElementById('suppress').addEventListener('click', () => {
  try {
    Object.defineProperty(window, '__wwCancelled', {
      configurable: true,
      get() { return null; },
      set() {},
    });
  } catch (_) {}
});
fetch('/g5-state?guard=' + typeof window.__wwSessionGuard);
</script>
</body></html>"#
            .to_string()),
        "/g0.html" => html(format!(
            r#"<!doctype html><html><head><title>g0</title></head><body>
<div id="g1" style="width:220px;height:70px;background:#eee">
  <template shadowrootmode="closed">
    <a target="_blank" href="{probe}/probe?g1=1" style="display:block;width:220px;height:70px">g1</a>
  </template>
</div>
<svg id="g2" width="220" height="70"><a target="_blank" href="{probe}/probe?g2=1"><rect width="220" height="70" fill="gray"></rect></a></svg>
<div id="g3" style="width:220px;height:70px;background:#eee">
  <template shadowrootmode="closed">
    <form action="{probe}/probe?g3=1" method="get" target="_blank">
      <button style="width:220px;height:70px">g3</button>
    </form>
  </template>
</div>
<form id="g4-form" action="{probe}/probe?g4=1" method="get" target="_blank"></form>
<button id="g4-go">g4</button>
<script>
const g4 = document.getElementById('g4-form');
Object.defineProperty(g4, 'target', {{ value: '_self', configurable: true }});
document.getElementById('g4-go').addEventListener('click', () => g4.submit());
</script>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/n1.html" => html(format!(
            r#"<!doctype html><html><head><title>n1</title></head><body>
<button id="proto-open">proto open</button>
<button id="form-submit" onclick="fetch('/n1-inline?which=form')">form submit</button>
<div id="shadow-host" style="display:block;width:200px;height:60px;background:#eee">shadow host</div>
<form id="blank-form" action="{probe}/probe?form=1" target="_blank" method="get"></form>
<script>
document.addEventListener('click', (event) => {{
  fetch('/n1-doc-click?target=' + encodeURIComponent(String(event.target && event.target.id)));
}}, true);
window.addEventListener('error', (event) => {{
  fetch('/n1-error?message=' + encodeURIComponent(String(event.message)));
}});
const descriptor = Object.getOwnPropertyDescriptor(Window.prototype, 'open');
fetch('/n1-guard?proto=' + (descriptor && descriptor.value ? 1 : 0));
document.getElementById('proto-open').addEventListener('click', () => {{
  // The server replaces `window.open` with a non-configurable own property, so
  // the native is unreachable from page script. On Chrome 156 there is no
  // Window.prototype descriptor at all; on any build where one exists the
  // reachable value is still the server stub. The activation attempt is
  // recorded so the 0 TCP result is not ambiguous (review round 5, n1).
  const own = Object.getOwnPropertyDescriptor(window, 'open');
  const proto = Object.getOwnPropertyDescriptor(Window.prototype, 'open');
  let opened = 'none';
  try {{ opened = String(window.open('{probe}/probe?proto=1')); }} catch (error) {{ opened = 'threw'; }}
  fetch('/n1-proto?own=' + (own ? 1 : 0) + '&proto=' + (proto && proto.value ? 1 : 0) +
        '&opened=' + encodeURIComponent(opened));
}});
document.getElementById('form-submit').addEventListener('click', () => {{
  const submitBefore = String(HTMLFormElement.prototype.submit).slice(0, 40);
  document.getElementById('blank-form').submit();
  setTimeout(() => {{
    fetch('/n1-state?submit=' + encodeURIComponent(submitBefore) +
          '&cancelled=' + encodeURIComponent(JSON.stringify(window.__wwCancelled || null)));
  }}, 200);
}});
const host = document.getElementById('shadow-host');
const root = host.attachShadow({{mode: 'closed'}});
const anchor = document.createElement('a');
anchor.href = '{probe}/probe?shadow=1';
anchor.target = '_blank';
anchor.textContent = 'shadow link';
anchor.style.display = 'block';
anchor.style.height = '60px';
root.appendChild(anchor);
fetch('/n1-ready?shadow=' + (host.shadowRoot !== null ? 1 : 0));
</script>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/popup.html" => html(format!(
            r#"<!doctype html><html><head><title>popup</title></head><body>
<base target="_blank">
<a id="blank" target="_blank" href="{probe}/probe">blank</a>
<a id="base-linked" href="{probe}/probe?base=1">base-linked</a>
<button id="open">open</button>
<script>
fetch('/guard?marker=' + (window.__wwSessionGuard === true ? 1 : 0) + '&openType=' + typeof window.open + '&native=' + String(window.open).includes('[native code]'));
document.getElementById('open').addEventListener('click', () => window.open('{probe}/probe'));
</script>
</body></html>"#,
            probe = state.worker_probe
        )),
        "/worker.html" => html(
            r#"<!doctype html><html><head><title>worker</title></head><body>
<h1>worker</h1><script>new Worker('/worker.js');</script>
</body></html>"#
                .to_string(),
        ),
        "/worker.js" => js(format!(
            "fetch('/worker-started').finally(() => fetch({}));",
            serde_json::json!(format!("{}/probe", state.worker_probe))
        )),
        "/sw.html" => html(
            r#"<!doctype html><html><head><title>sw</title></head><body>
<h1>service worker</h1>
<script>
navigator.serviceWorker.register('/sw.js').then(async () => {
  const ready = await navigator.serviceWorker.ready;
  ready.active.postMessage('go');
}).catch(() => fetch('/sw-blocked'));
</script>
</body></html>"#
                .to_string(),
        ),
        "/sw.js" => js(format!(
            "self.addEventListener('install', () => {{ self.skipWaiting(); }});
self.addEventListener('activate', (event) => {{ event.waitUntil(self.clients.claim()); }});
self.addEventListener('message', () => {{ fetch('/sw-started').finally(() => fetch({})); }});",
            serde_json::json!(format!("{}/probe", state.worker_probe))
        )),
        "/iframe.html" => html(format!(
            r#"<!doctype html><html><head><title>iframe</title></head><body>
<iframe src="{child}/child.html" width="200" height="100"></iframe>
</body></html>"#,
            child = state.child_origin
        )),
        "/child.html" => html(format!(
            r#"<!doctype html><html><head><title>child</title></head><body>
<script>fetch('/child-started').finally(() => fetch({}).catch(() => {{}}));</script>
</body></html>"#,
            serde_json::json!(format!("{}/probe", state.iframe_probe))
        )),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

struct Fixture {
    origin: String,
    log: Log,
    worker_probe: Probe,
    iframe_probe: Probe,
    _handles: Vec<tokio::task::JoinHandle<()>>,
}

async fn serve(host: &'static str, state: State) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().fallback(any(handle)).with_state(state);
    let listener = tokio::net::TcpListener::bind((host, 0))
        .await
        .expect("fixture bind");
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{host}:{port}"), handle)
}

async fn start_fixture() -> Fixture {
    let worker_probe = start_probe("127.0.0.2").await;
    let iframe_probe = start_probe("127.0.0.3").await;
    let log = Log::default();
    let state = State {
        log: log.clone(),
        worker_probe: format!("http://127.0.0.2:{}", worker_probe.port),
        iframe_probe: format!("http://127.0.0.3:{}", iframe_probe.port),
        child_origin: String::new(),
        second_origin: String::new(),
    };
    let (child_origin, child_handle) = serve("127.0.0.2", state.clone()).await;
    let (second_origin, second_handle) = serve("127.0.0.1", state.clone()).await;
    let state = State {
        child_origin: child_origin.clone(),
        second_origin: second_origin.clone(),
        ..state
    };
    let (origin, main_handle) = serve("127.0.0.1", state).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    Fixture {
        origin,
        log,
        worker_probe,
        iframe_probe,
        _handles: vec![main_handle, child_handle, second_handle],
    }
}

async fn open(
    manager: &InteractionManager,
    origin: &str,
    path: &str,
) -> web_widow::interaction::InteractOpenPayload {
    manager
        .open(InteractOpenInput {
            url: format!("{origin}{path}"),
            render: None,
            viewport_width: None,
            viewport_height: None,
        })
        .await
        .expect("session open")
}

async fn click(manager: &InteractionManager, session: &str, selector: &str) {
    manager
        .act(InteractActInput {
            session_id: session.to_string(),
            action: serde_json::json!({"kind": "click", "selector": selector}),
        })
        .await
        .expect("click");
}

fn manager(env: &common::TestEnv) -> Arc<InteractionManager> {
    Arc::new(InteractionManager::new(
        Arc::clone(&env.config),
        Arc::clone(&env.pipeline),
    ))
}

async fn env_with(chrome: &std::path::Path, allow: &[&str]) -> common::TestEnv {
    let mut config = common::rendering_test_config(chrome);
    config.fetch.allow_hosts = allow.iter().map(|value| value.to_string()).collect();
    config.fetch.min_interval_ms = 10;
    config.fetch.jitter_ms = 0;
    config.render.settle_ms = 250;
    common::TestEnv::new_rendering(config).await
}

fn dwell(fixture: &Fixture, duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = fixture;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f1_a_popup_target_never_reaches_the_private_listener() {
    let Some(chrome) =
        common::skip_without_chrome("f1_a_popup_target_never_reaches_the_private_listener")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);

    let opened = open(&manager, &fixture.origin, "/popup.html").await;
    click(&manager, &opened.session_id, "#blank").await;
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    println!(" guard_log={:?}", fixture.log.entries());
    println!(
        " popup_link connections={} requests={} counters={counters:?}",
        fixture.worker_probe.connections(),
        fixture.worker_probe.requests()
    );
    assert_eq!(
        fixture.worker_probe.connections(),
        0,
        "a target=_blank popup opened a TCP connection to the private listener"
    );
    assert_eq!(fixture.worker_probe.requests(), 0);

    // A `<base target>` must not turn an untargeted link into a popup.
    click(&manager, &opened.session_id, "#base-linked").await;
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    println!(
        " base_target connections={} requests={}",
        fixture.worker_probe.connections(),
        fixture.worker_probe.requests()
    );
    assert_eq!(
        fixture.worker_probe.connections(),
        0,
        "a base-target link opened a TCP connection to the private listener"
    );

    click(&manager, &opened.session_id, "#open").await;
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    println!(
        " window_open connections={} requests={} counters={counters:?}",
        fixture.worker_probe.connections(),
        fixture.worker_probe.requests()
    );
    assert_eq!(
        fixture.worker_probe.connections(),
        0,
        "window.open opened a TCP connection to the private listener"
    );
    assert_eq!(fixture.worker_probe.requests(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n3_top_navigation_is_allowed_and_named_targets_are_reported() {
    let Some(chrome) =
        common::skip_without_chrome("n3_top_navigation_is_allowed_and_named_targets_are_reported")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);

    let opened = open(&manager, &fixture.origin, "/nav-targets.html").await;
    let top = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#top"}),
        })
        .await
        .expect("top click");
    assert!(
        top.final_url.ends_with("/next.html"),
        "_top must navigate same-context: {}",
        top.final_url
    );
    assert!(top.blocked.is_none(), "{:?}", top.blocked);
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");

    let opened = open(&manager, &fixture.origin, "/nav-targets.html").await;
    let parent = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#parent"}),
        })
        .await
        .expect("parent click");
    assert!(
        parent.final_url.ends_with("/next.html"),
        "{}",
        parent.final_url
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");

    let opened = open(&manager, &fixture.origin, "/nav-targets.html").await;
    let named = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#named"}),
        })
        .await
        .expect("named click");
    assert!(
        named.final_url.ends_with("/nav-targets.html"),
        "a named target must not replace the session page: {}",
        named.final_url
    );
    assert_eq!(
        named
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.as_str()),
        Some("navigation_cancelled")
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n2_shared_and_nested_workers_are_observed_or_recorded() {
    let Some(chrome) =
        common::skip_without_chrome("n2_shared_and_nested_workers_are_observed_or_recorded")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/shared.html").await;
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    println!(
        "N2 shared connections={} requests={} counters={counters:?} log={:?}",
        fixture.worker_probe.connections(),
        fixture.worker_probe.requests(),
        fixture.log.entries()
    );
    assert_eq!(
        fixture.worker_probe.requests(),
        0,
        "a shared/nested worker reached the private listener"
    );
    assert!(
        fixture
            .log
            .entries()
            .iter()
            .any(|entry| entry.starts_with("/n2-error?shared=")),
        "the page never observed the refused SharedWorker constructor: {:?}",
        fixture.log.entries()
    );
    assert!(
        !fixture
            .log
            .entries()
            .iter()
            .any(|entry| entry == "/shared-started"),
        "the paused shared worker ran, so containment did not hold"
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n2_a_nested_worker_is_observed_or_recorded() {
    let Some(chrome) = common::skip_without_chrome("n2_a_nested_worker_is_observed_or_recorded")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/nested.html").await;
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    println!(
        "N2 nested connections={} requests={} counters={counters:?} log={:?}",
        fixture.worker_probe.connections(),
        fixture.worker_probe.requests(),
        fixture.log.entries()
    );
    assert_eq!(
        fixture.worker_probe.requests(),
        0,
        "a nested worker reached the private listener"
    );
    if !fixture
        .log
        .entries()
        .iter()
        .any(|entry| entry == "/nested-started")
    {
        // The worker-created script did not execute in this harness; do not
        // claim coverage the test cannot demonstrate (recorded residual).
        println!("N2 nested not-executed: no /nested-started; coverage recorded as a residual");
    }
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

/// G0 discriminator: with the JS guard **disabled**, does the close-on-attach
/// backstop prevent a popup's first request for each G1-G4 creation shape?
/// Ignored by default; run explicitly with
/// `WEB_WIDOW_TEST_DISABLE_POPUP_GUARD=1 cargo test ... -- --ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "G0 guard-disabled probe; run with WEB_WIDOW_TEST_DISABLE_POPUP_GUARD=1"]
async fn g0_guard_disabled_backstop_probe() {
    let Some(chrome) = common::skip_without_chrome("g0_guard_disabled_backstop_probe") else {
        return;
    };
    if std::env::var_os("WEB_WIDOW_TEST_DISABLE_POPUP_GUARD").is_none() {
        println!("G0 skipped: set WEB_WIDOW_TEST_DISABLE_POPUP_GUARD=1");
        return;
    }
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    for (shape, selector) in [
        ("g1", "#g1"),
        ("g2", "#g2"),
        ("g3", "#g3"),
        ("g4", "#g4-go"),
    ] {
        let opened = open(&manager, &fixture.origin, "/g0.html").await;
        let before_connections = fixture.worker_probe.connections();
        let before_requests = fixture.worker_probe.requests();
        let before_targets = manager
            .counters(&opened.session_id)
            .await
            .expect("counters")
            .child_targets;
        click(&manager, &opened.session_id, selector).await;
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        let counters = manager
            .counters(&opened.session_id)
            .await
            .expect("counters");
        let connections = fixture.worker_probe.connections() - before_connections;
        let requests = fixture.worker_probe.requests() - before_requests;
        println!(
            "G0 shape={shape} child_targets={} connections={connections} requests={requests}",
            counters.child_targets.saturating_sub(before_targets)
        );
        manager
            .close(web_widow::interaction::InteractCloseInput {
                session_id: opened.session_id,
            })
            .await
            .expect("close");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g5_g6_forged_or_suppressed_markers_stay_honest() {
    let Some(chrome) =
        common::skip_without_chrome("g5_g6_forged_or_suppressed_markers_stay_honest")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);

    let opened = open(&manager, &fixture.origin, "/g5.html").await;
    assert!(
        fixture
            .log
            .entries()
            .iter()
            .any(|entry| entry == "/g5-state?guard=function"),
        "the guard probe was not a function: {:?}",
        fixture.log.entries()
    );
    let forged = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#forge"}),
        })
        .await
        .expect("forge click");
    assert!(
        forged.blocked.is_none(),
        "a page-written marker was reported: {:?}",
        forged.blocked
    );
    manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#suppress"}),
        })
        .await
        .expect("suppress attempt");
    let named = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#named"}),
        })
        .await
        .expect("named click after a suppression attempt");
    assert_eq!(
        named
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.as_str()),
        Some("navigation_cancelled"),
        "suppression of the marker broke the cancellation report"
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

/// G6 fail-closed: with the isolated integrity script disabled by the test
/// hook, `interact_open` must fail rather than trust an unverified guard.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "G6 integrity probe; run with WEB_WIDOW_TEST_DISABLE_INTEGRITY=1"]
async fn g6_missing_integrity_guard_fails_the_open() {
    let Some(chrome) = common::skip_without_chrome("g6_missing_integrity_guard_fails_the_open")
    else {
        return;
    };
    if std::env::var_os("WEB_WIDOW_TEST_DISABLE_INTEGRITY").is_none() {
        println!("G6 skipped: set WEB_WIDOW_TEST_DISABLE_INTEGRITY=1");
        return;
    }
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let error = manager
        .open(InteractOpenInput {
            url: format!("{}/g5.html", fixture.origin),
            render: None,
            viewport_width: None,
            viewport_height: None,
        })
        .await
        .expect_err("an unverified guard must fail the open");
    println!("G6 integrity_error={error}");
    assert!(error.to_string().contains("integrity guard"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g7_a_cap_does_not_mask_a_later_cancellation() {
    let Some(chrome) = common::skip_without_chrome("g7_a_cap_does_not_mask_a_later_cancellation")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let mut config = common::rendering_test_config(&chrome);
    config.fetch.allow_hosts = vec!["127.0.0.1/32".to_string()];
    config.fetch.min_interval_ms = 10;
    config.fetch.jitter_ms = 0;
    config.render.settle_ms = 150;
    config.render.visual_max_requests = 1;
    let env = common::TestEnv::new_rendering(config).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g5.html").await;
    let named = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#named"}),
        })
        .await
        .expect("named click under a cap");
    println!("G7 named_blocked={:?}", named.blocked);
    let blocked = named.blocked.as_ref().expect("cancellation is visible");
    assert_eq!(blocked.reason, "navigation_cancelled");
    assert!(
        blocked
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("cap=visual_max_requests")),
        "the cap must stay visible alongside the cancellation: {:?}",
        blocked.detail
    );
    let later = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "wait", "ms": 50}),
        })
        .await
        .expect("wait");
    assert_eq!(
        later
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.as_str()),
        Some("visual_max_requests")
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g1_g4_creation_shapes_are_cancelled_before_any_request() {
    let Some(chrome) =
        common::skip_without_chrome("g1_g4_creation_shapes_are_cancelled_before_any_request")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    for (shape, selector, expect_block) in [
        ("g1", "#g1", true),
        ("g2", "#g2", true),
        ("g3", "#g3", true),
        ("g4", "#g4-go", true),
    ] {
        let opened = open(&manager, &fixture.origin, "/g0.html").await;
        let before_connections = fixture.worker_probe.connections();
        let before_requests = fixture.worker_probe.requests();
        let action = manager
            .act(InteractActInput {
                session_id: opened.session_id.clone(),
                action: serde_json::json!({"kind": "click", "selector": selector}),
            })
            .await
            .expect("creation-shape click");
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        let counters = manager
            .counters(&opened.session_id)
            .await
            .expect("counters");
        let connections = fixture.worker_probe.connections() - before_connections;
        let requests = fixture.worker_probe.requests() - before_requests;
        println!(
            "G-SHAPE {shape} child_targets={} connections={connections} requests={requests} blocked={:?}",
            counters.child_targets,
            action
                .blocked
                .as_ref()
                .map(|blocked| blocked.reason.clone())
        );
        assert_eq!(connections, 0, "{shape}: creation reached the listener");
        assert_eq!(requests, 0, "{shape}: creation made a private request");
        if expect_block {
            assert_eq!(
                action
                    .blocked
                    .as_ref()
                    .map(|blocked| blocked.reason.as_str()),
                Some("navigation_cancelled"),
                "{shape}: a refused creation must be reported"
            );
        }
        manager
            .close(web_widow::interaction::InteractCloseInput {
                session_id: opened.session_id,
            })
            .await
            .expect("close");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n1_guard_bypass_paths_are_contained() {
    let Some(chrome) = common::skip_without_chrome("n1_guard_bypass_paths_are_contained") else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);

    for selector in ["#form-submit", "#shadow-host", "#proto-open"] {
        let opened = open(&manager, &fixture.origin, "/n1.html").await;
        let before_connections = fixture.worker_probe.connections();
        let before_requests = fixture.worker_probe.requests();
        let action = manager
            .act(InteractActInput {
                session_id: opened.session_id.clone(),
                action: serde_json::json!({"kind": "click", "selector": selector}),
            })
            .await
            .expect("bypass click");
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        let counters = manager
            .counters(&opened.session_id)
            .await
            .expect("counters");
        let connections = fixture.worker_probe.connections() - before_connections;
        let requests = fixture.worker_probe.requests() - before_requests;
        let blocked = action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone());
        println!(
            "N1 {selector} final_url={} guard_log={:?} child_targets={} connections={} requests={} blocked={blocked:?}",
            action.final_url,
            fixture.log.entries(),
            counters.child_targets,
            connections,
            requests
        );
        assert_eq!(
            connections, 0,
            "{selector}: a popup bypass reached the private listener"
        );
        assert_eq!(
            requests, 0,
            "{selector}: a popup bypass made a private request"
        );
        if selector == "#proto-open" {
            // The fixture always attempts the activation through the only
            // reachable `window.open` (the server stub). Its own log line says
            // which descriptor exists and what the call returned, so 0 TCP is
            // attributed to a refused activation rather than an unexercised
            // shape (review round 5, n1).
            let guard = fixture
                .log
                .entries()
                .into_iter()
                .find(|entry| entry.starts_with("/n1-guard?proto="))
                .expect("guard probe");
            let probe = fixture
                .log
                .entries()
                .into_iter()
                .find(|entry| entry.starts_with("/n1-proto?"))
                .expect("proto activation probe");
            println!("N1 proto guard={guard} activation={probe}");
            assert_eq!(
                counters.child_targets, 0,
                "the prototype escape created a target"
            );
            assert!(
                probe.contains("opened=null") || probe.contains("opened=none"),
                "{probe}: the guarded window.open was not the result"
            );
            assert_eq!(
                blocked.as_deref(),
                Some("popup_blocked"),
                "{probe}: a refused activation must be reported"
            );
        } else {
            // Either the close backstop caught the target, or the guard
            // prevented its creation and reported the cancellation. Silence is
            // the only failure.
            assert!(
                counters.child_targets >= 1 || blocked.as_deref() == Some("navigation_cancelled"),
                "{selector}: bypass produced neither a contained target nor a reported cancellation"
            );
        }
        manager
            .close(web_widow::interaction::InteractCloseInput {
                session_id: opened.session_id,
            })
            .await
            .expect("close");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f2_a_dedicated_worker_request_is_gated_and_vet_observed() {
    let Some(chrome) =
        common::skip_without_chrome("f2_a_dedicated_worker_request_is_gated_and_vet_observed")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/worker.html").await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    println!(
        " worker connections={} requests={} counters={counters:?}",
        fixture.worker_probe.connections(),
        fixture.worker_probe.requests()
    );
    assert_eq!(
        fixture.worker_probe.requests(),
        0,
        "worker egress reached the listener"
    );
    assert!(
        counters.vet_observed > 0 || fixture.worker_probe.connections() > 0,
        "the worker request was neither vetted nor observed: {counters:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f2_a_service_worker_request_is_gated_and_vet_observed() {
    let Some(chrome) =
        common::skip_without_chrome("f2_a_service_worker_request_is_gated_and_vet_observed")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/sw.html").await;
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    println!(
        " sw connections={} requests={} counters={counters:?}",
        fixture.worker_probe.connections(),
        fixture.worker_probe.requests()
    );
    assert_eq!(
        fixture.worker_probe.requests(),
        0,
        "service worker egress reached the listener"
    );
    assert!(
        counters.vet_observed > 0 || fixture.worker_probe.connections() > 0,
        "the service-worker request was neither vetted nor observed: {counters:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f2_a_cross_site_iframe_request_is_gated_and_vet_observed() {
    let Some(chrome) =
        common::skip_without_chrome("f2_a_cross_site_iframe_request_is_gated_and_vet_observed")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32", "127.0.0.2/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/iframe.html").await;
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    println!(
        " iframe connections={} requests={} counters={counters:?}",
        fixture.iframe_probe.connections(),
        fixture.iframe_probe.requests()
    );
    assert_eq!(
        fixture.iframe_probe.requests(),
        0,
        "iframe egress reached the listener"
    );
    assert!(
        counters.vet_observed > 0 || fixture.iframe_probe.connections() > 0,
        "the iframe request was neither vetted nor observed: {counters:?}"
    );
}

#[allow(dead_code)]
fn _unused(fixture: &Fixture, log: &Log) {
    dwell(fixture, Duration::from_millis(0));
    let _ = log.entries();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f5_subresource_requests_take_the_per_host_lease() {
    let Some(chrome) =
        common::skip_without_chrome("f5_subresource_requests_take_the_per_host_lease")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let mut config = common::rendering_test_config(&chrome);
    config.fetch.allow_hosts = vec!["127.0.0.1/32".to_string()];
    config.fetch.min_interval_ms = 1_000;
    config.fetch.jitter_ms = 0;
    config.fetch.max_concurrent_per_host = 1;
    config.render.settle_ms = 100;
    let env = common::TestEnv::new_rendering(config).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/images.html").await;

    let deadline = Instant::now() + Duration::from_secs(20);
    while fixture.log.stamps_matching("/asset/img.png").len() < 6 {
        assert!(
            Instant::now() < deadline,
            "images did not arrive: {:?}",
            fixture.log.entries()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let stamps = fixture.log.stamps_matching("/asset/img.png");
    assert_eq!(stamps.len(), 6, "one request per image");
    for pair in stamps.windows(2) {
        let gap = pair[1].duration_since(pair[0]);
        assert!(
            gap >= Duration::from_millis(900),
            "subresource spacing below the 1 s host floor: {gap:?}"
        );
    }
    println!(
        " image_gaps={:?}",
        stamps
            .windows(2)
            .map(|p| p[1].duration_since(p[0]))
            .collect::<Vec<_>>()
    );
    let _ = opened;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f6_refused_requests_are_capped_before_the_vet() {
    let Some(chrome) = common::skip_without_chrome("f6_refused_requests_are_capped_before_the_vet")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/flood.html").await;

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let counters = manager
            .counters(&opened.session_id)
            .await
            .expect("counters");
        if counters.paused > 500 || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    println!(" counters={counters:?}");
    assert!(
        counters.paused > 500,
        "the flood did not reach the cap: {counters:?}"
    );
    assert!(
        counters.vet_observed <= 500,
        "the vet ran past the cap: {counters:?}"
    );
    let started = Instant::now();
    let wait = manager
        .act(InteractActInput {
            session_id: opened.session_id,
            action: serde_json::json!({"kind": "wait", "ms": 100}),
        })
        .await
        .expect("an action after the cap");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "action latency after the cap: {:?}",
        started.elapsed()
    );
    assert_eq!(
        wait.blocked.as_ref().map(|blocked| blocked.reason.as_str()),
        Some("request_cap")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f10_a_closed_session_releases_the_render_permit() {
    let Some(chrome) =
        common::skip_without_chrome("f10_a_closed_session_releases_the_render_permit")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let mut config = common::rendering_test_config(&chrome);
    config.fetch.allow_hosts = vec!["127.0.0.1/32".to_string()];
    config.render.max_concurrent_pages = 1;
    config.render.settle_ms = 100;
    let env = common::TestEnv::new_rendering(config).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/other.html").await;
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
    let url = url::Url::parse(&format!("{}/other.html", fixture.origin)).unwrap();
    let rendered = tokio::time::timeout(Duration::from_secs(5), env.renderer.render(&url))
        .await
        .expect("the closed session released its render permit")
        .expect("render after close");
    assert!(rendered.status == 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_blocked_by_the_gate_returns_the_typed_error() {
    let Some(chrome) =
        common::skip_without_chrome("an_open_blocked_by_the_gate_returns_the_typed_error")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let error = manager
        .open(InteractOpenInput {
            url: format!("{}/redirect-private", fixture.origin),
            render: None,
            viewport_width: None,
            viewport_height: None,
        })
        .await
        .expect_err("a redirected private navigation must be refused");
    println!("OPEN_BLOCKED code={:?} message={error}", error.code());
    assert_eq!(error.code(), ErrorCode::BlockedPrivateAddress);
    assert!(!error.to_string().contains("session navigation"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f11_origin_change_and_field_caps() {
    let Some(chrome) = common::skip_without_chrome("f11_origin_change_and_field_caps") else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);

    let opened = open(&manager, &fixture.origin, "/origin.html").await;
    let navigated = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#cross"}),
        })
        .await
        .expect("cross-origin click");
    assert!(navigated.origin_changed, "{navigated:?}");
    assert!(
        navigated
            .previous_origin
            .as_deref()
            .is_some_and(|origin| origin.contains("127.0.0.1")),
        "{navigated:?}"
    );
    assert_ne!(
        navigated.previous_origin.as_deref(),
        Some(navigated.final_url.as_str())
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");

    let opened = open(&manager, &fixture.origin, "/longurl.html").await;
    let big = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#big"}),
        })
        .await
        .expect("long URL click");
    assert!(
        big.final_url.chars().count() <= 4_096,
        "{}",
        big.final_url.chars().count()
    );
    assert_eq!(big.limit_hit.as_deref(), Some("final_url"));
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");

    let opened = open(&manager, &fixture.origin, "/longtitle.html").await;
    let state = manager
        .state(InteractStateInput {
            session_id: opened.session_id.clone(),
            screenshot: Some(false),
            console_limit: None,
        })
        .await
        .expect("long title state");
    let title = state.title.expect("title");
    assert!(title.chars().count() <= 300, "{}", title.chars().count());
    assert_eq!(state.limit_hit.as_deref(), Some("title"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f12_hostile_selectors_have_no_side_effects() {
    let Some(chrome) = common::skip_without_chrome("f12_hostile_selectors_have_no_side_effects")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/hostile.html").await;
    for selector in ["\"]", "`", "${", "\\", "a\nb", "x\"];fetch('/injected');//"] {
        let error = manager
            .act(InteractActInput {
                session_id: opened.session_id.clone(),
                action: serde_json::json!({"kind": "click", "selector": selector}),
            })
            .await
            .expect_err("a hostile selector must not succeed");
        assert_eq!(
            error.code(),
            ErrorCode::ActionFailed,
            "{selector:?}: {error}"
        );
    }
    assert!(
        !fixture
            .log
            .entries()
            .iter()
            .any(|entry| entry.contains("/injected")),
        "a selector injection executed page JavaScript: {:?}",
        fixture.log.entries()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g8_base_target_inheritance_is_prevented() {
    let Some(chrome) = common::skip_without_chrome("g8_base_target_inheritance_is_prevented")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    for selector in ["#g8a", "#g8b"] {
        let opened = open(&manager, &fixture.origin, "/g8.html").await;
        let before = fixture.worker_probe.connections();
        let action = manager
            .act(InteractActInput {
                session_id: opened.session_id.clone(),
                action: serde_json::json!({"kind": "click", "selector": selector}),
            })
            .await
            .expect("base-target click");
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        let connections = fixture.worker_probe.connections() - before;
        println!(
            "G8 {selector} connections={connections} blocked={:?}",
            action
                .blocked
                .as_ref()
                .map(|blocked| blocked.reason.clone())
        );
        assert_eq!(connections, 0, "{selector}: base-target creation escaped");
        assert_eq!(
            action
                .blocked
                .as_ref()
                .map(|blocked| blocked.reason.as_str()),
            Some("navigation_cancelled"),
            "{selector}: base inheritance must be refused"
        );
        manager
            .close(web_widow::interaction::InteractCloseInput {
                session_id: opened.session_id,
            })
            .await
            .expect("close");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g9_decoy_anchors_do_not_exhaust_the_walk() {
    let Some(chrome) = common::skip_without_chrome("g9_decoy_anchors_do_not_exhaust_the_walk")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g9.html").await;
    let before = fixture.worker_probe.connections();
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g9"}),
        })
        .await
        .expect("decoy click");
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let connections = fixture.worker_probe.connections() - before;
    println!(
        "G9 connections={connections} blocked={:?}",
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone())
    );
    assert_eq!(connections, 0, "500 decoys exhausted the walk");
    assert_eq!(
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.as_str()),
        Some("navigation_cancelled")
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g10_display_contents_anchor_is_prevented() {
    let Some(chrome) = common::skip_without_chrome("g10_display_contents_anchor_is_prevented")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g10.html").await;
    let before = fixture.worker_probe.connections();
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g10"}),
        })
        .await
        .expect("display:contents click");
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let connections = fixture.worker_probe.connections() - before;
    println!(
        "G10 connections={connections} blocked={:?}",
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone())
    );
    assert_eq!(connections, 0, "display:contents anchor escaped");
    assert_eq!(
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.as_str()),
        Some("navigation_cancelled")
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g11_request_submit_formtarget_is_prevented() {
    let Some(chrome) = common::skip_without_chrome("g11_request_submit_formtarget_is_prevented")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g11.html").await;
    let before = fixture.worker_probe.connections();
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g11-run"}),
        })
        .await
        .expect("requestSubmit click");
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let connections = fixture.worker_probe.connections() - before;
    println!(
        "G11 connections={connections} blocked={:?}",
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone())
    );
    assert_eq!(connections, 0, "requestSubmit(formtarget) escaped");
    assert_eq!(
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.as_str()),
        Some("navigation_cancelled")
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g12_page_script_activation_is_prevented() {
    let Some(chrome) = common::skip_without_chrome("g12_page_script_activation_is_prevented")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    for selector in ["#g12-click", "#g12-dispatch"] {
        let opened = open(&manager, &fixture.origin, "/g12.html").await;
        let before = fixture.worker_probe.connections();
        let action = manager
            .act(InteractActInput {
                session_id: opened.session_id.clone(),
                action: serde_json::json!({"kind": "click", "selector": selector}),
            })
            .await
            .expect("page-script activation click");
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        let connections = fixture.worker_probe.connections() - before;
        println!(
            "G12 {selector} connections={connections} blocked={:?}",
            action
                .blocked
                .as_ref()
                .map(|blocked| blocked.reason.clone())
        );
        assert_eq!(connections, 0, "{selector}: page-script creation escaped");
        assert_eq!(
            action
                .blocked
                .as_ref()
                .map(|blocked| blocked.reason.as_str()),
            Some("navigation_cancelled"),
            "{selector}: page-script activation must be refused"
        );
        manager
            .close(web_widow::interaction::InteractCloseInput {
                session_id: opened.session_id,
            })
            .await
            .expect("close");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g13_marker_is_frozen_against_page_mutation() {
    let Some(chrome) = common::skip_without_chrome("g13_marker_is_frozen_against_page_mutation")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g13.html").await;
    let first = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g13-mutate"}),
        })
        .await
        .expect("mutating form-submit click");
    println!("G13 first_blocked={:?}", first.blocked);
    assert_eq!(
        first
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.as_str()),
        Some("navigation_cancelled"),
        "page mutation rewrote the marker"
    );
    let second = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g13-again"}),
        })
        .await
        .expect("second form-submit click");
    println!("G13 second_blocked={:?}", second.blocked);
    assert_eq!(
        second
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.as_str()),
        Some("navigation_cancelled"),
        "a pinned sequence suppressed the next genuine cancellation"
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g14_two_base_elements_select_the_first_with_a_target() {
    let Some(chrome) =
        common::skip_without_chrome("g14_two_base_elements_select_the_first_with_a_target")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g14.html").await;
    let before = fixture.worker_probe.connections();
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g14"}),
        })
        .await
        .expect("two-base click");
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    let connections = fixture.worker_probe.connections() - before;
    println!(
        "G14 connections={connections} child_targets={} blocked={:?}",
        counters.child_targets,
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone())
    );
    assert_eq!(connections, 0, "the second <base target> was not honoured");
    assert_eq!(counters.child_targets, 0, "a target was created");
    assert_eq!(
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.as_str()),
        Some("navigation_cancelled"),
        "an attributeless anchor under <base target> must be refused"
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g16_page_tampered_attribute_reads_cannot_blind_the_check() {
    let Some(chrome) =
        common::skip_without_chrome("g16_page_tampered_attribute_reads_cannot_blind_the_check")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g16.html").await;
    let before = fixture.worker_probe.connections();
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g16"}),
        })
        .await
        .expect("tampered read click");
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    let connections = fixture.worker_probe.connections() - before;
    let installed = fixture
        .log
        .entries()
        .into_iter()
        .find(|entry| entry.starts_with("/g16-installed?"))
        .expect("the fixture did not install the page overrides");
    println!(
        "G16 installed={installed} connections={connections} child_targets={} blocked={:?}",
        counters.child_targets,
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone())
    );
    assert!(
        installed.contains("base_null=1") && installed.contains("attr_null=1"),
        "{installed}: the page overrides were not live"
    );
    assert_eq!(
        connections, 0,
        "a tampered attribute read blinded the check"
    );
    assert_eq!(counters.child_targets, 0, "a target was created");
    assert_eq!(
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.as_str()),
        Some("navigation_cancelled")
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g17_page_scroll_hook_never_runs_before_dispatch() {
    let Some(chrome) =
        common::skip_without_chrome("g17_page_scroll_hook_never_runs_before_dispatch")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g17.html").await;
    let before = fixture.worker_probe.connections();
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g17"}),
        })
        .await
        .expect("scroll-hook click");
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    let connections = fixture.worker_probe.connections() - before;
    let scroll_called = fixture
        .log
        .entries()
        .into_iter()
        .any(|entry| entry.starts_with("/g17-scroll-called"));
    println!(
        "G17 scroll_called={scroll_called} connections={connections} child_targets={} blocked={:?}",
        counters.child_targets,
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone())
    );
    assert!(
        !scroll_called,
        "the page's scrollIntoView ran in the dispatch path"
    );
    assert_eq!(connections, 0, "a relocation opened the trap anchor");
    assert_eq!(counters.child_targets, 0, "a target was created");
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g18_box_moved_between_check_and_dispatch_is_refused() {
    let Some(chrome) =
        common::skip_without_chrome("g18_box_moved_between_check_and_dispatch_is_refused")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g18.html").await;
    let before = fixture.worker_probe.connections();
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g18"}),
        })
        .await
        .expect("moving-box click");
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    let connections = fixture.worker_probe.connections() - before;
    let hovered = fixture
        .log
        .entries()
        .into_iter()
        .any(|entry| entry == "/g18-hover");
    println!(
        "G18 hovered={hovered} connections={connections} child_targets={} blocked={:?}",
        counters.child_targets,
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone())
    );
    assert!(hovered, "the hover handler did not run");
    assert_eq!(connections, 0, "a moved box was clicked anyway");
    assert_eq!(counters.child_targets, 0, "a target was created");
    assert_eq!(
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.as_str()),
        Some("click_point_moved"),
        "a box that moved after the check must refuse the click"
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g19_defeated_freeze_primitive_still_reports_the_cancellation() {
    let Some(chrome) =
        common::skip_without_chrome("g19_defeated_freeze_primitive_still_reports_the_cancellation")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g19.html").await;
    let before = fixture.worker_probe.connections();
    for (attempt, selector) in [(1, "#g19"), (2, "#g19")] {
        let action = manager
            .act(InteractActInput {
                session_id: opened.session_id.clone(),
                action: serde_json::json!({"kind": "click", "selector": selector}),
            })
            .await
            .expect("freeze-tampered click");
        println!(
            "G19 attempt={attempt} blocked={:?}",
            action
                .blocked
                .as_ref()
                .map(|blocked| blocked.reason.clone())
        );
        assert_eq!(
            action
                .blocked
                .as_ref()
                .map(|blocked| blocked.reason.as_str()),
            Some("popup_blocked"),
            "attempt {attempt}: a defeated Object.freeze hid the cancellation"
        );
    }
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let connections = fixture.worker_probe.connections() - before;
    let state = fixture
        .log
        .entries()
        .into_iter()
        .filter(|entry| entry.starts_with("/g19-state?"))
        .collect::<Vec<_>>();
    println!("G19 connections={connections} state={state:?}");
    assert_eq!(
        connections, 0,
        "a cancellation escaped as a real navigation"
    );
    assert!(
        state.len() >= 2,
        "the fixture did not record both attempts: {state:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g20_light_dom_sibling_form_is_refused() {
    let Some(chrome) = common::skip_without_chrome("g20_light_dom_sibling_form_is_refused") else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g20.html").await;
    let before = fixture.worker_probe.connections();
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g20"}),
        })
        .await
        .expect("light-DOM sibling form click");
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    let connections = fixture.worker_probe.connections() - before;
    println!(
        "G20 connections={connections} child_targets={} blocked={:?}",
        counters.child_targets,
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone())
    );
    assert_eq!(connections, 0, "the light-DOM sibling form escaped");
    assert_eq!(counters.child_targets, 0, "a target was created");
    assert_eq!(
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.as_str()),
        Some("navigation_cancelled"),
        "the submit listener did not cancel the form's _blank target"
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g21_shadow_sibling_form_owner_is_refused() {
    let Some(chrome) = common::skip_without_chrome("g21_shadow_sibling_form_owner_is_refused")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g21.html").await;
    let mut results = Vec::new();
    for selector in ["#g21a", "#g21b", "#g21c"] {
        let before = fixture.worker_probe.connections();
        let action = manager
            .act(InteractActInput {
                session_id: opened.session_id.clone(),
                action: serde_json::json!({"kind": "click", "selector": selector}),
            })
            .await
            .expect("shadow sibling form click");
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        let connections = fixture.worker_probe.connections() - before;
        let blocked = action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone());
        println!("G21 {selector} connections={connections} blocked={blocked:?}");
        results.push((selector, connections, blocked));
    }
    for (selector, connections, blocked) in &results {
        assert_eq!(
            *connections, 0,
            "{selector}: a sibling form owner in a shadow tree escaped"
        );
        assert_eq!(
            blocked.as_deref(),
            Some("navigation_cancelled"),
            "{selector}: the form owner was not resolved"
        );
    }
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g22_document_open_rewrite_does_not_drop_the_guard() {
    let Some(chrome) =
        common::skip_without_chrome("g22_document_open_rewrite_does_not_drop_the_guard")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g22.html").await;
    let before = fixture.worker_probe.connections();
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g22"}),
        })
        .await
        .expect("reopened document click");
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    let connections = fixture.worker_probe.connections() - before;
    println!(
        "G22 connections={connections} child_targets={} blocked={:?}",
        counters.child_targets,
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone())
    );
    assert_eq!(
        connections, 0,
        "a document.open rewrite dropped the guard's listeners"
    );
    assert_eq!(counters.child_targets, 0, "a target was created");
    assert_eq!(
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.as_str()),
        Some("navigation_cancelled"),
        "the mousedown target flip was not cancelled"
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g23_pointer_events_none_overlay_does_not_hide_the_target() {
    let Some(chrome) =
        common::skip_without_chrome("g23_pointer_events_none_overlay_does_not_hide_the_target")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g23.html").await;
    for (selector, note) in [
        ("#g23", "closed shadow root (server hit test only)"),
        ("#g23o", "open shadow root (guard also sees it)"),
    ] {
        let before = fixture.worker_probe.connections();
        let action = manager
            .act(InteractActInput {
                session_id: opened.session_id.clone(),
                action: serde_json::json!({"kind": "click", "selector": selector}),
            })
            .await
            .expect("pointer-events click");
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        let connections = fixture.worker_probe.connections() - before;
        println!(
            "G23 {selector} ({note}) connections={connections} blocked={:?}",
            action
                .blocked
                .as_ref()
                .map(|blocked| blocked.reason.clone())
        );
        assert_eq!(
            connections, 0,
            "{selector}: a pointer-events:none overlay hid the activation target"
        );
    }
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g24_page_selection_override_cannot_choose_the_click_target() {
    let Some(chrome) =
        common::skip_without_chrome("g24_page_selection_override_cannot_choose_the_click_target")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g24.html").await;
    let installed = fixture
        .log
        .entries()
        .into_iter()
        .find(|entry| entry.starts_with("/g24-installed?"))
        .expect("the fixture did not install the selection override");
    let before = fixture.worker_probe.connections();
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g24-real"}),
        })
        .await
        .expect("selection-override click");
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let connections = fixture.worker_probe.connections() - before;
    println!(
        "G24 installed={installed} connections={connections} blocked={:?}",
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone())
    );
    assert!(
        installed.contains("decoy=1"),
        "{installed}: the page querySelectorAll override was not live"
    );
    assert_eq!(
        connections, 0,
        "a page selection override chose a different click target"
    );
    assert_eq!(
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.as_str()),
        Some("navigation_cancelled"),
        "the real element was not resolved and refused"
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

///: with the isolated integrity world disabled, a page that forges the
/// heartbeat text (it can read the main-world nonce) must not satisfy it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = " heartbeat-forge probe; run with WEB_WIDOW_TEST_DISABLE_INTEGRITY=1"]
async fn g25_forged_heartbeat_still_fails_closed() {
    let Some(chrome) = common::skip_without_chrome("g25_forged_heartbeat_still_fails_closed")
    else {
        return;
    };
    if std::env::var_os("WEB_WIDOW_TEST_DISABLE_INTEGRITY").is_none() {
        println!("G25 skipped: set WEB_WIDOW_TEST_DISABLE_INTEGRITY=1");
        return;
    }
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let result = manager
        .open(InteractOpenInput {
            url: format!("{}/g25.html", fixture.origin),
            render: None,
            viewport_width: None,
            viewport_height: None,
        })
        .await;
    match &result {
        Ok(opened) => {
            let _ = manager
                .close(web_widow::interaction::InteractCloseInput {
                    session_id: opened.session_id.clone(),
                })
                .await;
            panic!("G25 forged heartbeat: the open succeeded without the isolated world");
        }
        Err(error) => println!("G25 forged heartbeat refused: {error}"),
    }
}

///: the internal console prefix must not hide page output.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g26_internal_console_prefix_does_not_hide_page_lines() {
    let Some(chrome) =
        common::skip_without_chrome("g26_internal_console_prefix_does_not_hide_page_lines")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g26.html").await;
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g26"}),
        })
        .await
        .expect("console prefix click");
    let lines: Vec<String> = action
        .console_tail
        .iter()
        .map(|entry| entry.text.clone())
        .collect();
    println!("G26 console_tail={lines:?}");
    assert!(
        lines.iter().any(|line| line == "WWIW:page-line"),
        "a page line with the old prefix was hidden: {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line == "WWIW2:page-line"),
        "a page line with the internal prefix was hidden: {lines:?}"
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

/// Label-delegated activation into a closed declarative shadow root: the hit
/// node is the `<label>`, its control (and the control's form target) is
/// outside the hit chain, and the guard cannot see it through the closed root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g27_label_delegated_activation_is_refused() {
    let Some(chrome) = common::skip_without_chrome("g27_label_delegated_activation_is_refused")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g27.html").await;
    let mut results = Vec::new();
    for (selector, note) in [
        ("#g27a", "label for= a sibling submit with formtarget"),
        ("#g27b", "label wrapping the submit control"),
        (
            "#g27c",
            "label for= a sibling submit whose form target is _blank",
        ),
    ] {
        let before = fixture.worker_probe.connections();
        let action = manager
            .act(InteractActInput {
                session_id: opened.session_id.clone(),
                action: serde_json::json!({"kind": "click", "selector": selector}),
            })
            .await
            .expect("label delegation click");
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        let connections = fixture.worker_probe.connections() - before;
        let blocked = action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone());
        println!("G27 {selector} ({note}) connections={connections} blocked={blocked:?}");
        results.push((selector, connections, blocked));
    }
    for (selector, connections, blocked) in &results {
        assert_eq!(
            *connections, 0,
            "{selector}: label-delegated activation escaped the gate"
        );
        assert_eq!(
            blocked.as_deref(),
            Some("navigation_cancelled"),
            "{selector}: the labeled control's effective target was not evaluated"
        );
    }
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

/// The same fixture with creation prevention disabled: the shape must leak, or
/// the test above is not proving anything (review round 7).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "g27 live-shape probe; run with WEB_WIDOW_TEST_DISABLE_POPUP_GUARD=1"]
async fn g27_disabled_prevention_shows_the_live_shape() {
    let Some(chrome) = common::skip_without_chrome("g27_disabled_prevention_shows_the_live_shape")
    else {
        return;
    };
    if std::env::var_os("WEB_WIDOW_TEST_DISABLE_POPUP_GUARD").is_none() {
        println!("G27-shape skipped: set WEB_WIDOW_TEST_DISABLE_POPUP_GUARD=1");
        return;
    }
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g27.html").await;
    for selector in ["#g27a", "#g27b", "#g27c"] {
        let before = fixture.worker_probe.connections();
        let _ = manager
            .act(InteractActInput {
                session_id: opened.session_id.clone(),
                action: serde_json::json!({"kind": "click", "selector": selector}),
            })
            .await;
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        let connections = fixture.worker_probe.connections() - before;
        println!("G27-shape {selector} connections={connections}");
        assert!(
            connections >= 1,
            "{selector}: the disabled-prevention shape did not leak, so the check is not live"
        );
    }
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

/// Over-block non-regression: a submitter's non-empty `formtarget="_self"`
/// overrides the form's `_blank`, so the click must go through (review low #2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g28_submitter_formtarget_self_is_not_over_blocked() {
    let Some(chrome) =
        common::skip_without_chrome("g28_submitter_formtarget_self_is_not_over_blocked")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g28.html").await;
    let before = fixture.worker_probe.connections();
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g28"}),
        })
        .await
        .expect("formtarget=_self click");
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let connections = fixture.worker_probe.connections() - before;
    println!(
        "G28 connections={connections} blocked={:?} final_url={}",
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone()),
        action.final_url
    );
    assert!(
        action.blocked.is_none(),
        "a same-tab submit was over-blocked: {:?}",
        action.blocked
    );
    assert_eq!(connections, 0, "nothing should reach the leak probe");
    assert!(
        action.final_url.contains("/next.html"),
        "the _self submit did not navigate: {}",
        action.final_url
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

/// `type_text` splits the input per character and looks each one up in the key
/// table; a newline is not a key name, so the action fails instead of pressing
/// Enter. Removing that accidental safety would make the guard's submit
/// listener the only control for an implicit `_blank` submission (review
/// low #2), so the behaviour is pinned here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g29_typed_newline_cannot_submit_a_blank_form() {
    let Some(chrome) = common::skip_without_chrome("g29_typed_newline_cannot_submit_a_blank_form")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g29.html").await;
    let before = fixture.worker_probe.connections();
    let result = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "type", "selector": "#g29", "text": "hello\n"}),
        })
        .await;
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let connections = fixture.worker_probe.connections() - before;
    println!(
        "G29 typed_newline_error={:?} connections={connections}",
        result.as_ref().err().map(|error| error.to_string())
    );
    assert!(
        result.is_err(),
        "typing a newline unexpectedly succeeded; an implicit _blank submit may be reachable"
    );
    assert_eq!(connections, 0, "a typed newline submitted a _blank form");
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

/// Target-name normalisation: a whitespace-only target. The server and the
/// guard must agree with the UA on whether `" "` is an empty target (base
/// fallback / `_self`) or a named browsing context (a new target).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g30_whitespace_target_names_match_the_ua() {
    let Some(chrome) = common::skip_without_chrome("g30_whitespace_target_names_match_the_ua")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g30.html").await;
    let mut results = Vec::new();
    for (selector, note) in [
        ("#g30a", "anchor target=\" \""),
        ("#g30b", "form target=\" \""),
        ("#g30c", "input formtarget=\" \""),
    ] {
        let before = fixture.worker_probe.connections();
        let action = manager
            .act(InteractActInput {
                session_id: opened.session_id.clone(),
                action: serde_json::json!({"kind": "click", "selector": selector}),
            })
            .await
            .expect("whitespace-target click");
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        let counters = manager
            .counters(&opened.session_id)
            .await
            .expect("counters");
        let connections = fixture.worker_probe.connections() - before;
        let blocked = action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone());
        println!(
            "G30 {selector} ({note}) connections={connections} child_targets={} blocked={blocked:?} final_url={}",
            counters.child_targets, action.final_url
        );
        results.push((selector, connections, counters.child_targets, blocked));
    }
    for (selector, connections, child_targets, blocked) in &results {
        assert_eq!(
            *connections, 0,
            "{selector}: a whitespace target created a named context"
        );
        assert_eq!(*child_targets, 0, "{selector}: a target was created");
        assert_eq!(
            blocked.as_deref(),
            Some("navigation_cancelled"),
            "{selector}: the whitespace target was not refused"
        );
    }
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

/// A label whose click target is interactive content does not activate its
/// labeled control: the UA runs the anchor and the label does nothing. The
/// label pass must not refuse that click.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g31_label_over_interactive_content_is_not_over_blocked() {
    let Some(chrome) =
        common::skip_without_chrome("g31_label_over_interactive_content_is_not_over_blocked")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g31.html").await;
    let before = fixture.worker_probe.connections();
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g31"}),
        })
        .await
        .expect("label-over-anchor click");
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let connections = fixture.worker_probe.connections() - before;
    println!(
        "G31 connections={connections} blocked={:?} final_url={}",
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone()),
        action.final_url
    );
    assert_eq!(connections, 0, "the labeled control's form submitted");
    assert!(
        action.blocked.is_none(),
        "an interactive-content descendant was over-blocked: {:?}",
        action.blocked
    );
    assert!(
        action.final_url.contains("/next.html"),
        "the anchor's same-tab navigation did not run: {}",
        action.final_url
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

/// Page-script activation of a label in an open shadow root: no input dispatch
/// reaches the server predicate, so the guard's label branch is the only
/// refusal. Mirrors g12 for labels.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g32_page_script_label_activation_is_refused() {
    let Some(chrome) = common::skip_without_chrome("g32_page_script_label_activation_is_refused")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g32.html").await;
    let before = fixture.worker_probe.connections();
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g32-run"}),
        })
        .await
        .expect("page-script label click");
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    let connections = fixture.worker_probe.connections() - before;
    println!(
        "G32 connections={connections} child_targets={} blocked={:?}",
        counters.child_targets,
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone())
    );
    assert_eq!(
        connections, 0,
        "a page-script label.click() created a new context"
    );
    assert_eq!(counters.child_targets, 0, "a target was created");
    assert_eq!(
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.as_str()),
        Some("navigation_cancelled"),
        "the guard's label branch did not refuse page-script activation"
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

/// The interactive-content suppression rule decides whether the labeled control
/// is modelled at all, so its membership must match the UA: a focusable span
/// (`tabindex`) and an `input[type=hidden]` are not interactive content.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g33_label_suppression_membership_matches_the_ua() {
    let Some(chrome) =
        common::skip_without_chrome("g33_label_suppression_membership_matches_the_ua")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g33.html").await;
    let mut results = Vec::new();
    for (selector, note) in [
        ("#g33a", "span tabindex=0 inside the label (server)"),
        ("#g33b", "input type=hidden styled visible (server)"),
        (
            "#g33c-run",
            "page-script dispatchEvent on a tabindex span (guard)",
        ),
    ] {
        let before = fixture.worker_probe.connections();
        let before_targets = manager
            .counters(&opened.session_id)
            .await
            .expect("counters")
            .child_targets;
        let action = manager
            .act(InteractActInput {
                session_id: opened.session_id.clone(),
                action: serde_json::json!({"kind": "click", "selector": selector}),
            })
            .await
            .expect("suppression membership click");
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        let counters = manager
            .counters(&opened.session_id)
            .await
            .expect("counters");
        let connections = fixture.worker_probe.connections() - before;
        let blocked = action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone());
        let child_targets = counters.child_targets.saturating_sub(before_targets);
        println!(
            "G33 {selector} ({note}) connections={connections} child_targets={child_targets} blocked={blocked:?}"
        );
        results.push((selector, connections, child_targets, blocked));
    }
    for (selector, connections, child_targets, blocked) in &results {
        assert_eq!(
            *connections, 0,
            "{selector}: a non-interactive descendant released the label's control"
        );
        assert_eq!(*child_targets, 0, "{selector}: a target was created");
        assert_eq!(
            blocked.as_deref(),
            Some("navigation_cancelled"),
            "{selector}: the labeled control was not modelled"
        );
    }
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

/// The guard predicate is the only control for page-script activation, so its
/// target normalisation needs its own measured variants.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g34_guard_target_normalisation_matches_the_ua() {
    let Some(chrome) = common::skip_without_chrome("g34_guard_target_normalisation_matches_the_ua")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);

    // `target=" "` is a named context: the guard must refuse it.
    let opened = open(&manager, &fixture.origin, "/g34.html").await;
    let before = fixture.worker_probe.connections();
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g34-run-space"}),
        })
        .await
        .expect("space target click");
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let connections = fixture.worker_probe.connections() - before;
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    println!(
        "G34 space connections={connections} child_targets={} blocked={:?}",
        counters.child_targets,
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone())
    );
    assert_eq!(connections, 0, "the guard allowed a whitespace target");
    assert_eq!(counters.child_targets, 0, "a target was created");
    assert_eq!(
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.as_str()),
        Some("navigation_cancelled"),
        "the guard did not refuse the whitespace target"
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");

    // `target="_SELF"` is `_self` (ASCII case-insensitive): same-tab, allowed.
    let opened = open(&manager, &fixture.origin, "/g34.html").await;
    let before = fixture.worker_probe.connections();
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g34-run-upper"}),
        })
        .await
        .expect("upper target click");
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let connections = fixture.worker_probe.connections() - before;
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    println!(
        "G34 upper connections={connections} child_targets={} blocked={:?} final_url={}",
        counters.child_targets,
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone()),
        action.final_url
    );
    assert_eq!(connections, 0, "the guard opened a target for _SELF");
    assert_eq!(counters.child_targets, 0, "a target was created");
    assert!(
        action.blocked.is_none(),
        "_SELF was over-blocked: {:?}",
        action.blocked
    );
    assert!(
        action.final_url.contains("/next.html"),
        "the same-tab navigation did not run: {}",
        action.final_url
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

/// An empty `formtarget` inside a `_blank` form: the UA measurement (the
/// disabled-prevention probe below) fixes which way this must go; both
/// predicates follow it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g35_empty_formtarget_matches_the_ua() {
    let Some(chrome) = common::skip_without_chrome("g35_empty_formtarget_matches_the_ua") else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g35.html").await;
    let before = fixture.worker_probe.connections();
    let before_requests = fixture.worker_probe.requests();
    let before_targets = manager
        .counters(&opened.session_id)
        .await
        .expect("counters")
        .child_targets;
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g35"}),
        })
        .await
        .expect("empty formtarget click");
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    let connections = fixture.worker_probe.connections() - before;
    let requests = fixture.worker_probe.requests() - before_requests;
    let child_targets = counters.child_targets.saturating_sub(before_targets);
    let blocked = action
        .blocked
        .as_ref()
        .map(|blocked| blocked.reason.clone());
    println!(
        "G35 connections={connections} requests={requests} child_targets={child_targets} blocked={blocked:?} final_url={}",
        action.final_url
    );
    // Measured UA behaviour (g35-shape): a present-but-empty `formtarget` is
    // same-tab, so neither predicate refuses it; the same-tab navigation to the
    // private probe is then refused by the ordinary network gate.
    assert_eq!(child_targets, 0, "an empty formtarget created a target");
    assert_eq!(requests, 0, "a request reached the private probe");
    assert_eq!(
        blocked.as_deref(),
        Some("blocked_private_address"),
        "the empty formtarget should be same-tab and gated as a private address"
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

/// Measures the empty-`formtarget` UA behaviour with creation prevention off:
/// 2 TCP means the form's `_blank` applies; 0 TCP means same-tab.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "g35 UA probe; run with WEB_WIDOW_TEST_DISABLE_POPUP_GUARD=1"]
async fn g35_disabled_prevention_shows_the_ua_member() {
    let Some(chrome) = common::skip_without_chrome("g35_disabled_prevention_shows_the_ua_member")
    else {
        return;
    };
    if std::env::var_os("WEB_WIDOW_TEST_DISABLE_POPUP_GUARD").is_none() {
        println!("G35-shape skipped: set WEB_WIDOW_TEST_DISABLE_POPUP_GUARD=1");
        return;
    }
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g35.html").await;
    let before = fixture.worker_probe.connections();
    let before_targets = manager
        .counters(&opened.session_id)
        .await
        .expect("counters")
        .child_targets;
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g35"}),
        })
        .await;
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let connections = fixture.worker_probe.connections() - before;
    let child_targets = manager
        .counters(&opened.session_id)
        .await
        .expect("counters")
        .child_targets
        .saturating_sub(before_targets);
    let (blocked, final_url) = match &action {
        Ok(payload) => (
            payload
                .blocked
                .as_ref()
                .map(|blocked| blocked.reason.clone()),
            payload.final_url.clone(),
        ),
        Err(error) => (Some(format!("error: {error}")), String::new()),
    };
    // A popup leaves the main page where it was and creates a child target; a
    // same-tab navigation would move the main frame (and is blocked by the
    // private-address gate because the probe is on 127.0.0.2).
    let verdict = if child_targets >= 1 {
        "popup (form target applies)"
    } else if final_url.contains("/g35.html") {
        "same-tab or no navigation (child target absent)"
    } else {
        "main frame navigated (same-tab)"
    };
    println!(
        "G35-shape connections={connections} child_targets={child_targets} blocked={blocked:?} final_url={final_url} verdict={verdict}"
    );
    // Popup (child target) or same-tab (the main frame left the page, here to
    // the gate's error page) — either way the probe discriminates its two
    // hypotheses.
    let popup = child_targets >= 1;
    let same_tab = final_url.contains("chrome-error") || final_url.contains("/probe");
    assert!(
        popup || same_tab,
        "the probe cannot discriminate: child_targets={child_targets} final_url={final_url}"
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

/// An `<a>` **without** `href` is not interactive content (the spec's
/// enumeration conditions the anchor on `href`; Blink's `IsInteractiveContent`
/// is `IsLink()`). The label activation therefore runs, so the labeled control
/// must be modelled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g33d_href_less_anchor_does_not_suppress_the_label() {
    let Some(chrome) =
        common::skip_without_chrome("g33d_href_less_anchor_does_not_suppress_the_label")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g33d.html").await;
    let before = fixture.worker_probe.connections();
    let before_targets = manager
        .counters(&opened.session_id)
        .await
        .expect("counters")
        .child_targets;
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g33d"}),
        })
        .await
        .expect("href-less anchor click");
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    let connections = fixture.worker_probe.connections() - before;
    let child_targets = counters.child_targets.saturating_sub(before_targets);
    println!(
        "G33D connections={connections} child_targets={child_targets} blocked={:?}",
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone())
    );
    assert_eq!(
        connections, 0,
        "an href-less anchor released the labeled control"
    );
    assert_eq!(child_targets, 0, "a target was created");
    assert_eq!(
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.as_str()),
        Some("navigation_cancelled"),
        "the label pass treated an href-less anchor as interactive content"
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

/// The UA arbitration for g33d: does the UA forward label activation for an
/// href-less anchor? 2 TCP + 2 child targets means the pre-fix suppression was
/// a real escape; 0/0 means the member was harmless.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "g33d UA probe; run with WEB_WIDOW_TEST_DISABLE_POPUP_GUARD=1"]
async fn g33d_disabled_prevention_shows_the_ua_member() {
    let Some(chrome) = common::skip_without_chrome("g33d_disabled_prevention_shows_the_ua_member")
    else {
        return;
    };
    if std::env::var_os("WEB_WIDOW_TEST_DISABLE_POPUP_GUARD").is_none() {
        println!("G33D-shape skipped: set WEB_WIDOW_TEST_DISABLE_POPUP_GUARD=1");
        return;
    }
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g33d.html").await;
    let before = fixture.worker_probe.connections();
    let before_targets = manager
        .counters(&opened.session_id)
        .await
        .expect("counters")
        .child_targets;
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g33d"}),
        })
        .await;
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let counters = manager
        .counters(&opened.session_id)
        .await
        .expect("counters");
    let connections = fixture.worker_probe.connections() - before;
    let child_targets = counters.child_targets.saturating_sub(before_targets);
    println!(
        "G33D-shape connections={connections} child_targets={child_targets} action_ok={}",
        action.is_ok()
    );
    assert!(
        connections >= 1 || child_targets >= 1,
        "the disabled-prevention shape did not leak, so the measurement cannot arbitrate"
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

/// The arbitration for the last unmeasured half of the explicit-target rule:
/// does an empty `formtarget` fall to `<base target>`?  A child target means the
/// base applies (and the pre-fix code is a bypass); zero means same-tab.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "g36 UA probe; run with WEB_WIDOW_TEST_DISABLE_POPUP_GUARD=1"]
async fn g36_disabled_prevention_shows_the_ua_member() {
    let Some(chrome) = common::skip_without_chrome("g36_disabled_prevention_shows_the_ua_member")
    else {
        return;
    };
    if std::env::var_os("WEB_WIDOW_TEST_DISABLE_POPUP_GUARD").is_none() {
        println!("G36-shape skipped: set WEB_WIDOW_TEST_DISABLE_POPUP_GUARD=1");
        return;
    }
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g36.html").await;
    let before = fixture.worker_probe.connections();
    let before_requests = fixture.worker_probe.requests();
    let before_targets = manager
        .counters(&opened.session_id)
        .await
        .expect("counters")
        .child_targets;
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g36"}),
        })
        .await;
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let connections = fixture.worker_probe.connections() - before;
    let requests = fixture.worker_probe.requests() - before_requests;
    let child_targets = manager
        .counters(&opened.session_id)
        .await
        .expect("counters")
        .child_targets
        .saturating_sub(before_targets);
    let (blocked, final_url) = match &action {
        Ok(payload) => (
            payload
                .blocked
                .as_ref()
                .map(|blocked| blocked.reason.clone()),
            payload.final_url.clone(),
        ),
        Err(error) => (Some(format!("error: {error}")), String::new()),
    };
    let verdict = if child_targets >= 1 {
        "base target applies (child context created)"
    } else if final_url.contains("chrome-error") || final_url.contains("/probe") {
        "same-tab (main frame navigated)"
    } else {
        "inconclusive"
    };
    println!(
        "G36-shape connections={connections} requests={requests} child_targets={child_targets} blocked={blocked:?} final_url={final_url} verdict={verdict}"
    );
    assert!(
        child_targets >= 1 || final_url.contains("chrome-error") || final_url.contains("/probe"),
        "the probe cannot discriminate: child_targets={child_targets} final_url={final_url}"
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

/// Post-fix pin for the measured UA: an empty `formtarget` under a
/// `<base target>` resolves through the base and must be refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g36_empty_formtarget_under_a_base_is_refused() {
    let Some(chrome) = common::skip_without_chrome("g36_empty_formtarget_under_a_base_is_refused")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g36.html").await;
    let before = fixture.worker_probe.connections();
    let before_requests = fixture.worker_probe.requests();
    let before_targets = manager
        .counters(&opened.session_id)
        .await
        .expect("counters")
        .child_targets;
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g36"}),
        })
        .await
        .expect("base + empty formtarget click");
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let connections = fixture.worker_probe.connections() - before;
    let requests = fixture.worker_probe.requests() - before_requests;
    let child_targets = manager
        .counters(&opened.session_id)
        .await
        .expect("counters")
        .child_targets
        .saturating_sub(before_targets);
    println!(
        "G36 connections={connections} requests={requests} child_targets={child_targets} blocked={:?}",
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone())
    );
    assert_eq!(
        child_targets, 0,
        "the empty formtarget fell to the base and created a target"
    );
    assert_eq!(requests, 0, "a request reached the private probe");
    assert_eq!(
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.as_str()),
        Some("navigation_cancelled"),
        "the empty formtarget under a base was not refused"
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

/// `form.requestSubmit(btn)` with `formtarget=""` inside a `_blank` form: the
/// present-but-empty formtarget takes precedence, so the effective target is
/// empty → base (none) → same-tab. The guard must not over-block it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g37_request_submit_with_empty_formtarget_is_not_over_blocked() {
    let Some(chrome) =
        common::skip_without_chrome("g37_request_submit_with_empty_formtarget_is_not_over_blocked")
    else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g37.html").await;
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g37"}),
        })
        .await
        .expect("requestSubmit click");
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let blocked = action
        .blocked
        .as_ref()
        .map(|blocked| blocked.reason.clone());
    println!("G37 blocked={blocked:?} final_url={}", action.final_url);
    assert!(
        blocked.is_none(),
        "requestSubmit with an empty formtarget was over-blocked: {blocked:?}"
    );
    assert!(
        action.final_url.contains("/next.html"),
        "the same-tab requestSubmit did not navigate: {}",
        action.final_url
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}

/// An href-less `<a target=_blank>` is not a link: the click must be allowed
/// (the UA does nothing), not refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g38_href_less_anchor_is_not_refused() {
    let Some(chrome) = common::skip_without_chrome("g38_href_less_anchor_is_not_refused") else {
        return;
    };
    let fixture = start_fixture().await;
    let env = env_with(&chrome, &["127.0.0.1/32"]).await;
    let manager = manager(&env);
    let opened = open(&manager, &fixture.origin, "/g38.html").await;
    let action = manager
        .act(InteractActInput {
            session_id: opened.session_id.clone(),
            action: serde_json::json!({"kind": "click", "selector": "#g38"}),
        })
        .await
        .expect("href-less anchor click");
    tokio::time::sleep(Duration::from_millis(500)).await;
    println!(
        "G38 blocked={:?} final_url={}",
        action
            .blocked
            .as_ref()
            .map(|blocked| blocked.reason.clone()),
        action.final_url
    );
    assert!(
        action.blocked.is_none(),
        "an href-less anchor was refused: {:?}",
        action.blocked
    );
    assert!(
        action.final_url.contains("/g38.html"),
        "an href-less anchor navigated: {}",
        action.final_url
    );
    manager
        .close(web_widow::interaction::InteractCloseInput {
            session_id: opened.session_id,
        })
        .await
        .expect("close");
}
