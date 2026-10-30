// Fixed OAuth settings shared by the provider and the sign-in page.

import type { OAuthHelpers } from "@cloudflare/workers-oauth-provider";

/** The Worker's bindings plus the helpers the OAuth library adds before calling our handlers. */
export type AppEnv = Env & { OAUTH_PROVIDER: OAuthHelpers };

/** Read access to the shared folder: what every token carries and every client asks for first. */
export const READ_SCOPE = "files:read";

/**
 * Everything this server can grant. offline_access is listed because Claude adds it to its
 * request when advertised; refresh tokens are issued either way.
 */
export const SCOPES_SUPPORTED = [READ_SCOPE, "offline_access"];

/** One user: the owner. Grants are stored under this ID. */
export const OWNER_USER_ID = "owner";

export const ACCESS_TOKEN_TTL = 60 * 60;
/** A grant lasts 30 days from sign-in and is extended by 30 days on each refresh. */
export const REFRESH_TOKEN_TTL = 30 * 24 * 60 * 60;
export const REFRESH_TOKEN_IDLE_TTL = 30 * 24 * 60 * 60;

/** The scopes approving a request grants: the read scope always, plus any other supported ones asked for. */
export function grantedScopes(requested: readonly string[]): string[] {
	return [...new Set([READ_SCOPE, ...requested.filter((scope) => SCOPES_SUPPORTED.includes(scope))])];
}
