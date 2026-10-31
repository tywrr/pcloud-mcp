//! The HTTP side of the `chance-of-files` binary: routes, the internal-token check and the
//! scope header, around [`chance_of_files_core::McpServer`].
//!
//! - `POST /mcp` (other methods get the core's 405): needs `Authorization: Bearer
//!   <INTERNAL_TOKEN>`. Only then is `X-CoF-Scopes` read; the auth Worker sets it from the
//!   OAuth grant and strips any copy a client sent.
//! - `GET /healthz`: the process is up. No auth, no backend call.
//! - `GET /readyz`: rclone answers `operations/about`. No auth, no details.

pub mod config;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use chance_of_files_core::{HttpRequest, McpServer, SCOPES_HEADER, Scopes, Storage};
use subtle::ConstantTimeEq;

/// How long a readiness probe waits for rclone.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct App<S: Storage> {
    pub mcp: McpServer<S>,
    internal_token: String,
    max_body_bytes: usize,
}

impl<S: Storage> App<S> {
    pub fn new(mcp: McpServer<S>, internal_token: String, max_body_bytes: usize) -> Self {
        Self {
            mcp,
            internal_token,
            max_body_bytes,
        }
    }
}

pub fn router<S: Storage + 'static>(app: Arc<App<S>>) -> Router {
    Router::new()
        .route("/mcp", any(mcp::<S>))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(ready::<S>))
        .fallback(|| async { (StatusCode::NOT_FOUND, "not found") })
        .with_state(app)
}

async fn ready<S: Storage>(State(app): State<Arc<App<S>>>) -> Response {
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

async fn mcp<S: Storage>(State(app): State<Arc<App<S>>>, request: Request) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    if !bearer_matches(
        request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        &app.internal_token,
    ) {
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
    let (rpc_method, tool) = describe(&body);
    let request = HttpRequest {
        method: method.as_str().to_ascii_uppercase(),
        path: "/mcp".into(),
        headers: plain_headers(&parts.headers),
        body,
        scopes,
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
