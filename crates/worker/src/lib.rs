//! The `pcloud-mcp-core` Worker. It sits behind the OAuth front (the `pcloud-mcp` Worker),
//! which reaches it only through a service binding, and serves:
//!
//! - `POST /mcp` (and the 405s for other methods), guarded by the internal bearer token;
//! - `GET /healthz`, which answers "ok" without auth or a backend call;
//! - 404 for everything else.
//!
//! Panics: workers-rs installs its own panic hook at start-up, which logs "Rust panic: ..."
//! with `console.error` (visible in `wrangler tail` and Workers Logs), and worker-build's
//! shim reinitialises the Wasm instance after one, so no extra hook crate is needed.

mod backend;
mod settings;

use std::time::Duration;

use pcloud_mcp_core::{HttpRequest, HttpResponse, McpServer};
use pcloud_mcp_webdav::WebDavStorage;
use subtle::ConstantTimeEq;
use worker::{
    Context, Delay, Env, Headers, Request, Response, ResponseBody, Result, console_error,
    console_warn, event,
};

use crate::backend::Backend;

/// JSON-RPC requests are small; this only stops someone (with the token) from making the
/// Worker buffer something huge.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// How long `initialize` waits for the owner's guide before answering without it, so a file
/// server that doesn't answer can't hold up the handshake.
const GUIDE_DEADLINE: Duration = Duration::from_millis(1500);

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    // The raw method: worker::Method maps methods it doesn't know to GET.
    let method = req.inner().method().to_ascii_uppercase();
    let path = req.path();
    match (method.as_str(), path.as_str()) {
        ("GET" | "HEAD", "/healthz") => Response::ok("ok"),
        (_, "/mcp") => mcp(req, method, &env).await,
        _ => Response::error("not found", 404),
    }
}

async fn mcp(mut req: Request, method: String, env: &Env) -> Result<Response> {
    let lookup = |name: &str| env_text(env, name);

    let Some(token) = settings::internal_token(&lookup) else {
        console_error!("INTERNAL_TOKEN is not set, so every /mcp request is refused");
        return Response::error("server misconfigured: INTERNAL_TOKEN is not set", 500);
    };
    if !bearer_matches(req.headers().get("Authorization")?.as_deref(), &token) {
        console_warn!("{method} /mcp refused: missing or wrong internal token");
        let mut response = Response::error("unauthorized", 401)?;
        response.headers_mut().set("WWW-Authenticate", "Bearer")?;
        return Ok(response);
    }

    let declared_len = req
        .headers()
        .get("Content-Length")?
        .and_then(|v| v.trim().parse::<usize>().ok());
    if declared_len.is_some_and(|n| n > MAX_BODY_BYTES) {
        return Response::error("request body too large", 413);
    }
    let body = if matches!(method.as_str(), "GET" | "HEAD") {
        Vec::new()
    } else {
        req.bytes().await?
    };
    if body.len() > MAX_BODY_BYTES {
        return Response::error("request body too large", 413);
    }

    let settings = settings::load(&lookup);
    for warning in &settings.warnings {
        console_warn!("{warning}");
    }
    let backend = match Backend::from_env(env) {
        Ok(backend) => backend,
        Err(why) => {
            console_error!("{why}");
            return Response::error(format!("server misconfigured: {why}"), 500);
        }
    };
    let via = backend.describe();
    let storage = WebDavStorage::new(backend, &settings.backend_user, &settings.backend_password)
        .with_max_listing_bytes(settings.max_listing_bytes);
    let server = McpServer::new(storage, settings.server)
        .with_guide_deadline(|| Delay::from(GUIDE_DEADLINE));

    let request = HttpRequest {
        method: method.clone(),
        path: "/mcp".into(),
        headers: req.headers().entries().collect(),
        body,
    };
    let origin = request.header("origin").map(loggable);
    let response = server.handle_http(request).await;
    if response.status >= 500 {
        console_error!(
            "{method} /mcp answered {} (backend via {via})",
            response.status
        );
    } else if response.status == 403
        && let Some(origin) = origin
    {
        // The core's only 403. Logged so a client that does send an Origin can be added.
        console_warn!(
            "{method} /mcp refused Origin {origin:?}; add it to ALLOWED_ORIGINS if it should be allowed"
        );
    }
    to_worker_response(response)
}

/// A header value cut to a length that keeps the log readable.
fn loggable(value: &str) -> String {
    const MAX_CHARS: usize = 200;
    match value.char_indices().nth(MAX_CHARS) {
        Some((end, _)) => format!("{}...", &value[..end]),
        None => value.to_owned(),
    }
}

/// A var or secret as it is set (blank included; `settings` decides what blank means). In
/// workers-rs 0.8.7 `Secret` and `Var` are the same string binding, so `Env::var` reads both:
/// any setting may be a secret, which keeps SEARCH_SKIP's folder names out of wrangler.toml.
fn env_text(env: &Env, name: &str) -> Option<String> {
    env.var(name).ok().map(|v| v.to_string())
}

/// Checks `Authorization: Bearer <token>`. The comparison takes the same time wherever the
/// first wrong byte is (it still returns early on a length mismatch, which only reveals the
/// token's length).
fn bearer_matches(header: Option<&str>, token: &str) -> bool {
    let Some((scheme, presented)) = header.map(str::trim).and_then(|h| h.split_once(' ')) else {
        return false;
    };
    scheme.eq_ignore_ascii_case("bearer")
        && bool::from(presented.trim().as_bytes().ct_eq(token.as_bytes()))
}

fn to_worker_response(response: HttpResponse) -> Result<Response> {
    let headers = Headers::new();
    for (name, value) in &response.headers {
        headers.append(name, value)?;
    }
    // A null body for empty responses: a 202 or 204 with an empty-but-present body throws.
    let out = if response.body.is_empty() {
        Response::empty()?
    } else {
        Response::from_body(ResponseBody::Body(response.body))?
    };
    Ok(out.with_status(response.status).with_headers(headers))
}

#[cfg(test)]
mod tests {
    use super::bearer_matches;

    #[test]
    fn bearer_token_check() {
        let token = "dev-internal-token";
        assert!(bearer_matches(Some("Bearer dev-internal-token"), token));
        assert!(bearer_matches(Some("bearer  dev-internal-token "), token));
        assert!(!bearer_matches(None, token));
        assert!(!bearer_matches(Some(""), token));
        assert!(!bearer_matches(Some("Bearer"), token));
        assert!(!bearer_matches(Some("Bearer "), token));
        assert!(!bearer_matches(Some("Basic dev-internal-token"), token));
        assert!(!bearer_matches(Some("Bearer dev-internal-tokem"), token));
        assert!(!bearer_matches(
            Some("Bearer dev-internal-token-and-more"),
            token
        ));
        assert!(!bearer_matches(Some("Bearer dev"), token));
    }
}
