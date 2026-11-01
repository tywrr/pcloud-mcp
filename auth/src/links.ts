// Signed links: /dl/<token>/<name> (downloads) and /ul/<token> (chunked uploads), for clients
// such as Duet's computer that fetch with curl and carry no OAuth token. The token is the
// credential. This Worker only pre-checks it (MAC and expiry), so junk never reaches the cluster;
// the core checks it again against its links table, with the IP binding, byte cap and revocation.

import { forwardToCore } from "./core";
import { clientIp, withinLimit } from "./security";

/**
 * The biggest upload part this Worker accepts. Cloudflare caps every request body at 100 MB on
 * the free plan; 90 MiB parts went through intact in phase 0.
 */
export const MAX_UPLOAD_PART_BYTES = 90 * 1024 * 1024;

export type LinkKind = "dl" | "ul";

export interface LinkToken {
	kind: LinkKind;
	id: string;
	/** Expiry, Unix seconds. */
	exp: number;
	mac: Uint8Array;
}

/** 128 bits: the MAC is HMAC-SHA256 truncated to its first 16 bytes. */
const MAC_BYTES = 16;

/** The core refuses to start with a shorter LINK_KEY; so does this Worker's link route. */
export const MIN_LINK_KEY_LENGTH = 32;

// <id>.<exp>.<mac>: id is 128 random bits and mac 128 bits, both unpadded base64url (22
// characters); exp is decimal Unix seconds. The core mints them (crates/core/src/links.rs).
const TOKEN = /^([A-Za-z0-9_-]{22})\.([1-9][0-9]{0,11})\.([A-Za-z0-9_-]{22})$/;

/** The bytes the MAC covers, "<kind>.<id>.<exp>"; must match the core byte for byte. */
export function macInput(kind: LinkKind, id: string, exp: number): Uint8Array {
	return new TextEncoder().encode(`${kind}.${id}.${exp}`);
}

/**
 * Splits /dl/<token>[/<name>] or /ul/<token>. The name after a download token is only for the
 * saved file's name and is not signed; the core picks the file from the link's row.
 */
export function parseLinkPath(pathname: string): LinkToken | undefined {
	const [, kind, token = "", ...rest] = pathname.split("/");
	if (kind !== "dl" && kind !== "ul") return undefined;
	if (kind === "ul" ? rest.length > 0 : rest.length > 1) return undefined;
	const match = TOKEN.exec(token);
	if (!match) return undefined;
	const mac = fromBase64url(match[3]!);
	// atob ignores the spare low bits of the last character; the core's decoder refuses them.
	if (!mac || mac.length !== MAC_BYTES || toBase64url(mac) !== match[3]) return undefined;
	return { kind, id: match[1]!, exp: Number(match[2]), mac };
}

/** Whether `token.mac` is right for LINK_KEY, compared in constant time. */
export async function macMatches(token: LinkToken, linkKey: string): Promise<boolean> {
	const key = await crypto.subtle.importKey(
		"raw",
		new TextEncoder().encode(linkKey),
		{ name: "HMAC", hash: "SHA-256" },
		false,
		["sign"],
	);
	const full = await crypto.subtle.sign("HMAC", key, macInput(token.kind, token.id, token.exp));
	return crypto.subtle.timingSafeEqual(full.slice(0, MAC_BYTES), token.mac);
}

/** Mints a token the way the core does; for tests and local development. */
export async function signLink(kind: LinkKind, id: string, exp: number, linkKey: string): Promise<string> {
	const key = await crypto.subtle.importKey(
		"raw",
		new TextEncoder().encode(linkKey),
		{ name: "HMAC", hash: "SHA-256" },
		false,
		["sign"],
	);
	const full = await crypto.subtle.sign("HMAC", key, macInput(kind, id, exp));
	return `${id}.${exp}.${toBase64url(new Uint8Array(full, 0, MAC_BYTES))}`;
}

function fromBase64url(text: string): Uint8Array | undefined {
	try {
		const binary = atob(text.replace(/-/g, "+").replace(/_/g, "/"));
		return Uint8Array.from(binary, (char) => char.charCodeAt(0));
	} catch {
		return undefined;
	}
}

export function toBase64url(bytes: Uint8Array): string {
	return btoa(String.fromCharCode(...bytes))
		.replace(/\+/g, "-")
		.replace(/\//g, "_")
		.replace(/=+$/, "");
}

/**
 * Headers on every link response, the core's included. Links share an origin with the sign-in
 * page, so a downloaded file must never render there: it is always an attachment, never sniffed,
 * and sandboxed with no sources should a browser display it anyway.
 */
const LINK_SECURITY_HEADERS: Record<string, string> = {
	"Content-Security-Policy": "default-src 'none'; sandbox; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
	"X-Content-Type-Options": "nosniff",
	"X-Frame-Options": "DENY",
	"Cache-Control": "no-store",
	"Referrer-Policy": "no-referrer",
	"Cross-Origin-Resource-Policy": "same-origin",
};

/** Content types a download may keep from the core; anything else becomes octet-stream. */
const DOWNLOAD_TYPES = new Set(["application/octet-stream", "application/zip"]);

function plain(status: number, message: string, extra: Record<string, string> = {}): Response {
	return new Response(`${message}\n`, {
		status,
		headers: { ...LINK_SECURITY_HEADERS, "Content-Type": "text/plain; charset=utf-8", ...extra },
	});
}

/** `attachment; filename*=…` for the last path segment, or a bare `attachment`. */
function attachment(pathname: string): string {
	const [, , , name] = pathname.split("/");
	if (!name) return "attachment";
	let decoded: string;
	try {
		decoded = decodeURIComponent(name);
	} catch {
		return "attachment";
	}
	// Re-encoded from the decoded name, so the header can carry no quotes, CR/LF or separators.
	const encoded = encodeURIComponent(decoded).replace(/['()*]/g, (char) => `%${char.charCodeAt(0).toString(16).toUpperCase()}`);
	return `attachment; filename*=UTF-8''${encoded}`;
}

/**
 * The core's response with the link headers forced on, body untouched (still a stream).
 * Content-Disposition from the core is kept only when it is already an attachment.
 */
function secureResponse(response: Response, kind: LinkKind, pathname: string): Response {
	const secured = new Response(response.body, response);
	const headers = secured.headers;
	headers.delete("Set-Cookie");
	for (const [name, value] of Object.entries(LINK_SECURITY_HEADERS)) headers.set(name, value);
	if (kind === "dl" && response.ok) {
		const disposition = headers.get("Content-Disposition") ?? "";
		if (!/^attachment\s*(;|$)/i.test(disposition)) headers.set("Content-Disposition", attachment(pathname));
		const type = (headers.get("Content-Type") ?? "").split(";")[0]!.trim().toLowerCase();
		if (!DOWNLOAD_TYPES.has(type)) headers.set("Content-Type", "application/octet-stream");
	} else {
		// Errors and upload answers are text or JSON; anything else is not meant to render here.
		const type = headers.get("Content-Type");
		if (type !== null && !/^(text\/plain|application\/json)\s*(;|$)/i.test(type)) {
			headers.set("Content-Type", "application/octet-stream");
		}
	}
	return secured;
}

/** `bytes a-b/total` → the part's length, or undefined if the header is malformed. */
function contentRangeLength(value: string): number | undefined {
	const match = /^bytes (\d{1,15})-(\d{1,15})\/(\d{1,15}|\*)$/.exec(value.trim());
	if (!match) return undefined;
	const first = Number(match[1]);
	const last = Number(match[2]);
	if (last < first) return undefined;
	if (match[3] !== "*" && last >= Number(match[3])) return undefined;
	return last - first + 1;
}

/**
 * Checks an upload part's size before any of it is read: Content-Length is required (curl sends
 * it for a file) and at most MAX_UPLOAD_PART_BYTES, and a Content-Range, when sent, must span
 * exactly that many bytes. The core checks offsets and the total.
 */
function checkUploadPart(request: Request): Response | undefined {
	const lengthHeader = request.headers.get("Content-Length");
	if (lengthHeader === null) return plain(411, "Upload parts need a Content-Length (send each part as a file).");
	if (!/^\d{1,15}$/.test(lengthHeader.trim())) return plain(400, "Content-Length is not a number.");
	const length = Number(lengthHeader.trim());
	if (length > MAX_UPLOAD_PART_BYTES) {
		return plain(413, `Upload parts can be at most ${MAX_UPLOAD_PART_BYTES} bytes (90 MiB); split the file, e.g. split -b 90M.`);
	}
	const range = request.headers.get("Content-Range");
	if (range !== null) {
		const span = contentRangeLength(range);
		if (span === undefined) return plain(400, "Content-Range must look like: bytes <first>-<last>/<total>.");
		if (span > MAX_UPLOAD_PART_BYTES) {
			return plain(413, `Upload parts can be at most ${MAX_UPLOAD_PART_BYTES} bytes (90 MiB); split the file, e.g. split -b 90M.`);
		}
		if (span !== length) return plain(400, "Content-Range and Content-Length disagree about the part's size.");
	}
	return undefined;
}

/**
 * Whether an upload request asks the core to (re)run the completion: the core treats any POST,
 * and a PUT with a `finish` query parameter, that way (crates/server/src/links.rs, `upload`).
 */
function asksToFinish(request: Request, url: URL): boolean {
	return request.method === "POST" || (request.method === "PUT" && url.searchParams.has("finish"));
}

/**
 * Checks a finish request: the query must be exactly `?finish` and the body empty. Returns the
 * refusal, or a copy of the request with no body to forward.
 */
async function checkFinish(request: Request, url: URL): Promise<Response | Request> {
	if (url.search !== "?finish") return plain(400, "To retry the completion, POST the link with exactly ?finish and no body.");
	const lengthHeader = request.headers.get("Content-Length");
	if (lengthHeader !== null) {
		if (!/^\d{1,15}$/.test(lengthHeader.trim())) return plain(400, "Content-Length is not a number.");
		if (Number(lengthHeader.trim()) !== 0) return plain(413, "A finish request carries no body.");
	}
	if (request.body) {
		// A chunked body has no Content-Length: read it, but give up at its first byte.
		const reader = request.body.getReader();
		try {
			for (let chunk = await reader.read(); !chunk.done; chunk = await reader.read()) {
				if (chunk.value.byteLength > 0) return plain(413, "A finish request carries no body.");
			}
		} finally {
			void reader.cancel().catch(() => {});
		}
	}
	return new Request(request.url, { method: request.method, headers: request.headers });
}

const ALLOWED_METHODS: Record<LinkKind, string> = { dl: "GET, HEAD", ul: "GET, HEAD, PUT, POST" };

/** Handles a request under /dl/ or /ul/. */
export async function handleLink(request: Request, env: Env, url: URL): Promise<Response> {
	const kind: LinkKind = url.pathname.startsWith("/ul/") ? "ul" : "dl";
	if (!ALLOWED_METHODS[kind].split(", ").includes(request.method)) {
		return plain(405, "Method not allowed.", { Allow: ALLOWED_METHODS[kind] });
	}
	// Before the MAC check, so guessing costs the caller too.
	if (!(await withinLimit(env.LINK_LIMITER, "link", request))) {
		return plain(429, "Too many link requests from this address; retry in a minute.", { "Retry-After": "60" });
	}

	// `echo key | wrangler secret put` stores a trailing newline; the core trims it the same way.
	// The key is the secret's UTF-8 bytes as given, as on the core.
	const linkKey = env.LINK_KEY?.replace(/[\r\n]+$/, "");
	if (!linkKey || linkKey.length < MIN_LINK_KEY_LENGTH) {
		return plain(503, `Links are not set up on this server (LINK_KEY is missing or under ${MIN_LINK_KEY_LENGTH} characters).`);
	}

	const token = parseLinkPath(url.pathname);
	if (!token || token.kind !== kind) return plain(404, "No such link.");
	if (!(await macMatches(token, linkKey))) return plain(403, "This link is not valid.");
	if (token.exp * 1000 <= Date.now()) return plain(410, "This link has expired.");

	let forwarded = request;
	if (kind === "ul" && asksToFinish(request, url)) {
		const checked = await checkFinish(request, url);
		if (checked instanceof Response) return checked;
		forwarded = checked;
	} else if (kind === "ul" && request.method === "PUT") {
		const refused = checkUploadPart(request);
		if (refused) return refused;
	}

	const ip = clientIp(request);
	if (!ip) return plain(400, "The client address is unknown.");
	const response = await forwardToCore(forwarded, env, { "X-CoF-Client-IP": ip });
	return secureResponse(response, kind, url.pathname);
}
