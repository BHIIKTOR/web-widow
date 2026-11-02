# Install web-widow for an AI agent

web-widow ships as two paired pieces:

1. an **MCP server** that fetches, extracts, crawls, stores and monitors web pages; and
2. a **skill** (`skill/web-widow/SKILL.md`) that teaches an agent how to drive that server well.

Install both. The skill assumes the server's tools are connected; the server enforces robots.txt, the
private-address gate, rate limits and caps regardless of what the skill says.

## 1. Build the server

Prerequisites: Git, a C toolchain, and Rust. The repository pins its toolchain in `rust-toolchain.toml`
(currently 1.99.1), so `rustup` selects it automatically.

```sh
git clone https://github.com/BHIIKTOR/web-widow.git
cd web-widow
cargo build --release --features http
mkdir -p ~/.local/bin
install -m 755 target/release/web-widow ~/.local/bin/web-widow
~/.local/bin/web-widow --help
```

- `--features http` adds the loopback HTTP transport and keeps JavaScript rendering enabled.
- `cargo build --release` alone is stdio-only.
- `cargo build --release --no-default-features` is a static-only build: no Chrome and no renderer.
- Add `--features http` to the static-only build if you need the HTTP transport without Chrome.

A later plain `cargo build --release` overwrites `target/release/web-widow` with a binary that lacks the
HTTP feature. Rebuild with the feature and reinstall before starting an HTTP unit.

## 2. Configure

The binary runs with built-in defaults, but the defaults use a placeholder contact address. Make the
configuration yours:

```sh
install -d -m 700 ~/.config/web-widow
install -m 600 config.example.toml ~/.config/web-widow/config.toml
```

Edit `~/.config/web-widow/config.toml` and set at least:

```toml
[fetch]
contact = "you@example.com"   # real, monitored; becomes part of the User-Agent
```

Keep the config `0600`. The data directory defaults to `$XDG_DATA_HOME/web-widow` (usually
`~/.local/share/web-widow`) and must be `0700`; create it explicitly if your environment does not:

```sh
install -d -m 700 "${XDG_DATA_HOME:-$HOME/.local/share}/web-widow"
```

One process per data directory. Do not share a data directory between a stdio server and an HTTP server.

### Optional runtime dependencies

All three are loaded at runtime and only affect their own tools; the rest of the server works without
them. A missing dependency produces a typed error (`render_unavailable`, `embedding_unavailable`), not a
crash.

**JavaScript rendering** — install Chrome or Chromium and point the config at it:

```toml
[render]
chrome_path = "/usr/bin/chromium"   # config file only; never a tool argument
```

When `chrome_path` is unset the server searches `$CHROME_PATH`, then well-known locations. The
`WEB_WIDOW_CHROME` environment variable used by the test suite is not a substitute for `chrome_path` in
a served instance.

**PDF text** — install PDFium (`libpdfium.so`, release 8125) and either set:

```toml
[pdf]
pdfium_path = "/usr/local/lib/web-widow/libpdfium.so"
```

or export `WEB_WIDOW_PDFIUM_PATH=/path/to/libpdfium.so`. The binary loads it with `libloading`, so `ldd`
will not list it. Without PDFium, `pdf_extract` returns `render_unavailable`.

**Local semantic search** — install ONNX Runtime 1.31.0 (`libonnxruntime.so`) and the pinned
`minilm-l6-v2-q` model files, then enable the feature:

```toml
[embeddings]
enabled = true
# model_dir = "/abs/path"          # default: <data_dir>/models/minilm-l6-v2-q
# ort_dylib_path = "/abs/path"     # unset: discovery
```

Export `WEB_WIDOW_ORT_PATH` (or `ORT_DYLIB_PATH`) when you prefer environment discovery for the
library; set `[embeddings].model_dir` for the model directory. The test suite uses `WEB_WIDOW_MODEL_DIR`,
but the served binary does not read it. Every model file is checked against a built-in SHA-256 manifest;
a mismatch is `embedding_unavailable`. See `docs/operations.md` for the artifact list.

## 3. Run it

### stdio (default)

In stdio mode the MCP client starts the process and talks JSON-RPC over stdin/stdout. Do not run it
interactively and expect a prompt; stdout is protocol traffic and logs go to stderr.

```sh
web-widow --config ~/.config/web-widow/config.toml serve
```

### Loopback HTTP

HTTP is for long-lived clients that prefer a URL over a spawned child. It requires a bearer token and
a loopback bind address; a non-loopback bind is refused unless `--allow-non-loopback` is also passed.

```sh
umask 077
openssl rand -hex 32 > ~/.config/web-widow/http-token
chmod 600 ~/.config/web-widow/http-token

WEB_WIDOW_HTTP_TOKEN="$(cat ~/.config/web-widow/http-token)" \
  web-widow --config ~/.config/web-widow/config.toml serve --http 127.0.0.1:8765
```

You can pass `--http-token-file ~/.config/web-widow/http-token` instead of the environment variable.
Verify the listener:

```sh
curl -sS http://127.0.0.1:8765/mcp \
  -H "Authorization: Bearer $(cat ~/.config/web-widow/http-token)" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"curl","version":"1"}}}'
```

A missing `Accept` header answers `406`; a missing or wrong token answers `401`. The transport is
stateless: no `Mcp-Session-Id`, POST only.

### Scheduled monitor checks

Monitors are due-based. Check every due monitor once from cron or a systemd timer:

```sh
web-widow --config ~/.config/web-widow/config.toml run-due
```

If the server is already running, go through it instead of opening the data directory:

```sh
WEB_WIDOW_HTTP_TOKEN="$(cat ~/.config/web-widow/http-token)" \
  web-widow --config ~/.config/web-widow/config.toml \
  run-due --http http://127.0.0.1:8765/mcp
```

One JSON summary is printed on stdout. Alternatively enable the opt-in in-process timer
(`[scheduler].enabled = true`) and use exactly one driver per instance — never both cron and the timer.

## 4. Register the MCP server in your client

The universal shape is a name plus either a command (stdio) or a URL (HTTP). Key names differ slightly
between clients; the values below are the stable part.

**stdio, Claude-style `mcpServers`:**

```json
{
  "mcpServers": {
    "web-widow": {
      "command": "/home/you/.local/bin/web-widow",
      "args": ["--config", "/home/you/.config/web-widow/config.toml", "serve"],
      "env": {}
    }
  }
}
```

**HTTP (clients that support remote HTTP; some require an explicit `"type": "http"` field):**

```json
{
  "mcpServers": {
    "web-widow": {
      "url": "http://127.0.0.1:8765/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

**opencode** (`opencode.jsonc`). V2 puts named servers under `mcp.servers`:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "servers": {
      "web-widow": {
        "type": "local",
        "command": ["/home/you/.local/bin/web-widow", "--config", "/home/you/.config/web-widow/config.toml", "serve"]
      }
    }
  }
}
```

Remote HTTP, with OAuth disabled and the bearer taken from OpenCode's environment:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "servers": {
      "web-widow": {
        "type": "remote",
        "url": "http://127.0.0.1:8765/mcp",
        "oauth": false,
        "headers": { "Authorization": "Bearer {env:WEB_WIDOW_HTTP_TOKEN}" }
      }
    }
  }
}
```

The CLI can add the local server for you:

```sh
opencode mcp add web-widow --global -- ~/.local/bin/web-widow --config ~/.config/web-widow/config.toml serve
opencode mcp list
```

OpenCode substitutes `{env:NAME}` from its own process environment, so export `WEB_WIDOW_HTTP_TOKEN`
where OpenCode starts, or paste the literal token and keep the client config `0600`. Remote servers use
OAuth by default; `oauth: false` is what makes the bearer header the credential.

Use absolute paths: clients often launch servers with a different working directory and `PATH`. Never
commit a literal bearer token; prefer the client's environment substitution.

## 5. Install the skill

The repository keeps the skill at `skill/web-widow/SKILL.md`. Install it as a directory named
`web-widow` containing that file.

**opencode** (global):

```sh
mkdir -p ~/.config/opencode/skills
rm -rf ~/.config/opencode/skills/web-widow
cp -r skill/web-widow ~/.config/opencode/skills/
test -f ~/.config/opencode/skills/web-widow/SKILL.md && echo "skill installed"
```

**Other Agent Skills clients** use the same convention with a different root (for example
`~/.claude/skills/web-widow/` for Claude Code, which OpenCode also reads as a compatibility source).
Copy the `skill/web-widow/` directory there and keep the directory name `web-widow`; the only required file is
`SKILL.md`. The skill ID comes from the directory name, not from the frontmatter `name`.

**Project-local instead of user-global:** copy `skill/web-widow/` into `<project>/.opencode/skills/` when
the skill should apply to one repository only.

Reload the client after installing (`/reload` in OpenCode; config and skill changes are watched). Skills
are guidance, not authority: loading this skill does not grant network access or bypass any server gate.

## 6. Verify the pair

1. Confirm the server exposes 34 tools. Fewer means the client is holding a stale tool list: reconnect
   or restart both sides.
2. Ask the agent:

   > Use web-widow to fetch https://example.com/ and report `final_url`, `fetched_at` and
   > `render_mode`.

   A healthy answer names `https://example.com/`, a timestamp and `static`. It should not paste the page
   as instructions.
3. If you configured semantic search, call `embedding_status`; `pending_pages` is real work and
   `last_error` names the cause when artifacts are missing.
4. If you configured rendering, fetch a JavaScript-heavy page and check `render_mode`; `static_fallback`
   means Chrome was unavailable.

## 7. Troubleshooting

| Symptom | Check |
| --- | --- |
| Client shows no web-widow tools | Server path in the client config; client logs; restart the client |
| Fewer than 34 tools | Stale session — reconnect or restart the server |
| `401` over HTTP | `WEB_WIDOW_HTTP_TOKEN` or the `0600` token file; token in the request header |
| `406` from `curl` | Add `Accept: application/json, text/event-stream` |
| `busy: data directory … in use` | One process per data directory; stop the other server |
| `render_unavailable` from a fetch | Chrome not found: set `[render].chrome_path` |
| `render_unavailable` from `pdf_extract` | PDFium not found: set `WEB_WIDOW_PDFIUM_PATH` or `[pdf].pdfium_path` |
| `embedding_unavailable` | ONNX Runtime/model path, `[embeddings].enabled`, SHA-256 manifest |
| Server refuses to start | Data directory must be `0700`; keep the config `0600` (the server warns if group/other can read it) |
| `unsupported_content_type` for CSV or PDF | Use `extract_rows` or `pdf_extract`; `fetch_page` refuses them by design |

## 8. Uninstall

```sh
rm -rf ~/.config/opencode/skills/web-widow
rm -f ~/.local/bin/web-widow
```

Then remove the client's MCP entry. Keep `~/.config/web-widow/` and the data directory if you want the
stored corpus and monitor history; delete both only after stopping the server.
