// PUBLIC_URL and ALLOWED_REDIRECT_URIS, exercised by calling the handler with an adjusted env.

import { createExecutionContext } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import worker from "../src/index";
import { RedirectPolicy } from "../src/redirects";

function fetchWith(overrides: Partial<Env>, url: string, init?: RequestInit): Promise<Response> {
	// The handler's type expects an incoming request's `cf` object, which a test request lacks.
	const request = new Request(url, init) as Request<unknown, IncomingRequestCfProperties>;
	return worker.fetch(request, { ...env, ...overrides }, createExecutionContext());
}

describe("PUBLIC_URL", () => {
	const PUBLIC_URL = "https://mcp.example.net";

	it("pins the resource to PUBLIC_URL", async () => {
		const response = await fetchWith({ PUBLIC_URL }, `${PUBLIC_URL}/.well-known/oauth-protected-resource/mcp`);
		expect(response.status).toBe(200);
		expect(await response.json()).toMatchObject({ resource: `${PUBLIC_URL}/mcp`, authorization_servers: [PUBLIC_URL] });
	});

	it("answers 421 on another hostname, except /healthz", async () => {
		const other = await fetchWith({ PUBLIC_URL }, "https://pcloud-mcp.someone.workers.dev/mcp", { method: "POST" });
		expect(other.status).toBe(421);
		const health = await fetchWith({ PUBLIC_URL }, "https://pcloud-mcp.someone.workers.dev/healthz");
		expect(health.status).toBe(200);
	});

	it("rejects a PUBLIC_URL with a path", async () => {
		const response = await fetchWith({ PUBLIC_URL: `${PUBLIC_URL}/mcp` }, `${PUBLIC_URL}/mcp`, { method: "POST" });
		expect(response.status).toBe(500);
	});

	it("refuses plain http on a public host", async () => {
		const response = await fetchWith({}, "http://mcp.example.net/.well-known/oauth-authorization-server");
		expect(response.status).toBe(500);
		expect(await response.text()).toContain("OAuth configuration error");
	});
});

describe("ALLOWED_REDIRECT_URIS", () => {
	it("defaults to Claude's callbacks and loopback", () => {
		const policy = new RedirectPolicy("");
		expect(policy.allows("https://claude.ai/api/mcp/auth_callback")).toBe(true);
		expect(policy.allows("https://claude.com/api/mcp/auth_callback")).toBe(true);
		expect(policy.allows("http://localhost:6274/oauth/callback")).toBe(true);
		expect(policy.allows("http://127.0.0.1:33418/callback")).toBe(true);
		expect(policy.allows("http://127.0.0.2:8080/cb")).toBe(false);
		expect(policy.allows("https://claude.ai/api/mcp/auth_callback/")).toBe(false);
		expect(policy.allows("https://localhost/callback")).toBe(false);
		expect(policy.allows("http://localhost.evil.example/callback")).toBe(false);
		expect(policy.allows("http://user@localhost/callback")).toBe(false);
		expect(policy.allows("not a url")).toBe(false);
	});

	it("drops loopback when the keyword is left out", async () => {
		const only = "https://claude.ai/api/mcp/auth_callback";
		const policy = new RedirectPolicy(only);
		expect(policy.allows("http://localhost:6274/oauth/callback")).toBe(false);

		const response = await fetchWith({ ALLOWED_REDIRECT_URIS: only }, "https://pcloud-mcp.example.com/register", {
			method: "POST",
			headers: { "Content-Type": "application/json", "CF-Connecting-IP": "127.0.0.200" },
			body: JSON.stringify({ redirect_uris: ["http://localhost:6274/oauth/callback"], token_endpoint_auth_method: "none" }),
		});
		expect(response.status).toBe(400);
		expect(await response.json()).toMatchObject({ error: "invalid_redirect_uri" });
	});
});

describe("several hostnames without PUBLIC_URL", () => {
	it("keeps each hostname's resource separate within one isolate", async () => {
		const second = "https://second.example.org";
		// A request that reaches the first hostname's default handler, which is where the OAuth
		// library attaches its helpers to env.
		await exports.default.fetch("https://pcloud-mcp.example.com/nope");

		const registered = await exports.default.fetch(
			new Request(`${second}/register`, {
				method: "POST",
				headers: { "Content-Type": "application/json", "CF-Connecting-IP": "127.0.0.201" },
				body: JSON.stringify({
					redirect_uris: ["https://claude.ai/api/mcp/auth_callback"],
					token_endpoint_auth_method: "none",
				}),
			}),
		);
		const { client_id } = await registered.json<{ client_id: string }>();
		const query = new URLSearchParams({
			response_type: "code",
			client_id,
			redirect_uri: "https://claude.ai/api/mcp/auth_callback",
			state: "s",
			code_challenge: "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
			code_challenge_method: "S256",
			resource: `${second}/mcp`,
		});
		const page = await exports.default.fetch(
			new Request(`${second}/authorize?${query}`, {
				headers: { "CF-Connecting-IP": "198.51.100.8" },
				redirect: "manual",
			}),
		);
		expect(page.status).toBe(200);
		expect(await page.text()).toContain('name="passphrase"');
	});
});
