// /dl and /ul: signed links that need no OAuth token, pre-checked here and forwarded to the core.

import { describe, expect, it } from "vitest";
import { MAX_UPLOAD_PART_BYTES, type LinkKind, parseLinkPath, signLink, toBase64url } from "../src/links";
import { type Echo, call, freshIp } from "./helpers";

// The same key the core's tests use (crates/core/src/links.rs), so the vectors below hold for both.
const LINK_KEY = "test-link-key-0123456789abcdef0123456789";

function randomId(): string {
	return toBase64url(crypto.getRandomValues(new Uint8Array(16)));
}

/** A valid token, by default for 15 minutes from now. */
function token(kind: LinkKind, expiresIn = 15 * 60, key = LINK_KEY): Promise<string> {
	return signLink(kind, randomId(), Math.floor(Date.now() / 1000) + expiresIn, key);
}

/** A core that answers with `respond` instead of echoing. */
function core(respond: (request: Request) => Response | Promise<Response>): Partial<Env> {
	return { CORE: { fetch: (input: RequestInfo | URL, init?: RequestInit) => respond(new Request(input, init)) } as Fetcher };
}

describe("link tokens", () => {
	it("matches the core's HMAC for a known vector", async () => {
		// openssl dgst -sha256 -mac HMAC over "dl.AAAAAAAAAAAAAAAAAAAAAA.1900000000", first 16 bytes.
		const id = "AAAAAAAAAAAAAAAAAAAAAA";
		expect(await signLink("dl", id, 1_900_000_000, LINK_KEY)).toBe(`${id}.1900000000.-m9jVnui9Q5UG9NP0NRQyQ`);
		expect(await signLink("ul", id, 1_900_000_000, LINK_KEY)).toBe(`${id}.1900000000.OrCNstMVWn-HHbWyszsjSw`);
	});

	it("parses /dl/<token>/<name> and /ul/<token>, nothing else", async () => {
		const dl = await token("dl");
		expect(parseLinkPath(`/dl/${dl}/photo.jpg`)?.kind).toBe("dl");
		expect(parseLinkPath(`/dl/${dl}`)?.kind).toBe("dl");
		expect(parseLinkPath(`/dl/${dl}/a/b`)).toBeUndefined();
		const ul = await token("ul");
		expect(parseLinkPath(`/ul/${ul}`)?.kind).toBe("ul");
		expect(parseLinkPath(`/ul/${ul}/name`)).toBeUndefined();
		expect(parseLinkPath(`/dl/${dl}.x/name`)).toBeUndefined();
		expect(parseLinkPath("/dl/not-a-token/name")).toBeUndefined();
		// The same MAC bytes with the last character's spare bits set: not canonical, so refused.
		const last = dl.at(-1)!;
		const spare = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
		const sibling = spare[spare.indexOf(last) ^ 1]!;
		expect(parseLinkPath(`/dl/${dl.slice(0, -1)}${sibling}/a`)).toBeUndefined();
	});

	it("forwards a valid download link to the core with the internal token and client IP", async () => {
		const ip = freshIp();
		const path = `/dl/${await token("dl")}/report%20final.pdf`;
		const response = await call(path, { headers: { Range: "bytes=0-99" } }, ip);
		expect(response.status).toBe(200);
		const echo = await new Response(response.body).json<Echo>();
		expect(new URL(echo.url).pathname).toBe(path);
		expect(echo.authorization).toBe("Bearer test-internal-token");
		expect(echo.headers["x-cof-client-ip"]).toBe(ip);
		expect(echo.headers.range).toBe("bytes=0-99");
		expect(echo.headers["x-cof-scopes"]).toBeUndefined();
	});

	it("refuses an expired link with 410 before the core", async () => {
		let reached = false;
		const response = await call(`/dl/${await token("dl", -1)}/a.bin`, {}, freshIp(), core(() => {
			reached = true;
			return new Response("core");
		}));
		expect(response.status).toBe(410);
		expect(reached).toBe(false);
	});

	it.each([
		["a flipped MAC", (t: string) => `${t.slice(0, -22)}${t.at(-22) === "A" ? "B" : "A"}${t.slice(-21)}`],
		["a later expiry", (t: string) => t.replace(/\.(\d+)\./, (_, exp: string) => `.${Number(exp) + 3600}.`)],
		["another id", (t: string) => `${randomId()}${t.slice(22)}`],
	])("refuses a token with %s with 403 before the core", async (_, tamper) => {
		let reached = false;
		const response = await call(`/dl/${tamper(await token("dl"))}/a.bin`, {}, freshIp(), core(() => {
			reached = true;
			return new Response("core");
		}));
		expect(response.status).toBe(403);
		expect(reached).toBe(false);
	});

	it("refuses a token signed with another key, or for the other kind", async () => {
		expect((await call(`/dl/${await token("dl", 900, "another-key-0123456789abcdef0123456789")}/a`)).status).toBe(403);
		// The kind is signed: an upload token can't be used as a download link, or back.
		expect((await call(`/dl/${await token("ul")}/a`)).status).toBe(403);
		const asUpload = await call(`/ul/${await token("dl")}`, { method: "PUT", headers: { "Content-Length": "1" }, body: "x" });
		expect(asUpload.status).toBe(403);
	});

	it("answers 404 for malformed tokens and 405 for other methods", async () => {
		expect((await call("/dl/junk/a.bin")).status).toBe(404);
		expect((await call("/ul/", { method: "PUT", headers: { "Content-Length": "1" }, body: "x" })).status).toBe(404);
		const post = await call(`/dl/${await token("dl")}/a`, { method: "POST", body: "x" });
		expect(post.status).toBe(405);
		expect(post.headers.get("Allow")).toBe("GET, HEAD");
		const del = await call(`/ul/${await token("ul")}`, { method: "DELETE" });
		expect(del.status).toBe(405);
		expect(del.headers.get("Allow")).toBe("GET, HEAD, PUT, POST");
		const patch = await call(`/ul/${await token("ul")}`, { method: "PATCH", body: "x" });
		expect(patch.status).toBe(405);
	});

	it("answers 503 when LINK_KEY is not set or too short", async () => {
		const unset = await call(`/dl/${await token("dl")}/a`, {}, freshIp(), { LINK_KEY: "" });
		expect(unset.status).toBe(503);
		const short = await call(`/dl/${await token("dl", 900, "short-key")}/a`, {}, freshIp(), { LINK_KEY: "short-key" });
		expect(short.status).toBe(503);
	});

	it("accepts a LINK_KEY with the trailing newline wrangler secret put keeps", async () => {
		const response = await call(`/dl/${await token("dl")}/a`, {}, freshIp(), { LINK_KEY: `${LINK_KEY}\n` });
		expect(response.status).toBe(200);
	});
});

describe("private headers", () => {
	it("strips client-sent X-CoF-* headers, cookies and Authorization", async () => {
		const ip = freshIp();
		const response = await call(
			`/dl/${await token("dl")}/a`,
			{
				headers: {
					"X-CoF-Client-IP": "203.0.113.9",
					"X-CoF-Scopes": "files:read files:write",
					"x-cof-anything": "1",
					Cookie: "a=b",
					Authorization: "Bearer someone-elses",
				},
			},
			ip,
		);
		const echo = await new Response(response.body).json<Echo>();
		expect(echo.headers["x-cof-client-ip"]).toBe(ip);
		expect(echo.headers["x-cof-scopes"]).toBeUndefined();
		expect(echo.headers["x-cof-anything"]).toBeUndefined();
		expect(echo.cookie).toBeNull();
		expect(echo.authorization).toBe("Bearer test-internal-token");
	});
});

describe("download headers", () => {
	it("forces attachment, octet-stream, nosniff and a sandbox CSP whatever the core sends", async () => {
		const response = await call(
			`/dl/${await token("dl")}/evil%22name.html`,
			{},
			freshIp(),
			core(
				() =>
					new Response("<script>alert(1)</script>", {
						headers: { "Content-Type": "text/html", "Content-Disposition": "inline", "Set-Cookie": "x=y" },
					}),
			),
		);
		expect(response.status).toBe(200);
		expect(response.headers.get("Content-Type")).toBe("application/octet-stream");
		expect(response.headers.get("Content-Disposition")).toBe(`attachment; filename*=UTF-8''evil%22name.html`);
		expect(response.headers.get("X-Content-Type-Options")).toBe("nosniff");
		const csp = response.headers.get("Content-Security-Policy") ?? "";
		expect(csp).toContain("default-src 'none'");
		expect(csp).toContain("sandbox");
		expect(response.headers.get("Cache-Control")).toBe("no-store");
		expect(response.headers.get("Set-Cookie")).toBeNull();
	});

	it("keeps the core's attachment header, zip type, ranges and status", async () => {
		const response = await call(
			`/dl/${await token("dl")}/x.zip`,
			{ headers: { Range: "bytes=10-19" } },
			freshIp(),
			core(
				() =>
					new Response("0123456789", {
						status: 206,
						headers: {
							"Content-Type": "application/zip",
							"Content-Disposition": `attachment; filename="photos.zip"`,
							"Content-Range": "bytes 10-19/100",
							"Accept-Ranges": "bytes",
						},
					}),
			),
		);
		expect(response.status).toBe(206);
		expect(response.headers.get("Content-Type")).toBe("application/zip");
		expect(response.headers.get("Content-Disposition")).toBe(`attachment; filename="photos.zip"`);
		expect(response.headers.get("Content-Range")).toBe("bytes 10-19/100");
		expect(new TextDecoder().decode(await response.arrayBuffer())).toBe("0123456789");
	});

	it("passes the core's errors on with the safety headers but no attachment", async () => {
		const response = await call(
			`/dl/${await token("dl")}/a`,
			{},
			freshIp(),
			core(() => new Response("byte cap reached\n", { status: 410, headers: { "Content-Type": "text/plain" } })),
		);
		expect(response.status).toBe(410);
		expect(response.headers.get("Content-Disposition")).toBeNull();
		expect(response.headers.get("X-Content-Type-Options")).toBe("nosniff");
		expect(await response.text()).toBe("byte cap reached\n");
	});
});

describe("upload parts", () => {
	it("forwards a part with its Content-Range and body", async () => {
		const response = await call(`/ul/${await token("ul")}`, {
			method: "PUT",
			headers: { "Content-Range": "bytes 0-4/10", "Content-Length": "5" },
			body: "hello",
		});
		expect(response.status).toBe(200);
		const echo = await new Response(response.body).json<Echo>();
		expect(echo.method).toBe("PUT");
		expect(echo.body).toBe("hello");
		expect(echo.headers["content-range"]).toBe("bytes 0-4/10");
	});

	it("forwards HEAD, which asks the core for the committed offset", async () => {
		let method = "";
		const response = await call(`/ul/${await token("ul")}`, { method: "HEAD" }, freshIp(), core((request) => {
			method = request.method;
			return new Response(null, { headers: { "Upload-Offset": "94371840" } });
		}));
		expect(response.status).toBe(200);
		expect(method).toBe("HEAD");
		expect(response.headers.get("Upload-Offset")).toBe("94371840");
	});

	/** Client headers the Worker must drop or replace on the way to the core. */
	const hostile = {
		"X-CoF-Client-IP": "203.0.113.9",
		"X-CoF-Scopes": "files:read files:write",
		Cookie: "a=b",
		Authorization: "Bearer someone-elses",
	};

	function expectPrivateHeadersReplaced(echo: Echo, ip: string): void {
		expect(echo.headers["x-cof-client-ip"]).toBe(ip);
		expect(echo.headers["x-cof-scopes"]).toBeUndefined();
		expect(echo.cookie).toBeNull();
		expect(echo.authorization).toBe("Bearer test-internal-token");
	}

	it("forwards GET, which asks the core for the upload's state as JSON", async () => {
		const ip = freshIp();
		const path = `/ul/${await token("ul")}`;
		const response = await call(path, { headers: hostile }, ip);
		expect(response.status).toBe(200);
		const echo = await new Response(response.body).json<Echo>();
		expect(echo.method).toBe("GET");
		expect(new URL(echo.url).pathname).toBe(path);
		expectPrivateHeadersReplaced(echo, ip);
	});

	it("forwards POST ?finish with no body, which retries a failed completion", async () => {
		const ip = freshIp();
		const path = `/ul/${await token("ul")}`;
		const response = await call(`${path}?finish`, { method: "POST", headers: hostile }, ip);
		expect(response.status).toBe(200);
		const echo = await new Response(response.body).json<Echo>();
		expect(echo.method).toBe("POST");
		expect(new URL(echo.url).pathname).toBe(path);
		expect(new URL(echo.url).search).toBe("?finish");
		expect(echo.body).toBe("");
		expectPrivateHeadersReplaced(echo, ip);
		// curl -X POST sends Content-Length: 0 for an empty body.
		const zero = await call(`${path}?finish`, { method: "POST", headers: { "Content-Length": "0" }, body: "" });
		expect(zero.status).toBe(200);
	});

	it("treats PUT ?finish as the core does: a finish request, not a part", async () => {
		const ip = freshIp();
		const response = await call(`/ul/${await token("ul")}?finish`, { method: "PUT", headers: hostile }, ip);
		expect(response.status).toBe(200);
		const echo = await new Response(response.body).json<Echo>();
		expect(echo.method).toBe("PUT");
		expect(echo.body).toBe("");
		expectPrivateHeadersReplaced(echo, ip);
		const withBody = await call(`/ul/${await token("ul")}?finish`, { method: "PUT", headers: { "Content-Length": "5" }, body: "hello" });
		expect(withBody.status).toBe(413);
	});

	it("refuses a POST other than ?finish with no body before the core", async () => {
		let reached = false;
		const unreached = core(() => {
			reached = true;
			return new Response("core");
		});
		const ul = `/ul/${await token("ul")}`;
		expect((await call(ul, { method: "POST" }, freshIp(), unreached)).status).toBe(400);
		expect((await call(`${ul}?finish=1`, { method: "POST" }, freshIp(), unreached)).status).toBe(400);
		expect((await call(`${ul}?finish&part=0`, { method: "POST" }, freshIp(), unreached)).status).toBe(400);
		expect((await call(`${ul}?finish`, { method: "POST", body: "hello" }, freshIp(), unreached)).status).toBe(413);
		const chunked = new ReadableStream<Uint8Array>({
			start(controller) {
				controller.enqueue(new Uint8Array(3));
				controller.close();
			},
		});
		const streamed = await call(`${ul}?finish`, { method: "POST", body: chunked, duplex: "half" } as RequestInit, freshIp(), unreached);
		expect(streamed.status).toBe(413);
		const junkLength = await call(`${ul}?finish`, { method: "POST", headers: { "Content-Length": "x" } }, freshIp(), unreached);
		expect(junkLength.status).toBe(400);
		expect(reached).toBe(false);
	});

	it("refuses parts over 90 MiB with 413 before reading them", async () => {
		let reached = false;
		const unreached = core(() => {
			reached = true;
			return new Response("core");
		});
		const ul = `/ul/${await token("ul")}`;
		// A stream that never ends: a 413 here proves the Worker answered from the headers alone.
		const endless = () => new ReadableStream<Uint8Array>({ pull: () => new Promise(() => {}) });
		const tooLong = await call(
			ul,
			{ method: "PUT", headers: { "Content-Length": String(MAX_UPLOAD_PART_BYTES + 1) }, body: endless(), duplex: "half" } as RequestInit,
			freshIp(),
			unreached,
		);
		expect(tooLong.status).toBe(413);
		const wideRange = await call(
			ul,
			{
				method: "PUT",
				headers: { "Content-Length": "5", "Content-Range": `bytes 0-${MAX_UPLOAD_PART_BYTES}/${MAX_UPLOAD_PART_BYTES * 2}` },
				body: "hello",
			},
			freshIp(),
			unreached,
		);
		expect(wideRange.status).toBe(413);
		expect(reached).toBe(false);
	});

	it("accepts a part of exactly 90 MiB by its headers", async () => {
		let length = "";
		const response = await call(
			`/ul/${await token("ul")}`,
			{
				method: "PUT",
				headers: {
					"Content-Range": `bytes 0-${MAX_UPLOAD_PART_BYTES - 1}/${MAX_UPLOAD_PART_BYTES}`,
					"Content-Length": String(MAX_UPLOAD_PART_BYTES),
				},
				body: new Uint8Array(MAX_UPLOAD_PART_BYTES),
			},
			freshIp(),
			core((request) => {
				length = request.headers.get("Content-Length") ?? "";
				return new Response("ok");
			}),
		);
		expect(response.status).toBe(200);
		expect(length).toBe(String(MAX_UPLOAD_PART_BYTES));
	});

	it("refuses parts without a length, or with a Content-Range that disagrees", async () => {
		const ul = `/ul/${await token("ul")}`;
		const stream = new ReadableStream<Uint8Array>({
			start(controller) {
				controller.enqueue(new Uint8Array(3));
				controller.close();
			},
		});
		const chunked = await call(ul, { method: "PUT", body: stream, duplex: "half" } as RequestInit);
		expect(chunked.status).toBe(411);
		const mismatch = await call(ul, { method: "PUT", headers: { "Content-Range": "bytes 0-9/100", "Content-Length": "5" }, body: "hello" });
		expect(mismatch.status).toBe(400);
		const malformed = await call(ul, { method: "PUT", headers: { "Content-Range": "bytes 5-4/100", "Content-Length": "5" }, body: "hello" });
		expect(malformed.status).toBe(400);
	});
});

describe("streaming", () => {
	it("returns the core's body as it arrives, without waiting for the end", async () => {
		let push!: ReadableStreamDefaultController<Uint8Array>;
		const body = new ReadableStream<Uint8Array>({ start: (controller) => void (push = controller) });
		const response = await call(`/dl/${await token("dl")}/big.bin`, {}, freshIp(), core(() => new Response(body)));
		// The core's stream is still open: a buffering Worker would not have answered yet.
		const reader = response.body!.getReader();
		push.enqueue(new TextEncoder().encode("first"));
		expect(new TextDecoder().decode((await reader.read()).value)).toBe("first");
		push.enqueue(new TextEncoder().encode("second"));
		expect(new TextDecoder().decode((await reader.read()).value)).toBe("second");
		push.close();
		expect((await reader.read()).done).toBe(true);
	});

	it("hands the upload body to the core before the client has finished sending it", async () => {
		let push!: ReadableStreamDefaultController<Uint8Array>;
		const upload = new ReadableStream<Uint8Array>({ start: (controller) => void (push = controller) });
		const fixed = new FixedLengthStream(10);
		void upload.pipeTo(fixed.writable);
		let firstChunk!: (text: string) => void;
		const sawFirst = new Promise<string>((resolve) => (firstChunk = resolve));
		const pending = call(
			`/ul/${await token("ul")}`,
			{
				method: "PUT",
				headers: { "Content-Range": "bytes 0-9/10", "Content-Length": "10" },
				body: fixed.readable,
				duplex: "half",
			} as RequestInit,
			freshIp(),
			core(async (request) => {
				const reader = request.body!.getReader();
				firstChunk(new TextDecoder().decode((await reader.read()).value));
				let rest = "";
				for (let chunk = await reader.read(); !chunk.done; chunk = await reader.read()) {
					rest += new TextDecoder().decode(chunk.value);
				}
				return new Response(`rest:${rest}`);
			}),
		);
		push.enqueue(new TextEncoder().encode("01234"));
		// The core sees the first half while the client still holds the second.
		expect(await sawFirst).toBe("01234");
		push.enqueue(new TextEncoder().encode("56789"));
		push.close();
		const response = await pending;
		expect(await response.text()).toBe("rest:56789");
	});
});
