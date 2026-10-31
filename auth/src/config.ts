// Fixed OAuth settings shared by the provider and the sign-in page.

import type { OAuthHelpers } from "@cloudflare/workers-oauth-provider";

/** The Worker's bindings plus the helpers the OAuth library adds before calling our handlers. */
export type AppEnv = Env & { OAUTH_PROVIDER: OAuthHelpers };

/** Read access to the drive: what every grant carries and every client asks for first. */
export const READ_SCOPE = "files:read";

/**
 * Changes (create, move, upload, delete to trash). Granted only when the owner ticks the box on
 * the consent page; the core hides and refuses write tools without it.
 */
export const WRITE_SCOPE = "files:write";

/** The scopes the core understands; only these are forwarded to it in X-CoF-Scopes. */
export const FILE_SCOPES = [READ_SCOPE, WRITE_SCOPE];

/**
 * Everything this server can grant. offline_access is listed because Claude adds it to its
 * request when advertised; refresh tokens are issued either way.
 */
export const SCOPES_SUPPORTED = [READ_SCOPE, WRITE_SCOPE, "offline_access"];

/** One user: the owner. Grants are stored under this ID. */
export const OWNER_USER_ID = "owner";

export const ACCESS_TOKEN_TTL = 60 * 60;
/** A grant lasts 30 days from sign-in and is extended by 30 days on each refresh. */
export const REFRESH_TOKEN_TTL = 30 * 24 * 60 * 60;
export const REFRESH_TOKEN_IDLE_TTL = 30 * 24 * 60 * 60;

/**
 * The scopes approving a request grants: the read scope always, files:write exactly when the
 * owner ticked it (whatever the client asked for, so a client can't get it by asking and the owner
 * can give it to a client that only asked for read), and any other supported scope asked for.
 */
export function grantedScopes(requested: readonly string[], allowWrite: boolean): string[] {
	const others = requested.filter((scope) => SCOPES_SUPPORTED.includes(scope) && !FILE_SCOPES.includes(scope));
	return [READ_SCOPE, ...(allowWrite ? [WRITE_SCOPE] : []), ...new Set(others)];
}
