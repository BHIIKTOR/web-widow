# web-widow

An agent-native web crawler exposed as an MCP server. It fetches one URL at a
time over HTTP, extracts the main content, returns LLM-ready Markdown with
metadata, and stores what it fetched.

Status: **current — 34 MCP tools, schema v8.** The server covers static and rendered
fetch, structured and tabular extraction, bounded crawling, change monitoring with
webhooks and a feed, PDF text, local semantic search, and stdio + loopback HTTP
transports.

## What it does

- Thirty-four MCP tools over **stdio** (default) or opt-in **loopback streamable HTTP**:
  `fetch_page`, `fetch_feed`, `sitemap`, `pdf_extract`, `search`, `search_content`,
  `search_semantic`, `embedding_status`, `extract`, `extract_rows`, `screenshot`,
  `blob_get`, `interact_open`,
  `interact_act`, `interact_state`, `interact_close`, `crawl_start`,
  `crawl_status`, `crawl_results`, `crawl_cancel`, `monitor_create`,
  `monitor_list`, `monitor_check`, `monitor_changes`, `monitor_delete`,
  `webhook_target`, `webhook_status`, `recipe_save`, `recipe_list`, `recipe_apply`,
  `corpus_export`, `snapshot_compare`, `robots_cache`, `events`; plus the one-shot
  `web-widow run-due` CLI for cron/systemd timers (local, or `--http` against a
  running server without touching its data directory) and an **opt-in**
  in-process timer (`[scheduler].enabled`, default off).
- `search` discovers candidate URLs through DuckDuckGo (html primary, lite
  fallback on a transport/parse failure) and returns titles, URLs and snippets —
  one page, at most 10 results, no result-page fetch and no persistence.
- `search_content` is **local**, not web discovery: it full-text searches the store
  of pages already fetched (SQLite FTS5, title weighted 4× the body) and returns
  ranked URLs with a plain capped snippet. `search` is the discovery tool; nothing
  here touches the network.
- `search_semantic` ranks the same local store by **local-model** similarity
  (all-MiniLM-L6-v2 quantized, no network); a bounded lazy backfill embeds pages
  after they are fetched, and `embedding_status` reports coverage. It needs the
  provisioned ONNX Runtime library and model files; otherwise it is the typed
  `embedding_unavailable`.
- `webhook_target`/`webhook_status` attach and report **operator-configured**
  webhook targets for monitor changes. Targets are named in the config file only
  (a tool never accepts a URL), HMAC-signed, retried with backoff, and dead
  letters are visible. Enabling them is a deliberate outbound egress; the
  disclosure is below.
- `fetch_feed` reads one RSS 2.0/Atom/JSON feed and returns its items in feed
  order (`id`, `title`, absolute `url`, RFC3339 `published`, `author`, plain
  `summary`) with `limit`/summary caps reported; no render and no persistence.
- `sitemap` reads one `<urlset>`, `<sitemapindex>` or plain-text sitemap (gzip
  included) and returns its URL entries, with index children followed one level,
  entry/child caps reported and `lastmod` as data only; nothing is stored.
- `pdf_extract` reads one PDF's **text layer** with the native PDFium engine (no
  OCR) into `## Page N` Markdown with document metadata, page/empty-page counts
  and an explicit `ok|no_text_layer|partial` outcome; a complete text layer is
  stored and indexed. `fetch_page`/`extract` still refuse PDFs and point here.
- `robots_cache` lists the local `hosts` cache and probes a path **from the
  cached body** (allow/deny, matched rule, `Crawl-delay`, expiry/failures) with
  **zero network** requests.
- `events` returns the local change feed as JSON or RSS 2.0; with `--http`, the
  loopback listener also serves `GET /feeds/changes.xml` behind its own
  capability token (separate from the MCP bearer).
- Main-content extraction with readability, converted to GitHub-flavoured
  Markdown; `mode` selects `main`, `full`, `text` or `raw`.
- `extract` evaluates a declarative CSS spec over the page's own DOM and returns
  typed records with per-field errors.
- `extract_rows` is the tabular sibling: CSV/TSV (RFC4180 quoting, delimiter
  sniff, BOM/CRLF), JSON through an RFC6901 pointer, and HTML tables — with the
  same fetch gates, sessions and caps. Only this tool fetches `text/csv` and
  `text/tab-separated-values`.
- Metadata (title, author, site name, published time, language) and optional
  link extraction (URLs only, off-origin links flagged).
- Safety gate on **every** request and redirect hop: only `http`/`https`, and no
  loopback, private, link-local, CGNAT, metadata, multicast or reserved address.
- `robots.txt` enforced per RFC 9309 for the user-agent group `web-widow`, with
  `Crawl-delay` raising (never lowering) the per-host interval. There is no
  per-call argument that can widen reach.
- Caps: 10 MiB decompressed body, 64 KiB response headers, 5 redirects, 30 s
  total fetch, 200 000 returned Markdown characters. Truncation is always
  reported (`truncated` + `limit_hit`).
- Content scope: HTML, XHTML, plain text, JSON and XML. Anything else is refused
  with `unsupported_content_type` **before** the body is read.
- Storage: SQLite (WAL, one writer) for the index, BLAKE3 content-addressed files
  for raw bytes, one row per canonical URL, conditional GET, and a startup
  retention pass. Schema **v6** adds a local FTS5 index (`pages_fts`, one row per
  page) maintained inside the page write, plus a one-time backfill of pages that
  predate it.
- **Optional JavaScript rendering** through an in-process CDP browser: `render`
  is `auto` (default), `never` or `always`; the rendered DOM goes through the same
  extractor and reports `render_mode` (`static`, `rendered`, `rendered_partial`,
  `static_fallback`). See “JavaScript rendering” below.

## Build

```sh
cargo build --release                # pinned toolchain 1.99.1 from rust-toolchain.toml
cargo build --release --features http # + opt-in loopback streamable HTTP
cargo build --release --no-default-features  # static-only, no CDP, no HTTP
./target/release/web-widow --help
```

Feature matrix:

| Build                                   | Render | HTTP | Use                                        |
| --------------------------------------- | ------ | ---- | ------------------------------------------ |
| `cargo build --release`                 | yes    | no   | desktop/CLI MCP clients that spawn a child |
| `--features http` (render is default)   | yes    | yes  | long-lived HTTP MCP clients                |
| `--no-default-features`                 | no     | no   | static-only host without Chrome            |
| `--no-default-features --features http` | no     | yes  | static-only HTTP host                      |

The `render` feature is enabled by default because rendering is part of the
delivered binary. `--no-default-features` removes `chromiumoxide` and keeps every
static path working without Chrome; `render=always` then returns
`render_unavailable`. The `http` feature adds `axum` and rmcp's streamable-HTTP
server; it is not in the default build.

A musl build is supported by the same source when the target is installed:
`rustup target add x86_64-unknown-linux-musl` and
`cargo build --release --target x86_64-unknown-linux-musl` (plus `--features
http` if needed). The packaged release was built on the host target; a musl
build was not shipped.

The dependency gate is part of the build contract:

```sh
cargo deny check           # licenses, advisories, bans, sources
cargo audit                # RustSec advisories
```

## Configure

No config file is required; the defaults in `config.example.toml` are what the
binary uses. Copy it to `~/.config/web-widow/config.toml` and pass `--config`, or
set `WEB_WIDOW_CONFIG`.

```toml
[fetch]
contact = "crawler@example.com"    # becomes part of the User-Agent
allow_hosts = []                   # exact hosts or CIDRs; config file only
```

Two knobs widen reach and exist only in the config file: `fetch.allow_hosts`
(private addresses / named hosts, each match logged at info) and
`robots.enabled = false` (announced loudly at startup). Neither is available
through the MCP tool surface, an environment variable or a CLI flag.

The data directory defaults to `$XDG_DATA_HOME/web-widow` and must be `0700`.
One process per data directory, enforced by an advisory lock.

Rendering configuration (all values shown are defaults):

```toml
[render]
# chrome_path = "/usr/bin/chromium"   # recommended; config file only
discovery = true                      # else check $CHROME_PATH and well-known paths
max_concurrent_pages = 2
settle_ms = 500                       # network-quiet window after the load event
ceiling_ms = 10000                    # hard navigation ceiling -> rendered_partial
max_bytes = 5242880                   # 5 MiB transferred per page
max_requests = 200                    # requests per page
restart_max = 3
restart_window_s = 600
launch_timeout_ms = 20000
```

Chrome is discovered lazily at the first render call. If none is found, static
requests keep working and `render=always` returns `render_unavailable` naming
`render.chrome_path` and the checked locations. `chrome_path` is config-only, so
an agent cannot point the browser at an arbitrary executable.

## Run

```sh
# stdio: the client spawns the process; stdout is the MCP protocol
web-widow --config ~/.config/web-widow/config.toml serve

# loopback HTTP (build with --features http): bearer token required
WEB_WIDOW_HTTP_TOKEN="$(cat ~/.config/web-widow/http-token)" \
  web-widow --config ~/.config/web-widow/config.toml serve --http 127.0.0.1:8765
# or: --http-token-file ~/.config/web-widow/http-token   (mode 0600)

# cron/systemd timer: check every due monitor once, print a JSON summary, exit
web-widow --config ~/.config/web-widow/config.toml run-due
```

HTTP notes:

- Endpoint: `http://127.0.0.1:8765/mcp`. Every request needs
  `Authorization: Bearer <token>` and `Accept: application/json,
  text/event-stream` (MCP clients send the latter for you; a raw `curl` probe
  must include it or the streamable transport answers `406`). A missing/wrong
  token is `401` with `WWW-Authenticate: Bearer`; the token is read from
  `WEB_WIDOW_HTTP_TOKEN` (wins) or a `0600` file and is never logged.
- `--http` refuses a non-loopback address unless `--allow-non-loopback` is also
  passed. Remote exposure, TLS and multi-user auth are out of scope; put a
  TLS-terminating reverse proxy in front if a network deployment is ever needed,
  and keep the bind on loopback.
- **The change feed is loopback-only.** A non-loopback `--http` bind with
  `[events].feed_enabled` is refused at startup (before any token is resolved or
  provisioned): set `[events].feed_enabled = false` for an MCP-only network
  deployment. `http::serve` re-checks the bound address, so a library caller
  cannot bypass the refusal.
- **The loopback HTTP transport is stateless.** Every POST is served directly;
  `Mcp-Session-Id` is neither required nor issued, and stale/unknown/absent ids
  are ignored instead of returning `404`. `initialize` completes the
  handshake normally (`application/json`). Only POST is served: `GET` and
  `DELETE` on `/mcp` return `405` with `Allow: POST`, and the server emits no
  server-initiated messages. Bearer auth, Host validation and the body cap are
  unchanged.
- Each request writes one access line to stderr: `method`, `path`, `status`,
  `session` (the first 8 characters of a supplied id, or `none`) and
  `duration_ms`; INFO for status ≥ 400, DEBUG otherwise. The bearer token, the
  body and the query string are never logged.
- stdout stays empty in HTTP mode; all logs are stderr.
- SIGINT/SIGTERM stops the listener, cancels job workers, closes the browser,
  wipes the throwaway profile and releases the data-directory lock (target:
  exit within 10 s).

See [`docs/operations.md`](docs/operations.md) for data layout, retention,
backup (`VACUUM INTO`), Chrome discovery, troubleshooting, upgrade/rollback and
uninstall.

`run-due` is single-shot and stdio-free: it opens the data directory (a second
process on the same directory is refused), checks each due monitor sequentially
through the same per-host gate and check implementation as `monitor_check`,
prints `{checked, skipped, changed, failed}` on stdout, shuts the renderer down
and exits 0. A non-zero exit means the pass itself could not run; per-monitor
failures appear in `failed` and in the stored fetch events.

`run-due --http http://127.0.0.1:8765/mcp` is the cron mode for a machine where
the server already runs: it never opens the data directory or the renderer, and
talks to the server over loopback JSON-RPC (`initialize` →
`monitor_check {all:true, wait_ms}`). It prints exactly one JSON object on stdout
(`{"via":"http","url":…, …monitor_check payload}`) and exits 0 for a completed
**or queued** pass (`pending:true` plus `job_id` is honest and still exit 0).
`--wait-ms` defaults to 30 000 and is bounded by the server's
`[monitor].max_wait_ms` (a larger value surfaces `schema_invalid`). The bearer
comes from `WEB_WIDOW_HTTP_TOKEN` or a `0600` `--http-token-file`; redirects are
disabled so the token cannot follow a `Location` to another host.

The **opt-in in-process timer** (`[scheduler].enabled = true`, default false) is
the alternative to cron: `serve` checks due monitors every `tick_ms` (default
30 s), up to `min([scheduler].max_checks_per_tick, [monitor].max_checks_per_call)`
per pass, through the same `run_due`/per-monitor lock/`next_due_at` path. Choose
**one driver per instance**: the serving process holds the data-directory lock,
so a local `run-due` fails while it runs; `run-due --http` remains valid and cannot
double-check (lock + `next_due_at` re-read). Enabling the timer means removing or
disabling any cron entry for that instance. `run-due` never starts the timer, and
the timer is cancelled and awaited on SIGTERM/EOF with the other tasks.

The server speaks MCP on stdout and logs to stderr; standard output carries
nothing but JSON-RPC frames. A minimal client exchange:

```jsonc
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"me","version":"1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"fetch_page","arguments":{"url":"https://example.com/","mode":"main","include_links":true}}}
```

The `text` content block is YAML-ish front matter followed by the Markdown.
`structuredContent` carries `url`, `final_url`, `status`, `content_type`,
`title`, `byline`, `site_name`, `published`, `lang`, `markdown_chars`, `markdown`,
`truncated`, `limit_hit`, `render_mode`, optional `render_warning`, `cached`,
`fetched_at`, `duration_ms`, `links`, `links_requested`, `links_truncated`,
optional `render_quality`, optional `content_warning`, `word_count`.

- `links` is **always a list**. It is `[]` when `include_links` was false or
  omitted; `links_requested` says which request shape produced the answer. A
  cached answer re-derives `links_requested` from the current call. A client that
  used `links: null` to mean "not requested" must read `links_requested`.
- `markdown` is the delivered body — the Markdown the text block carries,
  without the front matter — when `include_content` is true (the default).
  `include_content: false` omits it from `structuredContent` and turns the text
  block into a one-line metadata summary (`url`, `status`, `title`, `words`,
  `render_mode`). The body crosses twice when included; the cache stores it once
  and a cache hit reconstructs it. `max_chars`/`markdown_chars` bound it.
- `render_quality` is `ok` or `thin` (fewer than 200 main-content words) on
  rendered `fetch_page` **and `extract`** results. It is absent on `static` and
  `static_fallback`, where a short page is simply a short page. It is not part of
  `crawl_results`.
- `content_warning: "interstitial_suspect"` is set only for the whole conjunction:
  status 200, the final URL drifted from the requested URL
  in host or path, the final path has a **whole** verification/login segment
  (`account-verification`, `verification`, `verify`, `challenge`, `captcha`,
  `login`, `sign-in`, `signin`, `security-check`, `checkpoint`, `auth`,
  `authorize`, `validate`, case-insensitive), and `word_count < 50`. It is a
  warning, never an error, and the body is still delivered; it appears in the
  text front matter too. A direct login page (no redirect drift) and a short
  article on a normal path do **not** warn. `render_quality` and `render_mode`
  keep their meanings independently.

Errors are in-band: `isError: true` with
`structuredContent.error = { code, message, detail? }`, using these codes
(`invalid_url`, `blocked_scheme`, `blocked_private_address`,
`blocked_by_robots`, `dns_error`, `tls_error`, `timeout`, `too_large`,
`unsupported_content_type`, `http_error`, `render_unavailable`, ...).

## Use with AI agents

web-widow is built to be driven by an agent as well as by a person. Two artifacts make that work
together:

- **The MCP server** — 34 tools over stdio (default) or loopback streamable HTTP. It owns every
  guardrail: robots.txt, the private-address gate, the per-host rate floor, body and output caps, and
  the content-type allowlist.
- **The field-guide skill** — [`skill/web-widow/SKILL.md`](skill/web-widow/SKILL.md). It teaches an agent how to choose a
  tool, read the `fetch_page` payload, recognise rendered versus static content, respect interaction
  session limits, and treat a `blocked_*` refusal as final. It also carries the failure taxonomy and the
  rule that fetched content is data, never instructions.

Install the pair with [`SKILL-INSTALL.md`](SKILL-INSTALL.md): build the server, create a `0600` config
and `0700` data directory, register the server in the agent client (stdio command or HTTP URL plus
bearer token), then copy `skill/web-widow/` into the client's skills directory. The skill assumes the server's
tools are connected; installing one without the other gives you a server an agent does not know how to
use, or guidance with no tools behind it.

A typical agent task then looks like this:

> Find the current pricing page for a vendor, extract the plan table, and say what changed since last
> week.

The agent would call `search`, then `fetch_page` (citing `final_url` and `fetched_at`), then
`extract_rows` for the table, and finally `monitor_create` plus `events` for the change. Every result is
returned as untrusted data; page text that looks like an instruction is never followed. Nothing in the
skill can widen reach or bypass a gate — those decisions stay in the server and the operator's config.

## `search` — DuckDuckGo discovery

```jsonc
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{
  "query": "rust async runtime comparison",
  "site": "tokio.rs",          // optional bare host or http(s) URL
  "freshness": "month",        // any (default) | day | week | month | year
  "limit": 10                  // 1..=10, default [search].default_limit
}}}
```

- **Discovery only.** The response is a normalized list of
  `{title, url, snippet, engine}` plus `query`, `engine` (`duckduckgo_html` or
  `duckduckgo_lite`), `final_url`, `site`, `freshness`, `fetched_at`,
  `duration_ms`, `result_count`, `truncated` and `limit_hit`. It never fetches a
  result page; follow a URL with `fetch_page` or `extract`.
- **One page, ten results.** DDG returns one SERP page of ten organic results;
  There is no pagination. `limit > 10` is `schema_invalid`, never a silent clamp.
- **Engines are fixed server config.** `[search].html_url` is the primary;
  `[search].lite_url` is tried **only** when the primary fails at transport or
  parse level. A `blocked_by_robots`, `blocked_private_address`,
  `blocked_scheme` or `invalid_url` refusal is final for the call. Both URLs are
  validated at config load; there is no tool argument that changes the engine.
- **`site` is applied twice.** It becomes a `site:<host>` qualifier in the engine
  query and a host filter on the parsed results (equal host or `.`-suffix
  subdomain). A call can therefore return fewer than `limit` results even when
  the engine had more. A path or port in `site` is `schema_invalid`.
- **`freshness`** maps to DDG's `df` parameter (`day→d`, `week→w`, `month→m`,
  `year→y`); `any` sends none.
- **Privacy.** The query is caller input. Logs carry the engine, result count,
  duration and outcome — **never** the query text or the full SERP URL. Search
  results are untrusted data.
- **Politeness and safety are the shared ones.** Each SERP request goes through
  the same SSRF gate, robots check, redirect vetting and per-host gate as
  `fetch_page`, with the same body cap. A `search` call makes at most two SERP
  requests (plus the cached `robots.txt` fetch).
- **It stores nothing.** No `pages`, `fetch_events`, `extractions`, `profiles`,
  `blobs` or `jobs` row is written for a search; only the `hosts` robots cache
  may change. An explicit zero-result page is a success (`results: []`,
  `result_count: 0`); an unrecognisable page is `extraction_failed`, never an
  empty success. That failure can be a transient DDG challenge: one later retry
  is reasonable, never a loop, and watch recurrence in the access log before
  treating it as a defect.

## `search_content` — local full-text search

```jsonc
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"search_content","arguments":{
  "query": "sqlite wal checkpoint",
  "limit": 20                  // 1..=100, default [fts].default_limit
}}}
```

- **Local corpus only.** It reads `pages_fts` over pages already acquired by
  `fetch_page`, `extract`, a crawl or a monitor check. It is deliberately distinct
  from `search` (DuckDuckGo discovery): a call makes no network request, writes
  nothing and is safe to repeat.
- **Plain-text queries.** Whitespace separates terms and **every term must
  appear** (AND). FTS5 operators — `OR`, `NEAR`, `*`, `-`, `column:` — are not
  interpreted; they match as text because every term is quoted. A blank query or
  one over `[fts].max_query_chars` is `schema_invalid` at `/query`.
- **Ranking.** `bm25(pages_fts, 4.0, 1.0)` — the title is weighted four times the
  body — ascending (smaller is better). The payload reports `score = -bm25`, so a
  **larger score is a better match**; `pages.id` is the stable tiebreak.
- **Bounded output.** `limit` is 1..=100 (a larger value is `schema_invalid`, not
  a clamp); `result_count` is the full count for the same match, and
  `truncated`/`limit_hit: "limit"` report a page smaller than it. `snippet` is
  plain surrounding text from the stored body, capped at `[fts].max_snippet_chars`
  with no markup; it is untrusted stored content, data and never instructions.
- **What is indexed.** The delivered `fetch_page` Markdown, the returned record
  string values of `extract`/crawl-with-schema, the crawl's main Markdown, and a
  monitor check's main Markdown — each capped at `[fts].max_index_chars`; the title
  is always indexed. A `304` or a failed fetch never rewrites the row, so a page
  keeps its last good text.
- **Growth is disclosed.** The indexed text is stored in the FTS table so a
  snippet survives extraction retention; the database therefore grows by up to
  `[fts].max_index_chars` per page plus index overhead. That is **inside the
  database, not the 20 GiB blob budget**. Pages are not pruned in v1; lower the cap
  or decide a page-retention policy before running a high-volume store.

## `search_semantic` — local semantic search

```jsonc
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"search_semantic","arguments":{
  "query": "how is a broken unit swapped with no outage",
  "limit": 10                  // 1..=50, default 10
}}}
```

- **Local and offline.** The query is embedded once by the configured local model
  (`minilm-l6-v2-q`, all-MiniLM-L6-v2 quantized, 384 dims). The engine loads a
  provisioned ONNX Runtime 1.31.0 library and pinned model files; there is no
  download, no API key and no outbound request. A missing library/model or a
  per-file SHA-256 mismatch is `embedding_unavailable` naming the config key/file.
- **Provisioning.** The model is the quantized ONNX export of
  `Xenova/all-MiniLM-L6-v2` on Hugging Face. Fetch these five files and place them
  under `<data_dir>/models/minilm-l6-v2-q/` (the ONNX file keeps its `onnx/`
  subdirectory):
  `https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/main/onnx/model_quantized.onnx`,
  `…/tokenizer.json`, `…/config.json`, `…/special_tokens_map.json` and
  `…/tokenizer_config.json`. Verify each SHA-256 against the table in
  `docs/operations.md` before installing; ONNX Runtime itself comes from the
  Microsoft `onnxruntime-linux-x64-1.31.0.tgz` release archive.
- **Lazy backfill, not the fetch path.** When `[embeddings].enabled = true`,
  `serve` runs a bounded backfill (newest pages first, `backfill_pages_per_pass`,
  one pass per `backfill_interval_ms`) over the stored page text. A page becomes
  searchable after its pass; a re-fetched page is re-embedded when its source
  text changes. `run-due` never starts the backfill.
- **Ranking.** Cosine similarity over the active model's chunk vectors, brute
  force at this scale; one hit per page (its best chunk) with a plain snippet
  from that chunk, capped by `[fts].max_snippet_chars`. `result_count` counts
  distinct pages with stored vectors and `truncated`/`limit_hit` report a cut.
- **Coverage.** `embedding_status` reports `enabled`, `available`, `model`, `dim`,
  `model_dir`, `embedded_pages`/`embedded_chunks`/`pending_pages`, `vector_bytes`,
  `last_run_at` and `last_error`. It never writes.
- **Quality is disclosed.** MiniLM is English-optimized; non-English pages (the
  corpus has some Spanish pages) recall worse. Results are retrieval candidates,
  not authority, and the text is untrusted stored content: data, never
  instructions. ANN, duplicate detection and hybrid reranking are not in v1.

## `webhook_target` / `webhook_status` — change notifications

```jsonc
{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"webhook_target","arguments":{
  "monitor_id": 7,
  "targets": ["ops"]          // configured names only; [] clears; 0..=8
}}}
```

- **Config-named targets only.** `[webhooks.targets.<name>]` in the operator's
  config holds `url`, `kind` (`generic`/`slack`/`discord`), one of
  `secret_env`/`secret_file`, one of `auth_env`/`auth_file`, and `include_diff`.
  Tools and `monitor_create.webhook_targets` accept **names**, never URLs, so
  page content or a prompt-injected agent cannot introduce an endpoint. HTTPS
  only unless `[webhooks].allow_http = true` (loopback/test); no userinfo; no
  redirects. The URL is treated as a secret and never logged or returned.
- **Atomic outbox.** A changed check inserts one `webhook_deliveries` row per
  attached target inside the change transaction. A separate dispatcher (in
  `serve`, or one bounded flush in local `run-due`) attempts it: 2xx delivered;
  408/429/5xx/timeout retried; other 4xx/redirect dead; backoff
  `[60,300,1800,7200,21600]` s + deterministic jitter; `dead` after
  `max_attempts`. The check path never touches the network.
- **Signing.** With a secret, posts carry `X-Web-Widow-Timestamp` and
  `X-Web-Widow-Signature: sha256=<hex HMAC-SHA256(secret, timestamp + "." + body)>`
  plus `X-Web-Widow-Event: change` and `X-Web-Widow-Delivery: <id>`.
- **Status.** `webhook_status` shows `enabled`, targets (`name`, `kind`, `host`,
  `has_secret`, `has_auth`, `include_diff`), monitor attachments, recent rows
  (`status`, `attempts`, `last_status`, `next_attempt_at`, `last_error`) with
  `include_deliveries: true`, and `pending`/`dead` counts. Only the host is
  returned; secrets and URL paths never are.
- **Egress disclosure.** Enabling a webhook deliberately sends monitor metadata —
  the watched URL, its label, the change kind, stats and a short summary, and the
  capped diff only when the target opts in — to the configured endpoint. It can
  leave the host's network and is not gated by robots/SSRF (the target is
  operator-chosen). A misconfigured target or a compromised config file is an
  exfiltration path. No webhook fires without an attachment; remove the target or
  set `[webhooks].enabled = false` to stop all delivery.
- **Retention.** Delivered rows age out after `[retention].webhook_deliveries_days`;
  dead letters after the longer `webhook_dead_days`; pending rows are never
  pruned. Tombstoning a monitor dead-letters its pending rows.

## `fetch_feed` — RSS/Atom/JSON discovery

```jsonc
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"fetch_feed","arguments":{
  "url": "https://example.com/feed.xml",
  "limit": 20                  // 1..=100, default [feed].default_limit
}}}
```

- **Discovery only.** The response carries feed metadata (`feed_type` rss|atom|json,
  `title`, `site_url`, `language`, plain-text `description`, `final_url`,
  `content_type`, `fetched_at`, `duration_ms`) and `items`:
  `{id, title, url, published, author, summary}`. Follow an item URL with
  `fetch_page`, `extract`, or a monitor; `fetch_feed` never fetches item pages.
- **Feed order, first wins.** Items keep the feed's order; duplicates are removed
  by `id` (fallback `url`) and by canonical URL, keeping the first. Item URLs are
  absolute (resolved against the response's final URL); an item without a link
  returns `url: null`.
- **`url` is verbatim; `canonical_url` is identity.** `url` is the fetch target
  exactly as the feed states it. `canonical_url` applies the same canonicalizer
  as crawl/monitor (lowercase scheme/host, tracking parameters such as `utm_*`
  and `fbclid` dropped, remaining query sorted), so `?utm_source=` variants
  collapse to one item while the reported `id` and verbatim `url` stay the
  feed's. Unknown parameters such as `syn` are **not** tracking parameters and
  remain part of the canonical identity; adding one is a deliberate change with
  evidence, not a silent widening. `canonical_url` is `null` when the item has no
  link (or the link is not an http(s) URL).
- **Dates and summaries.** `published` is RFC3339 UTC (`null` when the feed has no
  usable date). `summary` is the item's content/summary converted to plain
  visible text (script/style/head skipped), capped at `[feed].max_summary_chars`
  (default 2 000). `truncated_summaries` counts cuts.
- **Caps.** `limit` defaults to `[feed].default_limit` and is clamped to
  `[feed].max_limit`; `item_count` is the parsed count before the limit.
  `truncated` is true when either cap fired, with `limit_hit` precedence
  `"limit"` > `"max_summary_chars"`.
- **Untrusted input.** A body with an internal DTD/entity declaration is refused
  with `extraction_failed` before parsing (entity-expansion surface); malformed
  XML/JSON is also `extraction_failed`. `text/html`, `application/xhtml+xml` and
  `text/plain` are `unsupported_content_type`.
- **Acquisition and privacy.** The same SSRF gate, robots check, redirect
  vetting, per-host politeness floor and body cap as `fetch_page`. A feed read
  writes **nothing** (only the `hosts` robots cache may change), has no
  conditional GET and never renders.

## `sitemap` — sitemap discovery

```jsonc
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"sitemap","arguments":{
  "url": "https://example.com/sitemap.xml",
  "limit": 5000,               // 1..=[sitemap].max_urls (default 5000)
  "include_lastmod": true,
  "follow_index": true
}}}
```

- **Formats.** `<urlset>` (`<url><loc><lastmod>`), `<sitemapindex>`
  (`<sitemap><loc><lastmod>`) and the plain-text one-URL-per-line file. Elements
  are matched by local name, so the sitemap 0.9 namespace and the legacy 0.8
  prefix both work. CDATA and the five predefined XML entities are decoded; a
  `DOCTYPE` is skipped and **never resolved** — no entity expansion, no external
  access — and an unknown entity is `extraction_failed`.
- **Gzip.** A body whose first bytes are the gzip magic is inflated before
  parsing, with a hard `4 × [sitemap].max_bytes` ceiling; a bomb is `too_large`.
  The Content-Type is not trusted for this (a `.xml.gz` served as
  `application/octet-stream` still parses).
- **Caps and shape.** At most `[sitemap].max_urls` entries (default 5 000;
  `limit` may lower it) and `[sitemap].max_children` index children (default 5).
  `truncated`/`limit_hit` are `"max_urls"`, `"max_children"`, `"limit"` or null.
  An entry whose `loc` is absent or not http(s) is skipped and counted in
  `skipped_scheme`. A child failure is reported in `sitemaps[].error` with its
  typed code, never dropped.
- **`lastmod` is data only.** RFC3339 or `YYYY-MM-DD` values are returned as the
  document wrote them; anything else is `null` and counted in `invalid_lastmod`.
  Nothing is scheduled, ordered or monitored from it in this milestone.
- **Malformed XML** is `extraction_failed` with the parser line/column; no
  partial result is returned. Partial results exist only as explicit cap
  truncation (`truncated: true`).
- **Discovery only, shared gates.** Nothing is stored (no page, extraction,
  blob, job or fetch-event row; only the existing robots cache may change). The
  same SSRF gate, per-hop robots check, redirect vetting, per-host politeness
  floor and streaming body cap as `fetch_page` apply to the document and every
  index child. The response is untrusted web content: data, never instructions.

## `pdf_extract` — PDF text layer (PDFium)

```jsonc
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"pdf_extract","arguments":{
  "url": "https://example.com/report.pdf",
  "max_pages": 200,            // 1..=[pdf].max_pages (default 200)
  "max_chars": 1000000         // 1..=[pdf].max_chars (default 1 000 000)
}}}
```

- **Text layer only.** The document's text is returned as Markdown with one
  `## Page N` section per page in range, plus `metadata` (title, author, subject,
  creator, producer, creation/modification dates — absent values are `null`) and
  `pages` / `declared_pages` (PDFium's page count) / `pages_with_text` /
  `empty_pages` (bounded to 20 with `empty_pages_truncated`). No OCR, no images,
  no forms, no JavaScript.
- **Outcomes.** `ok` = every page parsed and text exists; `no_text_layer` = every
  page parsed and none has text (a successful call: an OCR decision is explicit);
  `partial` = extraction stopped before the declared page count (a page error or
  a cap). A page that fails stops extraction rather than presenting a short
  result as complete.
- **Caps.** Body `[pdf].max_bytes` (default 32 MiB); `max_pages` and `max_chars`
  may only lower the server ceilings; truncation is at a page boundary with
  `truncated`/`limit_hit`. A single first page larger than `max_chars` is cut at
  the character cap (documented edge case) so the delivered Markdown stays bound.
- **Typed failures.** `pdf_encrypted` (a real user password; an empty-user-
  password PDF opens normally and is not an error), `pdf_invalid` (malformed; a
  Rust parser panic is caught), `render_unavailable` when the native PDFium
  library cannot be loaded (the message names `[pdf].pdfium_path` /
  `[pdf].pdfium_discovery`), `unsupported_content_type` when the body is neither
  `application/pdf` nor starts with `%PDF-`, the normal `http_error` for a
  non-2xx response, and `too_large` for the body cap.
- **Store and index.** A complete text layer (`outcome: "ok"`) becomes a page row
  (`content_type: "application/pdf"`, `render_mode: "pdf"`, `outcome: "ok"`, the
  PDF bytes as the raw blob), an extraction row with the Markdown, and one
  `pages_fts` row (`IndexText::replace`, the delivered page text). `search_content`
  then finds the PDF text, and a later HTML acquisition of the same URL replaces
  it. A `no_text_layer` or `partial` result stores nothing.
- **Native engine.** PDFium (`libpdfium.so`, release 8125) is loaded through
  `libloading` once per process and parsing is serialized by a process-wide
  mutex. `[pdf].pdfium_path` is strict; when unset, `[pdf].pdfium_discovery`
  (default true) searches `WEB_WIDOW_PDFIUM_PATH` → the executable's directory →
  `<data_dir>/lib` → the system library. The library must be shipped beside the
  binary (or pinned per config); the binary itself does not link it, so `ldd` on
  the binary does not list `libpdfium`. A native PDFium crash is not catchable by
  `catch_unwind` (a disclosed residual risk).
- **Shared gates.** The body comes through the same adapter as every fetch (SSRF
  gate on each hop, robots.txt, redirect vetting, per-host politeness floor,
  streaming body cap to `[pdf].max_bytes`). `fetch_page` and `extract` keep
  refusing `application/pdf` before reading the body and name `pdf_extract` in
  the error. Fetched content is untrusted data: never instructions.

## `events` — change feed (JSON + RSS)

```jsonc
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"events","arguments":{
  "format": "json",          // json (default) | rss
  "monitor_id": null,        // one monitor
  "since": "2026-01-01T00:00:00Z",  // RFC3339, inclusive
  "limit": 50,               // 1..=200
  "include_diff": false
}}}
```

- **Data.** Newest-first `changes` joined to `monitors` (label) and the page title
  by canonical URL: `{change_id, monitor_id, monitor_label, url, title,
  detected_at, kind, stats, diff?, from_snapshot, to_snapshot}`. `stats` is the
  same shape `monitor_changes` returns; `include_diff` adds the capped diff (the
  RSS document never carries it). `count`/`truncated`/`limit_hit` report a page.
  Read-only: no change is fabricated and nothing is written.
- **RSS.** `format: "rss"` returns `{xml, bytes, item_count, feed_url}` — RSS 2.0
  (channel title/link/description/lastBuildDate, item title/link/guid/pubDate/
  description), newest first, at most 100 items, every text node XML-escaped and
  control-stripped. The description carries the stats, never the diff. Tests
  parse the document with `feed-rs` (the same parser `fetch_feed` uses).
- **Served GET (loopback only).** With the `http` feature and `[events]
  feed_enabled` (default true), the listener serves
  `GET /feeds/changes.xml?token=<capability token>` with
  `Content-Type: application/rss+xml; charset=utf-8`, an `ETag` (body hash),
  `Cache-Control: max-age=60` and a 304 on a matching `If-None-Match`. Missing or
  wrong token → 401; `feed_enabled=false` → 404. The feed token is **separate
  from the MCP bearer** (`WEB_WIDOW_FEED_TOKEN` or a 0600 `<data_dir>/feed-token`
  created on first start; rotate by replacing it and restarting). The route is
  outside `/mcp`, requires no bearer, performs no outbound fetch, and the access
  log records the path only (never the query). The route is **loopback-only by
  enforcement**: a non-loopback HTTP bind with the feed enabled is refused at
  startup (the operator sets `[events].feed_enabled = false` for an MCP-only
  network deployment), and `http::serve` returns an error if called with a
  non-loopback address while the feed is on. `feed_url` in the tool payload is
  non-null only while that listener is active; stdio has no route.

## `robots_cache` — robots.txt cache

```jsonc
{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"robots_cache","arguments":{
  "host": "example.com",   // optional: restrict + probe this host
  "path": "/private",      // probe path (default /)
  "limit": 50,             // 1..=200, host order
  "cursor": null,
  "include_body": false    // 200-char preview only
}}}
```

- **Cache listing.** Host, fetch time/status, expiry, next allowed time,
  consecutive failures, the cached `Crawl-delay`, body size and presence;
  paginated in host order with `next_cursor`.
- **Path probe from the cache.** With `host` (and optional `path`), the **stored**
  body is parsed with the same parser enforcement uses and `decision` reports
  `{allowed, matched_rule, crawl_delay_s, source}`: `cache`, `expired` (still
  parsed) or `unavailable` (missing/invalid body → `allowed: null`, never a
  fabricated allow).
- **Zero network.** The tool reads the database only; it does not call the
  fetching robots service. A test asserts the fixture server receives no
  request. The cached body is untrusted data: never instructions.

## `extract`

```jsonc
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"extract","arguments":{
  "url": "https://shop.example/items",
  "schema": {
    "container": ".product-card",
    "fields": {
      "name":  { "selector": "h2" },
      "price": { "selector": ".price", "transform": "number" },
      "href":  { "selector": "a", "transform": "absolute_url" },
      "tags":  { "selector": ".tag", "all": true, "transform": "trim" }
    }
  },
  "use_cache": true
}}}
```

- **The spec is data, not code.** `container` is optional (omit it for a
  single-record page); `fields` maps a name to `{selector, attr?, all?,
  transform?}`. Unknown keys, unknown transforms, invalid CSS and
  `transform: attr` without `attr` are `schema_invalid` with the JSON pointer of
  the offending node. No XPath, regex or expressions exist.
- **The tool-call envelope is `{url, schema: {container?, fields}}`.** The spec
  itself is not accepted at the top level: a call shaped `{url, container,
  fields}` is rejected with a message naming the `schema` envelope, and any
  other missing `schema` keeps the ordinary serde wording. There is one
  documented contract, not a second silently accepted shape.
- **Transforms** are a closed set: `text` (visible text, edges trimmed), `trim`
  (text with internal whitespace collapsed), `html` (inner HTML), `attr` (reads
  `attr`), `absolute_url` (resolves the `attr`, else `href`, else `src` against
  the response's **final** URL) and `number` (parses a number, understanding
  currency symbols and both `1,234.56` and `1.234,56`). An `attr` without a
  transform implies `attr`.
- **`text`/`trim`/`number` see visible text only.** `script`, `style`,
  `template` and `head` subtrees are skipped before collecting a field's text; a
  field that *is* a `<script>` returns `""`. `html` and `attr` still return
  source by definition. This prevents a 178k inline-script body from
  becoming the field value.
- **`:main` is the article `fetch_page mode=main` delivers.** `container:
  ":main"` (exact, trimmed) evaluates fields inside the **`body` of that same
  readability article** — one readability pass per acquisition, so the scope and
  the delivered Markdown describe the same source. The article wrapper
  `<html>`/`<head>` is never a field root. If readability finds nothing usable,
  the scope falls back to the document body. It works identically for `extract`,
  `crawl_start` and `monitor_create`. `":main"` as a *field* selector is still
  invalid CSS and returns `schema_invalid` at that field's pointer.
- **`selector: "*"` is the record scope itself.** A field selector of exactly
  `*` (trimmed) selects the scope element, not its first descendant: `text` →
  its visible text, `html` → its inner HTML, `attr` → the named attribute, and
  `all: true` → a one-element array. With no container the scope is the document
  root, so `*` is the page's visible text. Any other selector (including
  `* .item` or `div *`) keeps ordinary CSS semantics.
- **Field selectors are descendant-scoped** within the record root (the
  container element itself is not a match, except for the `*` form above).
- **Failures are explicit.** A scalar selector that matches nothing is reported
  as `selector_no_match` with the field absent — never `null` or `""`. An
  `all: true` field that matches nothing is an empty array. Transform failures
  are `attr_missing`, `url_join_failed` or `number_parse_failed`. A container
  that matches nothing returns `records: []` plus a page-level error
  (`record: null`), never a fabricated record.
- **Caps and counts.** Records are capped by `extract.max_records` (5 000);
  `record_count` is the full match count and `truncated`/`limit_hit`
  (`max_records`) say so. Two additive presentation caps bound the response:
  `max_field_chars` (default `[extract].max_field_chars`, 16 384) cuts every
  string value and array element and reports `truncated_fields`; `max_chars`
  stops records in evaluation order at the serialized budget and reports
  `limit_hit: "max_chars"`. Precedence is `max_records` → `max_chars` →
  `max_field_chars`. The cache stores the **full** records (caps are
  presentation-only), so a later larger-cap call still sees the full values.
- **Rendered thin signal.** `render_quality: "ok" | "thin"` is present on
  `rendered`/`rendered_partial` extract results (an approximation from the
  delivered page's visible words), absent on static/static_fallback, and also in
  the text front matter.
- **Evaluation is independent of Markdown.** The spec runs over the parsed DOM
  with one parse per page, so content that `mode=main` omits from its Markdown is
  still reachable by a spec (see the readability limitation below).
- **Reuse.** `use_cache: true` may answer from a stored extraction for the same
  page and spec inside `extract.cache_freshness_s`, otherwise the page is
  revalidated and a 304 reuses the stored result. `profiles` stores each spec
  once, keyed by the BLAKE3 hash of its canonical JSON. The spec text is part of
  that identity, so the `:main` scope change and the `*` meaning do **not**
  change `profiles.hash`: a record cached under the earlier scope semantics can
  be served for up to `extract.cache_freshness_s` (default 300 s) after an upgrade —
  use `use_cache: false` when the new scope matters immediately.
- **Empty is a value.** A matched `text` field with nothing visible returns `""`
  and is not a `field_error`; `field_errors` reports selectors/attributes that
  did not match.

## `extract_rows` — tabular data (CSV/TSV, JSON, HTML tables)

```jsonc
{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"extract_rows","arguments":{
  "url": "https://example.com/prices.csv",
  "format": "auto",            // auto | csv | tsv | json | html_table
  "selector": "table.results", // html_table only; first <table> when omitted
  "delimiter": ",",            // csv only; omit to sniff
  "header_row": true,          // csv/tsv/html_table default true
  "json_pointer": "/items",    // json only; default "" (document root)
  "limit": 200,                // 1..=1000
  "max_field_chars": 8000,     // 1..=20000
  "render": "auto",            // html tables only: auto|never|always
  "use_cache": false
}}}
```

- **A sibling of `extract`, not a mode.** `extract` stays CSS-over-HTML and its
  identity stays `profiles.hash`; tabular input has its own spec
  (`{format, selector?, delimiter?, header_row, json_pointer?}`), stored in the
  same `profiles` table. `fetch_page` and `extract` keep their content scope and
  still refuse `text/csv`; only this tool's fetch adds `text/csv` and
  `text/tab-separated-values` to the media allowlist. robots.txt, the SSRF gate,
  the per-host politeness floor, charset decoding and the body caps are the same
  shared path.
- **`format: auto`** uses the response media type (`application/json`/`+json`,
  `text/csv`, `text/tab-separated-values`, `text/html`/XHTML); anything else is
  sniffed from the first 4 KiB (`{`/`[`, a tab/semicolon/comma header). An
  `application/octet-stream` body is `unsupported_content_type`, never guessed.
  An explicit `format` wins over both.
- **CSV/TSV:** RFC4180 quoting (`""`, embedded delimiters and newlines), CRLF or
  LF, a UTF-8 BOM, and the response charset. The delimiter is sniffed from the
  header line (unquoted counts; a tie keeps `,`) unless `delimiter` is given.
  Headers are trimmed; blank → `column_N`; repeats → `name_2`. Ragged rows put
  `null` in the missing fields and `column_N` for extra values, and each anomaly
  is a bounded `field_errors` entry (20, then `field_errors_truncated: true`). An
  unclosed quote is `extraction_failed` naming the line and byte.
- **JSON:** an RFC6901 pointer selects the record set. An array of objects is one
  record per element, a single object is one record, and an array with no object
  element becomes `{"value": …}` per element. Numbers, booleans, null and nested
  arrays/objects keep their JSON type. A bad pointer, a scalar target or a mixed
  array is `schema_invalid` at `/json_pointer` with the observed shape.
- **HTML table:** `selector` picks the table (first `<table>` when omitted).
  Headers come from `<thead>` when present, else the first row; cells are visible
  text. `colspan`/`rowspan` are **not** expanded in v1 — the cell text appears
  once and the table carries one `span_ignored` note. A selector that matches no
  table is a `table_not_found` note with zero records, not an error.
- **Bounds:** `limit` caps returned records and `row_count` reports what was
  parsed (one extra row proves truncation: `truncated` + `limit_hit: "limit"`).
  `max_field_chars` caps each string value and counts `truncated_fields`.
- **Store and search:** the extraction row keeps the uncapped records with
  `record_count = returned`, and the row text is indexed **fill-empty**: it gives
  a page its first indexed text and never replaces delivered page Markdown. A
  later `search_content` finds row values; `use_cache: true` reuses a stored
  result for the same URL and spec inside `extract.cache_freshness_s`.
- **Errors** use the existing taxonomy: `unsupported_content_type` when nothing
  classifies, `schema_invalid` at `/format`, `/selector`, `/json_pointer`,
  `/delimiter`, `/limit`, `/max_field_chars`, and `extraction_failed` for an
  unclosed quote or invalid JSON.

## `recipe_*` — named, versioned extraction specs

```jsonc
{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"recipe_save","arguments":{
  "name": "shop:products",
  "schema": {"container": ".product-card", "fields": {"name": {"selector": "h2"}}},
  "description": "product cards on the shop"
}}}

{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"recipe_apply","arguments":{
  "name": "shop:products", "version": 1,
  "url": "https://shop.example/items", "use_cache": true
}}}
```

- **CSS-spec only in v1.** A recipe's `schema` is exactly the `extract` envelope
  (`{container?, fields}`), validated by the same validator with the same JSON
  pointers. Tabular (`extract_rows`) specs are per-call arguments, not recipes.
- **Name is identity.** Lowercase `[a-z0-9][a-z0-9._-]{0,63}`, optionally one
  `:slug` segment (`shop:products`). Uppercase, whitespace and slashes are
  `schema_invalid` at `/name`; there is no rename (save under a new name) and no
  delete in v1 — history stays resolvable.
- **The spec stays in `profiles`.** `recipes`/`recipe_versions` hold only the
  name, the current version and a pointer to the content-addressed profile hash.
  An identical save is a no-op (`changed: false`); a description-only save
  updates metadata without a bump; a changed spec writes version N+1 while N
  keeps applying. A monitor created on an earlier version's `profile_hash` is
  unaffected by the advance.
- **`recipe_apply` is `extract`.** It resolves the stored spec and runs the
  ordinary extraction pipeline: robots.txt, the SSRF gate, the body caps,
  `render`, `use_cache` and `max_field_chars`/`max_chars` all behave exactly as
  `extract`. The response is the normal extraction payload plus `recipe`,
  `version` and `profile_hash`; the extraction is stored (and row text indexed
  fill-empty) exactly as `extract` does. An unknown recipe or version is
  `not_found`; version `0` is `schema_invalid` at `/version`.
- **`recipe_list`** returns `{name, version, profile_hash, description?,
  updated_at}` in name order; `prefix` is a literal name prefix (not a pattern),
  `include_schema: true` adds the stored canonical spec, and `limit` is 1..=200
  (default 50) with `count`/`total`/`truncated`. Read-only: no network.

## `corpus_export` and `snapshot_compare`

```jsonc
{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"corpus_export","arguments":{
  "format": "jsonl",            // jsonl | markdown
  "name": "corpus",             // slug [a-z0-9][a-z0-9._-]{0,63}
  "site": "example.com",        // optional canonical-site filter
  "since": "2026-10-01T00:00:00Z",
  "limit": 1000,                // 1..=10000
  "include_text": true, "include_extraction": true,
  "max_bytes": 268435456        // 1 MiB..=1 GiB
}}}

{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"snapshot_compare","arguments":{
  "kind": "monitor",            // monitor (snapshots.id) | page (fetch_events.id)
  "from": 12, "to": 15,
  "include_diff": true,
  "max_chars": 20000            // 1..=100000
}}}
```

- **What a corpus row is.** A page whose latest fetch succeeded and that has
  stored text. The text is the delivered Markdown or the record text, capped at
  200 000 characters per row (`text_truncated`); the latest extraction is embedded
  (omitted with `extraction_truncated: true` when it alone exceeds the same cap).
  A page with no stored text (failure-only, or an extraction pruned by retention)
  is counted in `skipped_no_text` and never emitted with an invented body.
- **The boundary.** **Monitors and recipes are not corpus rows**: monitor history
  is `monitor_changes`/`changes`, and the recipe library is a configuration
  surface over `profiles`. The export covers `pages` + their extractions only.
- **Shapes.** JSONL: one JSON object per line with `url, canonical_url, title,
  lang, site, first_seen, last_fetched, status, content_type, render_mode,
  text_hash, text, text_truncated, extraction, extraction_truncated`. Markdown: a
  directory with `index.md` (front matter: generated_at, filters, record count)
  and `pages/<url_hash>.md` (front matter url/title/fetched_at + body).
- **Paths are not caller-controlled.** The filename is `<slug>-<UTC stamp>`
  inside `<data_dir>/exports/` (created `0700`), written to a dot-temp and renamed
  on success, `create_new` and never overwritten. An invalid name is
  `schema_invalid` at `/name`.
- **Bounds.** Streamed page by page (no job, no corpus in the response): the
  record `limit` or `max_bytes` stops it, with `truncated` and `limit_hit`
  (`limit`|`max_bytes`). The response carries the path, counts and bytes only.
- **Compare pairs.** `kind: monitor` takes two `snapshots.id` values from the
  same monitor (text from the stored normalized Markdown; a schema monitor also
  compares its canonical records and reports the first 20 changed indices in
  `record_changes`). `kind: page` takes two `fetch_events.id` values with a stored
  extraction and the same canonical URL (the delivered text is compared as
  stored). Stats are never capped; `diff` is capped at `max_chars`
  (`truncated` + `limit_hit: "max_chars"`). A missing id/version is `not_found`
  naming the constraint; a cross-monitor or cross-URL pair is `schema_invalid`
  with `mismatch`. Nothing is written.

## JavaScript rendering

`fetch_page`, `extract` and `extract_rows` take `render` (for `extract_rows` it applies to HTML tables only):

- `never` — static HTTP only.
- `auto` (default) — static first; if the extracted main content has fewer than
  200 words **and** the document has more than three `<script>` elements or an
  app-root marker (`#root`, `#app`, `[data-reactroot]`, `ng-app`), or a
  `<noscript>`/“enable JavaScript” shell has fewer than 100 words, the page is
  rendered. Every decision is logged with the rule name.
- `always` — render first. A missing browser is `render_unavailable`; a rendering
  failure is `render_failed`. It never silently returns a static result.

A rendered page is serialized as `document.documentElement.outerHTML` and fed
through the same charset, blob, extraction and persistence path. `render_mode`
is `rendered` when the page settled, `rendered_partial` when the 5 MiB / 200
request caps or the 10 s ceiling stopped it, and `static_fallback` when `auto`
asked for rendering but it was unavailable or failed (an optional
`render_warning` says why).

The shared browser is launched lazily, gets a throwaway profile under the data
directory (wiped at start and exit), and is restarted at most three times per ten
minutes. Each page gets an ephemeral browser context (no cookies or storage are
shared). The **text profile** used by `fetch_page`/`extract` blocks `image`,
`media`, `font` and `stylesheet` requests while `script`, `xhr`, `fetch` and the
document are allowed. The **visual profile** used by `screenshot` (below) allows
stylesheets, fonts and images so the capture matches what a user sees; media
stays blocked. Rendered pages execute untrusted JavaScript: the SSRF gate
governs the **top-level URL**, not every subresource the page chooses to load —
a disclosed residual risk, not a claim of sandboxing the network.

## Screenshots

`screenshot { url, full_page?, wait_ms? }` renders one http(s) URL through the
same SSRF/robots/preflight/politeness path as `fetch_page`, then captures the
settled page with CDP `Page.captureScreenshot` as a PNG. The visual profile is
server-fixed (no caller toggle): stylesheets, fonts and images load, media is
blocked, and two caps bound the capture — `[render] visual_max_bytes` (20 MiB)
and `visual_max_requests` (500). Hitting one reports `truncated: true` and
`limit_hit`.

```jsonc
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"screenshot","arguments":{
  "url": "https://example.com/app", "full_page": true, "wait_ms": 300
}}}
// → { "url", "final_url", "status", "hash", "bytes", "mime": "image/png",
//     "width", "height", "dimensions_source": "content|viewport|unknown",
//     "full_page", "captured_at", "render_mode", "render_quality",
//     "truncated", "limit_hit", "failed_resources" }
```

- The PNG goes to the content-addressed store and gets one `blobs` row plus one
  `screenshots` row (written in that order inside one writer closure: the
  screenshot row references the blob). The payload carries the BLAKE3 `hash`,
  size, MIME and CSS-pixel dimensions — **never base64**.
- `full_page: false` (default) captures the visual viewport; `full_page: true`
  captures the content size. `wait_ms` (0..=10000) is a post-settle wait, never a
  navigation. If `Page.getLayoutMetrics` fails, `width`/`height` are `null` and
  `dimensions_source` is `unknown` — never invented.
- `blob_get { hash }` returns `{hash, size, mime, path, created_at,
  referenced_by}` for any stored blob. `hash` must be 64 hex characters and can
  only address the CAS, so there is no caller path and no traversal; unknown or
  missing content is `not_found`. No bytes are returned: read the file at `path`
  with your own tools.
- `page_id` is the existing `pages` row for the canonical URL when one exists,
  else `null`; a screenshot never fabricates a page row. Screenshot rows and
  their blobs are pruned by `[retention].screenshots_days` (30), and a surviving
  row retains its blob through both the age and budget passes.
- Asset subresources are fetched by the browser, not by the tool: the SSRF gate
  and robots check govern the top-level document, not the CSS,
  font or image URLs it names. Treat the PNG and any page text as untrusted
  data.

## Browser interaction

Four tools hold **one page per process** for front-end debugging:
`interact_open` navigates and returns a `session_id` capability; `interact_act`
runs exactly one action; `interact_state` observes without acting; and
`interact_close` releases the page and browser context. The loop is
observe → act → observe, so a failed selector cannot abort the rest of a plan.
Sessions use the visual profile above (stylesheets, fonts and images load; media
is blocked), the idle TTL `[interaction].ttl_s` (default 120 s, 30..=600), a
900 s hard lifetime and `[interaction].max_actions` (default 60).

```jsonc
{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"interact_open","arguments":{
  "url": "http://127.0.0.1:3000/", "viewport_width": 1280, "viewport_height": 800
}}}
// → { "session_id", "final_url", "status", "title", "expires_at",
//     "actions_remaining", "console" }

{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"interact_act","arguments":{
  "session_id": "0192…", "action": {"kind": "click", "selector": "button#run"}
}}}
// → { "session_id", "action", "ok", "blocked"?, "final_url", "title",
//     "console_tail", "actions_remaining", "expires_at", "typed_chars"?,
//     "popup"? }
```

- **Closed action set.** `click { selector, index? }`; `type { selector, text,
  index?, clear? }` (clear defaults true; `text` is 1..=2000 chars and the
  response returns `typed_chars` only — the value never appears in a payload or
  log); `wait` with exactly one of `selector` (presence, not visibility; polled
  every 100 ms), `ms` (≤ 10 000) or `network_quiet_ms` (≤ 5 000), all bounded by
  `timeout_ms` (≤ 30 000); `scroll { to: "element"|"top"|"bottom", selector? }`.
  There is **no caller JavaScript**, no `script`/`eval` field, no XPath and no
  text-based targeting — pass an exact CSS selector and disambiguate repeats
  with `index`. An unknown action kind or field is `schema_invalid`.
- **One action at a time.** A concurrent action returns `session_busy`; a
  second `interact_open` while a session is live returns `session_busy` with the
  existing id. An idle-expired id is `session_expired`; an unknown or
  already-closed id is `session_not_found`; the action ceiling is
  `limit_exceeded`.
- **Console capture.** `interact_state` returns the last `console_limit`
  entries (default 50, max 200; ring of 200): `{level, text, at, source}` from
  `Runtime.consoleAPICalled` and `Log.entryAdded`, plus uncaught exceptions.
  Primitive arguments become text; objects are reduced to their description or
  a bounded preview, never expanded, and each entry is capped at 500 chars.
  Console text is untrusted data.
- **Network gate (defense in depth).** Every request routed through the
  session page's target is paused with CDP `Fetch` before it leaves the browser:
  `data:`/`blob:` continue, `file:`/`ws:`/`wss:` and any non-http(s) scheme are
  refused, and every http(s) request passes the SSRF gate. **Documents**
  additionally pass robots.txt; **every** request takes a per-host politeness
  lease, so the session keeps the ≤1 request/s host floor. A refusal appears on
  the action as `blocked: {reason, detail}` (`blocked_private_address`,
  `blocked_by_robots`, `blocked_scheme`, `blocked_iframe`, `request_cap`, or a
  visual cap) and no request reaches the target. `interact_open` itself passes
  the normal top-level render path. Per-session bounds: the visual caps, a
  500-paused-request abuse cap (refused requests included), and 10 state
  screenshots.
- **Target scope (what the gate does and does not cover).** The gate covers
  requests routed through the intercepted page target's network stack. It does
  **not** cover requests from other targets, so this implementation refuses
  **subframe documents** (iframes are out of scope), disables service-worker
  registration and the `SharedWorker` constructor, and prevents popups at the
  document level (`window.open` own property and prototype, `form.submit()`/
  `requestSubmit()` reading the content attribute, shadow-root forms, and a
  **server-side pre-dispatch hit-test** at the click point: the deepest node is
  resolved in the pierced DOM and its ancestor chain (shadow hosts included) is
  checked against the hit node's own document's first `<base target>`. Every
  click-policy input — the base target, each `target`/`formtarget`/`type`
  attribute, and a submit control's **form owner** (a non-empty `form`
  attribute resolved by id in that control's own tree — resolved per chain
  node, so a chain crossing a shadow boundary never borrows the hit node's tree
  — so a sibling form in a shadow tree is modelled even though it is invisible
  to an event path) — is read through
  `DOM.querySelector`/`DOM.getAttributes` and the pierced DOM tree, so page
  JavaScript cannot intercept or fake it. A hit `<label>` is resolved to its
  labeled control (`for` in the same tree, otherwise the first labelable
  descendant) and that control is evaluated with the same rule, because label
  activation forwards a click the guard cannot see through a closed declarative
  root (review 2026-11-02). The label pass is suppressed only for the spec's
  interactive content — an anchor **with `href`**, button, form control outside
  the hidden state, select, textarea, details, embed, iframe, label,
  `audio`/`video` with `controls`, or `img` with `usemap` — not for a bare
  `tabindex` element or an href-less anchor, which the UA still forwards to the
  control (g33, g33d). The hit test sets
  `ignorePointerEventsNone`, so a `pointer-events:none` overlay is transparent
  to the check as it is to the press. The click point is computed once from
  the browser's content quads and dispatched with `Input.dispatchMouseEvent`;
  `DOM.scrollIntoViewIfNeeded` replaces page `scrollIntoView`, and the element's
  box is re-read immediately before the press, so a box that moved between the
  check and the dispatch is refused (`click_point_moved`). There is no
  page-order candidate budget; the pierced-tree read itself is bounded (5 s,
  50 000 examined nodes) and a limit is reported (`walk_truncated`), never an
  unchecked dispatch, and an action refusal is reported even when a subresource
  was blocked earlier in the same action). A `mousedown`/`pointerdown` handler
  *is* a page callback between the check and the click default action; the box
  re-read closes the layout version of it, and the guard's window `click`
  capture listener (re-asserted across `document.open`/`write`) cancels a
  target flipped by it; the `submit` listener applies the same effective-target
  rule (a **present** `formtarget` takes precedence over the form's `target`,
  even when empty; an empty effective target then falls to the base target and
  then `_self` — g35/g36 measured both halves), so a `formtarget="_self"`
  submit inside a `_blank` form is not over-blocked, and
  the listener is load-bearing for any submit whose effective target differs
  from the form's target and for shadow-tree submissions. Typing cannot deliver
  Enter (the key table has no newline entry, so a newline fails the action), so
  an implicit submission is not reachable from `type_text`.
  **The close-on-attach backstop is cleanup, not an egress control:** the
  guard-disabled discriminator evidence shows every
  creation shape reached the private listener (2 TCP) with the backstop alone,
  so creation prevention is the only egress control. A dedicated worker's own
  `fetch()` is observed on the page target and gated on the verified Chrome 156
  build; **worker-created targets (a nested `Worker`, `importScripts`, and a
  `SharedWorker` created from a worker's `WorkerGlobalScope`) are an unvetted
  egress channel — a known hole, not only untested — and are a recorded
  residual (review 2026-11-01).** It is an **SSRF filter,
  not an egress filter**: it does not stop a page sending data to a public host,
  and WebRTC/UDP and DNS-only channels are outside it (launch flags disable DNS
  prefetch and non-proxied WebRTC UDP). The vet resolves DNS and the browser
  resolves again, so a rebinding zone can still reach a private address — a
  disclosed residual (the wording is in this section and in
  `docs/operations.md` §7), not a solved problem.
- **Popups, targets, and cross-origin page changes.** `window.open` (own
  property and prototype) is replaced, `form.submit()`/`requestSubmit()` and
  non-self anchor/area/form targets are cancelled at document start, and closed
  shadow roots are forced open so a shadow anchor cannot hide from the cancel
  path (a page can still reach `window.open` through a target the guard does not
  see; the page target is then closed). `_self`, `_top`, `_parent` and an empty
  target on the top frame are allowed same-context navigation; `_blank` and
  **named** targets are cancelled and reported as
  `blocked.reason: "navigation_cancelled"` (a blocked `window.open` reports
  `popup_blocked`) so a click is never silently a no-op. `popup:
  "not_followed"` is per-action (only the action that observed the CDP event).
  A blocked same-tab navigation is failed before the request, and Chrome then
  shows its own error page in that tab — reopen a session if you need the
  previous DOM. When the page navigates to a different origin, the next result
  says so with `origin_changed: true` and `previous_origin`; `title` (300
  chars) and `final_url` (4096 chars) are capped, with `limit_hit` naming the
  field; control characters are stripped from title and console text.
- **Injection surface (disclosed, not pretend-absent).** Page content can
  instruct an agent to click or type; act only on selectors from your own plan
  and treat page text as data. `type` can enter secrets into forms, so never type
  credentials into a page you do not trust; the tool keeps the last few typed
  strings and scrubs them (raw and URL-encoded) out of console output, but a page
  can still encode a value in ways scrubbing will not catch. Sessions are
  capability strings: anyone with the id can act until close, TTL or shutdown,
  the id is only ever logged as an 8-character prefix, and server shutdown closes
  every session.

## `crawl_*` — bounded bulk crawling

```jsonc
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"crawl_start","arguments":{
  "start_urls": ["https://docs.example.com/"],
  "sitemap_urls": [],                    // up to 5; start_urls may be empty
  "scope": { "include_paths": ["/docs/"], "exclude_patterns": ["\\?page="],
             "max_depth": 3, "allow_offsite": false },
  "render_policy": "thin_only",          // never | thin_only | always; auto = thin_only
  "limits": { "max_pages": 500, "per_host_rate": 1.0, "max_duration_s": 1800 },
  "schema": { "container": ".entry", "fields": { "title": { "selector": "h1" } } }
}}}
```

- **Frontier in SQLite, dedup as a constraint.** Every enqueue is
  `INSERT ... ON CONFLICT DO NOTHING` on `PRIMARY KEY(job_id, url_hash)`, so a
  URL cannot be enqueued twice even if a caller forgets to check. Canonical
  identity is the same rule `fetch_page` uses (lowercase host, default port and
  fragment dropped, tracking parameters removed, query parameters sorted); a
  trailing slash is a different resource.
- **Leases, not in-memory queues.** A worker leases a row with `lease_until`; an
  expired lease is requeued with `attempts+1`, and after `crawl.max_attempts`
  the URL becomes `failed` with its last error. A killed process therefore
  resumes on the next start (`crawl_status` reports `running` until leases
  expire), and no completed page is fetched again.
- **Politeness and safety are the shared ones.** Each page goes through the same
  `fetch_page` path: SSRF gate on every address and redirect hop, robots.txt per
  hop, streamed caps, retries. `per_host_rate` adds a per-job interval floor;
  the configured minimum interval and any robots `Crawl-delay` can only raise it.
  A crawl cannot widen its own reach.
- **Scope** applies to discovered links (seeds are taken as given): depth,
  registrable site (off-site skipped unless `allow_offsite`), `include_paths`
  prefixes, and `exclude_patterns` regexes matched against `path?query`.
- **Trap resistance.** When a fetched page's content identity — the Markdown with
  link targets removed — was already seen in the same job, the page is stored but
  its links are not followed. A calendar trap therefore stops after the first
  duplicate instead of consuming the page budget.
- **Limits are honest.** `max_pages` and `max_duration_s` stop the job with
  `state: done`, `limits_hit` naming the limit, and the remaining frontier
  reported as `counters.queued`. `crawl_results` paginates with an opaque cursor
  bound to the job (a cursor from another job is `not_found`).
- **Sitemap seeding.** `sitemap_urls` (at most 5) are fetched and parsed
  **before** the job is persisted, each through the same gate. Their entries are
  *discovered* URLs, not explicit seeds: the job's `scope` filters them
  (`include_paths`, `exclude_patterns`, off-site) and they are deduped against
  `start_urls` and `max_pages`, then enqueued at depth 0. Explicit `start_urls`
  keep the "seeds bypass scope" rule. With empty `start_urls`, the scope's site
  set comes from the sitemap URL, so a cross-site entry is skipped. The start
  payload gains `sitemap: {urls_found, urls_enqueued, urls_skipped, errors:
  [{url, code}]}` only when `sitemap_urls` is supplied; a sitemap failure is
  recorded there and the other seeds still start. `lastmod` is never used for
  ordering or scheduling.
- **Cancel is cooperative.** In-flight pages finish, no new rows are leased, the
  job ends `cancelled`, and partial results stay readable.

## `monitor_*` — change monitoring

```jsonc
{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"monitor_create","arguments":{
  "url": "https://example.com/pricing",
  "mode": "main",                          // main | full | text (raw is rejected)
  "schema": { "fields": { "price": { "selector": "#price", "attr": "data-price" } } },
  "interval_s": 86400,
  "ignore_selectors": ["nav", "footer", ".ad-slot"],
  "label": "pricing:acme"                 // optional; project:slug convention
}}}
```

- **Snapshots, not crawl rows.** Each check stores the extracted Markdown,
  normalized for line endings, trailing whitespace, runs of blank lines and
  zero-width characters. `normalized_hash` decides "changed"; `raw_hash` keeps
  the exact body. The content lives in the CAS, so an unchanged check costs one
  row and one conditional request.
- **Conditional GET, deliberately.** A check uses the same acquisition path as
  `fetch_page` (SSRF gate, robots, caps) but not the crawl worker: crawls fetch
  unconditionally by design. A 304 is accepted only when the validator's
  representation is the one the previous snapshot saw; otherwise the body is
  fetched so "unchanged" is never inferred from the wrong baseline.
- **Classification.** `none` when the normalized text and watched fields are
  identical (line-ending and markup churn disappear here). `cosmetic` when only
  whitespace that survived normalization changed and it touches at most
  `monitor.cosmetic_max_changed_lines` lines. `substantive` for a changed word,
  a watched field, or a larger structural change.
- **Field-level monitoring.** A schema is evaluated against the same DOM parse
  that produces the Markdown. Compared fields are named in the change record even
  when the visible text is identical (an attribute-only price change still counts).
  `monitor_create` rejects the same invalid specs as `extract`.
- **Diffs are capped, stats are not.** `monitor_changes` returns newest-first
  with `from_snapshot`/`to_snapshot` timestamps, kind, full line/word/field
  counts and a unified diff capped at `monitor.max_diff_bytes`
  (`truncated: true`; the counts are never truncated).
- **Failures are one terminal result.** A permanent 404 is requested once, stored
  as a `fetch_event`, advances `next_due_at`, and appears in the check's `failed`
  list — no fabricated snapshot and no crawl-style retries.
- **Delete is a tombstone.** `monitor_delete` sets `active = 0`; history and
  snapshots stay queryable, and checks skip it.
- **Listing.** `monitor_list` returns up to 500 monitors in id order with
  `label`, `mode`, `active`, `has_schema`, `interval_s`, `created_at`,
  `last_checked_at` and `next_due_at`; `include_inactive: true` also returns
  tombstones. `truncated` and `limit_hit: "limit"` report a page smaller than the
  matching total. A `label` is optional (`project:slug` convention), trimmed,
  1..=64 characters, not unique and not pattern-enforced; every pre-v5 row is
  `null`.
- **Bounded bulk checks.** `monitor_check(all: true)` returns
  `pending: true` with a job id immediately. Add `wait_ms` (bounded by
  `[monitor] max_wait_ms`, default 30 000) to check inline and sequentially up to
  that budget: if the set finishes, `pending: false` and no job exists; when the
  deadline hits between monitors, the remainder is enqueued as **one** job with
  the remaining ids and the partial typed results are returned. One in-flight
  check may overrun by at most one check. `monitor_id` with `wait_ms` is
  `schema_invalid`.
- **Scheduler: opt-in in-process timer (amended).** The original design chose
  no scheduler; an amendment adds one: with
  `[scheduler].enabled = true` (default **false**), `serve` runs a resident timer
  that checks due monitors via the same `run_due` pass; `tick_ms` (default 30 s)
  and `max_checks_per_tick` (also clamped by `[monitor].max_checks_per_call`)
  bound each pass. `run-due` is unchanged, and one driver per instance is
  enforced by the data-directory lock: a local `run-due` fails while the server
  runs, while `run-due --http` goes through the server (the per-monitor lock and
  `next_due_at` re-read prevent doubles). Default off keeps cron/systemd valid.
- **Retention.** The newest 20 snapshots per monitor plus every change point (and
  the snapshots its diffs reference) survive GC; blobs are retained while a page
  or snapshot references them.

## Fetched content is data, not instructions

Every result carries `content_status: untrusted-data` in its front matter and the
tool description says the same. Page content may contain text that looks like
instructions; it is never safe to follow.

## Intended use and known limits

**Intended use.** Internal agent tooling over publicly accessible
pages (docs, news, reference sites); robots.txt respected; **≤1 request/s per
host**; no login-gated, authenticated, or paywalled content. There is no
open-web discovery, proxy rotation, distributed crawling, or multi-user auth.

**Readability can omit sibling blocks.** `mode=main` uses readability
and can drop whole sibling content blocks (for example a comparison table beside
the main article). v0.4 adds a **bounded, section-shaped repair**: between two
delivered blocks, a missing run is restored only when it begins with an
undelivered section heading that is a sibling of an adjacent delivered heading
(same parent element, no deeper level) and is followed by content. Heading-less
runs (card copy, teasers, landmark notes) are never admitted, and the whole
page's repair is skipped when it would exceed **25 %** of the delivered text. A
page without such a gap is byte-identical to the earlier output. Use `mode=full`
or `mode=text` when completeness matters. The 10-page real-site sample did
not show a common article-sibling omission; navigation/sidebars were excluded as
intended. Two observed site/selector cases are recorded in `docs/operations.md`:
python.org/about's JavaScript fallback and a broad `h2` schema selector picking
mdBook theme UI. This is a known limitation, not a hidden fallback.

Other limits:

- Monitors are **per URL**; multi-page "watch a site" semantics are later.
- Schema monitors compare the **declared fields**; they are not a full JSON delta.
- A rendered monitor **re-fetches every check** (the static validator cannot
  prove the rendered DOM is unchanged), so it costs one navigation per check.
- `monitor_check(all: true)` has no cancel tool; shutdown is cooperative, and a
  crash marks the job failed while the monitors stay due.
- A permanent 404 is **one terminal check** (one request, a stored fetch event,
  an advanced `next_due_at`, a `failed` entry) — not a three-attempt crawl retry.
- Crawl `crawl_results` rows are page-current, not per-attempt snapshots; a dead
  link can be requested up to `crawl.max_attempts` times at the frontier layer.
- A crawl whose seeds are all blocked ends `done` with `fetched: 0,
  skipped > 0`; `limits_hit` distinguishes a limit stop.
- Rendered pages execute untrusted JavaScript; the SSRF/robots gates cover the
  top-level navigation, not every subresource. Do not point the
  browser at an untrusted page if that residual risk is unacceptable.
- Binary extraction beyond the PDF **text layer** (images, forms, OCR), query
  operators (`OR`/`NEAR`/prefix) over the local index, page-level retention and
  authenticated crawling are out of scope. The change feed is served only on
  the loopback listener with its own capability token; it is not an
  authenticated multi-user service and a non-loopback bind with the feed enabled
  is refused at startup. Sitemap ingestion is discovery only
  (`sitemap`, `crawl_start.sitemap_urls`): `lastmod` does not schedule anything,
  and a sitemap index is followed one level (a nested index child is reported as
  a child error, not recursed). `pdf_extract` reads only the text layer, uses no
  password for a protected PDF, and stores only a complete result.

## Upgrade notes

- `profiles.hash` is versioned (`spec/v1/...`). Cached extraction rows written by
  an older unversioned build are simply missed and re-fetched; nothing is
  corrupted. An **in-flight pre-upgrade crawl job that carried a schema** should
  be restarted as a new job so its profile identity and `crawl_results.extracted`
  join stay consistent.
- If a unit passes `--http`, install a release built with the `http` feature and
  keep the rollback artifact outside the build tree. The shared unit's
  `ExecStart` points into `target/release`, so any feature-different
  `cargo build --release` overwrites the deployed file (the running process keeps
  its image) and the next restart fails with `this binary was built without the
  http feature`. Verify the installed file's `sha256sum` **and** that it is
  http-capable before starting; the default build is not a rollback for an
  `--http` unit. `docs/operations.md` names the durable release artifacts by sha.
- **v0.3 is code-only** (no schema or data change). The previous `99ab3a4` http
  binary is the rollback; preserve/install a feature-matched artifact and check
  its sha before start, as above. `docs/operations.md` records the v0.3
  upgrade/rollback note.
- Schema migrations are forward-only (current **v6**). v5 added the nullable
  `monitors.label`; v6 adds the FTS5 table `pages_fts` and the `meta` marker table,
  then backfills one index row per existing page in 200-page transactions at the
  first start (idempotent; the `fts_backfill_v6` marker makes a second start a
  no-op). Back up the database **and** the blob directory before running a newer
  binary; an older binary refuses a newer schema
  (`database schema vN is newer than this binary`), so rolling back the binary
  requires restoring a pre-migration backup. `docs/operations.md` has the stop →
  backup → install → start → verify runbook, the v6 note and the restore drill.
- Snapshots created before v4 have an unknown render mode; their first
  post-upgrade check fetches the body unconditionally and records the actual
  mode, so the static 304 shortcut only applies from then on.

## Layout

```text
src/core/      pure domain: URL canonicalization, IP classification, robots decisions, truncation
src/config.rs  TOML + env + CLI, path resolution, permission checks
src/fetch/     HTTP adapter, SSRF gate, per-host gate, robots cache, charset, content scope
src/extract/   readability + Markdown + metadata + links; spec validation (spec.rs)
               and spec evaluation over the DOM (spec_eval.rs)
src/render/    Renderer port, CDP browser lifecycle, discovery, auto heuristic
src/store/     SQLite repository, migrations, content-addressed blobs, retention,
               jobs/frontier (jobs.rs)
src/crawl/     crawl scope predicate, job worker, crawl_* service
src/monitor/   normalization, classification, monitor_* service, run-due
src/search/    pure DDG html/lite parser and the search payload/request shape
src/fts/       search_content contract: query validation, FTS5 expression, payload
src/mcp/http.rs  loopback streamable HTTP transport (feature `http`)
docs/operations.md  operator runbook: data, GC, backup, HTTP, troubleshooting
AGENTS.md      short repository build/test/run instructions
src/pipeline.rs  single-URL orchestration
src/mcp/       rmcp server and the fetch_page tool
```

## License

MIT — see [LICENSE](LICENSE). The crate metadata declares the same single
license (`license = "MIT"` in `Cargo.toml`), so packaged metadata, the
dependency-license gate and the repository file agree.
