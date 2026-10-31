//! An in-memory [`Storage`] and helpers for driving the server in tests.

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use chance_of_files_core::{
    ByteRange, Entry, HttpRequest, HttpResponse, McpServer, ServerConfig, Storage, StorageError,
    name_matches,
};
use serde_json::{Value, json};

/// A path under which every call fails like an unreachable backend would, with a URL and
/// credentials in the message (the tools must not pass those on).
pub const BROKEN: &str = "/broken";

/// A folder too big for the backend to list or search in one call.
pub const HUGE: &str = "/huge";

#[derive(Default)]
pub struct MemoryStorage {
    /// Normalized path to (is_dir, contents, content type).
    nodes: BTreeMap<String, (bool, Vec<u8>, Option<String>)>,
    /// Folders too big to search: searching them or any folder above them fails, as one
    /// WebDAV listing of the whole tree would.
    too_big: BTreeSet<String>,
    /// Every call, as "list /path", "search /path", "stat /path" or "read /path".
    pub calls: RefCell<Vec<String>>,
}

impl MemoryStorage {
    pub fn new() -> Self {
        let mut storage = Self::default();
        storage.nodes.insert("/".into(), (true, Vec::new(), None));
        storage
    }

    pub fn dir(mut self, path: &str) -> Self {
        self.add_parents(path);
        self.nodes.insert(path.into(), (true, Vec::new(), None));
        self
    }

    /// Marks a folder (created if missing) as too big to search.
    pub fn too_big(mut self, path: &str) -> Self {
        if !self.nodes.contains_key(path) {
            self = self.dir(path);
        }
        self.too_big.insert(path.into());
        self
    }

    /// The calls so far, and forgets them.
    pub fn take_calls(&self) -> Vec<String> {
        self.calls.take()
    }

    fn log(&self, call: &str, path: &str) {
        self.calls.borrow_mut().push(format!("{call} {path}"));
    }

    pub fn file(self, path: &str, data: impl Into<Vec<u8>>) -> Self {
        self.typed_file(path, data, None)
    }

    pub fn typed_file(
        mut self,
        path: &str,
        data: impl Into<Vec<u8>>,
        content_type: Option<&str>,
    ) -> Self {
        self.add_parents(path);
        self.nodes.insert(
            path.into(),
            (false, data.into(), content_type.map(str::to_owned)),
        );
        self
    }

    fn add_parents(&mut self, path: &str) {
        let mut parent = String::new();
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        for segment in &segments[..segments.len().saturating_sub(1)] {
            parent.push('/');
            parent.push_str(segment);
            self.nodes
                .entry(parent.clone())
                .or_insert((true, Vec::new(), None));
        }
    }

    fn entry(&self, path: &str) -> Option<Entry> {
        let (is_dir, data, content_type) = self.nodes.get(path)?;
        Some(Entry {
            path: path.into(),
            name: path.rsplit('/').next().unwrap_or("").into(),
            is_dir: *is_dir,
            size: (!is_dir).then_some(data.len() as u64),
            modified: Some("2026-10-01T09:00:00Z".into()),
            content_type: content_type.clone(),
        })
    }

    fn check(path: &str) -> Result<(), StorageError> {
        assert!(
            path.starts_with('/') && !path.contains("//") && !path.contains("/../"),
            "the server must normalize paths before calling storage: {path:?}"
        );
        if path == BROKEN || path.starts_with("/broken/") {
            return Err(StorageError::Backend(
                "GET http://mcp:dev-password@127.0.0.1:8080/broken failed: connection refused"
                    .into(),
            ));
        }
        Ok(())
    }

    fn check_listing(path: &str) -> Result<(), StorageError> {
        Self::check(path)?;
        if path == HUGE {
            return Err(StorageError::TooManyEntries(format!(
                "{HUGE} (a 500000-byte WebDAV listing; the limit is 400000 bytes)"
            )));
        }
        Ok(())
    }

    fn is_under(path: &str, root: &str) -> bool {
        root == "/" || path.starts_with(&format!("{root}/"))
    }
}

impl Storage for MemoryStorage {
    async fn list(&self, path: &str) -> Result<Vec<Entry>, StorageError> {
        self.log("list", path);
        Self::check_listing(path)?;
        if self.too_big.contains(path) {
            return Err(StorageError::TooManyEntries(format!(
                "{path} (a 500000-byte WebDAV listing; the limit is 400000 bytes)"
            )));
        }
        let entry = self
            .entry(path)
            .ok_or_else(|| StorageError::NotFound(path.into()))?;
        if !entry.is_dir {
            return Err(StorageError::NotADirectory(path.into()));
        }
        Ok(self
            .nodes
            .keys()
            .filter(|p| *p != path && Self::is_under(p, path))
            .filter(|p| !p[path.len()..].trim_start_matches('/').contains('/'))
            .filter_map(|p| self.entry(p))
            .collect())
    }

    async fn stat(&self, path: &str) -> Result<Entry, StorageError> {
        self.log("stat", path);
        Self::check(path)?;
        self.entry(path)
            .ok_or_else(|| StorageError::NotFound(path.into()))
    }

    async fn read(&self, path: &str, range: ByteRange) -> Result<Vec<u8>, StorageError> {
        self.log("read", path);
        Self::check(path)?;
        let (is_dir, data, _) = self
            .nodes
            .get(path)
            .ok_or_else(|| StorageError::NotFound(path.into()))?;
        if *is_dir {
            return Err(StorageError::IsADirectory(path.into()));
        }
        let start = (range.offset as usize).min(data.len());
        let end = start.saturating_add(range.length as usize).min(data.len());
        Ok(data[start..end].to_vec())
    }

    async fn search(
        &self,
        root: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Entry>, StorageError> {
        self.log("search", root);
        Self::check_listing(root)?;
        if self
            .too_big
            .iter()
            .any(|big| big == root || Self::is_under(big, root))
        {
            return Err(StorageError::TooManyEntries(format!(
                "{root} (a 500000-byte WebDAV listing; the limit is 400000 bytes)"
            )));
        }
        let root_entry = self
            .entry(root)
            .ok_or_else(|| StorageError::NotFound(root.into()))?;
        if !root_entry.is_dir {
            return Err(StorageError::NotADirectory(root.into()));
        }
        Ok(self
            .nodes
            .keys()
            .filter(|p| *p != root && Self::is_under(p, root))
            .filter_map(|p| self.entry(p))
            .filter(|e| name_matches(query, &e.name))
            .take(limit)
            .collect())
    }
}

/// A 1x1 PNG.
pub const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// The start of a JPEG (enough for type sniffing).
pub const JPEG_HEAD: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00,
];

/// The tree most tests use.
pub fn sample_storage() -> MemoryStorage {
    MemoryStorage::new()
        .file("/readme.txt", "Hello from pCloud.\n")
        .file("/Notes/todo.md", "- buy milk\n- call mum\n")
        .file("/Notes/Ideas.md", "Ideas\n")
        .file("/Notes/unicode.txt", "Grüße, €uro and 😀!\n")
        .dir("/Notes/archive")
        .dir("/Photos")
        .typed_file("/Photos/dot.png", PNG_1X1, Some("image/png"))
        .file("/Photos/really-a-jpeg.png", JPEG_HEAD)
        .file("/Photos/fake.png", "not an image at all")
        .file("/Photos/notes-about-photos.txt", "text")
        .file("/bin/blob.bin", vec![1, 2, 0, 3, 4])
        .file("/empty.txt", "")
        .dir("/Zeta")
        .dir("/alpha")
}

pub fn server(storage: MemoryStorage) -> McpServer<MemoryStorage> {
    McpServer::new(storage, ServerConfig::default())
}

pub fn server_with(storage: MemoryStorage, config: ServerConfig) -> McpServer<MemoryStorage> {
    McpServer::new(storage, config)
}

pub fn post(body: &Value) -> HttpRequest {
    post_raw(body.to_string().as_bytes())
}

pub fn post_raw(body: &[u8]) -> HttpRequest {
    HttpRequest {
        method: "POST".into(),
        path: "/mcp".into(),
        headers: vec![
            ("Content-Type".into(), "application/json".into()),
            (
                "Accept".into(),
                "application/json, text/event-stream".into(),
            ),
        ],
        body: body.to_vec(),
    }
}

pub fn with_header(mut req: HttpRequest, name: &str, value: &str) -> HttpRequest {
    req.headers.push((name.into(), value.into()));
    req
}

pub fn body_json(resp: &HttpResponse) -> Value {
    serde_json::from_slice(&resp.body).expect("response body is JSON")
}

pub fn request(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// Sends one JSON-RPC request and returns the whole response message.
pub fn rpc(server: &McpServer<MemoryStorage>, method: &str, params: Value) -> Value {
    pollster::block_on(server.handle_jsonrpc(request(1, method, params)))
        .expect("a request gets a response")
}

/// Calls a tool and returns its result object.
pub fn call(server: &McpServer<MemoryStorage>, tool: &str, arguments: Value) -> Value {
    let response = rpc(
        server,
        "tools/call",
        json!({"name": tool, "arguments": arguments}),
    );
    assert!(
        response.get("error").is_none(),
        "unexpected protocol error: {response}"
    );
    response["result"].clone()
}

pub fn is_error(result: &Value) -> bool {
    result["isError"] == json!(true)
}

/// The text of the first content block.
pub fn text(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content in {result}"))
        .to_owned()
}
