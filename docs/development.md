# Development

Working on Chance of Files locally: the server and a local `rclone rcd` run on your machine against a small sample tree in `fixtures/webdav-root`, with no Cloudflare account and no pCloud. For running it for real, see [setup.md](setup.md); for how the pieces fit, [architecture.md](architecture.md).

## Tools

| Tool | Version | Install |
|---|---|---|
| Rust | stable, edition 2024 | `rustup default stable` |
| rclone | 1.75.2 | `mise install` (pinned in `mise.toml`) |
| Node | 26 | `mise install` |
| wrangler | 4.156.0 | `mise install`; `auth/` also has it as a dev dependency |
| cargo-zigbuild, zig | 0.24.0, 0.14.0 | Only for `build-image`; fetched by `mise x` on first use |
| kubectl | any recent | Only for `lint` (kustomize render) and deploying |

[mise](https://mise.jdx.dev) isn't required, but the tasks below are the short way to run things; `mise tasks` lists them. Each is a plain command you can run by hand: see `mise.toml`.

## Layout

| Path | Crate or package | What |
|---|---|---|
| `crates/core` | `chance-of-files-core` | The MCP endpoint (Streamable HTTP, stateless), JSON-RPC dispatch, the tools, the `Storage` trait, `Policy`, scopes. No I/O, so its tests run against an in-memory store |
| `crates/rclone` | | `Storage` over rclone's remote-control API |
| `crates/webdav` | `chance-of-files-webdav` | `Storage` over WebDAV, for a local folder or NAS |
| `crates/server` | binary `chance-of-files` | axum on tokio: config file, internal token and scopes, `/mcp`, `/healthz`, `/readyz` |
| `auth/` | `chance-of-files-auth` (npm) | The TypeScript Worker `chance-of-files`: OAuth and the `/mcp` proxy over the `CORE` VPC binding |
| `deploy/k8s/base/` | | The kustomize base: Namespace, Deployment, ConfigMap from `config.toml`, PVC, NetworkPolicy |
| `deploy/k8s/apply.sh` | | Secret from local files, `kubectl apply -k`, rollout, tunnel and `/readyz` checks |
| `deploy/build-image.sh`, `deploy/oci-image.py` | | Static musl binary → OCI archive → optional `k3s ctr images import` over ssh |
| `deploy/dev.sh` | | The local loop's rclone rcd + server (`mise run dev`) |
| `scripts/smoke-test.sh` | | Every tool once through the MCP Inspector CLI |
| `fixtures/webdav-root/` | | The sample tree: text, a non-ASCII name, an image, a binary file, a deep folder |
| `config.example.toml` | | Every config option, documented |
| `spikes/` | | Phase 0 measurement tools, each its own Cargo project (excluded from the workspace) |

## The local loop

In two terminals:

```bash
mise run dev      # rclone rcd over fixtures/webdav-root on 127.0.0.1:5572, the server on http://127.0.0.1:8080
mise run smoke    # tools/list, then each tool, through the MCP Inspector CLI
```

`deploy/dev.sh` (what `mise run dev` runs) defines rclone's remote `pcloud` as an alias of the folder it serves, so the server runs with the same `deploy/k8s/base/config.toml` as the pod. Its `/var/lib/chance-of-files` paths are pointed at a scratch directory made per run (removed on exit; `STATE_DIR=...` keeps one), so the index works locally and starts empty. The dev token is `INTERNAL_TOKEN=dev-internal-token`; `RC_PASS` is random on each run, and rcd starts with an empty rclone config, so it can't reach your other remotes. Other sources:

```bash
mise run dev -- ~/some/folder          # a local folder
mise run dev -- mypcloud:              # a pcloud remote you've set up with `rclone config`: your real drive
CONFIG=my-config.toml mise run dev     # another config file, e.g. with your policy.hidden list
```

Against your real drive, put your `hidden` paths in that config first: rclone has no filters, so the core's policy is what keeps them out. `mise run smoke -- URL TOKEN` points the smoke test elsewhere, and `SMOKE_TEXT_FILE`, `SMOKE_IMAGE` and `SMOKE_QUERY` at files that exist there.

### By hand with curl

```bash
H=(-H 'Authorization: Bearer dev-internal-token' -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream')
curl -s http://127.0.0.1:8080/healthz; echo
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8080/readyz       # 200 while rclone answers
curl -s "${H[@]}" http://127.0.0.1:8080/mcp -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}'; echo
curl -s "${H[@]}" http://127.0.0.1:8080/mcp -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'; echo
curl -s "${H[@]}" -H 'X-CoF-Scopes: files:read files:write' http://127.0.0.1:8080/mcp -d '{"jsonrpc":"2.0","id":3,"method":"tools/list"}'; echo
curl -s "${H[@]}" http://127.0.0.1:8080/mcp -d '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"list_folder","arguments":{"path":"/"}}}'; echo
curl -si http://127.0.0.1:8080/mcp -d '{}' | head -1    # 401 without the token
```

The endpoint is stateless, so `tools/list` works without `initialize` first. Without `X-CoF-Scopes` a request has `files:read` only; in 0.2 both `tools/list` calls above show the same five tools.

### MCP Inspector and Claude Code

```bash
mise run inspector    # Streamable HTTP, http://127.0.0.1:8080/mcp, header Authorization: Bearer dev-internal-token
claude mcp add --transport http cof-dev http://127.0.0.1:8080/mcp --header "Authorization: Bearer dev-internal-token"
```

`claude mcp remove cof-dev` when you're done.

## The auth front locally

VPC services have no local simulation, so the local auth front reaches the core through a loopback `CORE_URL` instead of the `CORE` binding. Put `CORE_URL=http://127.0.0.1:8080` in `auth/.dev.vars` (copy it from `auth/.dev.vars.example`; its `INTERNAL_TOKEN` must be `dev-internal-token`), keep `mise run dev` running, and:

```bash
mise run dev:auth     # npm run dev in auth/, on http://127.0.0.1:8788
```

Point an OAuth-capable client at `http://127.0.0.1:8788/mcp`: the MCP Inspector UI with no header, or `claude mcp add --transport http cof-auth-dev http://127.0.0.1:8788/mcp` and `/mcp` in Claude Code. The local passphrase is in `auth/.dev.vars`. [auth/README.md](../auth/README.md) has more.

## Tests and checks

```bash
mise run test     # cargo test --workspace, then npm test in auth/
mise run lint     # cargo fmt --check, clippy -D warnings, the auth typecheck, shellcheck, kustomize render
mise run fmt      # cargo fmt
```

The rclone and WebDAV integration tests start their own `rclone` on scratch copies of the fixtures, on free ports, and skip themselves when rclone isn't on `PATH`. None of the default tests touch pCloud or Cloudflare.

CI (`.github/workflows/ci.yml`) runs four jobs on every push and pull request: **rust** (fmt, clippy with `-D warnings`, `cargo test --workspace` with rclone 1.75.2 installed), **cross** (the static `aarch64-unknown-linux-musl` build with cargo-zigbuild, packed with `deploy/oci-image.py`), **deploy** (shellcheck and `kubectl kustomize deploy/k8s/base`), and **auth** (`npm ci`, typecheck, tests). Nothing is deployed from CI.

Conventions: Rust edition 2024, `cargo fmt`, `cargo clippy --all-targets -- -D warnings` clean. Comments say why, not what, and sparingly. Personal values (IDs, hostnames, folder rules) never go in this repository: they belong in your overlay and in the gitignored `auth/wrangler.personal.jsonc`.

## Building images

```bash
mise run build-image                              # arm64 and amd64 archives in target/oci/
mise run build-image -- --arch arm64 --tag dev    # one architecture, another tag
mise run build-image -- --import <ssh-host>       # build for that node's architecture and import it into k3s
```

The binary is fully static (musl), so the image holds that one file and an empty `/tmp`; it runs as 65532:65532. `IMAGE=…` changes the image name (default `localhost/chance-of-files`). Published multi-arch images on ghcr.io come with tagged releases later.

## Testing with claude.ai

claude.ai connects from Anthropic's cloud, so it can't reach `localhost`. Deploy as in [setup.md](setup.md), then watch both ends while Claude uses the connector:

```bash
(cd auth && npx wrangler tail chance-of-files)
kubectl -n chance-of-files logs -f deploy/chance-of-files -c core
```

Claude caches a server's metadata and tool list for about 5 minutes, so a new tool can take that long to appear.
