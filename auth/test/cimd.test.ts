// Client ID Metadata Documents: the client_id is an HTTPS URL the server fetches. The Worker runs in
// the test's isolate, so stubbing global fetch stands in for the client's web server.

import { afterEach, describe, expect, it, vi } from "vitest";
import {
	CLAUDE_CALLBACK,
	ORIGIN,
	PASSPHRASE,
	RESOURCE,
	authorizeUrl,
	call,
	callCountingKv,
	freshIp,
	openSignIn,
	pkce,
	submitSignIn,
	token,
} from "./helpers";

const CLIENT_ID = "https://client.example/oauth/metadata.json";

function serveMetadata(redirectUris: string[], extra: Record<string, unknown> = {}): void {
	const realFetch = globalThis.fetch;
	vi.spyOn(globalThis, "fetch").mockImplementation((input, init) => {
		const url = input instanceof Request ? input.url : String(input);
		if (url !== CLIENT_ID) return realFetch(input, init);
		return Promise.resolve(
			Response.json({
				client_id: CLIENT_ID,
				client_name: "Metadata Client",
				redirect_uris: redirectUris,
				token_endpoint_auth_method: "none",
				grant_types: ["authorization_code", "refresh_token"],
				response_types: ["code"],
				...extra,
			}),
		);
	});
}

const base64url = (bytes: ArrayBuffer | Uint8Array): string =>
	btoa(String.fromCharCode(...new Uint8Array(bytes)))
		.replace(/\+/g, "-")
		.replace(/\//g, "_")
		.replace(/=+$/, "");

/** A key pair and a correctly signed private_key_jwt assertion for CLIENT_ID, as its owner would make. */
async function clientAssertion(): Promise<{ jwk: Record<string, unknown>; assertion: string }> {
	const keys = (await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, ["sign", "verify"])) as CryptoKeyPair;
	const jwk = await crypto.subtle.exportKey("jwk", keys.publicKey);
	const now = Math.floor(Date.now() / 1000);
	const encode = (value: unknown) => base64url(new TextEncoder().encode(JSON.stringify(value)));
	const signingInput = `${encode({ alg: "ES256", typ: "JWT", kid: "k1" })}.${encode({
		iss: CLIENT_ID,
		sub: CLIENT_ID,
		aud: `${ORIGIN}/token`,
		jti: crypto.randomUUID(),
		iat: now,
		exp: now + 300,
	})}`;
	const signature = await crypto.subtle.sign(
		{ name: "ECDSA", hash: "SHA-256" },
		keys.privateKey,
		new TextEncoder().encode(signingInput),
	);
	return { jwk: { ...jwk, kid: "k1", use: "sig", alg: "ES256" }, assertion: `${signingInput}.${base64url(signature)}` };
}

afterEach(() => {
	vi.restoreAllMocks();
});

describe("client ID metadata documents", () => {
	it("signs in a CIMD client as a public client with PKCE", async () => {
		serveMetadata([CLAUDE_CALLBACK]);
		const { verifier, challenge } = await pkce();
		const path = authorizeUrl({ client_id: CLIENT_ID, code_challenge: challenge });
		const ip = freshIp();

		const form = await openSignIn(path, ip);
		expect(form.response.status).toBe(200);
		expect(form.html).toContain("Metadata Client");
		expect(form.html).toContain("Published by <strong>client.example</strong>");

		const approved = await submitSignIn(path, ip, { passphrase: PASSPHRASE, decision: "approve" });
		expect(approved.status).toBe(302);
		const code = new URL(approved.headers.get("Location") ?? "").searchParams.get("code") ?? "";

		const exchanged = await token({
			grant_type: "authorization_code",
			code,
			redirect_uri: CLAUDE_CALLBACK,
			client_id: CLIENT_ID,
			code_verifier: verifier,
			resource: RESOURCE,
		});
		expect(exchanged.status).toBe(200);
		const { access_token } = await exchanged.json<{ access_token: string }>();
		const forwarded = await call("/mcp", { method: "POST", headers: { Authorization: `Bearer ${access_token}` }, body: "{}" });
		expect(forwarded.status).toBe(200);
	});

	it("writes nothing to KV for a page view (the library caches documents with the Cache API)", async () => {
		serveMetadata([CLAUDE_CALLBACK]);
		const { challenge } = await pkce();
		const { response, writes } = await callCountingKv(authorizeUrl({ client_id: CLIENT_ID, code_challenge: challenge }));
		expect(response.status).toBe(200);
		expect(await response.text()).toContain("Metadata Client");
		expect(writes).toEqual([]);
	});

	it("refuses client assertions, which the library would record in KV before checking the grant", async () => {
		// Anyone can publish a metadata document with their own key and sign valid assertions.
		const { jwk, assertion } = await clientAssertion();
		serveMetadata([CLAUDE_CALLBACK], { token_endpoint_auth_method: "private_key_jwt", jwks: { keys: [jwk] } });
		const { response, writes } = await callCountingKv("/token", {
			method: "POST",
			headers: { "Content-Type": "application/x-www-form-urlencoded" },
			body: new URLSearchParams({
				grant_type: "refresh_token",
				refresh_token: "owner:no-such-grant:secret",
				client_id: CLIENT_ID,
				client_assertion_type: "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
				client_assertion: assertion,
			}),
		});
		expect(response.status).toBe(401);
		expect(await response.json()).toMatchObject({ error: "invalid_client" });
		expect(writes).toEqual([]);
	});

	it("refuses a CIMD client whose redirect URI is not on the list", async () => {
		serveMetadata(["https://client.example/callback"]);
		const { challenge } = await pkce();
		const response = await call(
			authorizeUrl({ client_id: CLIENT_ID, code_challenge: challenge, redirect_uri: "https://client.example/callback" }),
		);
		expect(response.status).toBe(400);
		expect(response.headers.get("Location")).toBeNull();
		expect(await response.text()).toContain("not in this server");
	});
});
