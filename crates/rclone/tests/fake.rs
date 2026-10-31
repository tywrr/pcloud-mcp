//! RcloneStorage against a fake rclone rcd (a small axum server), to pin down exactly what is
//! sent: no `_filter`, split walks, ranges, URLs, auth.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chance_of_files_core::{ByteRange, Policy, Storage, StorageError, read_capped};
use chance_of_files_rclone::{RcloneConfig, RcloneStorage};
use serde_json::{Value, json};

const PASSWORD: &str = "fake-rc-pass";

#[derive(Default)]
struct Fake {
    /// Remote-relative path ("" is the root) to Some(contents) for files, None for folders.
    tree: BTreeMap<String, Option<Vec<u8>>>,
    /// Every request: "POST operations/list {body}" or "GET <path> <range>".
    log: Vec<String>,
    /// How rc-serve answers ranges.
    ranges: RangeMode,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum RangeMode {
    #[default]
    Honest,
    /// 206 whose Content-Range starts somewhere else.
    Broken,
    /// 200 with the whole file.
    Ignored,
}

type Shared = Arc<Mutex<Fake>>;

fn tree() -> Fake {
    let mut fake = Fake::default();
    for dir in [
        "",
        "Backups",
        "Backups/Immich",
        "Backups/Immich/2026",
        "Backups/phone",
        "Crypto Folder",
        "Notes",
        "Notes/deep",
    ] {
        fake.tree.insert(dir.into(), None);
    }
    for (file, data) in [
        ("Backups/Immich/2026/IMG_1.jpg", "jpg"),
        ("Backups/phone/report.pdf", "pdf"),
        ("Crypto Folder/keys.txt", "secret"),
        ("Notes/todo.md", "0123456789"),
        ("Notes/deep/a b#?.txt", "odd"),
        ("readme.txt", "hello"),
    ] {
        fake.tree
            .insert(file.into(), Some(data.as_bytes().to_vec()));
    }
    fake
}

fn item(path: &str, data: &Option<Vec<u8>>) -> Value {
    json!({
        "Path": path,
        "Name": path.rsplit('/').next().unwrap(),
        "Size": data.as_ref().map_or(-1, |d| d.len() as i64),
        "MimeType": if data.is_some() { "text/plain; charset=utf-8" } else { "inode/directory" },
        "ModTime": "2026-10-29T12:00:00.123456789Z",
        "IsDir": data.is_none(),
        "ID": if data.is_some() { "f1" } else { "d1" },
    })
}

fn authorized(headers: &HeaderMap) -> bool {
    use base64::Engine;
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("core:{PASSWORD}"))
    );
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        == Some(expected.as_str())
}

async fn rc(State(fake): State<Shared>, request: Request) -> Response {
    let method = request.uri().path().trim_start_matches('/').to_owned();
    let headers = request.headers().clone();
    let body = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap();
    let mut fake = fake.lock().unwrap();
    fake.log
        .push(format!("POST {method} {}", String::from_utf8_lossy(&body)));
    if !authorized(&headers) {
        return (StatusCode::UNAUTHORIZED, "{}").into_response();
    }
    let input: Value = serde_json::from_slice(&body).unwrap();
    let remote = input["remote"].as_str().unwrap_or("").to_owned();
    let answer = match method.as_str() {
        "operations/list" => {
            if fake.tree.get(&remote) != Some(&None) {
                return (
                    StatusCode::NOT_FOUND,
                    axum::Json(
                        json!({"error": "error in ListJSON: directory not found", "status": 404}),
                    ),
                )
                    .into_response();
            }
            let recurse = input["opt"]["recurse"] == json!(true);
            let prefix = if remote.is_empty() {
                String::new()
            } else {
                format!("{remote}/")
            };
            let list: Vec<Value> = fake
                .tree
                .iter()
                .filter(|(p, _)| !p.is_empty() && p.starts_with(&prefix) && **p != remote)
                .filter(|(p, _)| recurse || !p[prefix.len()..].contains('/'))
                .map(|(p, d)| item(p, d))
                .collect();
            json!({"list": list})
        }
        "operations/stat" => match fake.tree.get(&remote) {
            Some(data) => {
                let mut it = item(&remote, data);
                if input["opt"]["showHash"] == json!(true) {
                    it["Hashes"] = json!({"sha1": "abc", "md5": "def"});
                }
                json!({"item": it})
            }
            None => json!({"item": null}),
        },
        "operations/about" => json!({"total": 100, "used": 10, "free": 90}),
        "operations/fsinfo" => json!({"Hashes": ["md5", "sha1"], "Features": {"DirMove": true}}),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    axum::Json(answer).into_response()
}

async fn serve(State(fake): State<Shared>, request: Request) -> Response {
    let raw_path = request.uri().path().to_owned();
    let range = request
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let headers = request.headers().clone();
    let mut fake = fake.lock().unwrap();
    fake.log.push(format!("GET {raw_path} {range}"));
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(rest) = raw_path
        .strip_prefix("/%5Bpcloud%3A%5D/")
        .or(raw_path.strip_prefix("/[pcloud%3A]/"))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = percent_decode(rest);
    let Some(Some(data)) = fake.tree.get(&path).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (start, end) = range
        .strip_prefix("bytes=")
        .and_then(|r| r.split_once('-'))
        .map(|(a, b)| (a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap()))
        .unwrap_or((0, data.len() - 1));
    match fake.ranges {
        RangeMode::Ignored => (StatusCode::OK, Bytes::from(data)).into_response(),
        RangeMode::Broken => (
            StatusCode::PARTIAL_CONTENT,
            [(
                header::CONTENT_RANGE,
                format!("bytes {}-{}/{}", start + 1, end, data.len()),
            )],
            Bytes::new(),
        )
            .into_response(),
        RangeMode::Honest => {
            let end = end.min(data.len() - 1);
            (
                StatusCode::PARTIAL_CONTENT,
                [(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{}", data.len()),
                )],
                Bytes::from(data[start..=end].to_vec()),
            )
                .into_response()
        }
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            out.push(u8::from_str_radix(&text[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

struct Harness {
    fake: Shared,
    storage: RcloneStorage,
    policy: Policy,
}

impl Harness {
    async fn new(fake: Fake, max_listing_bytes: u64) -> Self {
        Self::with_password(fake, max_listing_bytes, PASSWORD).await
    }

    async fn with_password(fake: Fake, max_listing_bytes: u64, password: &str) -> Self {
        let fake = Arc::new(Mutex::new(fake));
        let app = Router::new()
            .route("/operations/{*op}", axum::routing::post(rc))
            .fallback(axum::routing::get(serve))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let storage = RcloneStorage::new(RcloneConfig {
            url: format!("http://{addr}"),
            user: "core".into(),
            password: password.into(),
            remote: "pcloud".into(),
            max_listing_bytes,
        })
        .unwrap();
        let policy =
            Policy::new(&["/Crypto Folder"], &[] as &[&str], &["/Backups/Immich"]).unwrap();
        Self {
            fake,
            storage,
            policy,
        }
    }

    fn log(&self) -> Vec<String> {
        std::mem::take(&mut self.fake.lock().unwrap().log)
    }

    fn path(&self, path: &str) -> chance_of_files_core::CheckedPath {
        self.policy.check(path).unwrap()
    }
}

#[tokio::test]
async fn list_maps_entries_and_never_filters() {
    let h = Harness::new(tree(), 1_000_000).await;
    let mut entries = h.storage.list(&h.path("/Notes")).await.unwrap();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let todo = &entries[1];
    assert_eq!(todo.path, "/Notes/todo.md");
    assert_eq!(todo.name, "todo.md");
    assert_eq!(todo.size, Some(10));
    assert_eq!(todo.modified.as_deref(), Some("2026-10-29T12:00:00Z"));
    assert_eq!(
        todo.content_type.as_deref(),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(todo.id.as_deref(), Some("f1"));
    let deep = &entries[0];
    assert!(deep.is_dir && deep.size.is_none() && deep.content_type.is_none());
    assert_eq!(
        h.log(),
        [r#"POST operations/list {"fs":"pcloud:","opt":{},"remote":"Notes"}"#]
    );
}

#[tokio::test]
async fn list_errors() {
    let h = Harness::new(tree(), 1_000_000).await;
    assert_eq!(
        h.storage.list(&h.path("/nope")).await.unwrap_err(),
        StorageError::NotFound("/nope".into())
    );
    assert_eq!(
        h.storage.list(&h.path("/readme.txt")).await.unwrap_err(),
        StorageError::NotADirectory("/readme.txt".into())
    );
}

#[tokio::test]
async fn a_walk_with_nothing_excluded_is_one_recursive_list() {
    let h = Harness::new(tree(), 1_000_000).await;
    let entries = h.storage.walk(&h.path("/Notes"), &[]).await.unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(
        h.log(),
        [r#"POST operations/list {"fs":"pcloud:","opt":{"recurse":true},"remote":"Notes"}"#]
    );
}

#[tokio::test]
async fn a_walk_splits_around_excluded_folders_without_fetching_them() {
    let h = Harness::new(tree(), 1_000_000).await;
    // The harness policy can't make a CheckedPath for a hidden folder, so the excludes come
    // from a policy that hides nothing; case differences must not matter.
    let lenient = Policy::default();
    let exclude = [
        lenient.check("/crypto folder").unwrap(),
        lenient.check("/Backups/Immich").unwrap(),
    ];
    let entries = h.storage.walk(&h.path("/"), &exclude).await.unwrap();
    let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    assert!(paths.contains(&"/Backups/Immich"), "{paths:?}");
    assert!(paths.contains(&"/Crypto Folder"), "{paths:?}");
    assert!(paths.contains(&"/Backups/phone/report.pdf"), "{paths:?}");
    assert!(paths.contains(&"/Notes/deep/a b#?.txt"), "{paths:?}");
    assert!(
        !paths.iter().any(|p| p.starts_with("/Backups/Immich/")),
        "{paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.starts_with("/Crypto Folder/")),
        "{paths:?}"
    );
    let log = h.log();
    assert_eq!(
        log,
        [
            r#"POST operations/list {"fs":"pcloud:","opt":{},"remote":""}"#,
            r#"POST operations/list {"fs":"pcloud:","opt":{},"remote":"Backups"}"#,
            r#"POST operations/list {"fs":"pcloud:","opt":{"recurse":true},"remote":"Backups/phone"}"#,
            r#"POST operations/list {"fs":"pcloud:","opt":{"recurse":true},"remote":"Notes"}"#,
        ]
    );
    assert!(!log.iter().any(|l| l.contains("_filter")));
    // A walk of an excluded folder fetches nothing.
    assert!(
        h.storage
            .walk(&exclude[1], &exclude)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(h.log().is_empty());
}

#[tokio::test]
async fn stat_with_and_without_hashes() {
    let h = Harness::new(tree(), 1_000_000).await;
    let entry = h
        .storage
        .stat(&h.path("/Notes/todo.md"), true)
        .await
        .unwrap();
    assert_eq!(entry.hashes.len(), 2);
    assert_eq!(
        h.storage
            .stat(&h.path("/missing"), false)
            .await
            .unwrap_err(),
        StorageError::NotFound("/missing".into())
    );
    let root = h.storage.stat(&h.path("/"), false).await.unwrap();
    assert!(root.is_dir);
    let log = h.log();
    assert_eq!(
        log[0],
        r#"POST operations/stat {"fs":"pcloud:","opt":{"showHash":true},"remote":"Notes/todo.md"}"#
    );
    assert_eq!(log.len(), 2, "the root needs no call: {log:?}");
}

async fn read(h: &Harness, path: &str, range: Option<ByteRange>) -> Result<Vec<u8>, StorageError> {
    let stream = h.storage.open(&h.path(path), range).await?;
    read_capped(stream, u64::MAX).await
}

#[tokio::test]
async fn ranged_reads_are_checked_against_the_size() {
    let h = Harness::new(tree(), 1_000_000).await;
    let r = |offset, length| Some(ByteRange { offset, length });
    assert_eq!(read(&h, "/Notes/todo.md", r(2, 3)).await.unwrap(), b"234");
    assert_eq!(read(&h, "/Notes/todo.md", r(8, 100)).await.unwrap(), b"89");
    assert_eq!(
        read(&h, "/Notes/todo.md", None).await.unwrap(),
        b"0123456789"
    );
    let log = h.log();
    assert!(
        log.contains(&"GET /%5Bpcloud%3A%5D/Notes/todo.md bytes=2-4".to_owned())
            || log.contains(&"GET /[pcloud%3A]/Notes/todo.md bytes=2-4".to_owned()),
        "{log:?}"
    );
    assert!(
        log.iter().any(|l| l.ends_with("todo.md bytes=8-9")),
        "{log:?}"
    );
    // Past the end: answered empty without asking rc-serve, which would send a broken 206.
    assert!(
        read(&h, "/Notes/todo.md", r(10, 5))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        read(&h, "/Notes/todo.md", r(99, 5))
            .await
            .unwrap()
            .is_empty()
    );
    let log = h.log();
    assert!(!log.iter().any(|l| l.starts_with("GET")), "{log:?}");
}

#[tokio::test]
async fn read_urls_escape_every_segment() {
    let h = Harness::new(tree(), 1_000_000).await;
    assert_eq!(
        read(&h, "/Notes/deep/a b#?.txt", None).await.unwrap(),
        b"odd"
    );
    let log = h.log();
    let get = log.iter().find(|l| l.starts_with("GET")).unwrap();
    assert!(
        get.ends_with("/Notes/deep/a%20b%23%3F.txt bytes=0-2"),
        "{get}"
    );
}

#[tokio::test]
async fn a_malformed_206_is_an_error_and_an_ignored_range_is_skipped_to() {
    let mut fake = tree();
    fake.ranges = RangeMode::Broken;
    let h = Harness::new(fake, 1_000_000).await;
    let err = read(
        &h,
        "/Notes/todo.md",
        Some(ByteRange {
            offset: 2,
            length: 3,
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, StorageError::Backend(_)), "{err:?}");

    let mut fake = tree();
    fake.ranges = RangeMode::Ignored;
    let h = Harness::new(fake, 1_000_000).await;
    let bytes = read(
        &h,
        "/Notes/todo.md",
        Some(ByteRange {
            offset: 2,
            length: 3,
        }),
    )
    .await
    .unwrap();
    assert_eq!(bytes, b"234");
}

#[tokio::test]
async fn folders_are_never_read_through_rc_serve() {
    let h = Harness::new(tree(), 1_000_000).await;
    assert_eq!(
        read(&h, "/Notes", None).await.unwrap_err(),
        StorageError::IsADirectory("/Notes".into())
    );
    assert!(!h.log().iter().any(|l| l.starts_with("GET")));
}

#[tokio::test]
async fn oversized_answers_are_refused() {
    let h = Harness::new(tree(), 200).await;
    let err = h.storage.walk(&h.path("/"), &[]).await.unwrap_err();
    assert!(matches!(err, StorageError::TooManyEntries(_)), "{err:?}");
}

#[tokio::test]
async fn health_capabilities_and_credentials() {
    let h = Harness::new(tree(), 1_000_000).await;
    h.storage.health().await.unwrap();
    assert!(h.storage.capabilities().hashes.is_empty());
    let caps = h.storage.load_capabilities().await.unwrap();
    assert_eq!(caps.hashes.len(), 2);
    assert!(caps.server_dir_move);
    assert_eq!(h.storage.capabilities(), caps);

    let h = Harness::with_password(tree(), 1_000_000, "wrong").await;
    let err = h.storage.health().await.unwrap_err();
    assert_eq!(
        err,
        StorageError::Backend("rclone refused the rc credentials".into())
    );
    assert!(!err.to_string().contains("wrong"));
}

#[tokio::test]
async fn an_unreachable_rclone_is_a_backend_error_without_the_url() {
    let storage = RcloneStorage::new(RcloneConfig {
        url: "http://127.0.0.1:9".into(),
        user: "core".into(),
        password: PASSWORD.into(),
        remote: "pcloud".into(),
        max_listing_bytes: 1000,
    })
    .unwrap();
    let err = storage.health().await.unwrap_err();
    let text = err.to_string();
    assert!(matches!(err, StorageError::Backend(_)), "{text}");
    assert!(
        !text.contains("127.0.0.1") && !text.contains(PASSWORD),
        "{text}"
    );
}
