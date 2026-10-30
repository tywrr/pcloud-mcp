// Where sign-in codes may go. Dynamic Client Registration lets anyone register a client with any
// redirect URI, and the OAuth library only checks a request against what its client registered,
// so this list is what keeps a stranger's client from receiving a code for the owner's files.

export const DEFAULT_ALLOWED_REDIRECT_URIS =
	"https://claude.ai/api/mcp/auth_callback,https://claude.com/api/mcp/auth_callback,loopback";

const LOOPBACK_KEYWORD = "loopback";
const LOOPBACK_HOSTS = new Set(["localhost", "127.0.0.1"]);

export class RedirectPolicy {
	readonly exact: ReadonlySet<string>;
	readonly loopback: boolean;

	/** `spec` is ALLOWED_REDIRECT_URIS; empty or unset means the defaults. */
	constructor(spec: string | undefined) {
		const entries = (spec?.trim() ? spec : DEFAULT_ALLOWED_REDIRECT_URIS)
			.split(",")
			.map((entry) => entry.trim())
			.filter(Boolean);
		const exact = new Set<string>();
		let loopback = false;
		for (const entry of entries) {
			if (entry.toLowerCase() === LOOPBACK_KEYWORD) {
				loopback = true;
			} else if (URL.canParse(entry)) {
				exact.add(entry);
			} else {
				console.warn(`ALLOWED_REDIRECT_URIS: ignoring "${entry}", which is not a URL`);
			}
		}
		this.exact = exact;
		this.loopback = loopback;
	}

	allows(uri: string): boolean {
		if (this.exact.has(uri)) return true;
		return this.loopback && isLoopbackHttp(uri);
	}

	/** At least one of a registration's redirect URIs must be usable here. */
	allowsAny(uris: unknown): boolean {
		return Array.isArray(uris) && uris.some((uri) => typeof uri === "string" && this.allows(uri));
	}
}

/** http on localhost or 127.0.0.1, any port and path (RFC 8252 native-app redirects). */
function isLoopbackHttp(uri: string): boolean {
	const url = URL.parse(uri);
	if (!url || url.protocol !== "http:" || url.username || url.password || url.hash) return false;
	return LOOPBACK_HOSTS.has(url.hostname);
}
