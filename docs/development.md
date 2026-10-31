# Development

Working on Chance of Files locally: everything runs on your machine against a small sample tree in `fixtures/webdav-root`, with no Cloudflare account and no pCloud. For running it for real, see [setup.md](setup.md); for how the pieces fit, [architecture.md](architecture.md).

## Tools

| Tool | Version | Install |
|---|---|---|
| Rust | stable, edition 2024 | `rustup default stable` |
| wasm target | | `rustup target add wasm32-unknown-unknown` |
| worker-build | 0.8.7 | `cargo install worker-build@0.8.7 --locked` |
| Node | 26 | `mise install` (pinned in `mise.toml`) |
| wrangler | 4.156.0 | `mise install`; `auth/` also has it as a dev dependency |
| rclone | 1.75 or later | `sudo pacman -S rclone` (or `mise use -g rclone`) |

[mise](https://mise.jdx.dev) isn't required, but the tasks below are the short way to run things; `mise tasks` lists them. Each one is a plain command you can run by hand: see `mise.toml`.

## Layout

| Path | Crate or package | What |
|---|---|---|
| `crates/core` | `chance-of-files-core` | The MCP endpoint (Streamable HTTP, stateless), JSON-RPC dispatch, the tools, the `Storage` trait. No Workers or WebDAV code, so its tests run natively |
| `crates/webdav` | `chance-of-files-webdav` | `Storage` over WebDAV, generic over the HTTP transport. Parser tests use captured rclone responses in `tests/data` |
| `crates/worker` | `chance-of-files-worker` | The Rust Worker, deployed as `chance-of-files-core`: auth check, config, the Workers transport (fetch or the VPC binding) |
| `auth/` | `chance-of-files-auth` (npm) | The TypeScript Worker `chance-of-files`: OAuth and the `/mcp` proxy |
| `scripts/` | | `dev-backend.sh` (local rclone), `smoke-test.sh` (MCP Inspector CLI) |
| `fixtures/webdav-root/` | | The sample tree: text, a non-ASCII name, an image, a binary file, a deep folder |
| `deploy/systemd/` | | User services for rclone and cloudflared on the laptop |

## The local loop

Once, copy the local settings. Both files are gitignored, and the example values (`dev-internal-token`, `dev-password`, `dev-passphrase`) are what the scripts below expect.

```bash
cp crates/worker/.dev.vars.example crates/worker/.dev.vars
cp auth/.dev.vars.example auth/.dev.vars
```

Then, in three terminals:

```bash
mise run backend    # rclone serves fixtures/webdav-root read-only on 127.0.0.1:8081 (user mcp, password dev-password)
mise run dev        # wrangler dev of the core on http://127.0.0.1:8787 (reads crates/worker/.dev.vars)
mise run smoke      # tools/list, then each tool, through the MCP Inspector CLI
```

`mise run backend -- ~/some/folder` serves another folder instead. It listens on 8081, so it can run next to a deployment's rclone service, which has 8080 (`BACKEND_ADDR` picks another address). `mise run dev` builds the wasm with worker-build first, which takes a minute the first time; it rebuilds when the Rust changes.

The core sends WebDAV requests to `BACKEND_URL` (`http://127.0.0.1:8081` in `.dev.vars`) whenever it's set, even when the `[[vpc_services]]` block is uncommented for a deployment, so there's no need to comment it out again. With the block in place, `wrangler dev` still opens a remote session for the binding, which needs `wrangler login`. To go through the real tunnel to your real folder instead, remove `BACKEND_URL` from `.dev.vars` and set `BACKEND_PASSWORD` there to the laptop's `RCLONE_PASS`.

`scripts/smoke-test.sh [URL] [TOKEN]` defaults to `http://127.0.0.1:8787/mcp` and `dev-internal-token`, and exits non-zero on the first failure. Against a real folder, `SMOKE_TEXT_FILE`, `SMOKE_IMAGE` and `SMOKE_QUERY` point it at files that exist there. Each step is one Inspector CLI call, for example:

```bash
npx @modelcontextprotocol/inspector@2.13.0 --cli http://127.0.0.1:8787/mcp --transport http \
  --header "Authorization: Bearer dev-internal-token" \
  --method tools/call --tool-name read_text_file --tool-args-json '{"path":"/notes/todo.md"}'
```

### By hand with curl

```bash
H=(-H 'Authorization: Bearer dev-internal-token' -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream')
curl -s http://127.0.0.1:8787/healthz; echo
curl -s "${H[@]}" http://127.0.0.1:8787/mcp -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}'; echo
curl -s "${H[@]}" http://127.0.0.1:8787/mcp -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'; echo
curl -s "${H[@]}" http://127.0.0.1:8787/mcp -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_folder","arguments":{"path":"/"}}}'; echo
curl -si http://127.0.0.1:8787/mcp -d '{}' | head -1    # 401 without the token
```

The endpoint is stateless, so `tools/list` works without `initialize` first.

### MCP Inspector UI

```bash
mise run inspector    # npx @modelcontextprotocol/inspector@2.13.0
```

It prints a URL with a session token; open it. Choose the **Streamable HTTP** transport, URL `http://127.0.0.1:8787/mcp`, add the header `Authorization: Bearer dev-internal-token`, and connect. The Tools tab lists the five tools and lets you call them; `read_image` on `/images/pixel.png` shows the image.

### Claude Code

Claude Code connects from your machine, so it can use the local core directly:

```bash
claude mcp add --transport http pcloud-dev http://127.0.0.1:8787/mcp --header "Authorization: Bearer dev-internal-token"
```

Ask it to list or read something in the fixtures, then `claude mcp remove pcloud-dev` when you're done.

## The auth front locally

```bash
mise run dev:auth     # npm run dev in auth/, on http://127.0.0.1:8788 (reads auth/.dev.vars)
```

Keep `mise run dev` running: the `MCP_CORE` service binding finds the core Worker through wrangler's local dev registry, so the two `wrangler dev` sessions talk to each other as they do in production. `/mcp` on 8788 needs an OAuth token; the core on 8787 still takes the internal token directly.

Or run both Workers in one process, serving the auth front on 8788. Start it from `crates/worker`, because the core's custom build (`worker-build`) runs in the current directory, and list the auth config first, since only the first Worker gets a port. It uses the wrangler from `auth/node_modules`, so run `npm ci` in `auth/` once first:

```bash
cd crates/worker
../../auth/node_modules/.bin/wrangler dev -c ../../auth/wrangler.jsonc -c wrangler.toml
```

Each Worker reads the `.dev.vars` next to its own config. [auth/README.md](../auth/README.md) has more on the auth front.

To walk through the whole OAuth flow, point a client that does OAuth at `http://127.0.0.1:8788/mcp`. The local auth front allows loopback redirects, and the passphrase is `dev-passphrase`.

- **MCP Inspector UI**: connect with no header; it discovers the metadata, registers, and opens the sign-in page. Registering works locally because `DCR_ALLOWED_CIDRS` includes loopback by default; against a deployed server, see [setup.md](setup.md#11-add-the-connector-in-claude) for opening it up briefly.
- **Claude Code**: `claude mcp add --transport http pcloud-auth-dev http://127.0.0.1:8788/mcp`, then `/mcp` in Claude Code to sign in.

```bash
curl -s http://127.0.0.1:8788/.well-known/oauth-authorization-server; echo
curl -s http://127.0.0.1:8788/.well-known/oauth-protected-resource/mcp; echo
curl -si -X POST http://127.0.0.1:8788/mcp | grep -i -e '^HTTP' -e www-authenticate
```

OAuth state (clients, grants, tokens) lives in the local KV under `auth/.wrangler/state`; delete that folder to start over.

## Tests and checks

```bash
mise run test     # cargo test --workspace, then npm test in auth/
mise run lint     # cargo fmt --check, clippy native and wasm32 with -D warnings, the auth typecheck
mise run fmt      # cargo fmt
mise run build    # worker-build --release for the core, and a dry-run bundle of the auth front
```

| Suite | Where | Covers | Needs |
|---|---|---|---|
| Core | `crates/core/tests/` | `http.rs`: methods, headers, status codes; `protocol.rs`: initialize, ping, tools/list, JSON-RPC errors; `tools.rs`: every tool's happy and error paths, against an in-memory `Storage`; `normalize.rs`: path rules | nothing |
| WebDAV unit | `crates/webdav/src/` | Request shapes (methods, `Depth`, `Range`, Basic auth), multistatus parsing from captured rclone answers in `tests/data`, search fallback | nothing |
| WebDAV integration | `crates/webdav/tests/rclone.rs` | The real `rclone serve webdav` on a scratch copy of the fixtures, on a free port: listings, ranged reads, search, the listing byte limit, a wrong password, an unreachable server, and `McpServer` end to end (including a search around a `SEARCH_SKIP` folder and the owner's guide) | rclone on `PATH`; each test skips itself without it |
| Auth front | `auth/test/` | Discovery metadata and the `401` challenge; the sign-in flow (passphrase, PKCE, refresh-token rotation, rate limit, CSRF, deny); the redirect allowlist; forwarding to a stub core with the internal token. Runs in workerd through Vitest | `npm ci` in `auth/` |

CI (`.github/workflows/ci.yml`) runs three jobs on every push and pull request: **rust** (fmt, clippy with `-D warnings`, `cargo test --workspace` with rclone installed), **wasm** (clippy for `wasm32-unknown-unknown` and `worker-build --release`), and **auth** (`npm ci`, typecheck, tests). Nothing is deployed from CI.

Conventions: Rust edition 2024, `cargo fmt`, `cargo clippy --all-targets -- -D warnings` clean. Comments say why, not what, and sparingly.

## Testing with claude.ai

claude.ai connects from Anthropic's cloud, so it can't reach `localhost`. Deploy as in [setup.md](setup.md), then watch both Workers while Claude uses the connector:

```bash
(cd auth && npx wrangler tail chance-of-files)
(cd crates/worker && wrangler tail chance-of-files-core)
```

Claude caches a server's metadata and tool list for about 5 minutes, so a new tool can take that long to appear.
