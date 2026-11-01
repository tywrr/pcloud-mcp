//! A changeable in-memory tree with pCloud-like ids and hashes, and helpers to open an index
//! over it and call tools through a server.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chance_of_files_core::{
    ByteRange, ByteStream, Capabilities, CheckedPath, Entry, HashKind, Health, McpServer, Policy,
    ServerConfig, Storage, StorageError, split_walk, stream_of,
};
use chance_of_files_index::{IndexConfig, IndexHandle, Indexer, RootConfig};
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct Node {
    pub is_dir: bool,
    pub size: u64,
    pub mtime: String,
    pub id: String,
    /// Stands in for the contents: files with the same `content` have the same hash.
    pub content: String,
}

#[derive(Default)]
pub struct Tree {
    nodes: Mutex<BTreeMap<String, Node>>,
    next_id: Mutex<u64>,
    /// Every backend call, as "list /x", "walk /x", "stat /x" or "hash /x".
    pub calls: Mutex<Vec<String>>,
    /// Paths whose listing fails.
    pub broken: Mutex<Vec<String>>,
    pub down: Mutex<bool>,
}

pub fn temp_dir() -> PathBuf {
    let mut bytes = [0u8; 8];
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .unwrap();
    let name: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("index-{name}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

impl Tree {
    pub fn new() -> Arc<Self> {
        let tree = Self::default();
        tree.nodes.lock().unwrap().insert(
            "/".into(),
            Node {
                is_dir: true,
                size: 0,
                mtime: "2026-01-01T00:00:00Z".into(),
                id: "d0".into(),
                content: String::new(),
            },
        );
        Arc::new(tree)
    }

    fn id(&self, dir: bool) -> String {
        let mut next = self.next_id.lock().unwrap();
        *next += 1;
        format!("{}{}", if dir { 'd' } else { 'f' }, *next)
    }

    fn parents(&self, path: &str) {
        let mut parent = String::new();
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        for segment in &segments[..segments.len().saturating_sub(1)] {
            parent.push('/');
            parent.push_str(segment);
            let missing = !self.nodes.lock().unwrap().contains_key(&parent);
            if missing {
                let id = self.id(true);
                self.nodes.lock().unwrap().insert(
                    parent.clone(),
                    Node {
                        is_dir: true,
                        size: 0,
                        mtime: "2026-01-01T00:00:00Z".into(),
                        id,
                        content: String::new(),
                    },
                );
            }
        }
    }

    pub fn dir(&self, path: &str) -> &Self {
        self.parents(path);
        let id = self.id(true);
        self.nodes.lock().unwrap().insert(
            path.into(),
            Node {
                is_dir: true,
                size: 0,
                mtime: "2026-01-01T00:00:00Z".into(),
                id,
                content: String::new(),
            },
        );
        self
    }

    pub fn file(&self, path: &str, size: u64) -> &Self {
        self.file_with(path, size, "2026-02-01T10:00:00Z", path)
    }

    pub fn file_with(&self, path: &str, size: u64, mtime: &str, content: &str) -> &Self {
        self.parents(path);
        let id = self.id(false);
        self.nodes.lock().unwrap().insert(
            path.into(),
            Node {
                is_dir: false,
                size,
                mtime: mtime.into(),
                id,
                content: content.into(),
            },
        );
        self
    }

    pub fn remove(&self, path: &str) {
        let mut nodes = self.nodes.lock().unwrap();
        let prefix = format!("{path}/");
        nodes.retain(|p, _| p != path && !p.starts_with(&prefix));
    }

    /// Renames a file or folder, keeping ids (as pCloud does).
    pub fn rename(&self, from: &str, to: &str) {
        let mut nodes = self.nodes.lock().unwrap();
        let prefix = format!("{from}/");
        let moved: Vec<(String, Node)> = nodes
            .iter()
            .filter(|(p, _)| *p == from || p.starts_with(&prefix))
            .map(|(p, n)| (format!("{to}{}", &p[from.len()..]), n.clone()))
            .collect();
        nodes.retain(|p, _| p != from && !p.starts_with(&prefix));
        nodes.extend(moved);
    }

    pub fn set_size(&self, path: &str, size: u64, mtime: &str) {
        let mut nodes = self.nodes.lock().unwrap();
        let node = nodes.get_mut(path).unwrap();
        node.size = size;
        node.mtime = mtime.into();
        node.content = format!("{path}@{mtime}");
    }

    pub fn take_calls(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }

    fn log(&self, what: &str, path: &str) {
        self.calls.lock().unwrap().push(format!("{what} {path}"));
    }

    fn entry(&self, path: &str, node: &Node, hash: bool) -> Entry {
        Entry {
            path: path.into(),
            name: path.rsplit('/').next().unwrap_or("").into(),
            is_dir: node.is_dir,
            size: (!node.is_dir).then_some(node.size),
            modified: Some(node.mtime.clone()),
            content_type: None,
            id: Some(node.id.clone()),
            hashes: if hash && !node.is_dir {
                vec![(HashKind::Sha1, format!("sha1-of-{}", node.content))]
            } else {
                Vec::new()
            },
        }
    }

    fn check(&self, path: &str) -> Result<(), StorageError> {
        if *self.down.lock().unwrap() {
            return Err(StorageError::Backend("connection refused".into()));
        }
        if self.broken.lock().unwrap().iter().any(|b| b == path) {
            return Err(StorageError::Backend(format!("listing {path} failed")));
        }
        Ok(())
    }
}

fn under(path: &str, folder: &str) -> bool {
    folder == "/" && path != "/" || path.starts_with(&format!("{folder}/"))
}

/// Read-only.
impl chance_of_files_core::StorageWrite for Tree {}

impl Storage for Tree {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            hashes: vec![HashKind::Sha1],
            ..Capabilities::default()
        }
    }

    async fn list(&self, path: &CheckedPath) -> Result<Vec<Entry>, StorageError> {
        let path = path.as_str();
        self.log("list", path);
        self.check(path)?;
        let nodes = self.nodes.lock().unwrap();
        match nodes.get(path) {
            None => return Err(StorageError::NotFound(path.into())),
            Some(n) if !n.is_dir => return Err(StorageError::NotADirectory(path.into())),
            _ => {}
        }
        Ok(nodes
            .iter()
            .filter(|(p, _)| {
                under(p, path) && !p[path.len()..].trim_start_matches('/').contains('/')
            })
            .map(|(p, n)| self.entry(p, n, false))
            .collect())
    }

    async fn walk(
        &self,
        root: &CheckedPath,
        exclude: &[CheckedPath],
    ) -> Result<Vec<Entry>, StorageError> {
        if !exclude.is_empty() {
            return split_walk(self, root, exclude).await;
        }
        let root = root.as_str();
        self.log("walk", root);
        self.check(root)?;
        let nodes = self.nodes.lock().unwrap();
        if !nodes.get(root).is_some_and(|n| n.is_dir) {
            return Err(StorageError::NotFound(root.into()));
        }
        Ok(nodes
            .iter()
            .filter(|(p, _)| under(p, root))
            .map(|(p, n)| self.entry(p, n, false))
            .collect())
    }

    async fn stat(&self, path: &CheckedPath, with_hash: bool) -> Result<Entry, StorageError> {
        let path = path.as_str();
        self.log(if with_hash { "hash" } else { "stat" }, path);
        if *self.down.lock().unwrap() {
            return Err(StorageError::Backend("connection refused".into()));
        }
        let nodes = self.nodes.lock().unwrap();
        let node = nodes
            .get(path)
            .ok_or_else(|| StorageError::NotFound(path.into()))?;
        Ok(self.entry(path, node, with_hash))
    }

    async fn open(
        &self,
        path: &CheckedPath,
        _range: Option<ByteRange>,
    ) -> Result<ByteStream, StorageError> {
        let nodes = self.nodes.lock().unwrap();
        let node = nodes
            .get(path.as_str())
            .ok_or_else(|| StorageError::NotFound(path.to_string()))?;
        Ok(stream_of(node.content.clone().into_bytes()))
    }

    async fn health(&self) -> Result<(), StorageError> {
        if *self.down.lock().unwrap() {
            Err(StorageError::Backend("connection refused".into()))
        } else {
            Ok(())
        }
    }
}

pub struct Fixture {
    pub tree: Arc<Tree>,
    pub index: Arc<IndexHandle>,
    pub indexer: Indexer<Arc<Tree>>,
    pub health: Arc<Health>,
    pub policy: Policy,
    pub dir: PathBuf,
}

pub fn roots(paths: &[&str]) -> Vec<RootConfig> {
    paths
        .iter()
        .map(|p| RootConfig {
            path: (*p).into(),
            every: Duration::from_secs(1800),
        })
        .collect()
}

pub fn open(tree: Arc<Tree>, root_paths: &[&str], policy: Policy) -> Fixture {
    open_in(temp_dir(), tree, root_paths, policy)
}

pub fn open_in(dir: PathBuf, tree: Arc<Tree>, root_paths: &[&str], policy: Policy) -> Fixture {
    let mut config = IndexConfig::new(&dir);
    config.roots = roots(root_paths);
    config.hash_in_background = false;
    let health = Arc::new(Health::new());
    let (index, indexer) =
        chance_of_files_index::open(config, policy.clone(), health.clone(), tree.clone()).unwrap();
    Fixture {
        tree,
        index,
        indexer,
        health,
        policy,
        dir,
    }
}

impl Fixture {
    pub async fn walk(&self, root: &str) {
        self.indexer.walk_now(root, None).await;
    }

    pub async fn walk_at(&self, root: &str, at: &str) {
        self.indexer.walk_now(root, Some(at)).await;
    }

    pub fn server(&self) -> McpServer<Arc<Tree>> {
        McpServer::new(self.tree.clone(), ServerConfig::default())
            .with_policy(self.policy.clone())
            .with_index(self.index.clone())
            .with_health(self.health.clone())
    }
}

pub async fn call(server: &McpServer<Arc<Tree>>, tool: &str, arguments: Value) -> Value {
    let response = server
        .handle_jsonrpc(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments}
        }))
        .await
        .unwrap();
    assert!(
        response.get("error").is_none(),
        "protocol error: {response}"
    );
    response["result"].clone()
}

pub fn text(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text in {result}"))
        .to_owned()
}

pub fn is_error(result: &Value) -> bool {
    result["isError"] == json!(true)
}

/// The cursor in a result's header line, if any.
pub fn next_cursor(result: &Value) -> Option<String> {
    let text = text(result);
    let header = text.lines().next()?;
    let at = header.find("next cursor=")?;
    Some(
        header[at + "next cursor=".len()..]
            .trim_end_matches(']')
            .to_owned(),
    )
}

/// Characters Claude would get: the text and the structured content.
pub fn result_size(result: &Value) -> usize {
    text(result).len()
        + result
            .get("structuredContent")
            .map_or(0, |v| v.to_string().len())
}
