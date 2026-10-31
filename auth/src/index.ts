// chance-of-files: the OAuth front of Chance of Files, an MCP server for your pCloud.
//
//   /mcp                                     protected; forwarded to the chance-of-files-core Worker
//   /authorize                               the owner's passphrase sign-in page
//   /token                                   OAuth token endpoint (public and client-secret clients)
//   /register                                Dynamic Client Registration, from DCR_ALLOWED_CIDRS only
//   /.well-known/oauth-authorization-server  RFC 8414 metadata
//   /.well-known/oauth-protected-resource    RFC 9728 metadata (also at .../oauth-protected-resource/mcp)
//   /healthz                                 "ok", for uptime checks
//
// Everything else is 404.

import { OAuthProvider } from "@cloudflare/workers-oauth-provider";
import { handleAuthorize } from "./authorize";
import {
	ACCESS_TOKEN_TTL,
	type AppEnv,
	READ_SCOPE,
	REFRESH_TOKEN_IDLE_TTL,
	REFRESH_TOKEN_TTL,
	SCOPES_SUPPORTED,
} from "./config";
import { forwardToCore } from "./core";
import { CidrList } from "./ip";
import { RedirectPolicy } from "./redirects";
import { clientIp, withinLimit } from "./security";

const MCP_PATH = "/mcp";
const ROOT_RESOURCE_METADATA = "/.well-known/oauth-protected-resource";
const AUTHORIZATION_SERVER_METADATA = "/.well-known/oauth-authorization-server";

interface Built {
	provider: OAuthProvider<Env>;
	policy: RedirectPolicy;
}

// The provider's options are fixed at construction, but the resource URL depends on where the
// Worker is deployed, so one is built per (origin, redirect list) and reused across requests.
const built = new Map<string, Built>();

function build(origin: string, redirectSpec: string | undefined): Built {
	const key = `${origin} ${redirectSpec ?? ""}`;
	const cached = built.get(key);
	if (cached) return cached;

	const policy = new RedirectPolicy(redirectSpec);
	const provider = new OAuthProvider<Env>({
		apiRoute: MCP_PATH,
		apiHandler: { fetch: (request, env) => forwardToCore(request, env) },
		defaultHandler: {
			fetch: (request, env) => {
				if (new URL(request.url).pathname === "/authorize") {
					return handleAuthorize(request, env as AppEnv, policy);
				}
				return new Response("Not Found\n", { status: 404 });
			},
		},
		authorizeEndpoint: "/authorize",
		tokenEndpoint: "/token",
		clientRegistrationEndpoint: "/register",
		// Claude prefers a Client ID Metadata Document; it needs the global_fetch_strictly_public flag.
		clientIdMetadataDocumentEnabled: true,
		scopesSupported: SCOPES_SUPPORTED,
		requiredScopes: [READ_SCOPE],
		resourceMetadata: {
			// Must equal the URL entered in Claude, so it is built from this Worker's own origin.
			resource: `${origin}${MCP_PATH}`,
			authorization_servers: [origin],
			resource_name: "pCloud files (Chance of Files)",
		},
		accessTokenTTL: ACCESS_TOKEN_TTL,
		refreshTokenTTL: REFRESH_TOKEN_TTL,
		refreshTokenIdleTTL: REFRESH_TOKEN_IDLE_TTL,
		// A registration that can't use any allowed redirect URI could never sign in; refusing it
		// up front saves a KV write.
		clientRegistrationCallback: ({ clientMetadata }) => {
			if (policy.allowsAny(clientMetadata.redirect_uris)) return;
			return {
				code: "invalid_redirect_uri",
				description: "None of the redirect_uris is allowed on this server",
			};
		},
	});

	// Hostnames are fixed by routing, so this stays tiny; the bound only guards against surprises.
	if (built.size >= 8) built.clear();
	const entry = { provider, policy };
	built.set(key, entry);
	return entry;
}

let registrants: { spec: string | undefined; list: CidrList } | undefined;

/** Who may register clients, parsed once per DCR_ALLOWED_CIDRS value (so bad entries warn once). */
function registrationAllowlist(spec: string | undefined): CidrList {
	if (!registrants || registrants.spec !== spec) registrants = { spec, list: new CidrList(spec) };
	return registrants.list;
}

/** An OAuth error JSON, readable by browser-based clients like the MCP Inspector. */
function oauthError(
	request: Request,
	status: number,
	error: string,
	description: string,
	headers: Record<string, string> = {},
): Response {
	const response = Response.json(
		{ error, error_description: description },
		{ status, headers: { "Cache-Control": "no-store", ...headers } },
	);
	const origin = request.headers.get("Origin");
	if (origin) {
		response.headers.set("Access-Control-Allow-Origin", origin);
		response.headers.set("Access-Control-Expose-Headers", "Retry-After");
		response.headers.set("Vary", "Origin");
	}
	return response;
}

/** Whether a /token request authenticates with a client assertion (private_key_jwt). */
async function sendsClientAssertion(request: Request): Promise<boolean> {
	// The library parses the body the same way, so a body this can't read, it refuses too.
	const form = await request
		.clone()
		.formData()
		.catch(() => undefined);
	return form !== undefined && (form.has("client_assertion") || form.has("client_assertion_type"));
}

/** The library's metadata, without the private_key_jwt support that /token refuses. */
async function withoutClientAssertions(response: Response): Promise<Response> {
	if (!response.ok) return response;
	const metadata = await response.json<Record<string, unknown>>();
	const methods = metadata.token_endpoint_auth_methods_supported;
	if (Array.isArray(methods)) {
		metadata.token_endpoint_auth_methods_supported = methods.filter((method) => method !== "private_key_jwt");
	}
	delete metadata.token_endpoint_auth_signing_alg_values_supported;
	const headers = new Headers(response.headers);
	headers.delete("Content-Length");
	return Response.json(metadata, { status: response.status, headers });
}

/** The public origin: PUBLIC_URL when set, else the origin the request arrived on. */
function publicOrigin(env: Env, url: URL): string | Response {
	const configured = env.PUBLIC_URL?.trim();
	if (!configured) return url.origin;
	const parsed = URL.parse(configured);
	if (!parsed || parsed.pathname !== "/" || parsed.search || parsed.hash) {
		return new Response("PUBLIC_URL must be an origin such as https://chance-of-files.example.workers.dev\n", {
			status: 500,
		});
	}
	if (parsed.origin !== url.origin) {
		return new Response(`This server answers at ${parsed.origin}\n`, { status: 421 });
	}
	return parsed.origin;
}

export default {
	async fetch(request, env, ctx): Promise<Response> {
		const url = new URL(request.url);

		if (url.pathname === "/healthz" && (request.method === "GET" || request.method === "HEAD")) {
			return new Response(request.method === "HEAD" ? null : "ok", {
				headers: { "Content-Type": "text/plain; charset=utf-8", "Cache-Control": "no-store" },
			});
		}

		const origin = publicOrigin(env, url);
		if (origin instanceof Response) return origin;

		let entry: Built;
		try {
			entry = build(origin, env.ALLOWED_REDIRECT_URIS);
		} catch (error) {
			// The library refuses a non-https resource on anything but a loopback host.
			const message = error instanceof Error ? error.message : String(error);
			return new Response(`OAuth configuration error: ${message}\n`, { status: 500 });
		}

		// The library serves the resource's metadata only at the path-specific well-known URL.
		// MCP clients that find no resource_metadata in the 401 also try the root, so the same
		// document is served there.
		if (url.pathname === ROOT_RESOURCE_METADATA) {
			url.pathname = `${ROOT_RESOURCE_METADATA}${MCP_PATH}`;
			request = new Request(url, request);
		}

		// Each registration is a KV write, and once the free plan's 1,000 a day are used up, sign-in
		// and token refresh fail until 00:00 UTC. Per-IP limits can't protect that budget from
		// callers with many addresses, so only DCR_ALLOWED_CIDRS may register at all: by default
		// Anthropic's outbound range, for Claude's fallback when it doesn't use its metadata
		// document, and loopback for local development. registration_endpoint stays advertised.
		if (url.pathname === "/register" && request.method === "POST") {
			const ip = clientIp(request);
			if (!registrationAllowlist(env.DCR_ALLOWED_CIDRS).allows(ip)) {
				console.warn(`register: refused ${ip ?? "a request without CF-Connecting-IP"}, not in DCR_ALLOWED_CIDRS`);
				return oauthError(request, 403, "access_denied", "Client registration is not open to this address");
			}
			// Anthropic's addresses are shared by everyone who uses Claude; this slows a flood down.
			if (!(await withinLimit(env.SIGNIN_LIMITER, "register", request))) {
				return oauthError(request, 429, "temporarily_unavailable", "Too many registrations; retry in a minute", {
					"Retry-After": "60",
				});
			}
		}

		// For CIMD clients the library also accepts private_key_jwt, and it stores each assertion's
		// jti in KV as soon as the signature checks out, before it looks at the code or refresh
		// token. Anyone can publish a metadata document with their own keys, so that would be one KV
		// write per anonymous request. Claude's clients authenticate as public clients (none), so
		// this server takes no client assertions and doesn't advertise them.
		if (url.pathname === "/token" && request.method === "POST" && (await sendsClientAssertion(request))) {
			return oauthError(request, 401, "invalid_client", "private_key_jwt client authentication is not supported");
		}

		// The library caches its helpers on env (env.OAUTH_PROVIDER), and env is shared by every
		// request in the isolate. A copy per request keeps one hostname's helpers, which carry its
		// resource URL, from serving another hostname.
		const response = await entry.provider.fetch(request, { ...env }, ctx);
		const isMetadata = url.pathname === AUTHORIZATION_SERVER_METADATA && request.method === "GET";
		return isMetadata ? withoutClientAssertions(response) : response;
	},
} satisfies ExportedHandler<Env>;
