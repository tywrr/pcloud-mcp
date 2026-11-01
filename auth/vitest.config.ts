import { cloudflareTest } from "@cloudflare/vitest-plugin";
import { defineConfig } from "vitest/config";

// Fixed test values, so tests never depend on a developer's .dev.vars or shell. Wrangler reads
// required secrets from process.env, which also keeps it from warning that they are missing.
const TEST_SECRETS = {
	INTERNAL_TOKEN: "test-internal-token",
	OWNER_PASSPHRASE: "test-owner-passphrase",
	LINK_KEY: "test-link-key-0123456789abcdef0123456789",
};
Object.assign(process.env, TEST_SECRETS);

export default defineConfig({
	plugins: [
		cloudflareTest({
			wrangler: { configPath: "./wrangler.jsonc" },
			// The CORE VPC binding is remote-only, and tests must never open a Cloudflare session.
			// Locally it just throws; test/helpers.ts swaps in a stub core instead.
			remoteBindings: false,
			miniflare: {
				bindings: { ...TEST_SECRETS, PUBLIC_URL: "" },
			},
		}),
	],
});
