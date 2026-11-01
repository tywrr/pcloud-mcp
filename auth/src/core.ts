// Forwarding authorized /mcp requests and signed /dl and /ul link requests to the core in the
// owner's cluster, over the Workers VPC service bound as CORE (→ tunnel chance-of-files →
// cloudflared → 127.0.0.1:8080 in the pod).

/**
 * The VPC service decides where requests go; the URL supplies only the path and the Host header.
 * This is the core's own listen address, so the Host it sees is the one it expects locally too.
 */
const CORE_ORIGIN = "http://127.0.0.1:8080";

/** Headers only this Worker may set: the core trusts them on requests with INTERNAL_TOKEN. */
const PRIVATE_HEADER_PREFIX = "x-cof-";

/**
 * How to reach the core: the CORE binding, or a loopback CORE_URL in local development. CORE_URL
 * is honoured only for loopback hosts, which a deployed Worker can't reach anyway, so a stray
 * value can never send the internal token anywhere else.
 */
function coreTarget(env: Env): { fetcher: Fetcher; origin: string; name: string } | string {
	const configured = env.CORE_URL?.trim();
	if (configured) {
		const url = URL.parse(configured);
		if (!url || url.protocol !== "http:" || !isLoopbackHost(url.hostname) || url.pathname !== "/" || url.search || url.hash) {
			return "CORE_URL must be a loopback origin such as http://127.0.0.1:8080 (local development only).";
		}
		const local = { fetch: (input: RequestInfo | URL, init?: RequestInit) => fetch(input, init) } as Fetcher;
		return { fetcher: local, origin: url.origin, name: "CORE_URL" };
	}
	if (!env.CORE) return "The CORE VPC service binding is missing from this Worker's configuration.";
	return { fetcher: env.CORE, origin: CORE_ORIGIN, name: "CORE" };
}

function isLoopbackHost(hostname: string): boolean {
	return hostname === "localhost" || hostname === "[::1]" || /^127(\.\d{1,3}){3}$/.test(hostname);
}

/**
 * Sends the request on unchanged except for credentials and private headers: the caller's
 * Authorization is replaced by the internal token the core checks, cookies and any client-sent
 * X-CoF-* header are dropped, and `privateHeaders` (X-CoF-Scopes and X-CoF-Public-Url for /mcp,
 * X-CoF-Client-IP for links) are set. Bodies stream both ways: the request body is passed on as
 * the client's stream, and the core's response comes back as is.
 */
export async function forwardToCore(
	request: Request,
	env: Env,
	privateHeaders: Record<string, string>,
): Promise<Response> {
	// `echo token | wrangler secret put` stores a trailing newline, which fetch rejects in a header.
	const token = env.INTERNAL_TOKEN?.replace(/[\r\n]+$/, "");
	if (!token) {
		return new Response("INTERNAL_TOKEN is not set on the chance-of-files Worker.\n", { status: 500 });
	}
	const target = coreTarget(env);
	if (typeof target === "string") return new Response(`${target}\n`, { status: 500 });

	const headers = new Headers(request.headers);
	// Collected first: deleting while iterating a Headers object skips entries.
	const privateNames = [...headers.keys()].filter((name) => name.toLowerCase().startsWith(PRIVATE_HEADER_PREFIX));
	for (const name of privateNames) headers.delete(name);
	headers.set("Authorization", `Bearer ${token}`);
	for (const [name, value] of Object.entries(privateHeaders)) {
		if (!name.toLowerCase().startsWith(PRIVATE_HEADER_PREFIX)) throw new Error(`${name} is not a private header`);
		headers.set(name, value);
	}
	headers.delete("Cookie");

	const incoming = new URL(request.url);
	const hasBody = request.method !== "GET" && request.method !== "HEAD";
	const forwarded = new Request(`${target.origin}${incoming.pathname}${incoming.search}`, {
		method: request.method,
		headers,
		body: hasBody ? request.body : null,
		redirect: "manual",
	});
	try {
		return await target.fetcher.fetch(forwarded);
	} catch (error) {
		// Typically the tunnel or the core pod is down, or the core isn't running in local dev.
		console.error(`${target.name} fetch failed: ${error instanceof Error ? error.message : String(error)}`);
		return new Response("The Chance of Files core could not be reached.\n", { status: 502 });
	}
}
