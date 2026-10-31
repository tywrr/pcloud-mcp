//! The axum app with a fake storage: auth, scopes, limits and probes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chance_of_files_core::{
    ByteRange, ByteStream, Capabilities, CheckedPath, Entry, McpServer, Policy, ServerConfig,
    Storage, StorageError, stream_of,
};
use chance_of_files_server::{App, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const TOKEN: &str = "test-internal-token-0123456789abcdef";

/// `/` holds `notes.txt` and the hidden folder `/Secret`.
#[derive(Default)]
struct Fake {
    broken: AtomicBool,
}

impl Storage for Fake {
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    async fn list(&self, path: &CheckedPath) -> Result<Vec<Entry>, StorageError> {
        match path.as_str() {
            "/" => Ok(vec![
                Entry {
                    path: "/notes.txt".into(),
                    name: "notes.txt".into(),
                    size: Some(5),
                    ..Entry::default()
                },
                Entry {
                    path: "/Secret".into(),
                    name: "Secret".into(),
                    is_dir: true,
                    ..Entry::default()
                },
            ]),
            other => Err(StorageError::NotFound(other.into())),
        }
    }
    async fn walk(
        &self,
        root: &CheckedPath,
        _: &[CheckedPath],
    ) -> Result<Vec<Entry>, StorageError> {
        self.list(root).await
    }
    async fn stat(&self, path: &CheckedPath, _: bool) -> Result<Entry, StorageError> {
        match path.as_str() {
            "/notes.txt" => Ok(Entry {
                path: "/notes.txt".into(),
                name: "notes.txt".into(),
                size: Some(5),
                ..Entry::default()
            }),
            other => Err(StorageError::NotFound(other.into())),
        }
    }
    async fn open(
        &self,
        path: &CheckedPath,
        _: Option<ByteRange>,
    ) -> Result<ByteStream, StorageError> {
        match path.as_str() {
            "/notes.txt" => Ok(stream_of("hello")),
            other => Err(StorageError::NotFound(other.into())),
        }
    }
    async fn health(&self) -> Result<(), StorageError> {
        if self.broken.load(Ordering::Relaxed) {
            Err(StorageError::Backend(
                "rclone said: secret-ish detail".into(),
            ))
        } else {
            Ok(())
        }
    }
}

fn app() -> (Router, Arc<App<Fake>>) {
    let policy = Policy::new(&["/secret"], &[] as &[&str], &[] as &[&str]).unwrap();
    let mcp = McpServer::new(Fake::default(), ServerConfig::default()).with_policy(policy);
    let app = Arc::new(App::new(mcp, TOKEN.into(), 4096));
    (router(app.clone()), app)
}

async fn send(
    router: &Router,
    request: Request<Body>,
) -> (StatusCode, Vec<(String, String)>, Vec<u8>) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_owned()))
        .collect();
    let body = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

fn rpc(method: &str, params: Value) -> Request<Body> {
    rpc_with(method, params, Some(TOKEN), None)
}

fn rpc_with(
    method: &str,
    params: Value,
    token: Option<&str>,
    scopes: Option<&str>,
) -> Request<Body> {
    let mut builder = Request::post("/mcp").header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(scopes) = scopes {
        builder = builder.header("x-cof-scopes", scopes);
    }
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    builder.body(Body::from(body.to_string())).unwrap()
}

async fn json_of(router: &Router, request: Request<Body>) -> Value {
    let (status, _, body) = send(router, request).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn health_and_readiness_need_no_token() {
    let (router, app) = app();
    let (status, _, body) = send(
        &router,
        Request::get("/healthz").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(
        (status, body.as_slice()),
        (StatusCode::OK, b"ok".as_slice())
    );
    let (status, _, body) = send(
        &router,
        Request::get("/readyz").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(
        (status, body.as_slice()),
        (StatusCode::OK, b"ready".as_slice())
    );

    app.mcp.storage().broken.store(true, Ordering::Relaxed);
    let (status, _, body) = send(
        &router,
        Request::get("/readyz").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, b"not ready", "no details leak");
}

#[tokio::test]
async fn mcp_needs_the_internal_token() {
    let (router, _) = app();
    for token in [None, Some("wrong"), Some("")] {
        let (status, headers, _) = send(
            &router,
            rpc_with(
                "tools/list",
                json!({}),
                token,
                Some("files:read files:write"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{token:?}");
        assert!(headers.contains(&("www-authenticate".into(), "Bearer".into())));
    }
    let request = Request::get("/mcp").body(Body::empty()).unwrap();
    assert_eq!(send(&router, request).await.0, StatusCode::UNAUTHORIZED);
    let request = Request::get("/mcp")
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&router, request).await.0,
        StatusCode::METHOD_NOT_ALLOWED
    );
}

fn tool_count(response: &Value) -> usize {
    response["result"]["tools"].as_array().unwrap().len()
}

#[tokio::test]
async fn scopes_come_from_the_header_once_the_token_checks_out() {
    let (router, _) = app();
    let none = json_of(&router, rpc("tools/list", json!({}))).await;
    assert_eq!(tool_count(&none), 5, "no header: read-only");
    for (scopes, expected) in [
        ("files:read", 5),
        ("files:read files:write", 5),
        ("files:write", 0),
        ("", 0),
    ] {
        let response = json_of(
            &router,
            rpc_with("tools/list", json!({}), Some(TOKEN), Some(scopes)),
        )
        .await;
        assert_eq!(tool_count(&response), expected, "{scopes:?}");
    }
    let refused = json_of(
        &router,
        rpc_with(
            "tools/call",
            json!({"name": "list_folder", "arguments": {}}),
            Some(TOKEN),
            Some("files:write"),
        ),
    )
    .await;
    assert_eq!(refused["result"]["isError"], json!(true));
}

#[tokio::test]
async fn tools_run_through_the_policy() {
    let (router, _) = app();
    let listing = json_of(
        &router,
        rpc(
            "tools/call",
            json!({"name": "list_folder", "arguments": {"path": "/"}}),
        ),
    )
    .await;
    let text = listing["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("notes.txt") && !text.contains("Secret"),
        "{text}"
    );
    let read = json_of(
        &router,
        rpc(
            "tools/call",
            json!({"name": "read_text_file", "arguments": {"path": "/notes.txt"}}),
        ),
    )
    .await;
    assert_eq!(read["result"]["content"][0]["text"], json!("hello"));
    let hidden = json_of(
        &router,
        rpc(
            "tools/call",
            json!({"name": "list_folder", "arguments": {"path": "/SECRET"}}),
        ),
    )
    .await;
    assert_eq!(hidden["result"]["isError"], json!(true));
    assert!(
        hidden["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("not found: /SECRET")
    );
}

#[tokio::test]
async fn big_bodies_and_unknown_routes() {
    let (router, _) = app();
    let request = Request::post("/mcp")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(vec![b' '; 5000]))
        .unwrap();
    assert_eq!(
        send(&router, request).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let request = Request::get("/nope").body(Body::empty()).unwrap();
    assert_eq!(send(&router, request).await.0, StatusCode::NOT_FOUND);
}
