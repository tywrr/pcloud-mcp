// /authorize: the owner's sign-in page. One owner, one passphrase, no accounts.
//
// GET validates the request (client, redirect URI, PKCE), checks the redirect URI against
// ALLOWED_REDIRECT_URIS and shows the consent form. It stores nothing, because anyone can open
// this page and the free plan's 1,000 KV writes a day have to last for sign-ins and token
// refreshes. (parseAuthRequest only reads: a registered client from KV, or a client metadata
// document, which the library caches with the Cache API, not KV.) The form has no action, so it
// posts back to the same URL with the authorization request still in the query string. POST
// checks the passphrase, then parses and checks that request again from scratch and completes the
// authorization, which writes the grant: only the owner can make this page write to KV.
//
// The passphrase is also the CSRF protection. Nothing ties the POST to an earlier GET, and nothing
// needs to: a page on another site can make the owner's browser post this form, but it doesn't
// know the passphrase, so it can't approve anything. All it can do is deny, which sends an
// access_denied error to an allowed redirect URI. No site can frame the page either
// (frame-ancestors 'none', X-Frame-Options: DENY), so none can lay its own content over the real
// form to trick the owner into typing the passphrase and clicking Allow (clickjacking).

import {
	type AuthRequest,
	AuthorizationError,
	CimdFetchError,
	authorizationErrorRedirect,
} from "@cloudflare/workers-oauth-provider";
import { type AppEnv, OWNER_USER_ID, grantedScopes } from "./config";
import { htmlResponse, messageResponse, signInPage } from "./page";
import type { RedirectPolicy } from "./redirects";
import { passphraseConfigured, passphraseMatches, withinLimit } from "./security";

const RETRY_AFTER = new Headers({ "Retry-After": "60" });

export async function handleAuthorize(request: Request, env: AppEnv, policy: RedirectPolicy): Promise<Response> {
	try {
		if (request.method === "GET") return await showSignIn(request, env, policy);
		if (request.method === "POST") return await submitSignIn(request, env, policy);
		return new Response("Method Not Allowed", { status: 405, headers: { Allow: "GET, POST" } });
	} catch (error) {
		return authorizeError(error, policy);
	}
}

async function showSignIn(request: Request, env: AppEnv, policy: RedirectPolicy): Promise<Response> {
	// Page views write nothing, but each one reads KV or fetches a client metadata document.
	if (!(await withinLimit(env.SIGNIN_LIMITER, "authorize", request))) return tooManyViews();
	const authRequest = await env.OAUTH_PROVIDER.parseAuthRequest(request);
	const refusal = refuse(authRequest, policy);
	if (refusal) return refusal;
	if (!passphraseConfigured(env.OWNER_PASSPHRASE)) return notConfigured();
	return consentPage(env, authRequest);
}

async function submitSignIn(request: Request, env: AppEnv, policy: RedirectPolicy): Promise<Response> {
	let form: FormData;
	try {
		form = await request.formData();
	} catch {
		return messageResponse(400, "Bad request", "The sign-in form could not be read. Start again from Claude.");
	}

	if (form.get("decision") !== "approve") return deny(request, env, policy);

	// Counted before the comparison, so a burst of guesses is refused without being checked.
	if (!(await withinLimit(env.PASSPHRASE_LIMITER, "passphrase", request))) {
		return messageResponse(429, "Too many attempts", "Wait a minute, then try the passphrase again.", RETRY_AFTER);
	}
	if (!passphraseConfigured(env.OWNER_PASSPHRASE)) return notConfigured();
	if (!(await passphraseMatches(String(form.get("passphrase") ?? ""), env.OWNER_PASSPHRASE))) {
		console.warn("authorize: wrong passphrase");
		return wrongPassphrase(request, env, policy);
	}

	// The request is the one in this URL, checked as strictly as at GET; the form carries no part of it.
	const authRequest = await env.OAUTH_PROVIDER.parseAuthRequest(request);
	const refusal = refuse(authRequest, policy);
	if (refusal) return refusal;
	const { redirectTo } = await env.OAUTH_PROVIDER.completeAuthorization({
		request: authRequest,
		userId: OWNER_USER_ID,
		metadata: { approvedAt: new Date().toISOString() },
		scope: grantedScopes(authRequest.scope),
		props: {},
	});
	return redirect(redirectTo);
}

/** Needs no passphrase: telling the client "no" (error, state, iss) is safe for anyone to trigger. */
async function deny(request: Request, env: AppEnv, policy: RedirectPolicy): Promise<Response> {
	if (!(await withinLimit(env.SIGNIN_LIMITER, "authorize", request))) return tooManyViews();
	const authRequest = await env.OAUTH_PROVIDER.parseAuthRequest(request);
	if (!policy.allows(authRequest.redirectUri)) return redirectNotAllowed(authRequest.redirectUri);
	return redirect(authorizationErrorRedirect(authRequest, "access_denied"));
}

/** Rules on top of the library's own checks: an allowed redirect URI, and PKCE for every client. */
function refuse(authRequest: AuthRequest, policy: RedirectPolicy): Response | undefined {
	if (!policy.allows(authRequest.redirectUri)) return redirectNotAllowed(authRequest.redirectUri);
	// The library requires PKCE only from public clients; confidential DCR clients could skip it.
	if (!authRequest.codeChallenge || authRequest.codeChallengeMethod !== "S256") {
		return redirect(authorizationErrorRedirect(authRequest, "invalid_request", "PKCE with S256 is required"));
	}
	return undefined;
}

async function consentPage(env: AppEnv, authRequest: AuthRequest, wrong?: { error: string }): Promise<Response> {
	const details = await env.OAUTH_PROVIDER.describeConsent(authRequest);
	return htmlResponse(
		(nonce) => signInPage(nonce, { details, scopes: grantedScopes(authRequest.scope), error: wrong?.error }),
		{ status: wrong ? 401 : 200, formAction: formTargets(authRequest) },
	);
}

/** Shows the form again; it posts to this same URL, so the request is still in the query string. */
async function wrongPassphrase(request: Request, env: AppEnv, policy: RedirectPolicy): Promise<Response> {
	const authRequest = await env.OAUTH_PROVIDER.parseAuthRequest(request).catch(() => undefined);
	if (!authRequest || refuse(authRequest, policy)) {
		return messageResponse(401, "Wrong passphrase", "Start the sign-in again from Claude.");
	}
	return consentPage(env, authRequest, { error: "Wrong passphrase. Try again." });
}

/** The form posts here, and its response redirects to the client's callback. */
function formTargets(authRequest: AuthRequest): string[] {
	return ["'self'", new URL(authRequest.redirectUri).origin];
}

function redirect(location: string): Response {
	return new Response(null, { status: 302, headers: { Location: location, "Cache-Control": "no-store" } });
}

function tooManyViews(): Response {
	return messageResponse(429, "Too many requests", "Wait a minute, then start the sign-in again from Claude.", RETRY_AFTER);
}

function redirectNotAllowed(redirectUri: string): Response {
	const host = URL.parse(redirectUri)?.host || redirectUri;
	return messageResponse(
		400,
		"Sign-in refused",
		`This sign-in would send access to ${host}, which is not in this server's ALLOWED_REDIRECT_URIS.`,
	);
}

function notConfigured(): Response {
	return messageResponse(
		500,
		"Not set up yet",
		"OWNER_PASSPHRASE is missing or too short (at least 12 characters). Set it with `npx wrangler secret put OWNER_PASSPHRASE`.",
	);
}

/**
 * What the OAuth library throws for a bad request. A redirect back to the client is safe only
 * when the library validated its redirect URI and the URI is on our list; anything else is shown
 * here. Unexpected errors (KV outages, bugs) surface as a 500.
 */
function authorizeError(error: unknown, policy: RedirectPolicy): Response {
	if (error instanceof AuthorizationError) {
		if (error.redirectTo && error.redirectUri && policy.allows(error.redirectUri)) return redirect(error.redirectTo);
		return messageResponse(400, "Sign-in failed", error.description);
	}
	if (error instanceof CimdFetchError) {
		console.warn(`authorize: client metadata document ${error.metadataUrl} failed: ${error.detail}`);
		return messageResponse(400, "Sign-in failed", "The app's client metadata document could not be fetched or is not valid.");
	}
	throw error;
}
