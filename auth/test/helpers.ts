// Drives the Worker the way an MCP client and the owner's browser would.

import { createExecutionContext } from "cloudflare:test";
import { env, exports } from "cloudflare:workers";
import worker from "../src/index";

export const ORIGIN = "https://chance-of-files.example.com";
export const RESOURCE = `${ORIGIN}/mcp`;
export const CLAUDE_CALLBACK = "https://claude.ai/api/mcp/auth_callback";
export const PASSPHRASE = "test-owner-passphrase";

let ipCounter = 0;
/**
 * A fresh caller address, so per-IP rate limits from other tests don't interfere. It is in
 * 198.18.0.0/15 (benchmarking, never routed), outside the default DCR_ALLOWED_CIDRS.
 */
export function freshIp(): string {
	ipCounter += 1;
	return `198.18.${(ipCounter >> 8) & 0xff}.${ipCounter & 0xff}`;
}

let registrantCounter = 0;
/** A fresh loopback address (127.0.0.0/8), which the default DCR_ALLOWED_CIDRS lets register. */
export function freshRegistrantIp(): string {
	registrantCounter += 1;
	return `127.1.${(registrantCounter >> 8) & 0xff}.${registrantCounter & 0xff}`;
}

let prefixCounter = 0;
/** A fresh IPv6 /64 (in 2001:db8::/32, for documentation); pass a host part to get an address in it. */
export function freshIpv6Prefix(): (host: string) => string {
	prefixCounter += 1;
	const prefix = `2001:db8:${prefixCounter.toString(16)}:1`;
	return (host) => `${prefix}:${host}`;
}

export function call(path: string, init: RequestInit = {}, ip = freshIp()): Promise<Response> {
	const headers = new Headers(init.headers);
	headers.set("CF-Connecting-IP", ip);
	return exports.default.fetch(new Request(new URL(path, ORIGIN), { ...init, headers, redirect: "manual" }));
}

/**
 * Like `call`, with an OAUTH_KV that records every write and delete ("put <key>", "delete <key>"),
 * to check what a request costs against the free plan's 1,000 KV writes a day.
 */
export async function callCountingKv(
	path: string,
	init: RequestInit = {},
	ip = freshIp(),
	overrides: Partial<Env> = {},
): Promise<{ response: Response; writes: string[] }> {
	const writes: string[] = [];
	const kv = env.OAUTH_KV;
	const counted = new Proxy(kv, {
		get(target, property) {
			const value: unknown = Reflect.get(target, property);
			if (typeof value !== "function") return value;
			if (property !== "put" && property !== "delete") return value.bind(target);
			return (key: string, ...rest: unknown[]) => {
				writes.push(`${property} ${key}`);
				return value.call(target, key, ...rest);
			};
		},
	});
	const headers = new Headers(init.headers);
	headers.set("CF-Connecting-IP", ip);
	// The handler's type expects an incoming request's `cf` object, which a test request lacks.
	const request = new Request(new URL(path, ORIGIN), { ...init, headers, redirect: "manual" }) as Request<
		unknown,
		IncomingRequestCfProperties
	>;
	const response = await worker.fetch(request, { ...env, ...overrides, OAUTH_KV: counted }, createExecutionContext());
	return { response, writes };
}

export interface Client {
	client_id: string;
	client_secret?: string;
}

export function registrationRequest(redirectUris: string[], extra: Record<string, unknown> = {}): RequestInit {
	return {
		method: "POST",
		headers: { "Content-Type": "application/json" },
		body: JSON.stringify({
			client_name: "Test Client",
			redirect_uris: redirectUris,
			token_endpoint_auth_method: "none",
			grant_types: ["authorization_code", "refresh_token"],
			response_types: ["code"],
			...extra,
		}),
	};
}

/** Registers the way claude.ai would, from Anthropic's outbound range. */
export async function register(redirectUris: string[], extra: Record<string, unknown> = {}): Promise<Response> {
	return call("/register", registrationRequest(redirectUris, extra), freshRegistrantIp());
}

export async function registerPublicClient(redirectUri = CLAUDE_CALLBACK): Promise<Client> {
	const response = await register([redirectUri]);
	if (response.status !== 201) throw new Error(`registration failed: ${response.status} ${await response.text()}`);
	return response.json();
}

const base64url = (bytes: Uint8Array): string =>
	btoa(String.fromCharCode(...bytes))
		.replace(/\+/g, "-")
		.replace(/\//g, "_")
		.replace(/=+$/, "");

export async function pkce(): Promise<{ verifier: string; challenge: string }> {
	const verifier = base64url(crypto.getRandomValues(new Uint8Array(32)));
	const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(verifier));
	return { verifier, challenge: base64url(new Uint8Array(digest)) };
}

export function authorizeUrl(params: Record<string, string>): string {
	const query = new URLSearchParams({
		response_type: "code",
		redirect_uri: CLAUDE_CALLBACK,
		scope: "files:read",
		state: "state-123",
		code_challenge_method: "S256",
		resource: RESOURCE,
		...params,
	});
	return `/authorize?${query}`;
}

export interface SignInForm {
	response: Response;
	html: string;
}

/** GET /authorize, as the owner's browser opens it. */
export async function openSignIn(path: string, ip: string): Promise<SignInForm> {
	const response = await call(path, {}, ip);
	return { response, html: await response.text() };
}

/** The form as the browser posts it: to the page's own URL, so `path` keeps the query string. */
export function signInForm(fields: Record<string, string>, headers: Record<string, string> = {}): RequestInit {
	return {
		method: "POST",
		headers: { "Content-Type": "application/x-www-form-urlencoded", ...headers },
		body: new URLSearchParams(fields),
	};
}

export function submitSignIn(path: string, ip: string, fields: Record<string, string>): Promise<Response> {
	return call(path, signInForm(fields), ip);
}

export function token(fields: Record<string, string>): Promise<Response> {
	return call("/token", {
		method: "POST",
		headers: { "Content-Type": "application/x-www-form-urlencoded" },
		body: new URLSearchParams(fields),
	});
}

export interface Tokens {
	access_token: string;
	refresh_token: string;
	expires_in: number;
	scope: string;
	token_type: string;
}

/** The browser part of the flow: register, sign in with the passphrase, return the code. */
export async function authorizeCode(): Promise<{ client: Client; verifier: string; code: string }> {
	const client = await registerPublicClient();
	const { verifier, challenge } = await pkce();
	const path = authorizeUrl({ client_id: client.client_id, code_challenge: challenge });
	const ip = freshIp();
	await openSignIn(path, ip);
	const approved = await submitSignIn(path, ip, { passphrase: PASSPHRASE, decision: "approve" });
	const location = new URL(approved.headers.get("Location") ?? "");
	return { client, verifier, code: location.searchParams.get("code") ?? "" };
}

export function exchangeCode(client: Client, code: string, verifier: string): Promise<Response> {
	return token({
		grant_type: "authorization_code",
		code,
		redirect_uri: CLAUDE_CALLBACK,
		client_id: client.client_id,
		code_verifier: verifier,
		resource: RESOURCE,
	});
}

/** The whole flow up to tokens. */
export async function signIn(): Promise<{ client: Client; tokens: Tokens }> {
	const { client, verifier, code } = await authorizeCode();
	const response = await exchangeCode(client, code, verifier);
	if (response.status !== 200) throw new Error(`token exchange failed: ${response.status} ${await response.text()}`);
	return { client, tokens: await response.json() };
}
