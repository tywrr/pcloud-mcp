import { describe, expect, it } from "vitest";
import {
	CLAUDE_CALLBACK,
	type Echo,
	ORIGIN,
	PASSPHRASE,
	authorizeCode,
	authorizeUrl,
	call,
	callCountingKv,
	exchangeCode,
	freshIp,
	freshIpv6Prefix,
	openSignIn,
	pkce,
	register,
	registerPublicClient,
	signIn,
	signInForm,
	submitSignIn,
	token,
} from "./helpers";

/** Registers a client and returns the /authorize URL its sign-in starts at. */
async function startSignIn(): Promise<{ path: string }> {
	const client = await registerPublicClient();
	const { challenge } = await pkce();
	return { path: authorizeUrl({ client_id: client.client_id, code_challenge: challenge }) };
}

describe("sign-in flow", () => {
	it("issues tokens after the passphrase and forwards /mcp with the internal token", async () => {
		const client = await registerPublicClient();
		const { verifier, challenge } = await pkce();
		const path = authorizeUrl({ client_id: client.client_id, code_challenge: challenge });
		const ip = freshIp();

		const form = await openSignIn(path, ip);
		expect(form.response.status).toBe(200);
		expect(form.html).toContain("Test Client");
		expect(form.html).toContain("claude.ai");
		expect(form.html).toContain("files:read");
		// No action: the form posts to the page's own URL, query string included.
		expect(form.html).toContain('<form method="post">');
		expect(form.html).not.toContain("handle");
		expect(form.response.headers.getSetCookie()).toEqual([]);

		const csp = form.response.headers.get("Content-Security-Policy") ?? "";
		expect(csp).toContain("default-src 'none'");
		expect(csp).toContain("frame-ancestors 'none'");
		expect(csp).toContain("form-action 'self' https://claude.ai");
		expect(form.response.headers.get("Cache-Control")).toBe("no-store");
		expect(form.response.headers.get("Referrer-Policy")).toBe("no-referrer");
		expect(form.response.headers.get("X-Frame-Options")).toBe("DENY");

		const approved = await submitSignIn(path, ip, { passphrase: PASSPHRASE, decision: "approve" });
		expect(approved.status).toBe(302);
		const location = new URL(approved.headers.get("Location") ?? "");
		expect(`${location.origin}${location.pathname}`).toBe(CLAUDE_CALLBACK);
		expect(location.searchParams.get("state")).toBe("state-123");
		expect(location.searchParams.get("iss")).toBe(ORIGIN);
		const code = location.searchParams.get("code") ?? "";
		expect(code).not.toBe("");

		const exchanged = await exchangeCode(client, code, verifier);
		expect(exchanged.status).toBe(200);
		const tokens = await exchanged.json<Record<string, unknown>>();
		expect(tokens).toMatchObject({ token_type: "bearer", expires_in: 3600, scope: "files:read" });
		expect(typeof tokens.refresh_token).toBe("string");
		const accessToken = String(tokens.access_token);

		const rpc = JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" });
		const forwarded = await call("/mcp", {
			method: "POST",
			headers: {
				Authorization: `Bearer ${accessToken}`,
				"Content-Type": "application/json",
				Cookie: "session=should-not-pass",
			},
			body: rpc,
		});
		expect(forwarded.status).toBe(200);
		const echo = await forwarded.json<Echo>();
		expect(echo.authorization).toBe("Bearer test-internal-token");
		expect(echo.authorization).not.toContain(accessToken);
		expect(echo.cookie).toBeNull();
		expect(echo.method).toBe("POST");
		expect(new URL(echo.url).pathname).toBe("/mcp");
		expect(echo.body).toBe(rpc);
	});

	it("writes nothing to KV for a page view, and only the grant on approval", async () => {
		const { path } = await startSignIn();
		const ip = freshIp();

		const view = await callCountingKv(path, {}, ip);
		expect(view.response.status).toBe(200);
		expect(await view.response.text()).toContain('name="passphrase"');
		expect(view.writes).toEqual([]);

		const approved = await callCountingKv(path, signInForm({ passphrase: PASSPHRASE, decision: "approve" }), ip);
		expect(approved.response.status).toBe(302);
		expect(approved.writes).toHaveLength(1);
		expect(approved.writes[0]).toMatch(/^put grant:owner:/);
	});

	it("refuses a code exchange with the wrong PKCE verifier", async () => {
		const { client, code } = await authorizeCode();
		const response = await exchangeCode(client, code, "x".repeat(43));
		expect(response.status).toBe(400);
		expect(await response.json()).toMatchObject({ error: "invalid_grant" });
	});

	it("rotates refresh tokens and refuses a superseded one", async () => {
		const { client, tokens } = await signIn();
		const refresh = (refreshToken: string) =>
			token({ grant_type: "refresh_token", refresh_token: refreshToken, client_id: client.client_id });

		const first = await refresh(tokens.refresh_token);
		expect(first.status).toBe(200);
		const second = await first.json<{ access_token: string; refresh_token: string }>();
		expect(second.refresh_token).not.toBe(tokens.refresh_token);
		expect(second.access_token).not.toBe(tokens.access_token);

		// Using the new refresh token retires the one before it.
		const third = await refresh(second.refresh_token);
		expect(third.status).toBe(200);
		const stale = await refresh(tokens.refresh_token);
		expect(stale.status).toBe(400);
		expect(await stale.json()).toMatchObject({ error: "invalid_grant" });

		const { access_token } = await third.json<{ access_token: string }>();
		const forwarded = await call("/mcp", { method: "POST", headers: { Authorization: `Bearer ${access_token}` }, body: "{}" });
		expect(forwarded.status).toBe(200);
	});

	it("shows the form again after a wrong passphrase, writes nothing, and lets the owner retry", async () => {
		const { path } = await startSignIn();
		const ip = freshIp();
		await openSignIn(path, ip);

		const wrong = await callCountingKv(path, signInForm({ passphrase: "not-the-passphrase", decision: "approve" }), ip);
		expect(wrong.response.status).toBe(401);
		expect(wrong.response.headers.get("Location")).toBeNull();
		expect(wrong.writes).toEqual([]);
		const html = await wrong.response.text();
		expect(html).toContain("Wrong passphrase");
		expect(html).toContain('name="passphrase"');
		expect(wrong.response.headers.get("Content-Security-Policy")).toContain("form-action 'self' https://claude.ai");

		const right = await submitSignIn(path, ip, { passphrase: PASSPHRASE, decision: "approve" });
		expect(right.status).toBe(302);
		expect(right.headers.get("Location")).toContain("code=");
	});

	it("rate-limits passphrase attempts per address", async () => {
		const { path } = await startSignIn();
		const ip = freshIp();
		const statuses: number[] = [];
		for (let attempt = 0; attempt < 6; attempt += 1) {
			const response = await submitSignIn(path, ip, { passphrase: `guess-${attempt}`, decision: "approve" });
			statuses.push(response.status);
		}
		expect(statuses.slice(0, 5)).toEqual([401, 401, 401, 401, 401]);
		expect(statuses[5]).toBe(429);
	});

	it("counts every address in an IPv6 /64 as one", async () => {
		const { path } = await startSignIn();
		const inPrefix = freshIpv6Prefix();
		const statuses: number[] = [];
		for (let attempt = 0; attempt < 6; attempt += 1) {
			const response = await submitSignIn(path, inPrefix(`${attempt}:0:0:1`), { passphrase: `guess-${attempt}`, decision: "approve" });
			statuses.push(response.status);
		}
		expect(statuses).toEqual([401, 401, 401, 401, 401, 429]);

		const elsewhere = freshIpv6Prefix()("0:0:0:1");
		expect((await submitSignIn(path, elsewhere, { passphrase: "guess", decision: "approve" })).status).toBe(401);
	});

	it("can't be approved by a cross-site POST without the passphrase (CSRF)", async () => {
		const { path } = await startSignIn();
		const forgedHeaders = { Origin: "https://evil.example", "Sec-Fetch-Site": "cross-site" };
		const attempts: Record<string, string>[] = [{ decision: "approve" }, { decision: "approve", passphrase: "" }];
		for (const fields of attempts) {
			const forged = await callCountingKv(path, signInForm(fields, forgedHeaders));
			expect(forged.response.status).toBe(401);
			expect(forged.response.headers.get("Location")).toBeNull();
			expect(await forged.response.text()).toContain("Wrong passphrase");
			expect(forged.writes).toEqual([]);
		}
	});

	it("checks the request again on POST, not just at GET", async () => {
		const registered = await register(["https://evil.example/callback", CLAUDE_CALLBACK]);
		const client = await registered.json<{ client_id: string }>();
		const { challenge } = await pkce();
		// The owner never saw a page for this request: GET would have refused it.
		const path = authorizeUrl({ client_id: client.client_id, code_challenge: challenge, redirect_uri: "https://evil.example/callback" });
		const posted = await callCountingKv(path, signInForm({ passphrase: PASSPHRASE, decision: "approve" }));
		expect(posted.response.status).toBe(400);
		expect(posted.response.headers.get("Location")).toBeNull();
		expect(posted.writes).toEqual([]);
	});

	it("sends access_denied back when the owner denies, without the passphrase", async () => {
		const { path } = await startSignIn();
		const ip = freshIp();
		await openSignIn(path, ip);
		const denied = await callCountingKv(path, signInForm({ decision: "deny" }), ip);
		expect(denied.response.status).toBe(302);
		expect(denied.writes).toEqual([]);
		const location = new URL(denied.response.headers.get("Location") ?? "");
		expect(`${location.origin}${location.pathname}`).toBe(CLAUDE_CALLBACK);
		expect(location.searchParams.get("error")).toBe("access_denied");
		expect(location.searchParams.get("state")).toBe("state-123");
		expect(location.searchParams.get("iss")).toBe(ORIGIN);
		expect(location.searchParams.get("code")).toBeNull();
	});

	it("shows a deny for a disallowed redirect URI here instead of redirecting", async () => {
		const registered = await register(["https://evil.example/callback", CLAUDE_CALLBACK]);
		const client = await registered.json<{ client_id: string }>();
		const { challenge } = await pkce();
		const path = authorizeUrl({ client_id: client.client_id, code_challenge: challenge, redirect_uri: "https://evil.example/callback" });
		const denied = await submitSignIn(path, freshIp(), { decision: "deny" });
		expect(denied.status).toBe(400);
		expect(denied.headers.get("Location")).toBeNull();
		expect(await denied.text()).toContain("evil.example");
	});
});

describe("redirect URI allowlist", () => {
	it("refuses to register a client with no allowed redirect URI", async () => {
		const response = await register(["https://evil.example/callback"]);
		expect(response.status).toBe(400);
		expect(await response.json()).toMatchObject({ error: "invalid_redirect_uri" });
	});

	it("refuses a disallowed redirect URI at /authorize before showing the page", async () => {
		const registered = await register(["https://evil.example/callback", CLAUDE_CALLBACK]);
		expect(registered.status).toBe(201);
		const client = await registered.json<{ client_id: string }>();
		const { challenge } = await pkce();
		const response = await call(
			authorizeUrl({ client_id: client.client_id, code_challenge: challenge, redirect_uri: "https://evil.example/callback" }),
		);
		expect(response.status).toBe(400);
		expect(response.headers.get("Location")).toBeNull();
		expect(response.headers.getSetCookie()).toEqual([]);
		const html = await response.text();
		expect(html).toContain("evil.example");
		expect(html).not.toContain('name="passphrase"');
	});

	it("accepts loopback redirects on any port and warns about them", async () => {
		const client = await registerPublicClient("http://localhost/callback");
		const { challenge } = await pkce();
		const path = authorizeUrl({
			client_id: client.client_id,
			code_challenge: challenge,
			redirect_uri: "http://localhost:33418/callback",
		});
		const form = await openSignIn(path, freshIp());
		expect(form.response.status).toBe(200);
		expect(form.html).toContain("an app on this computer");
		expect(form.response.headers.get("Content-Security-Policy")).toContain("form-action 'self' http://localhost:33418");
	});

	it("requires PKCE even from confidential clients", async () => {
		const registered = await register([CLAUDE_CALLBACK], { token_endpoint_auth_method: "client_secret_basic" });
		expect(registered.status).toBe(201);
		const client = await registered.json<{ client_id: string }>();
		const query = new URLSearchParams({
			response_type: "code",
			client_id: client.client_id,
			redirect_uri: CLAUDE_CALLBACK,
			state: "s",
		});
		const response = await call(`/authorize?${query}`);
		expect(response.status).toBe(302);
		const location = new URL(response.headers.get("Location") ?? "");
		expect(location.searchParams.get("error")).toBe("invalid_request");
	});
});

describe("PKCE", () => {
	it.each([
		["missing", {}],
		["plain", { code_challenge: "a".repeat(43), code_challenge_method: "plain" }],
	])("refuses a public client with %s PKCE", async (_label, pkceParams) => {
		const client = await registerPublicClient();
		const query = new URLSearchParams({
			response_type: "code",
			client_id: client.client_id,
			redirect_uri: CLAUDE_CALLBACK,
			state: "s",
			...pkceParams,
		});
		const response = await call(`/authorize?${query}`);
		expect(response.status).toBe(302);
		const location = new URL(response.headers.get("Location") ?? "");
		expect(location.searchParams.get("error")).toBe("invalid_request");
		expect(response.headers.getSetCookie()).toEqual([]);
	});
});
