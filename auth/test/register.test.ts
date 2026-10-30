// Dynamic Client Registration is open only to DCR_ALLOWED_CIDRS, because each registration is a
// KV write and the free plan has 1,000 a day.

import { exports } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { CLAUDE_CALLBACK, ORIGIN, call, callCountingKv, freshIp, freshRegistrantIp, registrationRequest } from "./helpers";

describe("client registration", () => {
	it.each([
		["IPv4 loopback (wrangler dev)", "127.0.0.1"],
		["IPv4 loopback (wrangler dev)", "127.200.0.9"],
		["IPv6 loopback (wrangler dev)", "::1"],
	])("accepts %s (%s) by default", async (_label, ip) => {
		const response = await call("/register", registrationRequest([CLAUDE_CALLBACK]), ip);
		expect(response.status).toBe(201);
		expect(await response.json()).toHaveProperty("client_id");
	});

	it.each([
		["the internet", "198.51.100.7"],
		["Anthropic's outbound range, which anyone using Claude can send from", "160.79.104.9"],
		["IPv6", "2001:db8::1"],
	])("refuses %s (%s) with 403 and writes nothing", async (_label, ip) => {
		const { response, writes } = await callCountingKv("/register", registrationRequest([CLAUDE_CALLBACK]), ip);
		expect(response.status).toBe(403);
		expect(response.headers.get("Cache-Control")).toBe("no-store");
		expect(await response.json()).toEqual({
			error: "access_denied",
			error_description: "Client registration is not open to this address",
		});
		expect(writes).toEqual([]);
	});

	it("refuses a request without CF-Connecting-IP", async () => {
		const response = await exports.default.fetch(new Request(`${ORIGIN}/register`, registrationRequest([CLAUDE_CALLBACK])));
		expect(response.status).toBe(403);
	});

	it("lets a browser-based client read the refusal", async () => {
		const init = registrationRequest([CLAUDE_CALLBACK]);
		const response = await call("/register", { ...init, headers: { ...init.headers, Origin: "http://localhost:6274" } });
		expect(response.status).toBe(403);
		expect(response.headers.get("Access-Control-Allow-Origin")).toBe("http://localhost:6274");
	});

	it("uses the defaults when DCR_ALLOWED_CIDRS is empty", async () => {
		const overrides = { DCR_ALLOWED_CIDRS: "" };
		const allowed = await callCountingKv("/register", registrationRequest([CLAUDE_CALLBACK]), freshRegistrantIp(), overrides);
		expect(allowed.response.status).toBe(201);
		expect(allowed.writes).toHaveLength(1);
		const refused = await callCountingKv("/register", registrationRequest([CLAUDE_CALLBACK]), freshIp(), overrides);
		expect(refused.response.status).toBe(403);
	});

	it("follows DCR_ALLOWED_CIDRS, and allows everyone with *", async () => {
		const own = { DCR_ALLOWED_CIDRS: "198.51.100.0/24, 2001:db8:5::/48" };
		const register = (ip: string, overrides: Partial<Env>) =>
			callCountingKv("/register", registrationRequest([CLAUDE_CALLBACK]), ip, overrides);
		expect((await register("198.51.100.9", own)).response.status).toBe(201);
		expect((await register("2001:db8:5:1::9", own)).response.status).toBe(201);
		expect((await register("160.79.104.9", own)).response.status).toBe(403);
		expect((await register("127.0.0.1", own)).response.status).toBe(403);
		expect((await register("203.0.113.9", { DCR_ALLOWED_CIDRS: "*" })).response.status).toBe(201);
		const anthropic = { DCR_ALLOWED_CIDRS: "160.79.104.0/21,127.0.0.0/8,::1" };
		expect((await register("160.79.111.255", anthropic)).response.status).toBe(201);
		expect((await register("160.79.112.0", anthropic)).response.status).toBe(403);
	});

	it("still advertises the registration endpoint", async () => {
		const response = await call("/.well-known/oauth-authorization-server");
		expect(await response.json()).toMatchObject({ registration_endpoint: `${ORIGIN}/register` });
	});
});
