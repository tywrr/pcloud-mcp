import { describe, expect, it } from "vitest";
import { ORIGIN, RESOURCE, call } from "./helpers";

describe("discovery", () => {
	it("serves authorization server metadata with CIMD, DCR, public clients and S256 PKCE", async () => {
		const response = await call("/.well-known/oauth-authorization-server");
		expect(response.status).toBe(200);
		const metadata = await response.json<Record<string, unknown>>();
		expect(metadata).toMatchObject({
			issuer: ORIGIN,
			authorization_endpoint: `${ORIGIN}/authorize`,
			token_endpoint: `${ORIGIN}/token`,
			registration_endpoint: `${ORIGIN}/register`,
			response_types_supported: ["code"],
			code_challenge_methods_supported: ["S256"],
			client_id_metadata_document_supported: true,
			scopes_supported: ["files:read", "offline_access"],
		});
		expect(metadata.token_endpoint_auth_methods_supported).toContain("none");
		// /token refuses client assertions, so they aren't advertised.
		expect(metadata.token_endpoint_auth_methods_supported).not.toContain("private_key_jwt");
		expect(metadata).not.toHaveProperty("token_endpoint_auth_signing_alg_values_supported");
		expect(metadata.grant_types_supported).toEqual(expect.arrayContaining(["authorization_code", "refresh_token"]));
	});

	it("answers HEAD and CORS preflight for the metadata", async () => {
		expect((await call("/.well-known/oauth-authorization-server", { method: "HEAD" })).status).toBe(200);
		const preflight = await call("/.well-known/oauth-authorization-server", {
			method: "OPTIONS",
			headers: { Origin: "http://localhost:6274", "Access-Control-Request-Method": "GET" },
		});
		expect(preflight.status).toBeLessThan(300);
	});

	it.each(["/.well-known/oauth-protected-resource/mcp", "/.well-known/oauth-protected-resource"])(
		"serves protected resource metadata for /mcp at %s",
		async (path) => {
			const response = await call(path);
			expect(response.status).toBe(200);
			expect(await response.json()).toEqual({
				resource: RESOURCE,
				authorization_servers: [ORIGIN],
				scopes_supported: ["files:read"],
				bearer_methods_supported: ["header"],
				resource_name: "pCloud files (Chance of Files)",
			});
		},
	);

	it("answers /mcp without a token with 401 and a resource_metadata challenge", async () => {
		const response = await call("/mcp", {
			method: "POST",
			headers: { "Content-Type": "application/json" },
			body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: {} }),
		});
		expect(response.status).toBe(401);
		const challenge = response.headers.get("WWW-Authenticate") ?? "";
		expect(challenge).toMatch(/^Bearer /);
		expect(challenge).toContain(`resource_metadata="${ORIGIN}/.well-known/oauth-protected-resource/mcp"`);
		expect(challenge).toContain('scope="files:read"');
	});

	it("rejects a token it did not issue", async () => {
		const response = await call("/mcp", { method: "POST", headers: { Authorization: "Bearer owner:grant:secret" } });
		expect(response.status).toBe(401);
		expect(response.headers.get("WWW-Authenticate")).toContain('error="invalid_token"');
	});

	it("answers /healthz without auth", async () => {
		const response = await call("/healthz");
		expect(response.status).toBe(200);
		expect(await response.text()).toBe("ok");
	});

	it.each(["/", "/nope", "/oauth/token"])("returns 404 for %s", async (path) => {
		expect((await call(path)).status).toBe(404);
	});
});
