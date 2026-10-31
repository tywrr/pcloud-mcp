# Chance of Files auth front

The public half of Chance of Files: a TypeScript Worker named `chance-of-files` that Claude connects to. It is an OAuth 2.1 authorization server for one person, the owner, built on [`@cloudflare/workers-oauth-provider`](https://github.com/cloudflare/workers-oauth-provider) 1.2.4. Requests to `/mcp` that carry a valid access token are forwarded to the Rust Worker `chance-of-files-core` over the `MCP_CORE` service binding, with Claude's token replaced by the shared `INTERNAL_TOKEN`. The core has no public URL.

| Path | What it does |
|---|---|
| `/mcp` | Needs `Authorization: Bearer <access token>`, else `401` with `WWW-Authenticate: Bearer … resource_metadata="…"`. Forwarded to the core with `Authorization: Bearer <INTERNAL_TOKEN>` and no `Cookie`; the core's response comes back unchanged |
| `/authorize` | The sign-in page: who is asking, where access goes, a passphrase field |
| `/token` | Code exchange, refresh, revocation, for public clients and DCR clients with a secret. Client assertions (`private_key_jwt`) get `401 invalid_client` |
| `/register` | Dynamic Client Registration (RFC 7591), only from the addresses in `DCR_ALLOWED_CIDRS`; others get `403` |
| `/.well-known/oauth-authorization-server` | RFC 8414 metadata |
| `/.well-known/oauth-protected-resource/mcp`, `/.well-known/oauth-protected-resource` | RFC 9728 metadata; `resource` is `<origin>/mcp` |
| `/healthz` | `ok`, without auth |

Everything else is `404`.

## Sign-in and safety

- **One passphrase.** `OWNER_PASSPHRASE` is compared in constant time (SHA-256 digests with `crypto.subtle.timingSafeEqual`). It must be at least 12 characters; use a long random one (`openssl rand -base64 24`). Anyone who knows it can connect a client.
- **Redirect allowlist.** Dynamic registration lets anyone register a client, so codes only go to the URIs in `ALLOWED_REDIRECT_URIS`: by default Claude's two callbacks plus `http://localhost` and `http://127.0.0.1` on any port and path, for Claude Code and the MCP Inspector. A request for any other redirect URI is refused before the page is shown, and a registration with no allowed redirect URI is refused outright.
- **PKCE with S256 for every client,** including confidential ones (the library only requires it from public clients).
- **Consent without server state.** `GET /authorize` validates the request and shows the form, and stores nothing: no KV record, no cookie. The form has no `action`, so it posts back to the same URL with the authorization request still in the query string. `POST /authorize` checks the passphrase (rate-limited, constant-time) first, then parses and checks that request again from scratch (client, redirect URI against the allowlist, PKCE) and completes the authorization; nothing in the form body is trusted but the passphrase. **Deny** needs no passphrase: it sends `error=access_denied` (with `state` and `iss`) to the request's redirect URI, after the same allowlist check.
- **CSRF and framing.** The passphrase is the CSRF secret: another site can make the owner's browser post the form, but without the passphrase it can only deny. Pages can't be framed (`frame-ancestors 'none'`, `X-Frame-Options: DENY`), so no site can overlay the real form to trick the owner into typing the passphrase and clicking Allow. Pages also send `Content-Security-Policy: default-src 'none'` (plus a per-response style nonce and `form-action` limited to this origin and the client's redirect origin), `Cache-Control: no-store` and `Referrer-Policy: no-referrer`.
- **The KV write budget.** The free plan allows 1,000 KV writes a day, and when they run out, sign-in and token refresh fail until 00:00 UTC. So nothing an anonymous caller can reach writes to KV: the sign-in page stores nothing, only a correct passphrase creates a grant, and only `DCR_ALLOWED_CIDRS` may register clients. That defaults to loopback only, for `wrangler dev`: Claude identifies itself with its Client ID Metadata Document and doesn't register. Anthropic's outbound range (`160.79.104.0/21`, [documented here](https://platform.claude.com/docs/en/api/ip-addresses)) is left out on purpose, because anyone using Claude can send requests from it; add it only if claude.ai ever falls back to DCR for your server. The registration endpoint stays advertised for that case. The library caches the Client ID Metadata Documents it fetches with the Cache API, never in KV. It would also accept `private_key_jwt` from a CIMD client and record each assertion's `jti` in KV before checking the code or refresh token, so anyone who publishes a metadata document with their own key could spend a write per request; Claude's clients authenticate as public clients, so `/token` refuses client assertions and the metadata doesn't advertise them.
- **Rate limits** (Workers Rate Limiting binding, per IP, with all of an IPv6 caller's /64 counted as one address; approximate and per Cloudflare location): 5 passphrase attempts a minute; 20 sign-in page views, denials or registrations a minute. They slow down guessing and floods; they are not what protects the KV budget, since a caller with many addresses gets around them.
- **Tokens.** Access tokens last 1 hour. Refresh tokens rotate on every use; a grant lasts 30 days and each refresh extends it by 30 days. Tokens are stored in KV only as hashes.
- **Discovery.** Client ID Metadata Documents (needs the `global_fetch_strictly_public` compatibility flag) and Dynamic Client Registration are both on, and `none` is among the token endpoint auth methods, which is what Claude needs to pick CIMD.

## Configuration

| Name | Kind | Meaning |
|---|---|---|
| `INTERNAL_TOKEN` | secret | Same value as the core's `INTERNAL_TOKEN` |
| `OWNER_PASSPHRASE` | secret | The sign-in passphrase (12+ characters) |
| `PUBLIC_URL` | var | Public origin, for example `https://chance-of-files.example.workers.dev`. Empty (default) uses the origin each request arrives on. When set, requests on any other hostname get `421` |
| `ALLOWED_REDIRECT_URIS` | var | Comma-separated exact redirect URIs, plus `loopback` for the loopback rule. Empty means the defaults |
| `DCR_ALLOWED_CIDRS` | var | Who may call `POST /register`: comma-separated IPv4 or IPv6 CIDRs (a bare address is one host), matched against `CF-Connecting-IP`, or `*` for everyone. Empty means the default, `127.0.0.0/8,::1`. Add `160.79.104.0/21` (Anthropic) only if claude.ai falls back to DCR. Claude Code doesn't need it, since it identifies itself with its own metadata document. A client on your own computer that does use DCR, such as the MCP Inspector, registers from your own address: add it for the moment you connect the client, or use `*` and change it back |
| `OAUTH_KV` | KV | Clients, grants, tokens. Created on the first deploy |
| `MCP_CORE` | service | The `chance-of-files-core` Worker |
| `PASSPHRASE_LIMITER`, `SIGNIN_LIMITER` | rate limit | See above; `namespace_id` must be unique in your account |

The `resource` in the protected-resource metadata is `<public origin>/mcp`, and it must equal the URL you enter in Claude exactly, so add the connector as `https://<your worker host>/mcp`.

## Develop

```bash
npm ci
cp .dev.vars.example .dev.vars
npm run dev          # http://localhost:8788
npm test             # Vitest in workerd; MCP_CORE is a stub that echoes what it receives
npm run typecheck    # checks worker-configuration.d.ts is current, then tsc
npm run cf-typegen   # regenerate worker-configuration.d.ts after editing wrangler.jsonc
```

To run it with the core, start the core in `crates/worker` (`wrangler dev`, port 8787, with its own `.dev.vars`) and `npm run dev` here (port 8788) in two terminals; wrangler's local dev registry connects the `MCP_CORE` binding. `mise run dev` and `mise run dev:auth` do the same.

Both Workers can also run in one process. Start it from `crates/worker`, not from `auth/`, because the core's custom build (`worker-build`) runs in the current directory, and list the auth config first, since only the first Worker gets a port:

```bash
cd crates/worker
../../auth/node_modules/.bin/wrangler dev -c ../../auth/wrangler.jsonc -c wrangler.toml
```

Each Worker reads the `.dev.vars` next to its own config.

Then point the MCP Inspector at `http://localhost:8788/mcp`; its loopback callback is allowed, and the passphrase is `dev-passphrase` from `.dev.vars.example`. Locally, wrangler sets `CF-Connecting-IP` to the connection's address (`127.0.0.1` or `::1`), so the default `DCR_ALLOWED_CIDRS` lets local clients register. Wrangler keeps a `CF-Connecting-IP` the client sends itself, so set one with curl to try a refusal: `curl -i -X POST -H 'CF-Connecting-IP: 198.51.100.7' -H 'Content-Type: application/json' -d '{"redirect_uris":["http://localhost:1/cb"]}' http://localhost:8788/register`. In production Cloudflare sets the header and clients can't.

## Deploy

Deploy `chance-of-files-core` first: the service binding needs it to exist. Then, from `auth/`:

```bash
npx wrangler login
# A new Worker can't hold secrets before its first deploy, and wrangler refuses to deploy
# without the required ones, so the first deploy passes them in a file it reads once:
npx wrangler deploy --secrets-file <(printf 'INTERNAL_TOKEN=%s\nOWNER_PASSPHRASE=%s\n' "$INTERNAL_TOKEN" "$OWNER_PASSPHRASE")
# Later changes:
npx wrangler secret put OWNER_PASSPHRASE
npx wrangler deploy
```

The `OAUTH_KV` namespace is created automatically on the first deploy. The Rate Limiting binding is generally available and not documented as paid-only, but it has not yet been deployed on a free account for this project.
