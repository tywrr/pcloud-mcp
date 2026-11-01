//! `/dl` and `/ul` through the axum app, over an in-memory tree and a real state.db: the
//! token, expiry, revocation, network binding and byte checks the core repeats after the
//! Worker, `Range` (with the 416 rclone gets wrong), streamed zips, and chunked uploads.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chance_of_files_core::memory::MemoryStore;
use chance_of_files_core::{
    Caller, LinkKey, LinkKind, LinkRow, LinkSettings, LinkState, McpServer, Policy, Scopes,
    ServerConfig, StateStore, time,
};
use chance_of_files_server::links::LinkServer;
use chance_of_files_server::state::State;
use chance_of_files_server::{App, router};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const TOKEN: &str = "test-internal-token-0123456789abcdef";
const KEY: &str = "test-link-key-0123456789abcdef0123456789";
const FRONT: &str = "https://cof.example.workers.dev";
const IP: &str = "160.79.106.131";

fn data() -> Vec<u8> {
    (0..1000u32).map(|i| (i * 7 % 251) as u8).collect()
}

struct Fixture {
    app: Arc<App<Arc<MemoryStore>>>,
    router: Router,
    store: Arc<MemoryStore>,
    state: Arc<State>,
    dir: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture() -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "cof-links-{}-{}",
        std::process::id(),
        chance_of_files_core::links::random_bytes::<6>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ));
    let store = Arc::new(
        MemoryStore::new()
            .file("/Data/a.bin", data())
            .file("/Data/sub/b.txt", "bee")
            .dir("/Data/empty")
            .file("/Data/Private/x", "hidden")
            .dir("/Uploads")
            .file("/Backups/Immich/p.jpg", "jpg"),
    );
    let state = Arc::new(State::open(&dir, &dir.join("staging")).unwrap());
    let mut links = LinkSettings::new(LinkKey::new(KEY).unwrap());
    links.part_bytes = 10;
    links.reserve_bytes = 0;
    let policy = Policy::new(&["/Data/Private"], &["/Backups/Immich"], &[] as &[&str]).unwrap();
    let mcp = McpServer::new(store.clone(), ServerConfig::default())
        .with_policy(policy)
        .with_state(state.clone() as Arc<dyn StateStore>)
        .with_links(links.clone());
    let app = Arc::new(
        App::new(mcp, TOKEN.into(), 1 << 20).with_links(LinkServer::new(state.clone(), links)),
    );
    Fixture {
        router: router(app.clone()),
        app,
        store,
        state,
        dir,
    }
}

impl Fixture {
    async fn tool(&self, name: &str, args: Value) -> String {
        let caller = Caller {
            scopes: Scopes::all(),
            public_url: Some(FRONT.into()),
        };
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": args}});
        let response = self.app.mcp.handle_jsonrpc_as(msg, &caller).await.unwrap();
        let result = &response["result"];
        let text = result["content"][0]["text"].as_str().unwrap().to_owned();
        assert_ne!(result["isError"], json!(true), "{name}: {text}");
        text
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, headers, body)
    }

    async fn get(
        &self,
        path: &str,
        extra: &[(&str, &str)],
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let mut builder = Request::get(path)
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("x-cof-client-ip", IP);
        for (k, v) in extra {
            builder = builder.header(*k, *v);
        }
        self.send(builder.body(Body::empty()).unwrap()).await
    }

    async fn put(&self, path: &str, body: Vec<u8>) -> (StatusCode, Value) {
        let request = Request::put(path)
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("x-cof-client-ip", IP)
            .header("content-length", body.len())
            .body(Body::from(body))
            .unwrap();
        let (status, _, body) = self.send(request).await;
        let value = serde_json::from_slice(&body).unwrap_or(json!(String::from_utf8_lossy(&body)));
        (status, value)
    }

    /// A link row of our own, for expiry and byte-cap cases.
    fn insert(&self, id: &str, expires: i64, max_bytes: u64, state: LinkState) -> String {
        self.state
            .insert_link(&LinkRow {
                id: id.into(),
                kind: LinkKind::Download,
                paths: vec!["/Data/a.bin".into()],
                zip: false,
                name: "a.bin".into(),
                created: time::now(),
                expires,
                bound_net: None,
                size: 1000,
                max_bytes,
                bytes_used: 0,
                committed: 0,
                overwrite: false,
                state,
                note: None,
            })
            .unwrap();
        let token = LinkKey::new(KEY)
            .unwrap()
            .token(LinkKind::Download, id, expires);
        format!("/dl/{token}/a.bin")
    }
}

/// The path part of the first link URL in a tool's text.
fn link_path(text: &str, route: &str) -> String {
    let start = text.find(&format!("{FRONT}{route}")).expect("a link") + FRONT.len();
    text[start..]
        .split(['\'', ' ', '\n', '"'])
        .next()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn single_file_downloads() {
    let f = fixture();
    let text = f
        .tool("create_download_link", json!({"paths": ["/Data/a.bin"]}))
        .await;
    let path = link_path(&text, "/dl/");

    let (status, _, _) = f
        .send(
            Request::get(&path)
                .header("x-cof-client-ip", IP)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, headers, body) = f.get(&path, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, data());
    assert_eq!(headers["content-security-policy"], "sandbox");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(headers["content-type"], "application/octet-stream");
    assert_eq!(headers["accept-ranges"], "bytes");
    assert!(
        headers["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment;")
    );

    let (status, headers, body) = f.get(&path, &[("range", "bytes=10-19")]).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers["content-range"], "bytes 10-19/1000");
    assert_eq!(body, &data()[10..20]);
    let (status, _, body) = f.get(&path, &[("range", "bytes=-5")]).await;
    assert_eq!(
        (status, body.as_slice()),
        (StatusCode::PARTIAL_CONTENT, &data()[995..])
    );
    let (status, headers, body) = f.get(&path, &[("range", "bytes=1000-")]).await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(headers["content-range"], "bytes */1000");
    assert!(!body.is_empty() && body.len() < 100);

    // HEAD answers headers only and costs nothing.
    let head = Request::head(&path)
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("x-cof-client-ip", IP)
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = f.send(head).await;
    assert_eq!((status, body.len()), (StatusCode::OK, 0));
    assert_eq!(headers["content-length"], "1000");

    // Bound to the first client's /24.
    let (status, _, _) = f
        .send(
            Request::get(&path)
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("x-cof-client-ip", "160.79.106.7")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "same /24");
    let (status, _, _) = f
        .send(
            Request::get(&path)
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("x-cof-client-ip", "203.0.113.9")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A wrong MAC, or a download token on the upload route, finds nothing.
    let mut tampered = path.clone();
    let at = tampered.rfind('/').unwrap() - 3;
    tampered.replace_range(at..at + 1, if &path[at..at + 1] == "A" { "B" } else { "A" });
    assert_eq!(f.get(&tampered, &[]).await.0, StatusCode::NOT_FOUND);
    let as_upload = path.replacen("/dl/", "/ul/", 1);
    let as_upload = &as_upload[..as_upload.rfind('/').unwrap()];
    assert_eq!(f.get(as_upload, &[]).await.0, StatusCode::NOT_FOUND);

    // Every use is logged.
    let id = f.state.links(1).unwrap()[0].id.clone();
    let uses = f.state.uses(&id, 50).unwrap();
    assert!(
        uses.iter()
            .any(|u| u.status == 403 && u.ip == "203.0.113.9")
    );
    assert!(uses.iter().any(|u| u.status == 416));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let uses = f.state.uses(&id, 50).unwrap();
    assert!(
        uses.iter()
            .any(|u| u.status == 206 && u.bytes == 10 && u.range == "bytes=10-19"),
        "{uses:?}"
    );

    // Revoked: gone.
    f.tool("revoke_link", json!({"id": id})).await;
    assert_eq!(f.get(&path, &[]).await.0, StatusCode::GONE);
}

#[tokio::test]
async fn expiry_and_the_byte_allowance() {
    let f = fixture();
    let expired = f.insert(
        "AAAAAAAAAAAAAAAAAAAAAA",
        time::now() - 1,
        5000,
        LinkState::Active,
    );
    let (status, _, body) = f.get(&expired, &[]).await;
    assert_eq!(status, StatusCode::GONE);
    assert!(String::from_utf8_lossy(&body).contains("expired"));

    let capped = f.insert(
        "BBBBBBBBBBBBBBBBBBBBBA",
        time::now() + 600,
        2500,
        LinkState::Active,
    );
    assert_eq!(f.get(&capped, &[]).await.0, StatusCode::OK);
    assert_eq!(f.get(&capped, &[]).await.0, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(f.get(&capped, &[]).await.0, StatusCode::GONE, "3000 > 2500");
    // A range that still fits goes through.
    assert_eq!(
        f.get(&capped, &[("range", "bytes=0-99")]).await.0,
        StatusCode::PARTIAL_CONTENT
    );
    let revoked = f.insert(
        "CCCCCCCCCCCCCCCCCCCCCA",
        time::now() + 600,
        5000,
        LinkState::Revoked,
    );
    assert_eq!(f.get(&revoked, &[]).await.0, StatusCode::GONE);
}

#[tokio::test]
async fn zip_downloads() {
    let f = fixture();
    let text = f
        .tool(
            "create_download_link",
            json!({"paths": ["/Data"], "zip": true}),
        )
        .await;
    let path = link_path(&text, "/dl/");
    assert!(path.ends_with("/Data.zip"), "{path}");
    let (status, headers, body) = f.get(&path, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-length"], body.len().to_string().as_str());
    assert_eq!(&body[..4], b"PK\x03\x04");
    let names = String::from_utf8_lossy(&body);
    assert!(names.contains("Data/sub/b.txt") && names.contains("Data/empty/"));
    assert!(
        !names.contains("Private") && !names.contains("hidden"),
        "hidden stays out"
    );
    // unzip checks every CRC, where it is installed.
    if std::process::Command::new("unzip")
        .arg("-v")
        .output()
        .is_ok()
    {
        let file = f.dir.join("t.zip");
        std::fs::write(&file, &body).unwrap();
        let out = std::process::Command::new("unzip")
            .arg("-t")
            .arg(&file)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        let listing = std::process::Command::new("unzip")
            .arg("-p")
            .arg(&file)
            .arg("Data/a.bin")
            .output()
            .unwrap();
        assert_eq!(listing.stdout, data());
    }
}

#[tokio::test]
async fn chunked_uploads() {
    let f = fixture();
    let text = f
        .tool(
            "create_upload_link",
            json!({"folder": "/Uploads", "name": "up.bin", "size": 25}),
        )
        .await;
    assert!(text.contains("3 parts"), "{text}");
    let path = link_path(&text, "/ul/");
    let content: Vec<u8> = (0..25u8).collect();

    let (status, body) = f
        .put(&format!("{path}?part=1"), content[10..20].to_vec())
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["committed"], 0);
    let (status, body) = f
        .put(&format!("{path}?part=0"), content[..10].to_vec())
        .await;
    assert_eq!(
        (status, &body["committed"]),
        (StatusCode::OK, &json!(10)),
        "{body}"
    );
    let (status, body) = f
        .put(&format!("{path}?part=0"), content[..10].to_vec())
        .await;
    assert_eq!((status, &body["skipped"]), (StatusCode::OK, &json!(true)));
    let (status, _) = f
        .put(&format!("{path}?part=1"), content[10..15].to_vec())
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "short part");
    // With Content-Range instead of ?part.
    let request = Request::put(&path)
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("x-cof-client-ip", IP)
        .header("content-range", "bytes 10-19/25")
        .body(Body::from(content[10..20].to_vec()))
        .unwrap();
    assert_eq!(f.send(request).await.0, StatusCode::OK);

    let head = Request::head(&path)
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("x-cof-client-ip", IP)
        .body(Body::empty())
        .unwrap();
    let (_, headers, _) = f.send(head).await;
    assert_eq!(headers["upload-offset"], "20");
    assert_eq!(headers["upload-length"], "25");

    let (status, body) = f
        .put(&format!("{path}?part=2"), content[20..].to_vec())
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let mut state = Value::Null;
    for _ in 0..50 {
        let (_, _, body) = f.get(&path, &[]).await;
        state = serde_json::from_slice(&body).unwrap();
        if state["state"] == "done" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(state["state"], "done", "{state}");
    assert!(state["note"].as_str().unwrap().contains("SHA-1"), "{state}");
    assert_eq!(f.store.contents("/Uploads/up.bin").unwrap(), content);
    let id = state["id"].as_str().unwrap().to_owned();
    assert!(!f.state.part_file(&id).exists(), "staging cleaned up");
    let (status, _) = f
        .put(&format!("{path}?part=0"), content[..10].to_vec())
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a finished upload takes no more parts"
    );
}

#[tokio::test]
async fn an_upload_whose_destination_appeared_meanwhile_fails() {
    let f = fixture();
    let text = f
        .tool(
            "create_upload_link",
            json!({"folder": "/Uploads", "name": "late.bin", "size": 5}),
        )
        .await;
    let path = link_path(&text, "/ul/");
    // The destination appears meanwhile; the link doesn't overwrite.
    f.tool(
        "write_file",
        json!({"path": "/Uploads/late.bin", "text": "first"}),
    )
    .await;
    let (status, _) = f.put(&format!("{path}?part=0"), b"12345".to_vec()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let mut state = Value::Null;
    for _ in 0..50 {
        let (_, _, body) = f.get(&path, &[]).await;
        state = serde_json::from_slice(&body).unwrap();
        if state["state"] != "finishing" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(state["state"], "failed", "{state}");
    assert!(
        state["note"].as_str().unwrap().contains("?finish"),
        "{state}"
    );
    assert_eq!(f.store.contents("/Uploads/late.bin").unwrap(), b"first");
}

#[tokio::test]
async fn a_part_that_breaks_off_gives_its_bytes_back() {
    let f = fixture();
    let text = f
        .tool(
            "create_upload_link",
            json!({"folder": "/Uploads", "name": "short.bin", "size": 25}),
        )
        .await;
    let path = link_path(&text, "/ul/");
    let id = f.state.links(1).unwrap()[0].id.clone();
    // No Content-Length, so only the body's end shows the part is short: write_part fails.
    for _ in 0..5 {
        let request = Request::put(&path)
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("x-cof-client-ip", IP)
            .header("content-range", "bytes 0-9/25")
            .body(Body::from(vec![1u8; 5]))
            .unwrap();
        assert_eq!(f.send(request).await.0, StatusCode::BAD_REQUEST);
    }
    let row = f.state.link(&id).unwrap().unwrap();
    assert_eq!(
        (row.bytes_used, row.committed),
        (0, 0),
        "nothing kept, nothing charged"
    );
    // So retries never run the allowance (3x the size) dry.
    let (status, body) = f.put(&format!("{path}?part=0"), vec![1u8; 10]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(f.state.link(&id).unwrap().unwrap().bytes_used, 10);
}
