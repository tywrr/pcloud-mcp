//! The HTTP side of the `chance-of-files` binary: routes, the internal-token check and the
//! scope header, around [`chance_of_files_core::McpServer`].
//!
//! - `POST /mcp` (other methods get the core's 405): needs `Authorization: Bearer
//!   <INTERNAL_TOKEN>`. Only then is `X-CoF-Scopes` read; the auth Worker sets it from the
//!   OAuth grant and strips any copy a client sent.
//! - `GET /healthz`: the process is up. No auth, no backend call.
//! - `GET /readyz`: rclone answers `operations/about`. No auth, no details.
//! - `/dl/<token>/<name>`, `/ul/<token>`: links (see [`links`]); the internal token too.

pub mod config;
pub mod links;
pub mod state;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use chance_of_files_core::{
    Health, HttpRequest, McpServer, SCOPES_HEADER, Scopes, Storage, StorageWrite,
};
use subtle::ConstantTimeEq;

use crate::links::LinkServer;

/// The auth Worker's public origin (`https://chance-of-files.<sub>.workers.dev`), which link
/// URLs start with; trusted only with the internal token.
pub const PUBLIC_URL_HEADER: &str = "x-cof-public-url";

/// How long a readiness probe waits for rclone.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one health check waits.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(15);
/// How often a failing backend is checked again.
const UNHEALTHY_EVERY: Duration = Duration::from_secs(30);

/// Checks the backend every `every` (every 30 s while it fails), and right away when a call
/// reports a backend error, and keeps `health` up to date for the tools and the indexer.
pub async fn health_loop<S: Storage>(storage: S, health: Arc<Health>, every: Duration) {
    loop {
        let result = match tokio::time::timeout(HEALTH_TIMEOUT, storage.health()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(err.to_string()),
            Err(_) => Err(format!("no answer within {} s", HEALTH_TIMEOUT.as_secs())),
        };
        match result {
            Ok(()) => {
                if let Some(was) = health.status() {
                    tracing::info!(since = was.since, "storage backend healthy again");
                }
                health.set_ok();
            }
            Err(error) => {
                if health.is_healthy() {
                    tracing::warn!(error = %error, "storage backend unhealthy");
                }
                health.set_failed(&error);
            }
        }
        let wait = if health.is_healthy() {
            every
        } else {
            every.min(UNHEALTHY_EVERY)
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {},
            () = health.poked() => {
                // A burst of failing calls asks many times; one check is enough.
                tokio::time::sleep(Duration::from_secs(1)).await;
            },
        }
    }
}

pub struct App<S: StorageWrite> {
    pub mcp: McpServer<S>,
    internal_token: String,
    max_body_bytes: usize,
    pub links: Option<LinkServer>,
}

impl<S: StorageWrite> App<S> {
    pub fn new(mcp: McpServer<S>, internal_token: String, max_body_bytes: usize) -> Self {
        Self {
            mcp,
            internal_token,
            max_body_bytes,
            links: None,
        }
    }

    /// Serves `/dl` and `/ul`.
    pub fn with_links(mut self, links: LinkServer) -> Self {
        self.links = Some(links);
        self
    }

    /// The request carries the internal token.
    pub fn authorized(&self, headers: &HeaderMap) -> bool {
        bearer_matches(
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            &self.internal_token,
        )
    }
}

pub fn router<S: StorageWrite + 'static>(app: Arc<App<S>>) -> Router {
    Router::new()
        .route("/mcp", any(mcp::<S>))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(ready::<S>))
        .route(
            "/dl/{*rest}",
            any(|State(app): State<Arc<App<S>>>, request: Request| links::download(app, request)),
        )
        .route(
            "/ul/{*rest}",
            any(|State(app): State<Arc<App<S>>>, request: Request| links::upload(app, request)),
        )
        .fallback(|| async { (StatusCode::NOT_FOUND, "not found") })
        .with_state(app)
}

async fn ready<S: StorageWrite>(State(app): State<Arc<App<S>>>) -> Response {
    match tokio::time::timeout(READY_TIMEOUT, app.mcp.storage().health()).await {
        Ok(Ok(())) => (StatusCode::OK, "ready").into_response(),
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "not ready: the storage backend failed its check");
            (StatusCode::SERVICE_UNAVAILABLE, "not ready").into_response()
        }
        Err(_) => {
            tracing::warn!("not ready: the storage backend didn't answer in time");
            (StatusCode::SERVICE_UNAVAILABLE, "not ready").into_response()
        }
    }
}

async fn mcp<S: StorageWrite>(State(app): State<Arc<App<S>>>, request: Request) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    if !app.authorized(request.headers()) {
        tracing::warn!(%method, path = "/mcp", status = 401, "missing or wrong internal token");
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "unauthorized",
        )
            .into_response();
    }

    let (parts, body) = request.into_parts();
    if parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<usize>().ok())
        .is_some_and(|n| n > app.max_body_bytes)
    {
        return too_large(&method);
    }
    let body = if method == Method::GET || method == Method::HEAD {
        Vec::new()
    } else {
        match to_bytes(body, app.max_body_bytes).await {
            Ok(bytes) => bytes.to_vec(),
            Err(_) => return too_large(&method),
        }
    };

    // Trusted only now that the request proved it came through the auth Worker.
    let scopes = Scopes::from_header(
        parts
            .headers
            .get(SCOPES_HEADER)
            .and_then(|v| v.to_str().ok()),
    );
    let public_url = parts
        .headers
        .get(PUBLIC_URL_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    let (rpc_method, tool) = describe(&body);
    let request = HttpRequest {
        method: method.as_str().to_ascii_uppercase(),
        path: "/mcp".into(),
        headers: plain_headers(&parts.headers),
        body,
        scopes,
        public_url,
    };
    let response = app.mcp.handle_http(request).await;
    tracing::info!(
        %method,
        path = "/mcp",
        status = response.status,
        rpc = rpc_method.as_deref().unwrap_or(""),
        tool = tool.as_deref().unwrap_or(""),
        ms = started.elapsed().as_millis() as u64,
        "request"
    );
    to_axum(response)
}

fn too_large(method: &Method) -> Response {
    tracing::warn!(%method, path = "/mcp", status = 413, "request body too large");
    (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response()
}

/// The JSON-RPC method and tool name, for the log. Arguments are never logged.
fn describe(body: &[u8]) -> (Option<String>, Option<String>) {
    let Ok(message) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (None, None);
    };
    let text = |v: Option<&serde_json::Value>| {
        v.and_then(serde_json::Value::as_str)
            .map(|s| s.chars().take(64).collect::<String>())
    };
    (
        text(message.get("method")),
        text(message.get("params").and_then(|p| p.get("name"))),
    )
}

/// Request headers for the core, minus the credentials it has no use for.
fn plain_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| **name != header::AUTHORIZATION && **name != header::COOKIE)
        .filter_map(|(name, value)| {
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect()
}

fn to_axum(response: chance_of_files_core::HttpResponse) -> Response {
    let mut out = Response::new(Body::from(response.body));
    *out.status_mut() =
        StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    for (name, value) in response.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::try_from(value.as_str()),
        ) {
            out.headers_mut().append(name, value);
        }
    }
    out
}

/// Checks `Authorization: Bearer <token>`. The comparison takes the same time wherever the
/// first wrong byte is (it still returns early on a length mismatch, which only reveals the
/// token's length).
pub fn bearer_matches(header: Option<&str>, token: &str) -> bool {
    let Some((scheme, presented)) = header.map(str::trim).and_then(|h| h.split_once(' ')) else {
        return false;
    };
    !token.is_empty()
        && scheme.eq_ignore_ascii_case("bearer")
        && bool::from(presented.trim().as_bytes().ct_eq(token.as_bytes()))
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
        assert!(!bearer_matches(Some("Bearer "), ""));
    }
}
