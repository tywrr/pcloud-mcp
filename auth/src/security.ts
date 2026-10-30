// Small security helpers: secret comparison, rate limiting and response headers.

import { limiterKey } from "./ip";

/** The shortest OWNER_PASSPHRASE the sign-in page accepts; shorter ones mean "not configured". */
export const MIN_PASSPHRASE_LENGTH = 12;

/**
 * Compares the typed passphrase with the configured one in constant time. Hashing both first
 * gives equal-length inputs, so neither the content nor the length of the secret leaks through
 * timing.
 */
export async function passphraseMatches(given: string, expected: string | undefined): Promise<boolean> {
	if (!passphraseConfigured(expected)) return false;
	const encoder = new TextEncoder();
	const [a, b] = await Promise.all([
		crypto.subtle.digest("SHA-256", encoder.encode(given)),
		crypto.subtle.digest("SHA-256", encoder.encode(expected)),
	]);
	return crypto.subtle.timingSafeEqual(a, b);
}

export function passphraseConfigured(passphrase: string | undefined): passphrase is string {
	return typeof passphrase === "string" && passphrase.length >= MIN_PASSPHRASE_LENGTH;
}

/** The caller's address as Cloudflare saw it. Absent only in some local setups. */
export function clientIp(request: Request): string | undefined {
	return request.headers.get("CF-Connecting-IP") ?? undefined;
}

/**
 * True when the caller (by address, or /64 for IPv6) is still under the limiter's budget for
 * `scope`. Every call counts as one attempt.
 */
export async function withinLimit(limiter: RateLimit, scope: string, request: Request): Promise<boolean> {
	const { success } = await limiter.limit({ key: `${scope}:${limiterKey(clientIp(request))}` });
	return success;
}

/** A fresh nonce for the one inline stylesheet on each HTML page. */
export function newNonce(): string {
	const bytes = crypto.getRandomValues(new Uint8Array(16));
	return btoa(String.fromCharCode(...bytes));
}

/**
 * Headers for every HTML page. `formAction` lists where the sign-in form may post and be
 * redirected: browsers apply form-action to the redirect after a POST, so the client's
 * redirect origin has to be allowed next to 'self'.
 */
export function htmlHeaders(nonce: string, formAction: string[] = []): Headers {
	const formSources = formAction.length > 0 ? formAction.join(" ") : "'none'";
	return new Headers({
		"Content-Type": "text/html; charset=utf-8",
		"Content-Security-Policy": [
			"default-src 'none'",
			`style-src 'nonce-${nonce}'`,
			`form-action ${formSources}`,
			"frame-ancestors 'none'",
			"base-uri 'none'",
		].join("; "),
		"Cache-Control": "no-store",
		"Referrer-Policy": "no-referrer",
		"X-Content-Type-Options": "nosniff",
		"X-Frame-Options": "DENY",
		"Cross-Origin-Opener-Policy": "same-origin",
	});
}
