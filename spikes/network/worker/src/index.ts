// chance-of-files-spike: forwards requests that know SPIKE_KEY to the spike server over the
// Workers VPC binding SPIKE, adding X-Spike-Token. Bodies stream both ways; nothing is buffered.
//
//   https://<worker>/<SPIKE_KEY>/mcp            MCP endpoint (Claude connector, "No sign-in")
//   https://<worker>/<SPIKE_KEY>/blob?bytes=N   or  https://<worker>/blob?bytes=N&k=<SPIKE_KEY>
//
// Anything without the key gets a bare 404 and never reaches the tunnel, so OAuth discovery
// probes (/.well-known/...) also 404 and Claude treats the connector as needing no sign-in.

/** The VPC service decides where requests go; the URL only supplies path, query and Host. */
const ORIGIN = "http://localhost";

const encoder = new TextEncoder();

async function sameSecret(given: string, expected: string): Promise<boolean> {
	// Hash both so the comparison is constant-time whatever the lengths.
	const [a, b] = await Promise.all([
		crypto.subtle.digest("SHA-256", encoder.encode(given)),
		crypto.subtle.digest("SHA-256", encoder.encode(expected)),
	]);
	return crypto.subtle.timingSafeEqual(a, b);
}

function notFound(): Response {
	return new Response("Not found\n", { status: 404 });
}

export default {
	async fetch(request: Request, env: Env): Promise<Response> {
		// `echo x | wrangler secret put` keeps the newline, which fetch rejects in a header.
		const key = env.SPIKE_KEY?.trim();
		const token = env.SPIKE_TOKEN?.trim();
		if (!key || !token) {
			return new Response("SPIKE_KEY and SPIKE_TOKEN must be set.\n", { status: 500 });
		}

		const url = new URL(request.url);
		const [, first = "", ...rest] = url.pathname.split("/");
		let path: string;
		if (await sameSecret(decodeURIComponent(first), key)) {
			path = "/" + rest.join("/");
		} else if (url.searchParams.has("k") && (await sameSecret(url.searchParams.get("k") ?? "", key))) {
			path = url.pathname;
		} else {
			return notFound();
		}
		url.searchParams.delete("k");

		const headers = new Headers(request.headers);
		headers.delete("Cookie");
		headers.set("X-Spike-Token", token);
		// Logged by the server: shows whether Duet's egress address stays put during a transfer.
		headers.set("X-Spike-Client-IP", request.headers.get("CF-Connecting-IP") ?? "unknown");
		// What the server's blob_url tool puts in front of /blob.
		headers.set("X-Spike-Public-Base", `${url.origin}/${encodeURIComponent(key)}`);

		const hasBody = request.method !== "GET" && request.method !== "HEAD";
		const forwarded = new Request(ORIGIN + path + url.search, {
			method: request.method,
			headers,
			body: hasBody ? request.body : null,
			redirect: "manual",
		});
		try {
			// Returned as is: the body stays a stream, with the server's Content-Length.
			return await env.SPIKE.fetch(forwarded);
		} catch (error) {
			const message = error instanceof Error ? error.message : String(error);
			console.error(`SPIKE fetch failed: ${message}`);
			return new Response(`The spike server could not be reached: ${message}\n`, { status: 502 });
		}
	},
} satisfies ExportedHandler<Env>;
