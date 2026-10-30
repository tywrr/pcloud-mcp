import { describe, expect, it } from "vitest";
import { CidrList, limiterKey, parseIp } from "../src/ip";

const bytes = (text: string) => (parseIp(text) ? [...parseIp(text)!] : undefined);

describe("parseIp", () => {
	it("reads IPv4", () => {
		expect(bytes("192.0.2.1")).toEqual([192, 0, 2, 1]);
		expect(bytes("0.0.0.0")).toEqual([0, 0, 0, 0]);
		expect(bytes("255.255.255.255")).toEqual([255, 255, 255, 255]);
	});

	it("reads IPv6 in full, compressed and with a dotted tail", () => {
		expect(bytes("2001:db8:0:0:0:0:0:1")).toEqual(bytes("2001:db8::1"));
		expect(bytes("::1")).toEqual([...Array<number>(15).fill(0), 1]);
		expect(bytes("::")).toEqual(Array<number>(16).fill(0));
		expect(bytes("1:2:3:4:5:6:7::")).toEqual([0, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0, 7, 0, 0]);
		expect(bytes("2001:DB8::A")).toEqual(bytes("2001:db8::a"));
		expect(bytes("64:ff9b::192.0.2.1")?.slice(12)).toEqual([192, 0, 2, 1]);
	});

	it("turns IPv4-mapped IPv6 into IPv4", () => {
		expect(bytes("::ffff:127.0.0.1")).toEqual([127, 0, 0, 1]);
		expect(bytes("::ffff:7f00:1")).toEqual([127, 0, 0, 1]);
	});

	it.each([
		"",
		"1.2.3",
		"1.2.3.4.5",
		"256.0.0.1",
		"01.2.3.4",
		"1.2.3.-4",
		" 1.2.3.4",
		"1::2::3",
		":::",
		"1:2:3:4:5:6:7:8:9",
		"1:2:3:4:5:6:7",
		"1:2:3:4:5:6:7:8::",
		"12345::",
		"g::1",
		":1:2:3:4:5:6:7",
		"fe80::1%eth0",
		"::ffff:1.2.3",
		"1.2.3.4::",
		"localhost",
	])("refuses %j", (text) => {
		expect(parseIp(text)).toBeUndefined();
	});
});

describe("CidrList", () => {
	it("matches IPv4 ranges at their edges", () => {
		const list = new CidrList("160.79.104.0/21");
		expect(list.allows("160.79.103.255")).toBe(false);
		expect(list.allows("160.79.104.0")).toBe(true);
		expect(list.allows("160.79.107.42")).toBe(true);
		expect(list.allows("160.79.111.255")).toBe(true);
		expect(list.allows("160.79.112.0")).toBe(false);
		expect(list.allows("::ffff:160.79.105.1")).toBe(true);
	});

	it("matches IPv6 ranges", () => {
		const list = new CidrList("2001:db8:abcd::/48,2001:db8:1:2:3:4:5:6");
		expect(list.allows("2001:db8:abcd::1")).toBe(true);
		expect(list.allows("2001:db8:abcd:ffff:ffff:ffff:ffff:ffff")).toBe(true);
		expect(list.allows("2001:db8:abce::")).toBe(false);
		expect(list.allows("2001:db8:1:2:3:4:5:6")).toBe(true);
		expect(list.allows("2001:db8:1:2:3:4:5:7")).toBe(false);
	});

	it("handles prefixes that end inside a byte, and /0", () => {
		const list = new CidrList("10.64.0.0/10,2001:db8:8000::/33");
		expect(list.allows("10.127.255.255")).toBe(true);
		expect(list.allows("10.128.0.0")).toBe(false);
		expect(list.allows("10.63.255.255")).toBe(false);
		expect(list.allows("2001:db8:ffff::1")).toBe(true);
		expect(list.allows("2001:db8:7fff::1")).toBe(false);
		expect(new CidrList("0.0.0.0/0").allows("203.0.113.1")).toBe(true);
	});

	it("keeps IPv4 and IPv6 apart", () => {
		expect(new CidrList("::/0").allows("203.0.113.1")).toBe(false);
		expect(new CidrList("0.0.0.0/0").allows("2001:db8::1")).toBe(false);
		expect(new CidrList("::ffff:10.0.0.0/104").allows("10.1.2.3")).toBe(true);
	});

	it("defaults to loopback only", () => {
		for (const spec of [undefined, "", " "]) {
			const list = new CidrList(spec);
			expect(list.allows("160.79.104.1")).toBe(false);
			expect(list.allows("127.0.0.1")).toBe(true);
			expect(list.allows("127.1.2.3")).toBe(true);
			expect(list.allows("::1")).toBe(true);
			expect(list.allows("::ffff:127.0.0.1")).toBe(true);
			expect(list.allows("198.51.100.1")).toBe(false);
			expect(list.allows("::2")).toBe(false);
			expect(list.allows(undefined)).toBe(false);
		}
	});

	it('allows everyone, even without an address, with "*"', () => {
		const list = new CidrList("*");
		expect(list.allows("198.51.100.1")).toBe(true);
		expect(list.allows("2001:db8::1")).toBe(true);
		expect(list.allows(undefined)).toBe(true);
	});

	it("ignores entries it can't read, and refuses everyone when none is left", () => {
		const list = new CidrList("not-an-ip, 10.0.0.0/33, 10.0.0.0/x, ::/129, 1.2.3.4/8/8, 198.51.100.0/24");
		expect(list.ranges).toHaveLength(1);
		expect(list.allows("198.51.100.1")).toBe(true);
		expect(new CidrList("junk").allows("198.51.100.1")).toBe(false);
	});
});

describe("limiterKey", () => {
	it("keeps IPv4 addresses as they are", () => {
		expect(limiterKey("198.51.100.7")).toBe("198.51.100.7");
		expect(limiterKey("::ffff:198.51.100.7")).toBe("198.51.100.7");
	});

	it("counts IPv6 by /64", () => {
		expect(limiterKey("2001:db8:1:2:aaaa:bbbb:cccc:dddd")).toBe("2001:db8:1:2::/64");
		expect(limiterKey("2001:db8:1:2::1")).toBe("2001:db8:1:2::/64");
		expect(limiterKey("2001:0db8:0001:0002::ffff")).toBe("2001:db8:1:2::/64");
		expect(limiterKey("2001:db8:1:3::1")).toBe("2001:db8:1:3::/64");
		expect(limiterKey("::1")).toBe("0:0:0:0::/64");
	});

	it("falls back to the raw value, or unknown", () => {
		expect(limiterKey("garbage")).toBe("garbage");
		expect(limiterKey(undefined)).toBe("unknown");
	});
});
