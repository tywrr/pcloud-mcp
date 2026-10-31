# Background

Why Chance of Files exists and how its shape was chosen. A decision record, written 2026-10-29 for 0.1 (two Workers and a laptop); some of it, such as the Workers CPU limits, no longer applies since 0.2 moved the core to your own machine. The v2 decisions are in [design/v2.md](design/v2.md).

## Why build it

The goal: ask Claude, and especially **Claude Duet**, about what's in pCloud. Notes, course material and documents live there, organized by topic, and Claude should be able to read them in place rather than have them uploaded one by one.

Claude reaches outside services through connectors, and Duet (a research preview since 2026-10-21, for US Pro and Max plans) uses the connectors set up on the claude.ai account. So whatever reads pCloud has to be a **remote MCP server**: public HTTPS, reachable from Anthropic's cloud (egress `160.79.104.0/21`, IPv4 only), with OAuth that Claude's connector client understands.

pCloud already runs one at `https://mcp.pcloud.com/mcp`. Its tools, as seen on 2026-10-29: `get_user_info`, `list_folder`, `get_file_info`, `get_file_checksum`, `search_files`, `create_folder`, `create_folder_if_not_exists`, `copy_file`, `copy_files`, `copy_folder`, `move_file`, `move_files`, `move_folder`, `rename_file`, `rename_folder`, `upload_file`, `upload_from_url`, `get_upload_progress`, `create_file_public_link`, `create_folder_public_link`, `change_public_link`, `list_public_links`.

That's a good file manager, but **none of them returns a file's contents**, and none deletes. Claude can see that `notes.md` exists but not read it. Chance of Files' niche is the missing half: reading text and images, self-hosted, in Rust.

Requirements, in order:

1. Works as a claude.ai custom connector, so in Duet and every Claude app.
2. Reads text and images; read-only is fine to start.
3. One user, the owner; nobody else can connect.
4. Free to run: Cloudflare's free plan, for a focused folder (the CPU budget sets the size; see [limits](architecture.md#limits)).
5. Open source under a permissive license (MIT OR Apache-2.0), so others can run it.

## Prior art

Ideas only: no code was copied from any of these, and the AGPL one was read for its design notes, not its code.

| Project | License | What it is | What we took | Why not use it as is |
|---|---|---|---|---|
| [pCloud MCP](https://mcp.pcloud.com/mcp) | Proprietary, hosted | pCloud's official remote MCP server | Tool names that match where the two overlap (`list_folder`, `get_file_info`, `search_files`) | Can't read file contents |
| [yora0418/pcloud-mcp-cloudflare](https://github.com/yora0418/pcloud-mcp-cloudflare) | AGPL-3.0 | Read-only pCloud MCP server on Workers, TypeScript; pCloud access token; Cloudflare Access in front | Its design notes on size limits and hardening confirmed the shape (bounded reads, read-only, one subtree) | AGPL; needs a pCloud API token, which now needs an approved OAuth app |
| [rghrb/pcloud-mcp](https://github.com/rghrb/pcloud-mcp) | MIT | Node MCP server for pCloud with read and write tools; pCloud token or OAuth app | Confirmed people want content reads | Needs a pCloud API token and an always-on Node host |
| [calebgrimes/claude-duet-bridge](https://github.com/calebgrimes/claude-duet-bridge) | MIT | Remote MCP server on Workers that lets Duet drive Claude Code | The single-owner pattern: OAuth on Workers with a passphrase consent page, rate-limited, used with Duet | Different purpose |
| [Asutorufa/hujiang_dictionary `worker/src/mcp.rs`](https://github.com/Asutorufa/hujiang_dictionary/blob/rust/worker/src/mcp.rs) | No license file | A hand-rolled MCP endpoint in Rust on Workers | Evidence that a stateless MCP endpoint in workers-rs is small | Different purpose; no license |
| [cloudflare/workers-oauth-provider](https://github.com/cloudflare/workers-oauth-provider) | MIT | OAuth 2.1 provider library for Workers | Used as the auth front | (dependency) |
| [rmcp](https://github.com/modelcontextprotocol/rust-sdk) | Apache-2.0 | The official Rust MCP SDK | Its `model` types only | Runtime doesn't run on Workers (below) |
| [workers-rs](https://github.com/cloudflare/workers-rs) | Apache-2.0 | Rust on Workers | Used for the core | (dependency) |
| [rclone](https://rclone.org) | MIT | `rclone serve webdav` | Serves the pCloud Drive folder | (dependency, runs on the laptop) |

## pCloud API facts and gotchas

What we found while deciding, for whoever writes the pCloud API backend.

- **Password authentication is gone.** In April 2026 pCloud removed username/password sign-in from its API and revoked the tokens issued that way; support confirmed it's permanent. ([hydrus.org.uk](https://www.hydrus.org.uk/journal/pcloud-auth.html); [FileFluss 0.10 release notes](https://newreleases.io/project/github/rana-gmbh/FileFluss/release/v0.10) report the same.) The API docs still describe the old flow.
- **OAuth apps are approved by hand.** You can't create one yourself: you ask pCloud, and they send a client ID and secret. Registration has been closed at times ([Home Assistant community, 2025-11](https://community.home-assistant.io/t/new-hacs-integration-pcloud-backup-for-home-assistant/951159)); this project's request is pending. The client secret can't ship in an open-source client, so the token exchange has to run server-side.
- **Two API hosts.** `api.pcloud.com` for US accounts, `eapi.pcloud.com` for EU accounts ([docs](https://docs.pcloud.com/)), depending on where the account was registered.
- **Download links are tied to the requesting IP.** [`getfilelink`](https://docs.pcloud.com/methods/streaming/getfilelink.html) returns hosts and a path, and a link used from another address fails with "This link was generated for another IP address". So a Worker must fetch the bytes itself; it can't pass the link to Claude.
- **`file_open` is deprecated.** Ranged reads go through `getfilelink` plus an HTTP `Range` request instead of the file-descriptor API.
- **There's an undocumented `/search` method.** Useful for `search_files`, but undocumented means it can change.
- **The Crypto Folder is unreadable while locked**, through the Drive mount and the API alike. Serve a folder outside it.
- **rclone kept working** through the auth change, with rclone's own approved OAuth client.

## Why read through the pCloud Drive mount (for now)

With no API access until an app is approved, the only working read path was pCloud's own Drive client: it's a FUSE mount at `~/pCloudDrive`, signed in, with a local cache. `rclone serve webdav --read-only` turns one folder of it into a small, well-understood HTTP API with Basic auth and `Range` support, and a Cloudflare Tunnel plus a Workers VPC binding gets the Worker to it privately.

The price is the laptop: it has to be on and awake. The tools don't know any of this: they're written against the `Storage` trait, so the API backend replaces WebDAV without touching them.

## Why not rmcp's runtime

[rmcp](https://crates.io/crates/rmcp) 3.8.0, the official Rust SDK, is the natural choice for an MCP server in Rust, but its service runtime and transports need tokio, and tokio's runtime panics on Workers (wasm32 in a JS isolate: no threads, no tokio timers or I/O driver). With `default-features = false`, the `rmcp::model` module (the JSON-RPC and MCP types) builds for `wasm32-unknown-unknown`, and that's all Chance of Files uses. The Streamable HTTP endpoint is hand-written in `crates/core`, which is little code in stateless mode: one `POST`, one JSON-RPC message, one JSON response.

## Alternatives considered

| Alternative | Why not |
|---|---|
| **Cloudflare Access [Managed OAuth](https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/managed-oauth/)** in front of the Worker, instead of our own OAuth | No OAuth code to own, and sign-in through an identity provider. But it needs a Zero Trust organization, an Access application and policy, and the Worker has to validate Access JWTs; it's more dashboard setup for every stranger who runs this, harder to test locally, and its fit with Claude's CIMD/DCR client was unverified. workers-oauth-provider runs anywhere wrangler does. |
| **An all-Rust OAuth server** in the core Worker | One language and one Worker. But PKCE, DCR, CIMD fetching with SSRF protection, refresh rotation and token storage are security-sensitive, and no maintained Rust crate does them on Workers. Kept on the [roadmap](roadmap.md). |
| **Grok Bot + rclone**: let a general-purpose agent with its own cloud shell run rclone against pCloud | It isn't Claude, so Duet can't use it, and Grok Bot attaches only remote MCP connectors, which brings back the need for a server like this one. It also hands broad pCloud credentials to an agent's shell. |
| **[Pipedream](https://pipedream.com/apps/pcloud)'s pCloud integration** (hosted MCP, Pipedream's approved pCloud app) | A third party holds the pCloud token. Its pCloud actions are file management (list, copy, move, upload, download to a folder); none was documented to return contents to the model. |
| **A public tunnel hostname** (or Tailscale Funnel) in front of rclone | Puts the WebDAV server on the internet behind Basic auth alone. Workers VPC keeps it private. |
| **rclone's `pcloud:` backend on an always-on machine** instead of the Drive mount | Would remove the laptop, and should work with the same Worker unchanged, since it's still WebDAV. Untested; a reasonable variation for anyone with a home server. |
