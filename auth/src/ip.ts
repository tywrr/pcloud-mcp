// Client addresses: who may register clients (DCR_ALLOWED_CIDRS), and what the rate limiters
// count. The address is CF-Connecting-IP, which Cloudflare sets on every request from the
// internet; locally, wrangler dev sets it to the socket's address (127.0.0.1 or ::1).

/**
 * Loopback only, for wrangler dev and the tests. Claude identifies itself with its Client ID
 * Metadata Document and doesn't register. Anthropic's outbound range (160.79.104.0/21) is not in
 * the default: any Claude user can send requests from it (web fetch, code execution), so
 * allowing it would let strangers spend the free plan's KV writes. Add it only if claude.ai ever
 * falls back to Dynamic Client Registration for this server.
 */
export const DEFAULT_DCR_ALLOWED_CIDRS = "127.0.0.0/8,::1";

const EVERYONE = "*";

interface Range {
	/** 4 bytes for IPv4, 16 for IPv6. */
	bytes: Uint8Array;
	prefix: number;
}

/** A comma-separated list of CIDRs (a bare address is one host), or "*" for everyone. */
export class CidrList {
	readonly everyone: boolean;
	readonly ranges: readonly Range[];

	/** `spec` is DCR_ALLOWED_CIDRS; empty or unset means the defaults. */
	constructor(spec: string | undefined) {
		const entries = (spec?.trim() ? spec : DEFAULT_DCR_ALLOWED_CIDRS)
			.split(",")
			.map((entry) => entry.trim())
			.filter(Boolean);
		const ranges: Range[] = [];
		let everyone = false;
		for (const entry of entries) {
			const range = parseCidr(entry);
			if (entry === EVERYONE) everyone = true;
			else if (range) ranges.push(range);
			else console.warn(`DCR_ALLOWED_CIDRS: ignoring "${entry}", which is not an IP address or CIDR`);
		}
		this.everyone = everyone;
		this.ranges = ranges;
	}

	/** A missing or unparseable address is allowed only by "*". */
	allows(ip: string | undefined): boolean {
		if (this.everyone) return true;
		const address = ip === undefined ? undefined : parseIp(ip.trim());
		return address !== undefined && this.ranges.some((range) => contains(range, address));
	}
}

/**
 * What the per-IP rate limiters count. An IPv6 host usually has a whole /64 to itself and can
 * take a new address from it for every request, so IPv6 callers are counted by their /64.
 */
export function limiterKey(ip: string | undefined): string {
	if (ip === undefined) return "unknown";
	const bytes = parseIp(ip.trim());
	if (!bytes) return ip;
	if (bytes.length === 4) return bytes.join(".");
	const groups: string[] = [];
	for (let index = 0; index < 8; index += 2) {
		groups.push(((bytes[index]! << 8) | bytes[index + 1]!).toString(16));
	}
	return `${groups.join(":")}::/64`;
}

/**
 * An address as 4 bytes (IPv4) or 16 (IPv6). IPv4-mapped IPv6 (::ffff:192.0.2.1) becomes IPv4,
 * so it matches IPv4 ranges. Zone IDs and other decorations are refused.
 */
export function parseIp(text: string): Uint8Array | undefined {
	if (!text.includes(":")) return parseIpv4(text);
	const bytes = parseIpv6(text);
	if (bytes && bytes.subarray(0, 10).every((byte) => byte === 0) && bytes[10] === 0xff && bytes[11] === 0xff) {
		return bytes.slice(12);
	}
	return bytes;
}

function parseIpv4(text: string): Uint8Array | undefined {
	const parts = text.split(".");
	if (parts.length !== 4) return undefined;
	const bytes = new Uint8Array(4);
	for (const [index, part] of parts.entries()) {
		// Decimal only, without leading zeros, which some parsers read as octal.
		if (!/^(0|[1-9]\d{0,2})$/.test(part) || Number(part) > 255) return undefined;
		bytes[index] = Number(part);
	}
	return bytes;
}

function parseIpv6(text: string): Uint8Array | undefined {
	const halves = text.split("::");
	if (halves.length > 2) return undefined;
	const head = halves[0] ? halves[0].split(":") : [];
	const tail = halves[1] ? halves[1].split(":") : [];
	const last = halves.length === 2 ? tail : head;
	// A dotted quad may stand for the last two groups (::ffff:192.0.2.1).
	let embedded: Uint8Array | undefined;
	if (last.at(-1)?.includes(".")) {
		embedded = parseIpv4(last.pop()!);
		if (!embedded) return undefined;
	}
	const wanted = embedded ? 6 : 8;
	const given = head.length + tail.length;
	// "::" stands for at least one group of zeros.
	if (halves.length === 2 ? given > wanted - 1 : given !== wanted) return undefined;
	const groups = [...head, ...Array<string>(wanted - given).fill("0"), ...tail];
	const bytes = new Uint8Array(16);
	for (const [index, group] of groups.entries()) {
		if (!/^[0-9a-f]{1,4}$/i.test(group)) return undefined;
		const word = Number.parseInt(group, 16);
		bytes[index * 2] = word >> 8;
		bytes[index * 2 + 1] = word & 0xff;
	}
	if (embedded) bytes.set(embedded, 12);
	return bytes;
}

function parseCidr(entry: string): Range | undefined {
	const [address = "", length, ...rest] = entry.split("/");
	const bytes = parseIp(address);
	if (!bytes || rest.length > 0) return undefined;
	const bits = bytes.length * 8;
	if (length === undefined) return { bytes, prefix: bits };
	if (!/^\d{1,3}$/.test(length)) return undefined;
	// An IPv4-mapped range (::ffff:10.0.0.0/104) counts the 96 bits before the IPv4 part.
	const prefix = Number(length) - (address.includes(":") && bytes.length === 4 ? 96 : 0);
	return prefix >= 0 && prefix <= bits ? { bytes, prefix } : undefined;
}

function contains(range: Range, address: Uint8Array): boolean {
	if (range.bytes.length !== address.length) return false;
	const whole = range.prefix >> 3;
	for (let index = 0; index < whole; index += 1) {
		if (range.bytes[index] !== address[index]) return false;
	}
	const rest = range.prefix & 7;
	if (rest === 0) return true;
	const mask = (0xff << (8 - rest)) & 0xff;
	return ((range.bytes[whole]! ^ address[whole]!) & mask) === 0;
}
