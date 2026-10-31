//! Runs WebDavStorage against a real `rclone serve webdav` of a copy of fixtures/webdav-root,
//! through a blocking native transport. Skipped (not failed) when rclone isn't installed.

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use chance_of_files_core::{ByteRange, Storage, StorageError};
use chance_of_files_webdav::{Transport, TransportRequest, TransportResponse, WebDavStorage};

const USER: &str = "mcp";
const PASSWORD: &str = "test-password";
/// Big enough to need several `read_text_file` pages at the default 100 kB.
const BIG_FILE_LINES: usize = 5_000;

struct UreqTransport {
    agent: ureq::Agent,
    base_url: String,
}

impl UreqTransport {
    fn new(base_url: String) -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .allow_non_standard_methods(true)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(20)))
            .build();
        Self {
            agent: config.into(),
            base_url,
        }
    }

    fn send_blocking(&self, request: TransportRequest) -> Result<TransportResponse, String> {
        let max = request.max_response_bytes;
        let method =
            ureq::http::Method::from_bytes(request.method.as_bytes()).map_err(|e| e.to_string())?;
        let mut builder = ureq::http::Request::builder()
            .method(method)
            .uri(format!("{}{}", self.base_url, request.path_and_query));
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        let response = if request.body.is_empty() {
            self.agent.run(builder.body(()).map_err(|e| e.to_string())?)
        } else {
            self.agent
                .run(builder.body(request.body).map_err(|e| e.to_string())?)
        }
        .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_owned()))
            .collect();
        let (body, truncated) = match max {
            // One byte past the limit tells a body that just fits from a longer one. Dropping
            // the reader before the end closes the connection, as the Worker's transport does.
            Some(max) => {
                let mut body = Vec::new();
                response
                    .into_body()
                    .into_reader()
                    .take(max as u64 + 1)
                    .read_to_end(&mut body)
                    .map_err(|e| e.to_string())?;
                let truncated = body.len() > max;
                body.truncate(max);
                (body, truncated)
            }
            None => (
                response
                    .into_body()
                    .with_config()
                    .limit(64 * 1024 * 1024)
                    .read_to_vec()
                    .map_err(|e| e.to_string())?,
                false,
            ),
        };
        Ok(TransportResponse {
            status,
            headers,
            body,
            truncated,
        })
    }
}

impl Transport for UreqTransport {
    async fn send(&self, request: TransportRequest) -> Result<TransportResponse, String> {
        // Blocking inside an async fn is fine here: the tests drive one future at a time.
        self.send_blocking(request)
    }
}

/// A running rclone serving a scratch copy of the fixtures; stops and cleans up on drop.
struct Server {
    child: Child,
    root: PathBuf,
    url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn rclone_available() -> bool {
    Command::new("rclone")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Starts rclone on a free port, or `None` (with a note on stderr) when rclone is missing.
fn start_server(name: &str) -> Option<Server> {
    if !rclone_available() {
        eprintln!("skipping {name}: rclone is not installed");
        return None;
    }
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/webdav-root");
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("webdav-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    copy_tree(&fixtures, &root);
    write_big_file(&root.join("big.txt"));

    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let child = Command::new("rclone")
        .args(["serve", "webdav"])
        .arg(&root)
        .args([
            "--addr",
            &addr.to_string(),
            "--read-only",
            "--config",
            "/dev/null",
        ])
        .env("RCLONE_USER", USER)
        .env("RCLONE_PASS", PASSWORD)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start rclone");
    let server = Server {
        child,
        root,
        url: format!("http://{addr}"),
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    while TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err() {
        assert!(
            Instant::now() < deadline,
            "rclone did not start listening on {addr}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    Some(server)
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Numbered lines, so a page boundary can be checked by content.
fn write_big_file(path: &Path) {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    for i in 0..BIG_FILE_LINES {
        writeln!(
            file,
            "line {i:05}: the quick brown fox jumps over the lazy dog"
        )
        .unwrap();
    }
}

fn big_file_contents() -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..BIG_FILE_LINES {
        writeln!(
            out,
            "line {i:05}: the quick brown fox jumps over the lazy dog"
        )
        .unwrap();
    }
    out
}

fn storage(server: &Server, password: &str) -> WebDavStorage<UreqTransport> {
    WebDavStorage::new(UreqTransport::new(server.url.clone()), USER, password)
}

fn block<F: Future>(f: F) -> F::Output {
    pollster::block_on(f)
}

#[test]
fn list_and_stat() {
    let Some(server) = start_server("list") else {
        return;
    };
    let s = storage(&server, PASSWORD);

    let root = block(s.list("/")).unwrap();
    let mut names: Vec<_> = root.iter().map(|e| e.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "Café menu.txt",
            "README.txt",
            "big.txt",
            "binary.bin",
            "deep",
            "images",
            "notes"
        ]
    );
    assert!(root.iter().all(|e| e.path == format!("/{}", e.name)));
    assert!(
        root.iter()
            .all(|e| e.modified.as_deref().is_some_and(|m| m.ends_with('Z')))
    );

    let notes = block(s.list("/notes")).unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].path, "/notes/todo.md");
    assert!(!notes[0].is_dir);

    let cafe = block(s.stat("/Café menu.txt")).unwrap();
    assert_eq!(cafe.name, "Café menu.txt");
    assert_eq!(cafe.size, Some(100));
    assert!(
        cafe.content_type
            .as_deref()
            .unwrap()
            .starts_with("text/plain")
    );

    let png = block(s.stat("/images/pixel.png")).unwrap();
    assert_eq!(png.content_type.as_deref(), Some("image/png"));

    let deep = block(s.stat("/deep/a/b/c")).unwrap();
    assert!(deep.is_dir);
    assert_eq!(deep.size, None);

    let root_entry = block(s.stat("/")).unwrap();
    assert!(root_entry.is_dir);
    assert_eq!(root_entry.path, "/");

    assert_eq!(
        block(s.stat("/missing.txt")).unwrap_err(),
        StorageError::NotFound("/missing.txt".into())
    );
    assert_eq!(
        block(s.list("/missing")).unwrap_err(),
        StorageError::NotFound("/missing".into())
    );
    assert_eq!(
        block(s.list("/README.txt")).unwrap_err(),
        StorageError::NotADirectory("/README.txt".into())
    );
}

#[test]
fn read_ranges() {
    let Some(server) = start_server("read") else {
        return;
    };
    let s = storage(&server, PASSWORD);
    let read = |path: &str, offset, length| block(s.read(path, ByteRange { offset, length }));

    let readme = std::fs::read(server.root.join("README.txt")).unwrap();
    assert_eq!(read("/README.txt", 0, 1_000_000).unwrap(), readme);
    assert_eq!(read("/README.txt", 7, 4).unwrap(), &readme[7..11]);
    // Asking past the end returns what is there, then nothing.
    let len = readme.len() as u64;
    assert_eq!(
        read("/README.txt", len - 3, 100).unwrap(),
        &readme[readme.len() - 3..]
    );
    assert_eq!(read("/README.txt", len, 100).unwrap(), b"");
    assert_eq!(read("/README.txt", len + 50, 100).unwrap(), b"");

    assert_eq!(
        read("/Café menu.txt", 0, 100).unwrap(),
        std::fs::read(server.root.join("Café menu.txt")).unwrap()
    );
    let binary = read("/binary.bin", 0, 64).unwrap();
    assert_eq!(binary.len(), 19);
    assert!(binary.contains(&0));
    assert!(
        read("/images/pixel.png", 0, 8)
            .unwrap()
            .starts_with(b"\x89PNG")
    );

    // Page through the generated big file and put it back together.
    let expected = big_file_contents();
    let mut assembled = Vec::new();
    let page = 100_000;
    loop {
        let chunk = read("/big.txt", assembled.len() as u64, page).unwrap();
        if chunk.is_empty() {
            break;
        }
        assembled.extend_from_slice(&chunk);
    }
    assert!(expected.len() > 2 * page as usize);
    assert_eq!(assembled, expected);

    assert_eq!(
        read("/missing.txt", 0, 10).unwrap_err(),
        StorageError::NotFound("/missing.txt".into())
    );
    assert_eq!(
        read("/notes", 0, 10).unwrap_err(),
        StorageError::IsADirectory("/notes".into())
    );
}

#[test]
fn search_the_tree() {
    let Some(server) = start_server("search") else {
        return;
    };
    let s = storage(&server, PASSWORD);

    let leaf = block(s.search("/", "LEAF", 10)).unwrap();
    assert_eq!(leaf.len(), 1);
    assert_eq!(leaf[0].path, "/deep/a/b/c/leaf.txt");

    let cafe = block(s.search("/", "café", 10)).unwrap();
    assert_eq!(cafe.len(), 1);
    assert_eq!(cafe[0].name, "Café menu.txt");

    let txt = block(s.search("/", ".txt", 10)).unwrap();
    assert_eq!(txt.len(), 4, "{txt:?}");
    assert_eq!(block(s.search("/", ".txt", 2)).unwrap().len(), 2);

    // Folders match too, and the search stays below its root.
    let folders = block(s.search("/deep", "b", 10)).unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].path, "/deep/a/b");
    assert!(block(s.search("/notes", "leaf", 10)).unwrap().is_empty());

    assert_eq!(
        block(s.search("/README.txt", "x", 10)).unwrap_err(),
        StorageError::NotADirectory("/README.txt".into())
    );
}

#[test]
fn listings_are_cut_off_at_the_byte_limit() {
    let Some(server) = start_server("bounded") else {
        return;
    };
    // The whole tree's answer is a few kB; the transport stops reading after 1,000 bytes, so
    // the size is never known.
    let s = storage(&server, PASSWORD).with_max_listing_bytes(1_000);
    assert_eq!(
        block(s.search("/", "leaf", 10)).unwrap_err(),
        StorageError::TooManyEntries("/ (a WebDAV listing, over the 1000-byte limit)".into())
    );
    // Small listings still fit, and use up the budget as they go.
    assert_eq!(block(s.list("/notes")).unwrap().len(), 1);
    assert!(matches!(
        block(s.list("/")).unwrap_err(),
        StorageError::TooManyEntries(_)
    ));
}

#[test]
fn wrong_password_is_permission_denied_without_the_password() {
    let Some(server) = start_server("auth") else {
        return;
    };
    let s = storage(&server, "not-the-password");
    let err = block(s.list("/")).unwrap_err();
    assert!(matches!(err, StorageError::PermissionDenied(_)), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("BACKEND_PASSWORD"), "{text}");
    assert!(!text.contains("not-the-password"), "{text}");
}

#[test]
fn unreachable_server_is_a_backend_error() {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let s = WebDavStorage::new(
        UreqTransport::new(format!("http://127.0.0.1:{port}")),
        USER,
        PASSWORD,
    );
    let err = block(s.stat("/")).unwrap_err();
    assert!(matches!(err, StorageError::Backend(_)), "{err:?}");
    assert!(!err.to_string().contains(PASSWORD));
}

/// The whole MCP path natively: McpServer over WebDavStorage over rclone.
#[test]
fn mcp_server_end_to_end() {
    use chance_of_files_core::{HttpRequest, McpServer, ServerConfig};
    use serde_json::{Value, json};

    let Some(server) = start_server("mcp") else {
        return;
    };
    let mcp = McpServer::new(storage(&server, PASSWORD), ServerConfig::default());
    let post = |body: Value| -> Value {
        let response = block(mcp.handle_http(HttpRequest {
            method: "POST".into(),
            path: "/mcp".into(),
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                (
                    "Accept".into(),
                    "application/json, text/event-stream".into(),
                ),
                ("MCP-Protocol-Version".into(), "2025-11-25".into()),
            ],
            body: serde_json::to_vec(&body).unwrap(),
        }));
        assert_eq!(
            response.status,
            200,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        serde_json::from_slice(&response.body).unwrap()
    };

    let init = post(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "webdav-e2e", "version": "0"}
        }
    }));
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");

    let tools = post(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for tool in [
        "list_folder",
        "get_file_info",
        "read_text_file",
        "read_image",
        "search_files",
    ] {
        assert!(names.contains(&tool), "{tool} missing from {names:?}");
    }

    let listing = post(json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "list_folder", "arguments": {"path": "/notes"}}
    }));
    assert_ne!(listing["result"]["isError"], true, "{listing}");
    assert!(listing.to_string().contains("todo.md"), "{listing}");

    let read = post(json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "read_text_file", "arguments": {"path": "/Café menu.txt"}}
    }));
    assert_ne!(read["result"]["isError"], true, "{read}");
    assert!(read.to_string().contains("Croissant"), "{read}");
}

/// A search split around a turned-off folder, and the owner's guide in the instructions,
/// through the real WebDAV server.
#[test]
fn mcp_search_skip_and_guide() {
    use chance_of_files_core::{McpServer, ServerConfig};
    use serde_json::{Value, json};

    let Some(server) = start_server("skip") else {
        return;
    };
    std::fs::write(
        server.root.join("AGENTS.md"),
        "# Guide\n\n- deep: a test tree\n",
    )
    .unwrap();
    let config = ServerConfig {
        search_skip: vec!["/deep/a/b".into()],
        ..ServerConfig::default()
    };
    let mcp = McpServer::new(storage(&server, PASSWORD), config);
    let rpc = |method: &str, params: Value| -> Value {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        block(mcp.handle_jsonrpc(body)).unwrap()["result"].clone()
    };
    let search = |args: Value| -> String {
        let result = rpc(
            "tools/call",
            json!({"name": "search_files", "arguments": args}),
        );
        result["content"][0]["text"].as_str().unwrap().to_owned()
    };

    let init = rpc(
        "initialize",
        json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}),
    );
    let instructions = init["instructions"].as_str().unwrap();
    assert!(
        instructions.contains("Search is turned off under /deep/a/b."),
        "{instructions}"
    );
    assert!(
        instructions.ends_with("(/AGENTS.md):\n\n# Guide\n\n- deep: a test tree"),
        "{instructions}"
    );

    // leaf.txt is under the turned-off folder; the other subfolders are searched whole.
    let leaf = search(json!({"query": "leaf"}));
    assert!(
        leaf.starts_with("No files or folders under / have names containing \"leaf\" in the folders that were searched."),
        "{leaf}"
    );
    assert!(
        leaf.contains("Skipped /deep/a/b: search is turned off there."),
        "{leaf}"
    );
    let found = search(json!({"query": "o"}));
    assert!(
        found.starts_with("2 matches for \"o\" under /:\n"),
        "{found}"
    );
    for path in ["/notes", "/notes/todo.md"] {
        assert!(found.contains(&format!("path: {path}\n")), "{found}");
    }
    let pixel = search(json!({"query": "PIXEL"}));
    assert!(pixel.contains("path: /images/pixel.png\n"), "{pixel}");
    let refused = search(json!({"query": "leaf", "path": "/deep/a/b/c"}));
    assert!(
        refused.starts_with("Search is turned off under /deep/a/b"),
        "{refused}"
    );
}
