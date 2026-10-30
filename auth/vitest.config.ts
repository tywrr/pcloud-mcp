import { cloudflareTest } from "@cloudflare/vitest-plugin";
import { defineConfig } from "vitest/config";

// Fixed test values, so tests never depend on a developer's .dev.vars or shell. Wrangler reads
// required secrets from process.env, which also keeps it from warning that they are missing.
const TEST_SECRETS = {
	INTERNAL_TOKEN: "test-internal-token",
	OWNER_PASSPHRASE: "test-owner-passphrase",
};
Object.assign(process.env, TEST_SECRETS);

export default defineConfig({
	plugins: [
		cloudflareTest({
			wrangler: { configPath: "./wrangler.jsonc" },
			miniflare: {
				bindings: { ...TEST_SECRETS, PUBLIC_URL: "" },
				// Stands in for the Rust core: echoes what reached it, so tests can check that the
				// client's token was swapped for INTERNAL_TOKEN.
				serviceBindings: {
					MCP_CORE: async (request) =>
						Response.json({
							method: request.method,
							url: request.url,
							authorization: request.headers.get("Authorization"),
							cookie: request.headers.get("Cookie"),
							body: await request.text(),
						}),
				},
			},
		}),
	],
});
