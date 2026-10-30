// The sign-in page and the plain pages around it. Everything that came from the client (its name,
// URI, scopes) is chosen by whoever registered it, so it is escaped before it reaches the HTML.

import type { ConsentDescription } from "@cloudflare/workers-oauth-provider";
import { htmlHeaders, newNonce } from "./security";

export const escapeHtml = (value: string): string =>
	value.replace(/[&<>"']/g, (char) => `&#${char.charCodeAt(0)};`);

const STYLE = `
:root { color-scheme: light dark; --bg: #f7f7f4; --fg: #1d1d1b; --muted: #5d5d58; --line: #d6d6cf;
  --accent: #17599e; --accent-fg: #fff; --warn: #a33b00; }
@media (prefers-color-scheme: dark) { :root { --bg: #181816; --fg: #ececE6; --muted: #a4a49c;
  --line: #3b3b37; --accent: #6aa8e8; --accent-fg: #0b1a2a; --warn: #f0a050; } }
* { box-sizing: border-box; }
body { margin: 0; padding: 32px 16px; background: var(--bg); color: var(--fg);
  font: 16px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif; }
main { max-width: 460px; margin: 0 auto; }
h1 { font-size: 22px; line-height: 1.3; margin: 0 0 12px; }
p { margin: 0 0 12px; color: var(--muted); }
strong, code { color: var(--fg); overflow-wrap: anywhere; }
.warn { color: var(--warn); }
.error { color: var(--warn); font-weight: 600; }
ul { margin: 0 0 16px; padding-left: 20px; color: var(--muted); }
label { display: block; margin: 16px 0 6px; font-weight: 600; }
input[type=password] { width: 100%; padding: 10px; font: inherit; color: var(--fg);
  background: transparent; border: 1px solid var(--line); border-radius: 6px; }
.row { display: flex; gap: 12px; margin-top: 16px; }
button { padding: 10px 18px; font: inherit; color: var(--fg); background: transparent;
  border: 1px solid var(--line); border-radius: 6px; cursor: pointer; }
button.primary { color: var(--accent-fg); background: var(--accent); border-color: var(--accent); }
`;

function layout(nonce: string, title: string, body: string): string {
	return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex">
<title>${escapeHtml(title)}</title>
<style nonce="${nonce}">${STYLE}</style>
</head>
<body><main>
${body}
</main></body>
</html>`;
}

export interface SignInPage {
	details: ConsentDescription;
	/** The scopes that approving will grant. */
	scopes: string[];
	error?: string;
}

/**
 * The consent form: who is asking, where access goes, and the passphrase field. It has no action,
 * so it posts to the page's own URL, whose query string is the authorization request.
 */
export function signInPage(nonce: string, { details, scopes, error }: SignInPage): string {
	const name = escapeHtml(details.clientName);
	const who = details.clientDomain
		? `<p>Published by <strong>${escapeHtml(details.clientDomain)}</strong>.</p>`
		: `<p>This app registered itself, so its name is not verified.${
				details.clientUri ? ` It gives its address as <code>${escapeHtml(details.clientUri)}</code>.` : ""
			}</p>`;
	const local = details.redirectIsLoopback
		? `<p class="warn">Access goes to an app on this computer. Any program here could be listening, so continue only if you just started this sign-in yourself (for example from Claude Code or the MCP Inspector).</p>`
		: "";
	const scopeList = scopes.map((scope) => `<li><code>${escapeHtml(scope)}</code></li>`).join("");
	return layout(
		nonce,
		`Allow ${details.clientName}?`,
		`<h1>Allow ${name} to read your pCloud files?</h1>
${who}
<p>Access will be sent to <strong>${escapeHtml(details.redirectHost)}</strong>.</p>
${local}
<p>It gets read-only access to the folder this server shares:</p>
<ul>${scopeList}</ul>
${error ? `<p class="error" role="alert">${escapeHtml(error)}</p>` : ""}
<form method="post">
<label for="passphrase">Owner passphrase</label>
<input id="passphrase" type="password" name="passphrase" autocomplete="current-password" required autofocus>
<div class="row">
<button class="primary" type="submit" name="decision" value="approve">Allow</button>
<button type="submit" name="decision" value="deny" formnovalidate>Deny</button>
</div>
</form>`,
	);
}

/** A short page with a heading and one paragraph, for errors and refusals. */
export function messagePage(nonce: string, title: string, message: string): string {
	return layout(nonce, title, `<h1>${escapeHtml(title)}</h1>\n<p>${escapeHtml(message)}</p>`);
}

/**
 * Renders a page with a fresh style nonce and the matching security headers. `formAction` is
 * where the page's form may post and redirect (leave it out for pages without a form);
 * `headers` adds more, such as Retry-After.
 */
export function htmlResponse(
	render: (nonce: string) => string,
	init: { status?: number; formAction?: string[]; headers?: Headers } = {},
): Response {
	const nonce = newNonce();
	const headers = htmlHeaders(nonce, init.formAction);
	for (const [name, value] of init.headers ?? []) headers.set(name, value);
	return new Response(render(nonce), { status: init.status ?? 200, headers });
}

export function messageResponse(status: number, title: string, message: string, headers?: Headers): Response {
	return htmlResponse((nonce) => messagePage(nonce, title, message), { status, headers });
}
