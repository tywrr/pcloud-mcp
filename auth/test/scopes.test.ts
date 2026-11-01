// files:read and files:write: what the owner grants on the consent page, and what reaches the core.

import { afterEach, describe, expect, it, vi } from "vitest";
import { grantedScopes } from "../src/config";
import {
	type Echo,
	ORIGIN,
	PASSPHRASE,
	authorizeUrl,
	call,
	freshIp,
	openSignIn,
	pkce,
	registerPublicClient,
	signIn,
	submitSignIn,
	token,
} from "./helpers";

function mcp(accessToken: string, headers: Record<string, string> = {}, overrides: Partial<Env> = {}): Promise<Response> {
	return call(
		"/mcp",
		{
			method: "POST",
			headers: { Authorization: `Bearer ${accessToken}`, "Content-Type": "application/json", ...headers },
			body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" }),
		},
		freshIp(),
		overrides,
	);
}

async function forwardedHeaders(accessToken: string, headers: Record<string, string> = {}): Promise<Record<string, string>> {
	const response = await mcp(accessToken, headers);
	expect(response.status).toBe(200);
	return (await response.json<Echo>()).headers;
}

describe("consent page", () => {
	it("offers write access with an unticked checkbox", async () => {
		const client = await registerPublicClient();
		const { challenge } = await pkce();
		const path = authorizeUrl({ client_id: client.client_id, code_challenge: challenge, scope: "files:read files:write" });
		const { response, html } = await openSignIn(path, freshIp());
		expect(response.status).toBe(200);
		expect(html).toContain("Also allow changes (create, move, upload, delete)");
		const checkbox = html.match(/<input type="checkbox"[^>]*>/)?.[0] ?? "";
		expect(checkbox).toContain('name="allow_write"');
		expect(checkbox).not.toContain("checked");
		// The scopes listed are what Allow grants as the page stands, so no files:write yet.
		expect(html).not.toContain("<li><code>files:write</code></li>");
	});

	it("comes back unticked after a wrong passphrase, even if the post ticked it", async () => {
		const client = await registerPublicClient();
		const { challenge } = await pkce();
		const path = authorizeUrl({ client_id: client.client_id, code_challenge: challenge });
		const wrong = await submitSignIn(path, freshIp(), { passphrase: "not-it", decision: "approve", allow_write: "yes" });
		expect(wrong.status).toBe(401);
		const checkbox = (await wrong.text()).match(/<input type="checkbox"[^>]*>/)?.[0] ?? "";
		expect(checkbox).toContain('name="allow_write"');
		expect(checkbox).not.toContain("checked");
	});

	it("ignores allow_write on a denial", async () => {
		const client = await registerPublicClient();
		const { challenge } = await pkce();
		const path = authorizeUrl({ client_id: client.client_id, code_challenge: challenge });
		const denied = await submitSignIn(path, freshIp(), { decision: "deny", allow_write: "yes", passphrase: PASSPHRASE });
		expect(denied.status).toBe(302);
		expect(new URL(denied.headers.get("Location") ?? "").searchParams.get("error")).toBe("access_denied");
	});
});

describe("granted scopes", () => {
	it("grants read only when the owner leaves the box unticked", async () => {
		const { tokens } = await signIn();
		expect(tokens.scope).toBe("files:read");
		expect((await forwardedHeaders(tokens.access_token))["x-cof-scopes"]).toBe("files:read");
	});

	it("grants read and write when the owner ticks the box", async () => {
		const { tokens } = await signIn({ scope: "files:read files:write", allowWrite: true });
		expect(tokens.scope).toBe("files:read files:write");
		expect((await forwardedHeaders(tokens.access_token))["x-cof-scopes"]).toBe("files:read files:write");
	});

	it("gives a client that asks for files:write only read unless the owner ticks the box", async () => {
		const { tokens } = await signIn({ scope: "files:read files:write" });
		expect(tokens.scope).toBe("files:read");
		expect((await forwardedHeaders(tokens.access_token))["x-cof-scopes"]).toBe("files:read");
	});

	it("lets the owner give write to a client that asked only for read", async () => {
		const { tokens } = await signIn({ scope: "files:read", allowWrite: true });
		expect(tokens.scope).toBe("files:read files:write");
	});

	it("keeps offline_access on the grant but forwards only file scopes", async () => {
		const { tokens } = await signIn({ scope: "files:read offline_access", allowWrite: true });
		expect(tokens.scope.split(" ").sort()).toEqual(["files:read", "files:write", "offline_access"]);
		expect((await forwardedHeaders(tokens.access_token))["x-cof-scopes"]).toBe("files:read files:write");
	});

	it("keeps the scopes across refreshes", async () => {
		const { client, tokens } = await signIn({ allowWrite: true });
		let refreshToken = tokens.refresh_token;
		for (let round = 0; round < 2; round += 1) {
			const refreshed = await token({ grant_type: "refresh_token", refresh_token: refreshToken, client_id: client.client_id });
			expect(refreshed.status).toBe(200);
			const next = await refreshed.json<{ access_token: string; refresh_token: string; scope: string }>();
			expect(next.scope).toBe("files:read files:write");
			expect((await forwardedHeaders(next.access_token))["x-cof-scopes"]).toBe("files:read files:write");
			refreshToken = next.refresh_token;
		}
	});

	it("forwards a refresh's narrower scope, and refuses a token without files:read", async () => {
		const { client, tokens } = await signIn({ scope: "files:read offline_access", allowWrite: true });
		const refresh = (refreshToken: string, scope: string) =>
			token({ grant_type: "refresh_token", refresh_token: refreshToken, client_id: client.client_id, scope });

		const readOnly = await refresh(tokens.refresh_token, "files:read");
		expect(readOnly.status).toBe(200);
		const narrowed = await readOnly.json<{ access_token: string; refresh_token: string }>();
		expect((await forwardedHeaders(narrowed.access_token))["x-cof-scopes"]).toBe("files:read");

		const noRead = await refresh(narrowed.refresh_token, "offline_access");
		expect(noRead.status).toBe(200);
		const { access_token } = await noRead.json<{ access_token: string }>();
		const refused = await mcp(access_token);
		expect(refused.status).toBe(403);
		expect(refused.headers.get("WWW-Authenticate")).toContain('error="insufficient_scope"');
		expect(refused.headers.get("WWW-Authenticate")).toContain('scope="files:read"');
	});
});

describe("headers to the core", () => {
	it("strips every client-sent X-CoF-* header and sets X-CoF-Scopes and X-CoF-Public-Url", async () => {
		const { tokens } = await signIn();
		const headers = await forwardedHeaders(tokens.access_token, {
			"X-CoF-Scopes": "files:read files:write",
			"X-CoF-Public-Url": "https://evil.example",
			"x-cof-client-ip": "203.0.113.9",
			"X-COF-Anything": "1",
			"X-Other": "kept",
		});
		const cof = Object.keys(headers)
			.filter((name) => name.startsWith("x-cof-"))
			.sort();
		expect(cof).toEqual(["x-cof-public-url", "x-cof-scopes"]);
		expect(headers["x-cof-scopes"]).toBe("files:read");
		expect(headers["x-cof-public-url"]).toBe(ORIGIN);
		expect(headers["x-other"]).toBe("kept");
	});

	it("sends /mcp to the core's own address through the binding", async () => {
		const { tokens } = await signIn();
		const response = await call(`/mcp?probe=1`, { method: "GET", headers: { Authorization: `Bearer ${tokens.access_token}` } });
		const echo = await response.json<Echo>();
		expect(echo.url).toBe("http://127.0.0.1:8080/mcp?probe=1");
		expect(echo.method).toBe("GET");
	});
});

describe("CORE_URL", () => {
	afterEach(() => {
		vi.restoreAllMocks();
	});

	it("sends /mcp to a loopback core instead of the binding", async () => {
		const { tokens } = await signIn();
		const seen: string[] = [];
		vi.spyOn(globalThis, "fetch").mockImplementation(async (input, init) => {
			const request = new Request(input, init);
			seen.push(`${request.url} ${request.headers.get("Authorization")} ${request.headers.get("X-CoF-Scopes")}`);
			return new Response("local core");
		});
		const response = await mcp(tokens.access_token, {}, { CORE_URL: "http://127.0.0.1:18080" });
		expect(await response.text()).toBe("local core");
		expect(seen).toEqual(["http://127.0.0.1:18080/mcp Bearer test-internal-token files:read"]);
	});

	it.each(["https://core.example.com", "http://core.example.com", "http://127.0.0.1:8080/base", "not a url"])(
		"refuses %s, which isn't a loopback http origin",
		async (coreUrl) => {
			const { tokens } = await signIn();
			const fetchSpy = vi.spyOn(globalThis, "fetch");
			const response = await mcp(tokens.access_token, {}, { CORE_URL: coreUrl });
			expect(response.status).toBe(500);
			expect(await response.text()).toContain("CORE_URL must be a loopback origin");
			expect(fetchSpy).not.toHaveBeenCalled();
		},
	);
});

describe("grantedScopes", () => {
	it("always has files:read, files:write only when allowed, and keeps other supported scopes", () => {
		expect(grantedScopes([], false)).toEqual(["files:read"]);
		expect(grantedScopes(["files:write"], false)).toEqual(["files:read"]);
		expect(grantedScopes([], true)).toEqual(["files:read", "files:write"]);
		expect(grantedScopes(["offline_access", "offline_access", "admin", "files:write"], true)).toEqual([
			"files:read",
			"files:write",
			"offline_access",
		]);
	});
});
