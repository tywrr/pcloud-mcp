// Forwarding authorized /mcp requests to the Rust Worker over the MCP_CORE service binding.

/**
 * Sends the request on unchanged except for credentials: Claude's access token is replaced by
 * the internal token the core checks, and cookies are dropped. The core's response, including a
 * streamed body, comes back as is.
 */
export async function forwardToCore(request: Request, env: Env): Promise<Response> {
	// `echo token | wrangler secret put` stores a trailing newline, which fetch rejects in a header.
	const token = env.INTERNAL_TOKEN?.replace(/[\r\n]+$/, "");
	if (!token) {
		return new Response("INTERNAL_TOKEN is not set on the chance-of-files Worker.\n", { status: 500 });
	}
	const headers = new Headers(request.headers);
	headers.set("Authorization", `Bearer ${token}`);
	headers.delete("Cookie");
	const hasBody = request.method !== "GET" && request.method !== "HEAD";
	const forwarded = new Request(request.url, {
		method: request.method,
		headers,
		body: hasBody ? request.body : null,
		redirect: "manual",
	});
	try {
		return await env.MCP_CORE.fetch(forwarded);
	} catch (error) {
		// Typically the core isn't deployed yet, or isn't running in local dev.
		console.error(`MCP_CORE fetch failed: ${error instanceof Error ? error.message : String(error)}`);
		return new Response("The chance-of-files-core Worker could not be reached.\n", { status: 502 });
	}
}
