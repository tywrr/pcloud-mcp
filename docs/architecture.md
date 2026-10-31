# Architecture

How Chance of Files 0.2 is put together, and why. For running it, see [setup.md](setup.md); for working on it, [development.md](development.md). The full design, with what comes after 0.2, is [design/v2.md](design/v2.md).

## Components

| Component | Runs on | Code | Job |
|---|---|---|---|
| `chance-of-files` (auth Worker) | Cloudflare Workers, free plan, public | `auth/`, TypeScript, [`@cloudflare/workers-oauth-provider`](https://github.com/cloudflare/workers-oauth-provider) | OAuth 2.1 authorization server (PKCE, Client ID Metadata Documents, Dynamic Client Registration) and protected resource for `/mcp`; forwards authorized requests to the core with the internal token and the grant's scopes |
| `OAUTH_KV` | Workers KV | | OAuth clients, grants and token hashes |
| Workers VPC service + Cloudflare Tunnel | Cloudflare | | A private path from the auth Worker's `CORE` binding to `127.0.0.1:8080` in the pod; the tunnel has no public hostname |
| `cloudflared` | The pod | `cloudflare/cloudflared` image | The tunnel's connector; dials out over QUIC |
| `chance-of-files` (the core) | The pod | `crates/server` (binary), `crates/core`, `crates/rclone` | MCP over Streamable HTTP (stateless), the tools, path policy, scopes; `/healthz` and `/readyz` |
| `rclone rcd` | The pod | `rclone/rclone` image | rclone's remote-control API on loopback: listings, stat, ranged reads (`--rc-serve`) against the remote `pcloud` |
| PVC `chance-of-files-data` | The node's disk | | `/var/lib/chance-of-files`: a state directory in 0.2; the index, mirror and link staging from later releases |

| Crate | What |
|---|---|
| `chance-of-files-core` (`crates/core`) | The MCP endpoint, JSON-RPC dispatch, the tools, the `Storage` trait, `Policy` and scopes. No I/O of its own; tests run against an in-memory store |
| `crates/rclone` | `Storage` over rclone's rc API (`operations/list`, `operations/stat`, `--rc-serve` for ranged reads, `operations/about` for health) |
| `chance-of-files-webdav` (`crates/webdav`) | `Storage` over WebDAV, for serving a local folder or a NAS instead of pCloud |
| `crates/server` | The `chance-of-files` binary: axum on tokio, config file, internal-token check, routes |

## Request flow

A `read_text_file` call from claude.ai, once the connector is signed in:

```mermaid
sequenceDiagram
  autonumber
  participant C as Claude (Anthropic cloud)
  participant A as chance-of-files (auth Worker)
  participant K as OAUTH_KV
  participant T as VPC service + tunnel + cloudflared
  participant M as core (127.0.0.1:8080)
  participant R as rclone rcd (127.0.0.1:5572)
  participant P as pCloud API
  C->>A: POST /mcp, Authorization: Bearer (access token)
  A->>K: look up token hash
  A->>T: CORE.fetch: Authorization: Bearer INTERNAL_TOKEN, X-CoF-Scopes: files:read
  T->>M: POST /mcp on the pod's loopback
  M->>M: token check, scope check, JSON-RPC, Policy on the path
  M->>R: GET /[pcloud:]/notes/todo.md, Range (Basic auth)
  R->>P: getfilelink + ranged download
  R-->>M: 206 Partial Content
  M-->>A: 200, JSON-RPC result (text content)
  A-->>C: 200
```

Before that, the connector signs in once:

1. Claude probes `/.well-known/oauth-authorization-server` and the protected-resource metadata (`/.well-known/oauth-protected-resource/mcp`). An unauthenticated `POST /mcp` gets `401` with `WWW-Authenticate: Bearer resource_metadata="…"`.
2. Claude identifies itself with its Client ID Metadata Document, or registers with `/register` (DCR), which accepts only addresses in `DCR_ALLOWED_CIDRS` (loopback by default).
3. Your browser opens `/authorize`, a consent page that asks for the owner passphrase, with an unticked box for `files:write`. On success it redirects back to Claude with a code.
4. Claude exchanges the code at `/token` (PKCE S256) for an access token and a refresh token, then sends `initialize` and `tools/list`.

## Auth layers and scopes

Each hop has its own credential, so a leak at one layer doesn't open the others.

| Hop | Who can call | Credential | Where it's checked |
|---|---|---|---|
| Internet → auth Worker | Anyone can reach it; only you can sign in | OAuth access token for `<public URL>/mcp`, issued after the passphrase | `workers-oauth-provider` in `auth/` |
| Auth Worker → core | Only Workers bound to the VPC service | VPC binding (the tunnel has no public hostname), then `INTERNAL_TOKEN` | Cloudflare, then the core (constant-time compare; otherwise `401`) |
| Core → rclone | Processes in the pod (loopback only) | rc Basic auth, user `core`, `RC_PASS` | rclone |
| rclone → pCloud | | rclone's OAuth token for its own registered pCloud app | pCloud |

**Scopes.** The auth Worker offers `files:read` (always granted) and `files:write` (only if the owner ticks the box, whatever the client asked for). It strips any `X-CoF-*` header a client sent and adds `X-CoF-Scopes` (space-separated) after it has replaced `Authorization` with the internal token. The core trusts `X-CoF-Scopes` only on requests that carry a valid internal token; a request without the header gets `files:read` only. `tools/list` lists only the tools the request's scopes allow, and a call to any other tool fails with an authorization error. In 0.2 every tool needs `files:read`, so this changes nothing visible yet; the write tools of 0.4 need `files:write`. To change a grant, reconnect the connector.

**Why each layer:**

- **OAuth at the front** is what Claude's custom connectors speak, and Duet uses the same connectors as claude.ai.
- **Workers VPC instead of a public tunnel hostname:** with a VPC service the tunnel is reachable only from Workers you bind to it. Nothing about the core is on the public internet.
- **`INTERNAL_TOKEN`** keeps Claude's access tokens at the front, and means the core refuses everything if the binding were ever misconfigured.
- **rclone's rc password** is root-equivalent inside the rclone container (`--rc-serve` also serves local paths), so it stays in the pod, on loopback, and the core builds rc-serve URLs only from the configured remote name and a policy-checked path.

## The core

### Why a native core

0.1 ran the core as a Rust Worker, which meant 10 ms of CPU per request, 50 subrequests and 128 MB of memory, and reading pCloud through a laptop's pCloud Drive mount. 0.2 runs the same tool code natively on tokio and axum, next to rclone, so listings of tens of thousands of entries, long reads and (later) a whole-drive index are cheap, and the laptop is out of the path. The OAuth front stays a Worker because it needs a public HTTPS endpoint and workers-oauth-provider, and the free plan is enough for it.

### Why stateless MCP

The endpoint is Streamable HTTP in stateless mode: every `POST /mcp` carries one JSON-RPC message and gets a plain JSON response, and there is no `Mcp-Session-Id`. Every tool is a read with no state between calls; paging uses explicit offsets. MCP 2025-11-25 makes sessions optional, Claude's connector client doesn't rely on them, and MCP 2026-07-28 moves further toward stateless requests. So `GET /mcp` and `DELETE /mcp` answer `405`, and notifications get `202`. `initialize` negotiates `2025-03-26`, `2025-06-18` and `2025-11-25`; `2026-07-28` clients put their version in each request's `_meta`. The dispatch is hand-written over [rmcp](https://crates.io/crates/rmcp)'s `model` types (design decision D1): rmcp's server runtime would bring sessions and SSE that buy nothing here.

### Routes

| Route | Auth | Answer |
|---|---|---|
| `POST /mcp` | `Authorization: Bearer <INTERNAL_TOKEN>` | MCP; `X-CoF-Scopes` decides which tools exist |
| `GET /healthz` | none | `200` while the process runs |
| `GET /readyz` | none | `200` when rclone answers `operations/about`, else `503`; no details either way |

The core listens on `127.0.0.1:8080` only. Health routes are for `kubectl port-forward` and local checks; nothing outside the pod can reach them.

### Storage and policy

The tools see only the `Storage` trait; `crates/rclone` implements it over rclone's rc API, and `crates/webdav` over WebDAV. Paths are normalized (absolute, no `.` or `..`, no control characters, NFC, case-folded for matching) and checked against `Policy` before a backend sees them:

| Rule | Effect |
|---|---|
| `policy.hidden` | Never listed, searched or read; errors say "not found" |
| `policy.protected` | Visible and readable; any write touching it is refused (from 0.4) |
| `policy.search_skip` | `search_files` goes around these folders; Claude can still list and read them |

**rclone runs with no filters at all.** Phase 0 measured that any directory filter makes rclone drop pCloud's recursive listing (a 133k-entry folder: 6 s became 133 s) and turn folder moves into per-file moves, and that filters don't affect single-object calls (`stat`, `copyfile`, `deletefile`, rc-serve `GET`) anyway ([phase0-rclone.md](design/phase0-rclone.md)). So `Policy` in the core is the only enforcement point, and the rclone client accepts only paths that `Policy` produced.

`initialize` also reads the first 8 KiB of the owner's guide (`guide.path`, `/AGENTS.md` by default) and appends it to the instructions. Any error leaves it out.

## Configuration

One TOML file, from `--config`, else `CHANCE_OF_FILES_CONFIG`, else `/etc/chance-of-files/config.toml`. Secrets come only from the environment: `INTERNAL_TOKEN` and `RC_PASS`. Every option is documented in [`config.example.toml`](../config.example.toml); the cluster's copy is [`deploy/k8s/base/config.toml`](../deploy/k8s/base/config.toml).

| Section | Keys |
|---|---|
| `[server]` | `listen` (`127.0.0.1:8080`), `deadline` (`180s`), `allowed_origins` |
| `[rclone]` | `url` (`http://127.0.0.1:5572`), `user` (`core`), `remote` (`pcloud`) |
| `[policy]` | `hidden`, `protected`, `search_skip` |
| `[guide]` | `path` (`/AGENTS.md`) |
| `[limits]` | `max_read_bytes`, `max_image_bytes`, `max_listing_bytes` |

## The pod

`deploy/k8s/base` is a kustomize base: Namespace `chance-of-files` (Pod Security `restricted`), Deployment `chance-of-files` (one replica, `Recreate`, three containers), the ConfigMap generated from `config.toml`, PVC `chance-of-files-data`, and a NetworkPolicy that denies all ingress and allows egress only to DNS, 443 and 7844. The Secret `chance-of-files` (`internal-token`, `rc-pass`, `pcloud-token`, `pcloud-hostname`, `tunnel-token`) is created by `deploy/k8s/apply.sh` from local files and is never in git.

- **One replica, `Recreate`:** Cloudflare spreads requests across every connector of a tunnel, so two pods would split traffic.
- **No probes:** the kubelet's HTTP probes go to the pod IP, and nothing listens there; exec probes need a shell the images don't have; and readiness would gate nothing, since no Service selects the pod. `apply.sh` checks `/readyz` through `kubectl port-forward` after each rollout instead.
- **The image** is the static musl binary alone (`deploy/build-image.sh`, no Docker needed). rclone and cloudflared stay on their pinned upstream images, so their upgrades are independent.

## Limits

| Limit | Value | Set by |
|---|---|---|
| Text per `read_text_file` call | 100,000 bytes (`limits.max_read_bytes`) | This project, under Claude's ~150,000-character cap on a tool result |
| Image size for `read_image` | 1.5 MiB (`limits.max_image_bytes`) | This project; base64 makes it a third larger in the result |
| One rclone listing the core parses | 64 MB (`limits.max_listing_bytes`) | This project; a recursive listing of 133k entries is ~35 MB |
| Time per tool call | 180 s (`server.deadline`); Claude allows 240 s | 200 s measured to pass end to end ([phase0-network.md](design/phase0-network.md)) |
| pCloud API calls | 8 per second (`--tpslimit=8`) | This project, to stay well inside pCloud's limits |
| Request body through Cloudflare | 100 MB | Cloudflare (matters for uploads, from 0.4) |
| Workers requests / KV writes | 100,000 / 1,000 a day | Workers free plan; only sign-ins and token refreshes write KV |
| Metadata cache | about 5 minutes | Claude caches the server's metadata and tool list |

## Why a TypeScript auth front

OAuth is TypeScript because [`@cloudflare/workers-oauth-provider`](https://github.com/cloudflare/workers-oauth-provider) already implements what Claude needs and keeps up with the MCP authorization spec: RFC 8414 and RFC 9728 metadata, DCR, CIMD (with SSRF-safe fetching), PKCE, refresh-token rotation, and hashed tokens in KV. Writing that in Rust would mean owning security-sensitive code with no maintained equivalent. See [background.md](background.md#alternatives-considered).
