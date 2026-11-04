# web-widow operations

Operator runbook for the packaged server. Every command here was executed
on a clean data directory; the defaults below are checked against
`config.example.toml` and the running binary, not inferred from internal
notes.

## 1. Install and verify

```sh
rustc --version                       # pinned 1.99.1 (rust-toolchain.toml)
cargo build --release                 # stdio server, render feature on
cargo build --release --features http # + loopback streamable HTTP
./target/release/web-widow --help
```

The release binary is self-contained (bundled SQLite, rustls). Chrome is the only
optional external runtime prerequisite and only for the `render` feature. The
binary does not need root and binds no port unless `--http` is passed.

Recommended install is to copy `target/release/web-widow` to a directory on
`PATH` (for example `~/.local/bin/`) and keep the config and data under
`~/.config/web-widow/` and `$XDG_DATA_HOME/web-widow/`.

## 2. Configuration

`config.example.toml` is the annotated default set. Copy it:

```sh
install -d -m 700 ~/.config/web-widow
install -m 600 config.example.toml ~/.config/web-widow/config.toml
```

Set `fetch.contact` to a real, monitored address (the example uses
`crawler@example.com`); it becomes part of the User-Agent and is how a site
operator can reach you. Keep the config file `0600` (the server warns at startup
if group/other can read it). The data directory must be `0700`; the server
refuses to start otherwise.

Only two settings widen **inbound** reach and they are config-only:
`fetch.allow_hosts` (private/CIDR exceptions, each match logged) and
`robots.enabled = false` (announced loudly). Nothing on the MCP surface or CLI
can widen reach. Webhooks are the deliberate **outbound** path: targets are
config-only names, no tool can add a URL, and §9.8 states the egress disclosure.

`[sitemap]` bounds sitemap discovery: `max_bytes` (fetched body; a gzip body is
decompressed only up to 4×), `max_urls` (entries kept) and `max_children` (index
children fetched). All three are validated at load.

`[pdf]` bounds PDF text extraction and locates the native engine: `pdfium_path`
(strict; config file only), `pdfium_discovery` (default true), `max_bytes`
(32 MiB), `max_pages` (200) and `max_chars` (1 000 000). A per-call value may
lower `max_pages`/`max_chars`, never raise the server ceiling.

`[events]` controls the change feed: `feed_enabled` (default true; `false`
removes the route and returns 404) and `feed_items` (1..=200, default 100; the
RSS document itself caps at 100).

`[embeddings]` and `[webhooks]` are **live**. With
`enabled = true`, `serve` loads the local embedding artifacts and runs the lazy
backfill, and/or starts the webhook dispatcher. `[scheduler]` is the
**opt-in in-process timer** (default false). This
build starts no timer unless enabled. All three `enabled` keys default to
`false`.

- `[embeddings]`: `model` (`minilm-l6-v2-q`, the only v1 id), optional
  `model_dir`/`ort_dylib_path`, and bounds `chunk_chars` (200..=2000),
  `chunk_overlap` (0..=chunk_chars/2), `max_chunks_per_page` (1..=200),
  `max_source_chars` (1000..=200000), `backfill_pages_per_pass` (1..=200),
  `backfill_interval_ms` (1000..=600000), `query_max_chars` (1..=4096).
  `model_dir` defaults to `<data_dir>/models/<model>`. The library is strict when
  `ort_dylib_path` is set; otherwise discovery is `WEB_WIDOW_ORT_PATH` →
  `ORT_DYLIB_PATH` → `<exe dir>/libonnxruntime.so` → `<data_dir>/lib/libonnxruntime.so`.
  Every model file's length and SHA-256 are checked against the built-in
  manifest; a mismatch is `embedding_unavailable` naming the file. There is no
  network access on this path.
- `[webhooks]`: `timeout_ms` (500..=30000), `max_attempts` (1..=10),
  `allow_http` (loopback/test only). Targets exist **only** as named
  `[webhooks.targets.<name>]` entries with `url`, `kind`
  (`generic`/`slack`/`discord`), at most one of `secret_env`/`secret_file`, at
  most one of `auth_env`/`auth_file`, and `include_diff`. The name matches
  `^[a-z0-9][a-z0-9_-]{0,31}$`; the URL must be `https` unless `allow_http`,
  and must not carry userinfo. No target is configured by default, so nothing
  fires. A check enqueues an outbox row inside its transaction; the separate
  dispatcher attempts it (2xx delivered; 408/429/5xx/timeout retried;
  other 4xx/redirect dead; backoff `[60,300,1800,7200,21600]` s). The URL is
  treated as a secret and is never logged or returned — status shows the host
  only. See §9.8 for the egress disclosure and HMAC recipe.
- `[auth]`: config-only credential profiles, manager binaries and the
  `auth_audit` window. Validated at load and inert until the operator configures a profile;
  `fetch_page.profile` is the only tool argument that selects one. The full contract, the audit
  row/outcomes/retention and the operator runbook are in §12.
- `[scheduler]`: the opt-in in-process timer. `enabled` is **false by default**;
  `tick_ms` is 1 000..=600 000
  (default 30 000) and `max_checks_per_tick` 1..=500 (also clamped by
  `monitor.max_checks_per_call`). It runs only in `serve`; `run-due` is unchanged
  and never starts the timer. One driver per instance: the data-directory lock
  makes a local `run-due` fail while the server runs, and `run-due --http` goes
  through the server. One pass per tick, no catch-up burst; `next_due_at` is the
  single source of truth. The task is cancelled and awaited with the other
  background tasks on shutdown (10 s bound). See §9.9.

## 3. Run

```sh
# stdio (default; the MCP client spawns this process)
web-widow --config ~/.config/web-widow/config.toml serve

# loopback streamable HTTP (needs the http feature)
WEB_WIDOW_HTTP_TOKEN="$(cat ~/.config/web-widow/http-token)" \
  web-widow --config ~/.config/web-widow/config.toml serve --http 127.0.0.1:8765

# one-shot cron pass
web-widow --config ~/.config/web-widow/config.toml run-due
```

Shutdown: SIGINT/SIGTERM stops accepting work, cancels job workers, closes the
browser, removes the throwaway Chrome profile and releases the data-directory
lock. The target is exit within 10 s; a SIGKILL leaves the profile/lock file and
the next start wipes the profile.

## 4. Data layout

`$XDG_DATA_HOME/web-widow` (or `--data-dir`) is created `0700`:

```text
web-widow.db          SQLite index (WAL; one writer, four readers), including
                      the pages_fts full-text table (schema v6)
web-widow.db-wal/-shm WAL sidecars (normal while running)
blobs/aa/bb/<hash>    content-addressed raw bodies, snapshot text, records and
                      screenshot PNGs
render/chrome-profile throwaway Chrome profile (wiped at start/exit)
server.lock           advisory lock; contains the holding pid
```

One process per data directory. A second process fails with
`busy: data directory … is already in use by pid N`. Do not share a data
directory between a stdio and an HTTP server, and do not delete `server.lock`
while a process holds it.

## 5. Retention and GC

Defaults (`[retention]`):

| Data | Rule |
|---|---|
| `fetch_events` | 30 days, always keep the most recent per page and any event a snapshot references |
| `extractions` | 30 days, keep those referenced by a snapshot |
| `snapshots` | newest `snapshots_per_monitor` (20) per monitor **plus every change point** and every snapshot a change diff references |
| `screenshots` | rows older than `screenshots_days` (30, 1..=3650) are pruned before the blob pass |
| `webhook_deliveries` (delivered) | rows older than `webhook_deliveries_days` (30, 1..=3650) are pruned; pending rows are never pruned |
| `webhook_deliveries` (dead) | dead letters are kept for `webhook_dead_days` (90, 1..=3650) so a failure stays diagnosable |
| blobs | evicted only when unreferenced by `pages`/`fetch_events`/`snapshots`/`screenshots` and older than 30 days, or over budget oldest-first |
| total blob budget | `max_blob_bytes` (20 GiB default); referenced blobs are never evicted to meet it |

`pages_fts` is **not** a retention input: it has exactly one row per `pages` row,
the store never deletes pages, and a page's index text is replaced (never
duplicated) on each successful acquisition. A `304` or a failed fetch leaves the
previous index row in place. The indexed text lives in the database, not in
`blobs/`, so it is outside the 20 GiB blob budget and outside this GC; the only
control is `[fts].max_index_chars` (default 50 000 chars/page). At the planning
ceiling (~3 000 pages/day) that is on the order of 150 MB/day worst case — check
the database size before enabling a high-volume store, and lower the cap (or
decide a page-retention policy) if that is too much.

GC runs at startup and is idempotent. A referenced blob surviving over budget is
intentional: content the index or change history reads is never silently traded
for space. The retention drill (`tests/retention.rs`, plus the real-data transcript)
verifies eviction order, change-point survival, and that every `raw_hash`,
`normalized_hash` and `result_hash` in `pages`, `fetch_events` and `snapshots`
still has a `blobs` row and a file.

Schema v8 adds no retention key: `embeddings` and `embedding_state` cascade from
`pages` (the store never deletes pages), and `monitor_webhooks`/
`webhook_deliveries` cascade from `monitors`/`changes` (changes are retained).
Their growth is bounded by the rows written into them, not by this GC.

## 6. Backup and restore

Take the SQLite backup with `VACUUM INTO` (safe while the server runs; it reads a
consistent snapshot):

```sh
sqlite3 ~/.local/share/web-widow/web-widow.db \
  "VACUUM INTO '/backup/web-widow-$(date +%F).db'"
rsync -a ~/.local/share/web-widow/blobs/ /backup/blobs-$(date +%F)/
```

Back up the database and the blob directory **together**. The database is the
index of truth; blobs are content addressed, so a newer blob set is harmless but
a missing blob is a dangling reference. For a cold backup, stop the server first
and copy the whole data directory. To restore: stop the server, restore the
database and blob tree, ensure the data directory is `0700`, start the binary.

### Upgrade v8 → v9 (`auth_audit`, additive)

Schema v9 is additive: one `auth_audit` table and its two indexes
(`auth_audit_at`, `auth_audit_site_at`). No existing table, column or row is
touched. The table is empty on migration and
stays empty until an authenticated fetch writes one value-free row per attempt;
the release also adds the inert `[auth]` config contract, and `[auth].audit_days`
(default 90, 1..=3650) prunes rows in the startup GC. There is no backfill.

A failed migration rolls back inside its transaction; a v8 binary refuses a v9
database (`database schema v9 is newer than this binary (v8)`), so a binary
rollback means restoring the pre-migration database.

1. Preserve the v8 binary and `VACUUM INTO` each database; record
   `PRAGMA user_version` and the counts (`pages`, `pages_fts`, `blobs`,
   `fetch_events`, `extractions`, `snapshots`, `changes`, `monitors`, `jobs`).
2. Stop the unit, install the v9 binary, start. The migration runs before
   serving; there is no backfill.
3. Verify: `PRAGMA user_version` = 9; `auth_audit` exists and is empty; the
   `auth_audit_at` and `auth_audit_site_at` indexes exist; every existing count
   is unchanged; `PRAGMA foreign_key_check` returns no rows.
4. Rollback: stop, restore the pre-migration database and blob tree, restore the
   v8 binary, start.

### Upgrade v9 → v10 (authenticated page labels, additive)

Schema v10 is additive: two page labels and one index —
`pages.authenticated INTEGER NOT NULL DEFAULT 0`, `pages.auth_site TEXT`
and `pages_authenticated (authenticated, id)`. No existing column, row or value
is rewritten; every pre-v10 page is unauthenticated by construction
(`authenticated = 0`, `auth_site = NULL`), and the upgrade never changes a count.
There is no backfill and no background task; the labels are written only by an
authenticated acquisition when `[auth].persist = true`.

Persistence stays opt-in and default-safe: `[auth].persist` remains false, so
return-only behavior is unchanged after the upgrade; `index_authenticated`,
`events_authenticated` and `page_retention_days` (default 7, 1..=3650) keep
their defaults. A failed migration rolls back inside its transaction; a v9
binary refuses a v10 database (`database schema v10 is newer than this binary
(v9)`), so a binary rollback means restoring the pre-migration database.

1. Preserve the v9 binary and `VACUUM INTO` each database; record
   `PRAGMA user_version` and the counts (`pages`, `pages_fts`, `blobs`,
   `fetch_events`, `extractions`, `snapshots`, `changes`, `monitors`,
   `auth_audit`).
2. Stop the unit, install the v10 binary, start. The migration runs before
   serving; there is no backfill.
3. Verify: `PRAGMA user_version` = 10; `pages` has `authenticated` and
   `auth_site`; `SELECT COUNT(*) FROM pages WHERE authenticated <> 0 OR auth_site IS NOT NULL`
   returns 0; the `pages_authenticated` index exists; every existing count is
   unchanged; `PRAGMA foreign_key_check` returns no rows.
4. Rollback: stop, restore the pre-migration database and blob tree, restore the
   v9 binary, start.

### Upgrade v4 → v5 (`monitors.label`)

Schema v5 is one additive, nullable column (`ALTER TABLE monitors ADD COLUMN
label TEXT`); every pre-v5 monitor reads as `NULL`. Migrations are forward-only,
so **the backup is the rollback unit**, not a down-migration.

1. Stop the unit that owns the data directory (`systemctl --user stop
   web-widow.service`; the same for a second instance). The advisory lock refuses
   a second writer, and a quiescent database makes the invariants and the backup
   describe exactly the same state.
2. Record `PRAGMA user_version`, the counts for `monitors` (active/total),
   `snapshots`, `changes`, `pages`, `fetch_events`, `jobs`, and the blob file
   count.
3. `sqlite3 "$DB" "VACUUM INTO '$BACKUP'"`; open the backup read-only and verify
   the same version and counts. Record its path, size and `sha256sum`. Blobs are
   untouched by v5.
4. Preserve a rollback binary built with the unit's **exact feature set** at a
   durable path outside the build tree, and record its `sha256sum`. Then install
   the new release at the unit's `ExecStart` path. The unit file itself does not
   change. **If the unit passes `--http`, both the new artifact and the preserved
   rollback must be http builds**; a default build has no HTTP transport and
   exits with `this binary was built without the http feature`. Verify the
   installed file's `sha256sum` immediately before starting.
5. Start the unit. The first start applies v5 in one transaction; a crash rolls
   it back and leaves `user_version = 4`.
6. Verify: unit active with no restart loop; `user_version = 5`; every invariant
   equal; `monitor_list` returns the same monitors with `label: null`; `tools/list`
   shows 12 tools; one `monitor_check` returns a typed result.
7. Rollback (if a verification fails): stop the unit, restore the backup
   database, restore the preserved binary whose `sha256sum` matches the unit's
   feature set (**never** a default build for an `--http` unit), start, and
   verify `user_version = 4` and the same monitor list. Do not leave a failed
   start to `Restart=always` / `Restart=on-failure`; stop first.

**Artifact-path hazard.** The shared unit's `ExecStart` points inside the
repository's `target/release` tree. Any `cargo build --release` with a different
feature set overwrites the deployed file (a running process keeps its mapped
image), so the next restart runs the wrong build. Keep the rollback artifact
outside the build tree and check both identity and feature set before starting:

```sh
sha256sum "$BIN"                                  # must equal the recorded rollback sha
strings "$BIN" | grep -q 'text/event-stream'      # http-capable when the unit passes --http
```

For the deployed release the durable artifacts are
`~/.local/share/web-widow-backups/shared-rollback-binary-ac31015-http`
(`403254f0…`, built from `ac31015 --features http`, schema v4, 11 tools) and
`~/.local/share/web-widow-backups/shared-previous-runtime-9ba61b68`
(`9ba61b68…`, the v0.1 runtime image, http + v4, 10 tools). The **default**
build `e7fd8a4c…` is not a valid rollback for an `--http` unit.

A **v4 binary refuses a v5 database**:

```
database schema v5 is newer than this binary (v4); downgrade is not supported
```

Restore drill (never on production): copy the pre-migration backup to a scratch
data directory, run the v5 binary against the copy (migrate, rows intact,
`monitor_list` populated), then restore the backup over the copy and run the
preserved v4 rollback binary (expect the refusal above against the v5 copy; the
pristine v4 file opens normally). Also start the rollback binary over
`serve --http` on a copy of the v4 backup, verify `initialize` + `tools/list`
shows the **11-tool v4-era surface**, and stop it cleanly. The exact executed
commands and observations are retained outside this repository.

### Upgrade v7 → v8 (embeddings/webhooks, additive)

Schema v8 is additive: `embeddings`, `embedding_state`,
`monitor_webhooks` and `webhook_deliveries`, plus their indexes. No existing
table, column or row is changed, and this release adds no tool and no background task. A
failed migration rolls back inside its transaction; a v7 binary refuses a v8
database (`database schema v8 is newer than this binary (v7)`), so a binary
rollback means restoring the pre-migration database.

1. Preserve the v7 binary and `VACUUM INTO` each database; record
   `PRAGMA user_version` and the counts (`pages`, `pages_fts`, `blobs`,
   `fetch_events`, `extractions`, `snapshots`, `changes`, `monitors`).
2. Stop, install the v8 binary, start. The migration runs before serving; there
   is no backfill.
3. Verify: `PRAGMA user_version` = 8; the four new tables exist (empty on
   migration); every existing count is unchanged; `PRAGMA foreign_key_check`
   returns no rows.
4. `[embeddings]`, `[webhooks]` and `[scheduler]` are validated at load.
   `[embeddings]` is live once the operator provisions the library and model
   and sets `enabled = true`; `[webhooks]` is live once targets are configured
   and `enabled = true` (nothing fires otherwise); the `[scheduler]` timer is
   live when `enabled = true` (default off). Enabling the
   timer means removing/disabling any cron `run-due` entry for that instance
   (one driver); leaving it off keeps cron valid.

### Embedding artifacts

`search_semantic` needs two provisioned artifacts; they are **not** crates and
must not be committed. Verify their pinned SHA-256 before installing:

| Artifact | Size | SHA-256 |
|---|---|---|
| `libonnxruntime.so` (ONNX Runtime 1.31.0, from `onnxruntime-linux-x64-1.31.0.tgz`) | 29 345 384 B extracted (11 408 003 B tgz) | `469849977cb60744ea160927e1a3a0643fd4c55c0aff743251c6d293402e334e` |
| `models/minilm-l6-v2-q/onnx/model_quantized.onnx` | 22 972 370 B | `afdb6f1a0e45b715d0bb9b11772f032c399babd23bfc31fed1c170afc848bdb1` |
| `models/minilm-l6-v2-q/tokenizer.json` | 711 661 B | `da0e79933b9ed51798a3ae27893d3c5fa4a201126cef75586296df9b4d2c62a0` |
| `models/minilm-l6-v2-q/config.json` | 650 B | `7135149f7cffa1a573466c6e4d8423ed73b62fd2332c575bf738a0d033f70df7` |
| `models/minilm-l6-v2-q/special_tokens_map.json` | 125 B | `b6d346be366a7d1d48332dbc9fdf3bf8960b5d879522b7799ddba59e76237ee3` |
| `models/minilm-l6-v2-q/tokenizer_config.json` | 366 B | `9261e7d79b44c8195c1cada2b453e55b00aeb81e907a6664974b4d7776172ab3` |

The model files are the quantized ONNX export of `Xenova/all-MiniLM-L6-v2` on
Hugging Face. Fetch them (the ONNX file keeps its `onnx/` subdirectory; the four
JSON files sit at the model root), then verify the table above:

```sh
base=https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/main
mkdir -p "<data_dir>/models/minilm-l6-v2-q/onnx"
curl -fsSL "$base/onnx/model_quantized.onnx" \
  -o "<data_dir>/models/minilm-l6-v2-q/onnx/model_quantized.onnx"
for f in tokenizer.json config.json special_tokens_map.json tokenizer_config.json; do
  curl -fsSL "$base/$f" -o "<data_dir>/models/minilm-l6-v2-q/$f"
done
```

ONNX Runtime itself comes from the Microsoft
`onnxruntime-linux-x64-1.31.0.tgz` release archive (extract
`lib/libonnxruntime.so`). Nothing is fetched at runtime.

Install the library beside the binary (or set `[embeddings].ort_dylib_path`) and
the model directory under `<data_dir>/models/minilm-l6-v2-q/`. The server never
downloads either file; a missing or mismatched file is the typed
`embedding_unavailable` and every other tool still works. To re-embed after a
model/provider change, ship the new artifacts and delete the old
`embeddings`/`embedding_state` rows for the non-active model (startup prunes
them automatically); `[embeddings].enabled = false` disables the whole path.

### Upgrade v6 → v7 (recipes/screenshots, additive)

Schema v7 is additive: `recipes`, `recipe_versions` and
`screenshots`, plus their indexes. No existing table, column or row is changed,
and this release adds no tool. A failed migration rolls back inside its transaction; a v6
binary refuses a v7 database (`database schema v7 is newer than this binary
(v6)`), so a binary rollback means restoring the pre-migration database.

1. Preserve the v6 binary and `VACUUM INTO` each database; record
   `PRAGMA user_version` and the `pages`/`pages_fts` counts.
2. Stop, install the v7 binary, start. The migration runs before serving; there
   is no backfill.
3. Verify: `PRAGMA user_version` = 7; `recipes`, `recipe_versions`,
   `screenshots` exist (empty on migration); every existing table and
   `pages_fts` count is unchanged.
4. Screenshot retention: `[retention].screenshots_days` (default 30, 1..=3650)
   prunes screenshot rows before the blob pass, and a blob referenced by a
   surviving screenshot is retained by the age and budget passes.

### Upgrade v5 → v6 (local FTS index + one-time backfill)

Schema v6 is additive: a new FTS5 virtual table `pages_fts(title, body)` and a
small `meta` key/value table. Nothing existing is altered or dropped, so a failed
migration rolls back to v5 and leaves the page data intact. **The backup is still
the rollback unit**: a v5 binary refuses a v6 database (`database schema v6 is
newer than this binary (v5)`), so rolling the binary back means restoring the
pre-migration database.

1. Back up the database **and** the blob directory (`VACUUM INTO` + `rsync`,
   §6 above). Record `PRAGMA user_version` and the `pages` count.
2. Stop the unit (`systemctl --user stop web-widow.service`).
3. Install the new binary; verify its `sha256sum` and that it is feature-matched
   for the unit (see the v0.2 note below).
4. Start. The first start migrates to v6 and runs the backfill **before serving**:
   200 pages per transaction, capped at `[fts].max_index_chars`, idempotent. The
   log line is `fts backfill complete (schema v6)` with `scanned`, `with_text`,
   `title_only` and `max_index_chars`. A crash mid-backfill re-runs from the start
   on the next launch (upserts replace, so nothing duplicates) because
   `meta.fts_backfill_v6` is only written after the last batch.
5. Verify: `PRAGMA user_version` = 6; `SELECT COUNT(*) FROM pages_fts` equals
   `SELECT COUNT(*) FROM pages`; `SELECT value FROM meta WHERE key =
   'fts_backfill_v6'` is set; a known title/topic query returns the expected page
   with a non-empty snippet; monitors and `crawl` history are unchanged.
6. Rollback: stop, restore the pre-migration database and blob tree, restore the
   previous binary, start.

Backfill cost is proportional to the page count (tens of pages: instant; tens of
thousands: a startup pause). A page whose stored extraction the backfill cannot
parse (a monitor-only page, whose stored payload is a bare records array) is
indexed by **title only** until its next check stores the delivered text.

### v0.3 code-only release (no schema change)

v0.3 changes only the HTTP transport (stateless) and the `fetch_page`/
`extract` payload/quality paths. There is **no schema or data change**:
`user_version` stays 5, monitor rows and snapshots are untouched, and the
rollback is a binary swap, not a database restore.

1. Preserve the deployed v0.2 http artifact `99ab3a4…` as
   `~/.local/share/web-widow-backups/v0.3-shared-rollback-m02-http` (and a
   second-instance copy) with `sha256sum`; verify it has
   the HTTP transport (`strings "$BIN" | grep -q 'text/event-stream'`).
2. Stop the unit, install the new `--features http` artifact at its
   `ExecStart` path, and verify its sha **before** starting.
3. Start; verify active, `stateless=true`, `user_version=5`, the same monitor
   rows/counts, 12 tools, bearer `401`, a stale-id call returning `200`, `GET
   /mcp` returning `405 Allow: POST`, and a 7-call concurrent burst returning
   7×200. One live `fetch_page` must carry `structuredContent.markdown`; one
   live `extract` must carry `truncated_fields` (and `render_quality` when
   rendered).
4. Rollback: stop, swap `99ab3a4…` back, start, verify 12 tools and the same
   monitor list. No database restore is involved.

### v0.4 behaviour note (`:main` scope, `selector:"*"`)

v0.4 does not change the schema or the tool surface. Two evaluation meanings
change and are worth knowing at upgrade time:

- `container: ":main"` now evaluates fields inside the **`body` of the same
  readability article** that `fetch_page mode=main` delivers (one readability
  pass per acquisition). The article wrapper `<html>`/`<head>` is never a field
  root. Before this change a `:main` spec evaluated at the wrapper `<html>`, so
  `*` matched `<head>` first (an empty value with no `field_error`) and a broad
  selector could include wrapper/head content around the article.
- A field selector of exactly `*` now means **the record scope itself**
  (`text`/`html`/`attr` on the scope; `all: true` → one element). Other
  selectors keep ordinary CSS semantics.

The canonical spec text is unchanged, so `profiles.hash` is unchanged: an
extraction cached under the old meaning can be served for up to
`extract.cache_freshness_s` (default 300 s) after the swap. Pass
`use_cache: false` when the new scope must apply immediately. The rollback is a
binary swap; no database restore is involved.

### v0.4 code-only release (13 tools, no schema change)

v0.4 adds `fetch_feed`, `run-due --http`, normalized tool schemas, the `:main`/
`selector:"*"` semantics and the bounded section-gap repair. There is
**no schema or data change**: `user_version` stays 5 and monitor/snapshot rows
are untouched; the rollback is a binary swap.

- **Feed politeness.** `fetch_feed` is one GET through the same SSRF gate, robots
  check, redirect vetting, per-host floor and body cap as `fetch_page`. It caches
  nothing and writes no content rows (only the `hosts` robots cache may change).
  A body with an internal DTD/entity declaration is refused before parsing.
- **Cron beside the server.** `web-widow run-due --http http://127.0.0.1:8765/mcp`
  checks due monitors through the running instance and never opens the data
  directory, so it cannot fight the lock. One JSON line on stdout, exit 0 for a
  completed or queued pass, non-zero for transport/HTTP/typed errors. Use
  `WEB_WIDOW_HTTP_TOKEN` or a `0600` `--http-token-file`; redirects are disabled.
- **Schemas.** `tools/list` no longer advertises the schemars integer formats
  (`uint*`, `int64`, `double`); `type`/`minimum`/`maximum` and string formats are
  unchanged and serde validation is untouched. The surface is now **13 tools**.
- **Deployment.** Preserve the deployed v0.3 artifact `366ab64c…` as
  shared and second-instance copies under `~/.local/share/web-widow-backups/`
  (sha + `strings … | grep -q 'text/event-stream'`) **before** the swap; stop,
  install the new `--features http` binary at the unit's `ExecStart`, verify its
  sha and http string, start, and probe: `401`, 13 tools incl. `fetch_feed`,
  `stateless=true`, `user_version=5`, the same monitor rows, one live
  `fetch_feed`, one `run-due --http` pass, and a `tools/list` schema with no
  `uint*`/`int64`/`double` formats. Rollback: stop, swap `366ab64c…` back, start,
  verify 12 tools and the same monitors. No database restore is involved.

## 7. Chrome and rendering

- `render.chrome_path` pins the executable; `discovery = false` with a pinned
  path is the reproducible setup. Without `chrome_path`, discovery checks
  `$CHROME_PATH` and well-known locations.
- The verified build uses Chrome for Testing 156.0.8078.4. Another Chrome/CDP
  version can change settle/cap observations; re-run the render tests after an
  upgrade.
- Each page gets an ephemeral browser context; the profile is wiped at start and
  clean shutdown.
- **Text profile** (`fetch_page`, `extract`, crawl, monitor): image/media/font/
  stylesheet requests are blocked.
- **Visual profile** (`screenshot`): stylesheets, fonts and images are allowed
  (media stays blocked) under `[render] visual_max_bytes` (20 MiB) and
  `visual_max_requests` (500). Hitting a cap reports `truncated: true` and
  `limit_hit: "visual_max_bytes" | "visual_max_requests"`; the 10 s navigation
  ceiling reports `"ceiling"`.
- `screenshot` writes the PNG to the CAS and one `screenshots` row (blob row
  first, same writer closure). The response carries the hash/size/MIME/
  dimensions, never base64. `blob_get { hash }` returns the absolute CAS path
  (hash validated, no caller path), size, MIME and bounded references; unknown
  or missing content is `not_found`. `page_id` is the existing page row for the
  canonical URL or null; a screenshot never creates a page row.
- Visual assets are browser subresources: the top-level SSRF/robots gate does
  not vet them. Treat captured pages and PNG bytes as untrusted
  data.
- `render=always` on a missing browser is `render_unavailable`; a render failure
  is `render_failed`, never a silent static result (`auto` reports
  `static_fallback` + `render_warning` instead).
- `render=always` on JSON/XML treats the browser DOM as `text/html`; use
  `render=never` for non-markup content. Subresources and iframes/popups are
  outside the top-level SSRF/robots gate.
- A rendered **monitor** re-fetches every check; the static validator cannot
  prove the rendered DOM is unchanged. Budget one navigation per check.

### Interactive sessions (`interact_*`)

- One held page per process, opened by `interact_open` (needs the `render`
  feature and Chrome). The idle TTL is `[interaction].ttl_s` (default 120 s,
  30..=600), the hard lifetime is 900 s and the action ceiling is
  `[interaction].max_actions` (default 60). A lazy reaper closes an idle page
  every 10 s; server shutdown closes the live session before the browser stops.
- The session uses the visual profile above. On top of it, a per-session CDP
  Fetch handler pauses **every** request routed through the page target:
  `data:`/`blob:` continue, `file:`, `ws:`, `wss:` and other non-http(s) schemes
  are refused, and every http(s) request is SSRF-vetted. **Documents** also pass
  robots.txt; **every** request takes the per-host politeness lease (≤1 req/s).
  A refused request is visible on the action as `blocked: {reason, detail}`; a
  blocked same-tab navigation leaves Chrome's error page in the tab, so reopen
  the session to continue.
- Visual caps (`visual_max_requests`, `visual_max_bytes`), a 500-paused-request
  abuse cap (refused requests included, so the gate cannot become an unbounded
  DNS channel) and a 10-screenshot-per-session budget all refuse further work and
  report a sticky reason.
- **Target scope.** The gate covers the intercepted page target. Popups are
  prevented at document start (`window.open` own property and prototype;
  `form.submit()`/`requestSubmit()`; non-self anchor/form targets; closed shadow
  roots are forced open so the cancel path sees them) and any page target that
  still appears is closed. Subframe documents are refused (`blocked_iframe`)
  because an OOPIF's requests are outside the page Fetch domain; service-worker
  registration and the `SharedWorker` constructor are refused for the same
  reason. A dedicated worker's own `fetch()` is observed on the page target and
  gated **on the verified Chrome 156 build** (an engine behaviour, one pinned
  build, not a design property). **Worker-created targets — a nested `Worker`,
  `importScripts`, and a `SharedWorker` created from a worker's
  `WorkerGlobalScope` — are an unvetted egress channel: their requests are not
  attributed to the page target, so they leave without the SSRF vet, robots,
  the host lease or the caps. This is a known hole in the N2 control, not only
  an untested path; the residual was reviewed 2026-11-01.** The close-on-attach
  backstop is cleanup only: the guard-disabled G0 probe shows
  every creation shape reached the private listener with the backstop alone, so
  creation prevention is the only egress control (server pre-dispatch hit-test
  at the click point + document guard). The hit-test resolves the deepest node
  and walks its ancestor chain (shadow hosts included) against the hit node's
  own document's first `<base target>`, read with
  `DOM.querySelector`/`DOM.getAttributes`; every other predicate attribute
  (`target`/`formtarget`/`type`) is read the same way, and a submit control's
  form owner is resolved from a non-empty `form` attribute by id in that
  control's own tree (per chain node), so a sibling form in a shadow tree is
  modelled; a hit `<label>` resolves to its labeled control (`for` in the same
  tree, else the first labelable descendant) and that control is evaluated with
  the same rule, unless the click targets the spec's interactive content inside
  the label (a bare `tabindex` element and an href-less anchor are not
  interactive content). No click-policy input passes through page JavaScript. The hit
  test ignores `pointer-events:none` nodes, matching what the press will hit. The point is computed once from the browser's
  content quads and dispatched with `Input.dispatchMouseEvent` at those exact
  coordinates (`DOM.scrollIntoViewIfNeeded`, never page `scrollIntoView`), and
  the box is re-read immediately before the press: a box that moved is refused
  as `click_point_moved`. There is no page-order candidate budget; the
  pierced-tree read is bounded (5 s, 50 000 examined nodes) and its limit is
  refused as `walk_truncated`. Page-script activation is intercepted for
  `element.click()`, `dispatchEvent('click')` and `requestSubmit(submitter)`
  (submitter `formtarget`), and the guard's own primitives are captured at
  install time, so replacing `Object.freeze`/`getAttribute`/`querySelector`
  later cannot un-freeze or blind it. The window `click` capture listener is
  re-asserted after `document.open`/`write`, which remove every listener on the
  Document and its Window. A `mousedown` handler is a page callback between the
  check and the click default action: the box re-read closes the layout version
  and that listener cancels an attribute flip. The `submit` listener applies the
  submitter's **present** `formtarget` (even when empty) before the form's
  `target`, then the base target for an empty effective target (so a
  `_self` submit in a `_blank` form is not over-blocked), and it is the control
  for shadow-tree submissions; typing cannot deliver Enter (a newline is not a
  key name, so the action fails). The isolated-world heartbeat uses its own
  secret and a non-default execution context (a context with no usable
  `auxData.isDefault` is classified by its name, so unknown metadata fails
  closed), and page console lines are never hidden behind its prefix. `_top`/`_parent`/`_self`/empty targets navigate
  same-context; `_blank` and named targets are cancelled with
  `blocked.reason: "navigation_cancelled"` (a cancelled `window.open` reports
  `popup_blocked`); the marker channel is non-forgeable/non-suppressible and a
  missing guard fails the action closed. The gate is an **SSRF filter, not an
  egress filter**, and it does not cover WebRTC/UDP or DNS-only channels (DNS
  prefetch and non-proxied WebRTC UDP are disabled by launch flags).
  **Rebinding:** the vet resolves DNS and the browser resolves again, so a
  short-TTL zone can still reach a private address (this wording,
  plus the README "Target scope" bullet, is the lane's discoverable record —
  the risk card is owned by Implementation-Flow).
- `interact_state` captures an optional viewport screenshot through the same CAS
  path (`source = "interaction"`, `[render].max_screenshot_bytes`). When the page
  changes origin, the result carries `origin_changed`/`previous_origin`;
  `title` (300 chars) and `final_url` (4096 chars) are capped with `limit_hit`;
  control characters are stripped from title and console text. Session ids are
  logged only as an 8-character prefix; URLs in interaction and render log lines
  go through the query-redaction convention.
- **Injection and secrets.** Page content is untrusted data; act only on
  selectors from your own plan. `type` can enter secrets into a page's forms —
  never type credentials into a page you do not trust. The `type` response
  returns `typed_chars` only, the text is never logged, and the last few typed
  strings (raw and URL-encoded) are scrubbed from console output; a page can
  still encode a value in a way that scrubbing will not catch.

## 8. HTTP transport and token

Build with `--features http`. The token is never a config value:

```sh
install -d -m 700 ~/.config/web-widow
head -c 32 /dev/urandom | base64 > ~/.config/web-widow/http-token
chmod 600 ~/.config/web-widow/http-token
WEB_WIDOW_HTTP_TOKEN="$(cat ~/.config/web-widow/http-token)" \
  web-widow --config ~/.config/web-widow/config.toml serve --http 127.0.0.1:8765
```

- `WEB_WIDOW_HTTP_TOKEN` wins over `--http-token-file`; the file must be mode
  `0600` and non-empty.
- Endpoint `http://127.0.0.1:8765/mcp`; every request needs
  `Authorization: Bearer <token>` and `Accept: application/json,
  text/event-stream`. Missing/wrong token → `401` + `WWW-Authenticate: Bearer`;
  a request without the streamable `Accept` header gets `406` (MCP clients send
  it automatically).
- A non-loopback bind is refused unless `--allow-non-loopback` is passed.
  Local HTTPS is available on the same loopback listener with
  `[http].tls = true` (see "Local HTTPS" below); multi-user auth remains
  out of scope. Remote exposure still needs a deliberate review.
- **Stateless streamable HTTP.** Every POST is served
  directly; `Mcp-Session-Id` is neither required nor issued, and stale, unknown
  or absent ids are ignored rather than answered with `404`. `initialize`
  completes normally with an `application/json` response. Only POST is served:
  `GET` and `DELETE` on `/mcp` return `405` with `Allow: POST` (no
  server-initiated messages, no resumable streams). Bearer auth, Host
  validation, the request-body cap and both-transport tool parity are unchanged.
- **Access log.** Each request logs one line on stderr with `method`, `path`,
  `status`, `session` (first 8 characters of a supplied `Mcp-Session-Id`, or
  `none`) and `duration_ms`; INFO for status ≥ 400, DEBUG otherwise. It never
  reads or logs the Authorization header, the body or the URL query. Use
  `journalctl --user -u web-widow.service | grep 'http request'` to decide
  whether a field failure presented an id and whether the server accepted it.
- The server negotiates `2026-07-28` / `2025-11-25` through rmcp; both are
  served statelessly.
- All logs stay on stderr; HTTP mode writes nothing to stdout.

### Local HTTPS

Set `[http].tls = true` to make the `--http` listener HTTPS-only on the same
`/mcp` and `/feeds` routes. TLS is strict either-or: that port is HTTPS only, no
plaintext and no downgrade. `tls = true` without `--http` is a typed
`tls_requires_http` startup error. The bearer and feed capability tokens, the
loopback policy and the access log are unchanged; only the transport changes.

```toml
[http]
tls = true
# Optional operator override; both or neither. When both are set, generated
# material under <data>/tls/ is neither created nor read.
# tls_cert_file = "/abs/path/cert.pem"   # PEM chain, leaf first
# tls_key_file  = "/abs/path/key.pem"    # PEM private key, mode 0600
```

With no override, first start generates a local CA and leaf under `<data>/tls/`
(directory `0700`):

| File | Mode | Content |
|---|---|---|
| `ca.pem` | 0644 | local CA, CN `web-widow local CA`, `CA:TRUE`, ~10 years |
| `ca-key.pem` | **0600** | CA private key, PKCS#8 ECDSA |
| `server.pem` | 0644 | leaf, SANs `localhost`/`127.0.0.1`/`::1`, ~397 days |
| `server-key.pem` | **0600** | leaf private key, PKCS#8 ECDSA |
| `meta.json` | 0644 | format, expiry timestamps, SAN list and renewal timestamps |

Generation runs under the data-directory lock and only when the set is absent.
A valid set is reused byte-identically (no churn). The leaf is re-minted by the
**same CA** at startup when it expires within 30 days, is already expired (a
clock jump forward or a long stop), is not yet valid, is missing a required SAN,
or when `server.pem`/`server-key.pem`/`meta.json` are missing. Renewal never
requires the stored leaf to still verify, and the CA bytes are preserved, so
clients do not re-import. An expired CA is a typed `tls_ca_expired` error: delete
`<data>/tls/` to re-mint, then re-import the new CA. The CA is never rotated
automatically. Unparseable, foreign or mismatched leaf material and a corrupt or
mode-widened CA are typed `tls_material_invalid` / `tls_material_incomplete`
errors and are never overwritten. `meta.json` is diagnostics only: losing it
rebuilds the leaf record instead of blocking startup. Generated material covers
loopback only; a non-loopback bind without an override is
`tls_bind_not_loopback`.

The server logs `tls="generated"` or `tls="override"` at startup; generated mode
also logs the CA path. Keys are never logged. Generated material is not encrypted
at rest: the data-directory `0700` and the key `0600` modes are the boundary. The
CA key is the sensitive file; trust the CA only in the client that connects, not
system-wide, where possible.

**Verify and trust the generated CA** (replace `<data>` with the data directory):

```sh
openssl x509 -in <data>/tls/ca.pem -noout -subject -dates -fingerprint -sha256
openssl verify -CAfile <data>/tls/ca.pem <data>/tls/server.pem
curl --cacert <data>/tls/ca.pem https://127.0.0.1:8765/feeds/changes.xml
export NODE_EXTRA_CA_CERTS=<data>/tls/ca.pem   # Node-based MCP clients
```

- **Firefox:** Settings → Privacy & Security → Certificates → View Certificates
  → Authorities → Import → `<data>/tls/ca.pem`, then tick “Trust this CA to
  identify websites”.
- **Chrome/Chromium (Linux, NSS):**
  `certutil -d sql:$HOME/.pki/nssdb -A -t "C,," -n "web-widow local CA" -i <data>/tls/ca.pem`
  (needs `nss-tools`), or import through `chrome://settings/certificates`.
- **System store (Debian/Ubuntu):** copy `ca.pem` to
  `/usr/local/share/ca-certificates/web-widow-local-ca.crt` and run
  `sudo update-ca-certificates`. On macOS, add it to the login keychain and
  mark it trusted.
- **SAN rule:** connect to `127.0.0.1`, `::1` or `localhost`; those are the only
  generated SANs. A client that cannot trust a private CA needs an operator
  override with a certificate from an already-trusted local/corporate CA.
  Do not disable certificate verification; that defeats the transport.

The advertised `feed_url` in `events` and webhook payloads is
`https://<bind>/feeds/changes.xml` while TLS is active. ALPN is HTTP/1.1 only in
v1; there is no mTLS/client-certificate authentication, no ACME, no HSM and no
background renewal timer (renewal is evaluated at startup). A process that runs
longer than the leaf lifetime must be restarted. Rollback: set `tls = false` and
restart with the previous binary/config; generated files may remain unused.

## 9. Monitoring operations

- **`run-due` is the cron entry point.** It is single-shot: opens the data dir
  (refusing a second writer), checks each due monitor sequentially through the
  same per-host gate as `monitor_check`, prints `{checked, skipped, changed,
  failed}` as one JSON line on stdout, shuts the renderer down and exits 0. A
  per-monitor fetch failure appears in `failed` and as a stored fetch event; a
  non-zero exit means the pass itself (lock, config, storage) could not run.
  Example cron: `17 * * * * /usr/local/bin/web-widow --config
  /etc/web-widow/config.toml run-due >> /var/log/web-widow-run-due.log 2>&1`.
  **Either-or:** when the serving process has `[scheduler].enabled = true`, a local
  `run-due` on the same data directory fails with the holder pid; use
  `run-due --http` (through the server) or turn the timer off. The timer and a
  local cron entry are never both a driver for one instance.
- **List monitors without an id.** `monitor_list` returns up to 500 in id order
  with `label`, `active`, `has_schema`, `interval_s`, `created_at`,
  `last_checked_at` and `next_due_at`; `include_inactive: true` includes
  tombstones. No job and no network.
- **Bound a bulk check.** `monitor_check(all: true)` returns `pending: true` and
  a job id at once. `wait_ms` (≤ `[monitor] max_wait_ms`, default 30 000) checks
  inline and sequentially, testing the deadline between monitors; if it finishes,
  `pending: false` and no job exists, otherwise the remainder is enqueued as one
  job with the remaining ids and the partial typed results are returned. One
  in-flight check may overrun by at most one check. `monitor_id` + `wait_ms` is
  `schema_invalid`.
- **Scheduler is opt-in.** The v0.1 decision chose no scheduler; v0.7
  amends it. With `[scheduler].enabled = true`
  (default false), `serve` runs a resident timer that drives the same `run_due`
  pass; `run-due` remains supported and is unchanged. One driver per instance:
  the serving process holds the data-directory lock, so a local `run-due` fails
  while it runs; `run-due --http` goes through the server, and the per-monitor
  lock + `next_due_at` re-read prevent a double check. See §9.9.
- **404s are one terminal result.** A permanent 404 is one request, one stored
  fetch event, an advanced `next_due_at`, and a `failed` entry — not a
  three-attempt crawl retry and no fabricated snapshot.
- **Monitors are per URL.** Multi-page "watch a site" semantics are later.
- **Declared-field diffs only.** A schema monitor names changed fields from the
  spec's fields; it is not a full JSON delta.
- **No bulk-check cancel.** `monitor_check(all: true)` returns a job id; a
  shutdown cancels the job cooperatively, and a crash marks it failed while the
  monitors stay due for the next `run-due` or scheduler tick.
- **Rendered re-fetch cost** is documented above. Static monitors keep the 304
  path; a rendered previous check disables conditional GET.
- **Snapshot render mode (v4).** Snapshots created before v4 have an unknown
  mode; their first post-upgrade check fetches the body and records `static`/
  `rendered`/`rendered_partial`. The 304 shortcut is only used when the previous
  snapshot exists and is exactly `static`.
- **`profiles.hash` is `spec/v1/...`.** Old unversioned cache rows are missed and
  re-fetched. Restart an in-flight pre-upgrade crawl job that carried a schema as
  a new job so its profile identity and result join stay consistent.

### 9.8 Webhook emission

Webhooks are a deliberate **outbound egress** of monitor metadata. Enabling one
is the operator's decision, and the config file is the control surface: it is not
gated by robots/SSRF because the target is operator-chosen, but no URL can enter
through a tool, a monitor, page content or a prompt.

- **Targets are config-only and named.** `[webhooks.targets.<name>]` holds `url`,
  `kind` (`generic`/`slack`/`discord`), one of `secret_env`/`secret_file`, one of
  `auth_env`/`auth_file`, and `include_diff`. Tools accept only names
  (`monitor_create.webhook_targets`, `webhook_target`); an unknown name is
  `schema_invalid`. HTTPS only unless `[webhooks].allow_http = true` (loopback/
  test); userinfo is rejected; no redirects are followed. The URL is treated as
  a secret (Slack/Discord embed a token in the path) and is never logged or
  returned — `webhook_status` shows the **host** only.
- **Enqueue is atomic with the change.** `record_check` inserts one
  `webhook_deliveries` row per attached target inside the change transaction, only
  for a real change and only when `[webhooks].enabled = true`. A monitor with no
  attachment produces nothing; a disabled process inserts nothing. The check path
  never performs a network call.
- **Delivery is a separate dispatcher.** `serve` starts it when enabled; local
  `run-due` performs one bounded flush after the check pass and exits
  (`run-due --http` is a client and relies on the server). Each pass attempts up
  to 20 due rows, sequentially, with `[webhooks].timeout_ms` and a discarded,
  capped response. 2xx → `delivered`; 408/429/5xx/timeout/DNS/TLS/connection →
  retry; any other 4xx or a redirect → `dead` immediately. After
  `[webhooks].max_attempts` the row is `dead`. Backoff is
  `[60, 300, 1800, 7200, 21600]` seconds, clamped, with deterministic ±20%
  jitter from `(delivery_id, attempts)`.
- **Signing.** When the target has a secret, every POST carries
  `X-Web-Widow-Timestamp: <unix>`,
  `X-Web-Widow-Signature: sha256=<hex HMAC-SHA256(secret, timestamp + "." + body)>`,
  `X-Web-Widow-Event: change` and `X-Web-Widow-Delivery: <id>`. A receiver
  verifies the HMAC over exactly `timestamp + "." + body` with the shared secret.
  Secrets come from env or a `0600` file, never inline TOML or the DB; a missing
  secret makes the delivery `dead` with a typed reason, not a crash.
- **Payloads.** `generic`: `{event, delivery_id, monitor:{id,label,url},
  change:{id,detected_at,kind,stats,summary,from_snapshot,to_snapshot}, feed_url,
  diff?}`; the diff is omitted unless the target sets `include_diff = true`.
  `slack`: `{text}` (≤3 000 chars); `discord`: `{content}` (≤1 900 chars). Slack/
  Discord bodies strip `<@…>`, `<!channel|here|everyone>`, `@channel|here|everyone`
  and control characters; the text is data, never markup. `stats`/`summary` come
  from the same change data the `events` feed uses.
- **Isolation.** A dead, hanging or slow target costs at most one timeout per
  attempt and never blocks a check, a monitor or another target; the check
  transaction has already committed. Tombstoning a monitor marks its pending
  deliveries `dead` and the dispatcher also refuses an inactive monitor.
- **Visibility and retention.** `webhook_status` shows configured targets (host
  only), attachments, recent rows, `pending`/`dead` counts and a bounded,
  redacted `last_error`; dead letters are visible but there is no manual retry in
  v1 — fix the target/config and let the next change deliver. Delivered rows age
  out after `[retention].webhook_deliveries_days`; dead letters after the longer
  `webhook_dead_days`; pending rows are never pruned.
- **Exfiltration disclosure.** A webhook sends the watched URL, its label, the
  change kind, stats and a short summary (and the capped diff only when opted in)
  to the configured endpoint. It can leave the host's network. A misconfigured
  target or a compromised config file is an exfiltration path for stored page
  metadata. No webhook fires for a monitor with no attached target; remove the
  target from config or set `[webhooks].enabled = false` to stop all delivery.

### 9.9 In-process scheduler (opt-in)

v0.1 chose no scheduler: `monitor_check` on demand plus a
single-shot `run-due` for cron/systemd. **v0.7 amends it** with a
resident timer that is **opt-in and off by default**; every surrounding guarantee
stays.

- **Placement and enablement.** `src/monitor/scheduler.rs`; spawned in `serve`
  (stdio or HTTP) only when `[scheduler].enabled = true`, never in `run-due`. One
  task, no daemonization, no pid file, no new tool/service.
- **Tick and bounds.** Fixed `[scheduler].tick_ms` (1 000..=600 000, default
  30 000) via `tokio::time::interval` with `MissedTickBehavior::Delay`; each pass
  calls `run_due(min([scheduler].max_checks_per_tick, [monitor].max_checks_per_call))`.
  One pass per tick, sequentially; a late tick shifts the next check rather than
  queueing extras. A long downtime leaves monitors simply due — **one** check per
  monitor, not a catch-up burst.
- **Semantics unchanged.** `run_due`/`check_one` are untouched: each check takes
  the existing per-monitor lock and re-reads `next_due_at` after acquiring it, so
  a timer pass, `monitor_check` and `run-due --http` cannot double-check one
  monitor. `next_due_at` remains the single source of truth. A failed pass is
  logged and retried on the next tick; per-monitor failures are terminal results
  in the pass summary.
- **Either-or (one driver).** The serving process holds the data-directory lock,
  so a local `run-due` on the same directory fails with
  `already in use by pid …`. `run-due --http` remains valid: it calls
  `monitor_check` through the running server, and the lock + `next_due_at` re-read
  make it a no-op for monitors the timer already checked. Choose **one** driver
  per instance: enabling the timer means removing/disabling any cron `run-due` for
  that instance; leaving it off keeps cron/systemd valid.
- **Shutdown.** The task holds a child cancellation token; SIGTERM/SIGINT/stdio
  EOF cancels it with the backfill and dispatcher and awaits it within the
  existing 10 s bound. It never outlives the store.
- **Observability.** One INFO line at spawn (`enabled`, `tick_ms`,
  `max_checks_per_tick`) and one DEBUG line per pass (`checked`, `skipped`,
  `changed`, `failed`). `monitor_list` already exposes `last_checked_at` /
  `next_due_at`; there is no new tool.

## 10. Search (DuckDuckGo discovery)

- **One SERP page per call.** DDG returns ten organic results; v0.2 has no
  pagination. A call makes at most two SERP requests: the configured html
  endpoint, then the lite endpoint only when the primary fails at transport or
  parse level. Route refusals (`blocked_by_robots`, `blocked_private_address`,
  `blocked_scheme`, `invalid_url`) are final and never fall back.
- **Politeness.** The same per-host gate applies: ≤1 request/s per host by
  default, plus any robots `Crawl-delay`. Search traffic normally uses
  `html.duckduckgo.com`; three live monitors (ids 2–4) use
  `lite.duckduckgo.com` (5 and 6 watch news sections), so they do not contend
  except on the lite fallback path.
- **Endpoints are operator config.** `[search].html_url` and
  `[search].lite_url` are validated at startup. A re-pointed endpoint with
  different markup yields `extraction_failed`; it never silently becomes another
  engine. There is no engine, region or language argument.
- **Search stores nothing.** No `pages`, `fetch_events`, `extractions`,
  `profiles`, `blobs` or `jobs` row is written for a successful or failed search.
  Only the `hosts` robots cache may change. Audit behaviour in journald:
  `journalctl --user -u web-widow.service | grep 'search completed'` shows
  `engine`, `result_count`, `duration_ms`, `truncated` — and never the query or
  the SERP URL. Do not lower `logging.level` to `debug` on a shared host if the
  caller's queries are sensitive; the query is not logged at any level by
  design, but request-level debug logs elsewhere may include URLs you supply to
  other tools.
- **Result pages are not fetched.** `search` returns candidate URLs and snippets
  only. Use `fetch_page`/`extract` for a result, and keep the same politeness and
  robots rules.
- **Failure meaning.** A real zero-result page is a success (`results: []`,
  `result_count: 0`). A 200 page with no result block and no empty marker is
  `extraction_failed` (DDG challenge/anomaly or markup drift) — never an empty
  success. Transient DDG challenge: one later retry is reasonable, never a loop;
  watch recurrence in the access log (`grep 'search failed'`/`'search completed'`)
  before treating it as a defect. A lite fallback rate near 100 % is the
   indicator that the html markup drifted.

### Field signals (no schema or data change)

- **`fetch_page` `content_warning`.** `"interstitial_suspect"` appears only when
  all four hold: status 200, the final URL drifted from the request in host or
  path, the final path contains a whole verification/login segment
  (`account-verification`, `login`, `checkpoint`, …), and `word_count < 50`. It is
  a warning (never `isError`), the body is still delivered, and it is also in the
  front matter. A direct login page and a short normal article do not warn. The
  cached path re-derives the field, so a payload stored before the upgrade still
  warns when revalidated.
- **`fetch_feed` `canonical_url`.** `url` stays verbatim (the fetch target);
  `canonical_url` is the crawl/monitor identity (tracking parameters dropped,
  query sorted). Items dedupe by canonical URL (first wins) as well as by `id`.
  Unknown parameters such as `syn` remain identity; adding one to the global
  tracking list is a separate decision with evidence.
- **`extract` envelope.** The tool-call shape is `{url, schema: {container?,
  fields}}`. A spec passed at the top level (`{url, container, fields}`) is
  rejected with a message naming that envelope; it is not silently accepted.

### `search_content` — local corpus, not discovery

`search_content` reads `pages_fts`; it never makes a network request and writes
nothing. **Index write policy:** `fetch_page`, a crawl and a monitor check replace
a page's indexed body with the delivered Markdown, while `extract` only fills a
page that has no indexed body yet — so an extract can never hide delivered page
text from local search, and the v6 backfill prefers the most recent stored
Markdown over a later record-text extraction. Audit it with
`journalctl --user -u web-widow.service | grep 'search_content completed'`: the
line carries `result_count`, `returned`, `duration_ms` and `truncated` — never the
query text. If `result_count` is unexpectedly low for a page you know was fetched,
check that the page was acquired by a *successful* fetch (`pages.outcome = 'ok'`);
a page created only by a failed fetch has no index row. If the count is low across
the board, check `PRAGMA user_version` (must be 6) and
`meta.fts_backfill_v6`; without the marker, a backfill is still pending (it runs
only at startup).

### `search_semantic` — local semantic index

`search_semantic` embeds the query once with the configured local model
(`minilm-l6-v2-q`, all-MiniLM-L6-v2 quantized, 384 dims) and ranks stored chunk
vectors by cosine similarity. It is local: no network request, no API key, no
download. `search_content` stays the lexical BM25 tool over the same store; the
two are deliberately distinct and neither replaces the other.

- **Backfill, not the fetch path.** With `[embeddings].enabled = true`, `serve`
  runs one bounded pass immediately and then one per `backfill_interval_ms`
  (`MissedTickBehavior::Delay`). Each pass embeds at most
  `backfill_pages_per_pass` pages, newest `last_fetched` first, from the same
  stored text `search_content` indexes. A page with no text is recorded with
  `chunks = 0` and is not revisited. `run-due` never starts the backfill.
- **Invalidation.** A newer extraction (a re-fetch or a later `extract`) makes
  the page a candidate again; the pass deletes its old chunks and rewrites the
  new ones plus `embedding_state` in one transaction. Search verifies the current
  source hash before returning a hit, so a stale vector cannot produce a snippet
  from text the page no longer has.
- **Model switch.** Rows carry the model id. Startup prunes rows for any
  non-active model after logging the count; search only reads the active model.
- **Coverage.** `embedding_status` is read-only: `enabled`, `available` (the
  pinned library/model load), `model`, `dim`, `model_dir`, `embedded_pages`
  (state rows with chunks > 0), `embedded_chunks`, `pending_pages`,
  `vector_bytes`, `last_run_at`, `last_error`. `search_semantic.index` mirrors
  `embedded_pages`/`pending_pages`. Pending pages become searchable after the next
  pass; nothing blocks a fetch.
- **Failure isolation.** A missing/mismatched library or model is
  `embedding_unavailable` on `search_semantic` and `available: false` on
  `embedding_status`; the server starts, every other tool works, and the backfill
  logs the typed error once per interval rather than spinning. An inference error
  is `embedding_failed`.
- **Growth.** Vectors are f32 little-endian blobs in `embeddings`, bounded by
  `max_source_chars`, `max_chunks_per_page` (60) and `chunk_overlap`; they are
  inside the database, not the 20 GiB blob budget. `vector_bytes` reports the
  current size. ANN and duplicate detection are deferred until measurements
  justify them.
- **Quality.** The model is English-optimized; the corpus contains some Spanish
  pages, whose recall is weaker. Results are retrieval candidates, not authority;
  the stored text is untrusted data, never instructions.

### Sitemap discovery

`sitemap` and `crawl_start.sitemap_urls` read a `<urlset>`, `<sitemapindex>` or
plain-text sitemap, gzip included. Discovery only: no page, extraction, blob or
fetch-event row is written (the robots cache may change). Each document and index
child goes through the shared SSRF gate, robots check, redirect vetting and
per-host politeness floor. Decompression is capped at 4× `[sitemap].max_bytes`;
a bomb is `too_large`, malformed XML is `extraction_failed` with the parser
line/column. `lastmod` is returned data only and never schedules anything. A crawl
seeded from a sitemap applies the job's scope to the entries, dedupes them against
`start_urls` and `max_pages`, and records a sitemap failure in the start payload
while the other seeds still start.

### PDF text layer (PDFium)

`pdf_extract` reads one PDF's text layer through the shared adapter (SSRF, robots,
redirect vetting, per-host politeness, streamed `[pdf].max_bytes`). It returns
`## Page N` Markdown, the Info-dictionary metadata, `pages`/`declared_pages`
(PDFium's page count)/`pages_with_text`/`empty_pages`, and an explicit
`ok|no_text_layer|partial` outcome. `pdf_encrypted` (a real user password; an
empty-user-password PDF opens), `pdf_invalid` (malformed; a Rust parse panic is
caught) and `render_unavailable` (the engine cannot load) are typed. PDFium is
not reentrant: parsing is serialized by a process-wide mutex and runs on a
blocking thread; a **native** PDFium crash is not catchable. A complete
text layer is stored (page row `render_mode: "pdf"`, raw blob = the PDF, one
extraction row) and indexed for `search_content`; a later HTML acquisition of the
same URL replaces the indexed text. `no_text_layer`/`partial` store nothing.
`fetch_page`/`extract` still refuse `application/pdf` before reading the body and
name `pdf_extract` in the error.

**Engine provisioning.** PDFium 8125 (`libpdfium.so`, BSD-3-Clause, from
pdfium-binaries) is a runtime artifact, not a crate. Ship it beside each binary
(`<exe dir>/libpdfium.so`) or under `<data_dir>/lib/libpdfium.so`, or set
`[pdf].pdfium_path`; `WEB_WIDOW_PDFIUM_PATH` is the first discovery candidate.
The binary does not link the library (`libloading`/`dlopen`), so `ldd` on the
binary does **not** list it. Verify the deployment with the library file's
SHA-256, `ldd libpdfium.so` for its own dependencies, and a `pdf_extract` smoke on
a fixture PDF that is then findable via `search_content`. The library belongs in
both units' rollback artifacts alongside the binary and the DB copy. The test
suite needs `WEB_WIDOW_PDFIUM_PATH`; without it the PDF tests print `SKIP`.

### Change feed and robots cache

`events` reads the local `changes` join (JSON or RSS 2.0); `robots_cache` reads
the `hosts` cache and probes a path from the **stored** body with zero network
requests.

**Feed route.** With `--http` and `[events].feed_enabled`, the loopback listener
serves `GET /feeds/changes.xml?token=<capability token>`. The MCP bearer is
scoped to `/mcp`; the feed has its own 256-bit token from
`WEB_WIDOW_FEED_TOKEN` (≥ 32 characters) or the `0600`
`<data_dir>/feed-token`, created from 32 random bytes on first start when
absent. Compare is constant time. The response carries
`Content-Type: application/rss+xml; charset=utf-8`, an `ETag` (body hash),
`Cache-Control: max-age=60` and 304 on `If-None-Match`; missing/wrong token is
401 and a disabled feed is 404. **Rotate** by replacing the file (or the env
value) and restarting; the route is read-only and makes no outbound request.
The access log records `uri().path()` only, so the token in the query is never
journaled. **Loopback-only, enforced:** `main` refuses to start when the
`--http` bind is non-loopback and `[events].feed_enabled` is true (before any
token is resolved or written); for an MCP-only network deployment set
`[events].feed_enabled = false`. `http::serve` re-checks the bound address and
returns an error (dropping the listener) if a library caller bypasses `main`.

**Robots cache.** `robots_cache` lists `hosts` rows with expiry/status/failures
and probes a path with the cached body: `source` is `cache`, `expired` (still
parsed) or `unavailable` (missing/invalid body; `allowed` is null). The tool
must never call the fetching robots service; its test asserts a fixture server
sees zero requests.

## 11. Intended use and risk statements

**Intended use.** Internal agent tooling over publicly accessible pages
(docs, news, reference sites); robots.txt respected; ≤1 request/s per host.
Authenticated fetching is an **operator-configured, per-site opt-in** path (§12): a
secret reference for bearer/basic/header profiles, or an operator-captured
`kind = "session"` login for session-gated sites. The agent names a profile on
`fetch_page` only, and the body is return-only unless `[auth].persist` is set. The
server adds no paywall or access-control bypass; the operator must be entitled to
the access it supplies.

**Readability omission.** `mode=main` can omit whole sibling content
blocks (readability limitation). v0.4 repairs the **section-shaped** class (the
sqlite.org/whentouse.html case): a gap between delivered blocks is restored only
when it begins with an undelivered section heading that is a sibling (same
parent, no deeper level) of an adjacent delivered heading and is followed by
content, within a 25 % per-page bound that otherwise skips the whole repair.
Heading-less runs — card copy, related teasers, complementary notes — are never
admitted, and pages without such a gap keep byte-identical output. Use
`mode=full` or `mode=text` when
completeness matters. The operator run observed **no** whole article-sibling
omission in a 10-page docs/news/reference sample; navigation, sidebars and
footers were excluded as intended. Two adjacent observations are recorded rather
than folded into the risk statement: python.org/about serves a JavaScript-fallback shell to
both static and headless clients (site behavior; About content not delivered),
and a broad `h2` schema selector on mdBook pages can pick theme UI — scope docs
schemas with `container`. No mitigation is claimed beyond `mode=full`/`text` and
schema scoping.

**Residual rendering risk.** The SSRF gate and robots policy cover
the top-level navigation only. Rendered pages execute untrusted JavaScript and
may load subresources outside that gate; iframes/popups are not traversed.

## 12. Authenticated fetching

Credentials are **operator configuration**, never tool input. A reference
(`op://…`, `bw://…`, `pass://…`) lives only in the 0600 config file; the agent
names a **configured profile**, and only through `fetch_page.profile`. No other
tool accepts a credential-bearing argument: `profile`, `auth_profile`,
`reference`, `username_reference`, `password_reference`, `header`, `credential`,
`authorization` and `cookie` are refused with `schema_invalid` before dispatch,
with no fetch and no side effect.

### 12.1 Configuration

```toml
[auth]
audit_days = 90                  # auth_audit pruning, 1..=3650 (default 90)
manager_timeout_ms = 5000        # hard per-resolution timeout, 100..=60000
allow_loopback_http = false      # plain HTTP for literal loopback addresses only
persist = false # v0.9: store authenticated bodies (return-only when false)
index_authenticated = false      # include stored ones in FTS + embeddings
events_authenticated = false     # include events derived from them in the feed
page_retention_days = 7          # retention for stored authenticated pages, 1..=3650

[auth.managers.op]               # one entry per provider you reference
path = "/usr/bin/op"             # absolute; PATH is never searched
env_allow = ["OP_SERVICE_ACCOUNT_TOKEN"]   # names only, read from the service env

[auth.managers.bw]
path = "/usr/bin/bw"
env_allow = ["BW_SESSION"]

[auth.managers.pass]
path = "/usr/bin/pass"
env_allow = []

[auth.profiles.github]           # bearer (default) | basic | header
kind = "bearer"
host = "github.com"              # lowercase bare host; no scheme, port or path
port = 443                       # exact credential origin port, 1..=65535 (default 443)
reference = "op://Private/github/token"
allow_subdomains = false         # subdomains are reachable; credentials stay exact-origin

[auth.profiles.legacy]
kind = "basic"
host = "legacy.example.com"
username_reference = "op://Private/legacy/username"
password_reference = "op://Private/legacy/password"

[auth.profiles.internal]
kind = "header"
host = "internal.example.com"
header = "X-Api-Key"
reference = "op://Private/internal/key"
```

- **Kinds.** `bearer` sends `Authorization: Bearer <value>`; `basic` sends
  `Authorization: Basic base64(user:password)` built in memory from **two
  references** (a literal username is a config error); `header` sends the exact
  operator-written header name with the value. `Host`, `Cookie`,
  `Content-Length`, `Transfer-Encoding`, `Connection`, `Proxy-*` and any name
  containing CR, LF, NUL, space or a colon are refused at load.
- **References.** `op://<vault>/<item>/<field>` (exactly three segments),
  `bw://<item>[/<field>]` (one or two), `pass://<store>/<relative/path>` (two or
  more). Segments are `[A-Za-z0-9._-]{1,64}` with no leading `-`, no `.`/`..`,
  no absolute path, no whitespace or control character; the whole reference is
  at most 256 bytes. The scheme selects the provider binary and cannot cross
  providers, so a referenced provider must have an `[auth.managers.<name>]`
  entry.
- **Managers.** Every declared manager is validated at load: the path must be
  absolute, an existing regular file and executable. `env_allow` names the only
  variables the child sees in addition to `HOME` (passed when the service sets it); their **values** are read from
  the service environment at invocation. `PATH` is refused (the child runs with a
  cleared environment; a POSIX shell may install its own default), and so is any
  `WEB_WIDOW_*` name, so the server's own tokens can never reach a provider.
  The child gets `stdin` = null, its stderr is captured and never logged, stdout
  is capped at 8 KiB, and one resolution is attempted per fetch under
  `manager_timeout_ms`.
- **HTTPS only.** An authenticated fetch must be `https`. `allow_loopback_http =
  true` re-admits plain HTTP for the **literal** addresses `127.0.0.1` and `::1`
  only; every name, including `localhost` and anything that merely resolves to
  loopback, stays refused (`auth_insecure_url`). It never widens a non-loopback
  destination. This is for local probes, not a production posture.
- **Exact origin.** A credential attaches only to the configured
  `host:port` pair. `port` defaults to `443` and must be `1..=65535`; a URL to
  the bound host on any other port is `egress_blocked` with reason
  `credential`, decided before the secret is resolved. `allow_subdomains`
  widens **reachability** for the authenticated browser egress policy only — it
  never widens credential delivery, and a credential-bearing request to a
  subdomain is `egress_blocked`, not `auth_host_not_allowed`. A site that
  serves a subdomain from its own credentials needs its own profile.
- **Error channel.** Configuration is validated at startup; a malformed entry is
  a load error naming the profile and field, surfaced under the config
  validation channel (`invalid url: …`), before any listener or tool exists.

### 12.2 What an authenticated fetch does

`fetch_page` with `profile` is **return-only by default**: the authenticated body
is delivered to the caller and nothing is cached, indexed or stored. There is no
`pages`/`extractions` row, no blob/CAS write, no FTS or embedding, no
`corpus_export` trace, no event, no webhook. `cached` is always `false`, the
payload carries `authenticated: true` and the profile name, and a requested
freshness window is ignored.

`[auth].persist = true` opts in to storing the body: the page is written with
`authenticated = 1` and `auth_site` (the profile name) and the extraction under
`auth:<site>`. The stored authenticated page is **extraction-only** — it has no
`blobs` row and `raw_hash` is `NULL`, so the body deliberately lives in the
extraction JSON and the store invariant "a `blobs` row implies its CAS file"
holds. Stored pages are excluded from `corpus_export`, the served feed and
webhooks unconditionally, and from FTS/embeddings and events unless the
corresponding `[auth]` flag is set (§12.8).

**Header/bearer/basic/header profiles** stay static: `render = always` with such
a profile is `auth_render_unsupported`, and `render = auto` is served statically
with `render_skipped: "auth_static_only"`. **`kind = "session"` profiles** render
through the authenticated browser (`render = always` included) — §12.8.

Robots is fetched **without** credentials and the SSRF/private-address gate still
applies; the profile's host binding narrows where the credential may be attached,
and does not widen the global `fetch.allow_hosts`. A redirect that resolves to a
host outside the binding is refused — `auth_redirect_blocked` for header
profiles, `egress_blocked` (reason `credential`) for a session cookie — with no
credential on the hop and no unauthenticated follow. Header-profile SSO/login
redirects are therefore unsupported in v1; a session profile is how an SSO site
is used.

### 12.3 The audit row and its retention

Every `fetch_page` call that names a valid profile writes **one value-free row**
to `auth_audit` (success or failure). A malformed profile name (not
`^[A-Za-z0-9._-]{1,64}$`) is `schema_invalid` before any work and writes no row.
The columns are:

| Column | Meaning |
|---|---|
| `id` | row id |
| `at` | RFC3339 UTC timestamp |
| `site` | the configured profile name |
| `auth_id` | the binding id (the canonical bound host); empty when the profile is unknown |
| `method` | `GET` (both the static and the authenticated-browser paths; the browser refuses a non-GET/HEAD request as `auth_method_refused` before it reaches the origin, and the audit row records `egress_blocked`) |
| `host` | the canonical host(s) that received the credential on this attempt, comma-separated in hop order (deduplicated); the requested host when the attempt was refused before any credential-bearing request. No path, no port, no query |
| `status` | HTTP status when a response existed, else `NULL` |
| `bytes` | response body bytes observed when known, else `NULL` |
| `outcome` | the vocabulary below |
| `resolution_ms` | time spent resolving the credential (0 when none ran) |

No reference, header value, cookie, URL path, query, body or client identifier
is recorded. Rows are pruned by the normal retention pass once they are older
than `[auth].audit_days` (default 90; bounds 1..=3650); the pass is idempotent
and reports `auth_audit_pruned`.

`outcome` is a closed vocabulary:

`ok` · `auth_rejected` (401/403 on the authenticated response) ·
`auth_redirect_blocked` · `auth_secret_unavailable` · `auth_manager_timeout` ·
`auth_insecure_url` · `auth_host_not_allowed` · `auth_profile_unknown` ·
`robots_blocked` (robots refused the destination before any credential was
resolved — distinct from a transport failure) · `egress_blocked` (the egress
policy refused a request; also the row for a non-GET/HEAD attempt) ·
`transport_error`.

The v0.9 session additions are value-free too: `session_expired` (the jar was
expired, or the origin answered 401/403, or it redirected to the site's login
page — the jar is destroyed), `session_missing` (no sealed jar for the profile),
`challenge_refused` (the origin asked for an HTTP credential and the browser
answered with a failure,), and `auth_store_invalid` (the sidecar's host does
not match the configured binding host; nothing is fetched).

Two tool-error codes deliberately fold into `auth_secret_unavailable` because the
audit vocabulary is closed: `auth_manager_failed` (the provider could not be
started, or its stdout was over the cap) and `auth_header_rejected` (the resolved
value cannot be put in a header — refused before the request, never repaired
into an empty header). The tool error keeps the precise code; only the audit row
is coarser.

A `render = always` call with a **header/bearer/basic/header** profile is refused
**before** any credential or binding work, so it writes no audit row: no
credential could have been selected. With a **session** profile, `render =
always` is supported and writes its normal row (§12.8). An audit write that fails
is a **warning**, never a failed fetch: the body is still returned and the
failure appears on stderr (`could not record the auth audit row`).

### 12.4 Operator runbook

Inspect the trail with any SQLite client against `<data>/web-widow.db` (read-only
is enough; the server may be running):

```sql
-- what has been used in the last week, and how it ended
SELECT at, site, host, status, bytes, outcome, resolution_ms
FROM auth_audit
WHERE at >= strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-7 days')
ORDER BY at DESC
LIMIT 100;

-- refusals only (no credential was sent)
SELECT outcome, site, host, count(*) AS attempts
FROM auth_audit
WHERE outcome NOT IN ('ok', 'auth_rejected', 'transport_error')
GROUP BY outcome, site, host
ORDER BY attempts DESC;

-- provider health: resolution latency and timeouts
SELECT site, count(*) AS attempts,
       sum(outcome = 'auth_manager_timeout') AS timeouts,
       max(resolution_ms) AS slowest_ms
FROM auth_audit
GROUP BY site;
```

`GROUP BY host` is deliberately coarse: `host` is the comma-separated hop list, so
a redirect chain is one group. For a per-hop count, split the list in SQL (for
example with a recursive CTE over `instr`); for the operator's usual question —
which profile reached which destination — the list itself is the answer.

A profile that never appears when the agent uses it means the argument was
refused before dispatch (`schema_invalid`); a profile with only
`auth_profile_unknown` rows means the agent used a name the config does not
define. Errors carry the code and never the value: `auth_secret_unavailable`
means the provider failed (locked vault, missing binary at runtime, empty or
non-UTF-8 output), `auth_manager_timeout` means it exceeded
`manager_timeout_ms`, `auth_rejected` means the origin answered 401/403 with the
configured credential.

### 12.5 Disclosure

Authenticated fetching is **per-site opt-in** and the operator's responsibility:
the operator supplies the reference, the provider credentials and the binding
host, and must have the right to use that credential against that site. Respect
each site's terms of service and robots policy (robots is still fetched and
enforced for the requested document, and per hop on the static path; a rendered
page's own subresources are outside robots — §12.8), do not use it to bypass
access controls, and prefer a
dedicated read-only token with the narrowest scope. By default an authenticated
body is returned to the calling agent only and is **not** persisted; with
`[auth].persist = true` it is stored **labelled** and excluded from
`corpus_export`, the served feed and webhooks (and, by default, from
FTS/embeddings/events) per §12.8. The agent's own handling of that content is
outside this server's control.

### 12.6 Sealed session store

The store seals one **per-site jar** — cookies under a 32-byte key held in the
OS keyring, the site name as AEAD associated data — and is consumed by the
assisted login, which writes it after a verified capture (§12.7), and by
authenticated reuse, which unseals it per fetch (§12.8). It holds no plaintext:
a jar exists only after a successful capture, and `logout` removes both files
and the key.

**Layout.** `<data>/auth/` is `0700` and is created on the first seal.

| Path | Mode | Content |
|---|---|---|
| `<data>/auth/<site>.sess` | `0600` | `WWS1` · 24-byte nonce · XChaCha20-Poly1305 ciphertext (tag appended) |
| `<data>/auth/<site>.meta` | `0600` | value-free JSON: `captured_at`, `expires_at_min`, `cookie_count`, `hosts`, `verify_url` |

`<site>` is the profile name. The AEAD **associated data is the site name**, so a
jar copied to another site name does not authenticate; a tampered byte, a wrong
key or a missing key is a typed refusal, never a plaintext fallback.

**Key.** The 32-byte per-site data key lives only in the OS keyring — Secret
Service on Linux — with service `web-widow` and account `session/<site>` (hex).
Nothing that can decrypt a jar is written under the data directory. A new seal
**rotates** the key and replaces both files atomically (temp file `0600`, fsync,
rename); a reader sees the old jar or the new one, never a partial file. The key
is stored *before* any file is created, so an unavailable or locked keyring
leaves nothing behind.

**Fail closed.**

| Code | Meaning | Remedy |
|---|---|---|
| `auth_store_unavailable` | no reachable credential store (no session bus, no Secret Service provider, a jar without its key, a jar that does not authenticate) | start a session bus with an unlocked collection; `auth logout <site>` then re-capture |
| `auth_store_locked` | the collection exists but is locked | unlock the keyring, then retry |

The service units inherit `DBUS_SESSION_BUS_ADDRESS` from the user manager; a
host without a session bus is a typed refusal, recorded as such (the operator
runbook shows both the healthy path and the refusal).

**CLI (takes no data-dir lock).** `auth` branches before the data directory is
opened, so it runs while a `serve` process owns `<data>`:

```sh
# presence, cookie count and expiry for every sealed site (no values)
web-widow --config config.toml --data-dir /srv/web-widow auth status
# {"sites":[{"site":"github","present":true,"key_present":true,"cookie_count":12,
#            "captured_at":1800000000,"expires_at_min":1800003600,"expired":false,
#            "hosts":["github.com"],"verify_url":"https://github.com/settings"}]}

# one site, or a site that is not sealed yet
web-widow --data-dir /srv/web-widow auth status github

# delete the jar, its sidecar and its keyring key (the file is removed first,
# so a keyring failure still leaves no usable session)
web-widow --data-dir /srv/web-widow auth logout github
# {"site":"github","removed":true}
```

`stdout` is one JSON object (like `run-due`) and carries no cookie, key or
reference. A failure exits non-zero with `code: message` on stderr. `status`
consults the keyring only for a site that has a jar, so it works on a host with
no session bus as long as nothing is sealed.

## 13. Troubleshooting

| Symptom | Check |
|---|---|
| `data directory … accessible by group/other` | `chmod 700` the data dir |
| `data directory … already in use by pid N` | a server (or a hung process) holds the lock; stop it, or `ps -p N`; do not delete `server.lock` |
| `render_unavailable` | pin `render.chrome_path`, check it is executable, or build with the `render` feature |
| `blocked_by_robots` | expected; robots is enforced, `robots.enabled=false` is a loud operator override |
| `blocked_private_address` | expected; add an exact host/CIDR to `fetch.allow_hosts` only for a deliberate local target |
| `timeout` / `too_large` | raise the specific cap or retry; the caps are per request and enforced while streaming |
| `sitemap` `too_large` | the gzip ceiling is 4× `[sitemap].max_bytes`; raise it deliberately, and check the document is a sitemap rather than a binary |
| `pdf_extract` `render_unavailable` | PDFium is not loadable: check `[pdf].pdfium_path`, `pdfium_discovery`, `WEB_WIDOW_PDFIUM_PATH`, `<exe dir>/libpdfium.so`, `<data_dir>/lib/libpdfium.so`; the message lists what was tried |
| `pdf_encrypted` | the PDF needs a real user password; `pdf_extract` never guesses (a separate decrypt step is not implemented) |
| `pdf_invalid` | the body is not a parseable PDF (or is a PDF with a content-type mismatch); check `%PDF-`/the origin, not a retry loop |
| `pdf_extract` `no_text_layer` | the PDF is image-only; that is a successful call, OCR is out of scope |
| feed `401` | the request lacks the exact `?token=` capability token; check `WEB_WIDOW_FEED_TOKEN` or `<data_dir>/feed-token` (it is not the MCP bearer) |
| feed `404` | `[events].feed_enabled = false`, or the binary was built without the `http` feature / was not started with `--http` |
| `robots_cache` `unavailable` | the host has no cached body (a failure row) or the body is unparseable; the tool never fetches one |
| HTTP `401` | the request lacks the exact `Authorization: Bearer <token>`; check env/file and mode `0600` |
| HTTP bind refused | non-loopback needs `--allow-non-loopback`; keep it loopback and proxy TLS externally |
| `run-due` non-zero | read stderr: lock, config, storage, or the renderer; per-monitor failures are in the JSON `failed` list, not the exit code |
| High disk use | lower `retention.max_blob_bytes`; referenced blobs are retained by design; `VACUUM INTO` then replace the DB to compact |

## 14. Uninstall

Stop the server (SIGTERM) and remove the data directory:

```sh
rm -rf ~/.local/share/web-widow
```

Remove the binary from `PATH` and the config/token files if no longer needed.
This deletes all fetched content, page history, snapshots and changes; there is
no remote copy unless you made one.

### 12.7 Assisted login and capture

`kind = "session"` is a profile whose credential is a **cookie jar the operator
captures by logging in themselves**. It is an operator command, never a tool:
`auth login` refuses without a terminal, and no MCP tool name can reach it.

```toml
[auth.profiles.github]
kind = "session"
host = "github.com"                         # the binding host (bare, lowercase)
port = 443                                  # exact credential origin port
login_url = "https://github.com/login"      # https only
login_origins = ["github.com", "accounts.google.com"]   # exact hosts the window may visit
verify_url = "https://github.com/"          # https, on the binding host
assets = ["github.githubassets.com"]        # exact egress hosts for the later browser path
session_max_age_days = 30                   # 1..=365 (default 30)
```

Validation refuses: a session profile with `reference`/`username_reference`/
`password_reference`/`header`; a non-session profile with any session field; a
non-https `login_url`/`verify_url`; userinfo or a fragment in either URL; a
`verify_url` off the binding host; an empty `login_origins`; an origin that is
not a bare exact host (no wildcard, port or path); and a lifetime outside
`1..=365`. `login_origins` must contain the binding host, because capture is
refused unless the window ends there.

**What `auth login <profile>` does**

1. Refuses without a TTY (`auth_login_not_terminal`), before anything else.
2. Probes the keyring **before** opening a window (`auth_store_unavailable` /
   `auth_store_locked`, and no profile directory is created).
3. Launches a **dedicated, headed** Chrome with `--remote-debugging-pipe` — no
   `--remote-debugging-port`, no `DevToolsActivePort`, no TCP DevTools listener —
   and a temp profile `<data>/auth/login/<site>/` at `0700`.
4. Gates **documents**: a navigation to a host outside `login_origins` is
   refused at the request stage, recorded, and the window is closed
   (`auth_login_origin_refused`). HTTP auth challenges are answered with a
   failure, never a credential.
5. The operator logs in (including 2FA). web-widow never types, clicks or fills.
   On Enter it prints the **final origin** only (never a path or query) and
   refuses capture unless that origin is the binding host.
6. Captures only cookies whose domain is the **binding host exactly** — no IdP,
   no parent domain, no subdomain widening — with `Secure`/`HttpOnly`/`SameSite`/
   expiry/path preserved.
7. Verifies **before sealing**: a plain `GET` of `verify_url` carrying the
   captured cookies must return 2xx (redirects not followed). Otherwise it
   refuses with `auth_verify_failed` and seals nothing.
8. Seals through the store (`§12.6`), wipes the profile even on refusal, and
   prints a value-free JSON summary (site, host, cookie count, expiry, verify
   status, browser isolation facts).

```sh
web-widow --config config.toml --data-dir /srv/web-widow auth login github
# {"site":"github","host":"github.com","cookie_count":14,"expires_at_min":1800003600,
#  "verify_status":200,"refused_navigations":[],"profile_wiped":true,
#  "browser":{"pid":1234,"pids":[...],"tcp_listeners":[],"devtools_active_port_absent":true}}
```

The login path takes **no data-dir lock** and writes **no `auth_audit` row**;
session use and expiry are audited by the authenticated fetch path (§12.8). A
profile that survives capture is a hard error, not a warning.

`kind = "session"` is refused by the request-header resolver
(`auth_session_unsupported`), so a session profile can never leak a header; the
sealed jar is used only by the authenticated fetch path (§12.8).

### 12.8 Authenticated reuse

A `kind = "session"` profile pairs the sealed jar from §12.7 with an
**authenticated browser** and a **static cookie path**. The credential is used,
never re-captured: expiry, a 401/403, or a redirect to the site's own login page
means `auth_session_expired` and the jar is destroyed — there is no fallback to
an unauthenticated fetch.

**The authenticated browser** is a dedicated headless process launched over the
DevTools pipe (no TCP DevTools endpoint, no `DevToolsActivePort`), with:

- an **ephemeral browser context per fetch**, disposed afterwards, and a temp
  profile wiped at exit — nothing about the session survives a fetch;
- cookies injected **host-only for the binding host only** (captured attributes
  preserved); a jar entry for any other domain is refused at injection;
- the **egress policy on every request** — document and subresource, in every
  attached target — before the request continues, plus the credential predicate
  for any request that carries a cookie or `Authorization`;
- a **loopback filtering proxy** below the renderer (`--proxy-server` with
  `--proxy-bypass-list=<-loopback>`) applying the same policy to every request
  and `CONNECT` tunnel. The proxy is the enforcement layer for the channels the
  Fetch domain cannot see (a `ws://` handshake is refused at `CONNECT`);
- `Fetch.handleAuthRequests(true)`: any HTTP auth challenge is answered with a
  failure and the fetch returns `auth_challenge_refused`. A credential is
  never supplied to a challenge.

**Read-only scope.** The authenticated browser allows **GET and HEAD only**;
any other method is refused as `auth_method_refused` before the request reaches
the origin (the audit row records `egress_blocked`), and the static paths issue
`GET` only. No authenticated tool exposes a method, a body or any other mutation
authority; adding one would be a separate, explicitly decided change, not a
configuration change.

**Document vetting and its honest edge.** Before the browser is launched,
the session render runs the same top-level vet as an unauthenticated render —
SSRF address vetting, then robots.txt, then the per-host politeness lease — so a
robots-disallowed or private-address target is refused with no browser and no
profile created. That gate covers the **requested document**; a rendered page's
own subresources are outside robots (they are still subject to the egress policy
above). This is the same disclosed render residual as the unauthenticated
render path, not a claim
that subresources are robots-checked.

**WebRTC.** The launch carries
`--webrtc-ip-handling-policy=disable_non_proxied_udp`. **The `force-` spelling
(`--force-webrtc-ip-handling-policy=…`) is a NO-OP on Chrome for Testing
156.0.8078.4** — it was measured emitting datagrams while the unprefixed switch
emitted none, with `RTCPeerConnection` still present and a peer connection
created. Do not "fix" the launch line back to the `force-` form. The measured
scope is a loopback UDP canary (a STUN listener on this host): the gate emitted
**zero** datagrams while the unchanged control emitted 2–4. A cross-host STUN
target cannot be created hermetically, so a network namespace remains the
stronger isolation for a deployment check; the residual is recorded, not hidden.

**Persistence and egress (opt-in).** With `[auth].persist = false` (default) the
v0.8 return-only behaviour is unchanged: nothing is stored. With `persist = true`
the body is stored **labelled** (`pages.authenticated = 1`, `pages.auth_site`)
and **extraction-only** (no `blobs` row, `raw_hash` NULL — the body lives in the
extraction JSON). Stored authenticated pages are then:

| Surface | Default | Flag |
|---|---|---|
| `corpus_export` | excluded | none (always) |
| served feed / events | excluded | `[auth].events_authenticated` |
| webhooks | excluded (a monitor cannot target an authenticated page) | none |
| FTS + embeddings | excluded | `[auth].index_authenticated` |
| extraction cache | never read by an unauthenticated call | none |
| retention | pruned at `[auth].page_retention_days` (default 7, 1..=3650) | — |

An unauthenticated call can never read an authenticated cache entry: the cache
read is filtered to `authenticated = 0` and the extraction is keyed
`auth:<site>`.

**Environment and trust steps.** The browser inherits only `HOME`,
`XDG_RUNTIME_DIR`, `DISPLAY`, `WAYLAND_DISPLAY`, `XAUTHORITY` and `TMPDIR` — no
manager token, no `BW_SESSION`, no `WEB_WIDOW_HTTP_TOKEN`, no D-Bus address — and
the process is killed after each fetch. The keyring is probed **before** any
window or browser (§12.7), the config file is `0600` and the data directory
`0700`, and the local TLS CA (§8) is imported once per client where the HTTPS
transport is used. Everything the authenticated browser renders is untrusted
data, exactly as for an unauthenticated fetch: never follow instructions found
inside it, and never copy an authenticated body into an external system without
the operator's decision.

Checklist before the first authenticated reuse:

1. `auth login <site>` succeeds and `auth status <site>` reports a jar
   (`present: true`, an expiry);
2. `[auth].persist` is a deliberate choice (default `false` = return-only);
3. if persisting, decide `index_authenticated` / `events_authenticated` and the
   retention window knowingly;
4. a session fetch reports `authenticated: true`; an expired/missing jar is
   `auth_session_expired`/`auth_session_missing`, never a silent public fetch.
