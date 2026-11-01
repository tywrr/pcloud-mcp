//! In-memory [`StorageWrite`] and [`StateStore`] for tests (feature `testing`): a tree with a
//! trash, folder operations that run as jobs (done on the second look), and every call
//! recorded.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use bytes::Bytes;
use sha1::{Digest, Sha1};

use crate::policy::CheckedPath;
use crate::state::{JobRow, JobStatus, LinkRow, LinkState, LinkUse, StateResult, StateStore};
use crate::storage::{
    ByteRange, ByteStream, Capabilities, Done, Entry, HashKind, JobId, JobInfo, JobState, Storage,
    StorageError, StorageWrite, stream_of,
};

#[derive(Debug, Clone)]
struct Node {
    is_dir: bool,
    data: Vec<u8>,
    id: String,
}

#[derive(Debug, Default)]
struct Tree {
    nodes: BTreeMap<String, Node>,
    trash: Vec<String>,
    /// Job id → looks left before it reports done.
    jobs: HashMap<u64, u32>,
    next: u64,
    calls: Vec<String>,
}

/// A writable tree. Paths are the normalized ones the policy hands out.
#[derive(Debug)]
pub struct MemoryStore {
    tree: Mutex<Tree>,
    trash_on_delete: bool,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

fn under(path: &str, dir: &str) -> bool {
    dir == "/" || path.starts_with(&format!("{dir}/"))
}

fn parent(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}

impl MemoryStore {
    pub fn new() -> Self {
        let mut tree = Tree::default();
        tree.nodes.insert(
            "/".into(),
            Node {
                is_dir: true,
                data: Vec::new(),
                id: "d0".into(),
            },
        );
        Self {
            tree: Mutex::new(tree),
            trash_on_delete: true,
        }
    }

    /// A backend whose deletes would be final (and so are refused).
    pub fn without_trash(mut self) -> Self {
        self.trash_on_delete = false;
        self
    }

    fn lock(&self) -> MutexGuard<'_, Tree> {
        self.tree.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn dir(self, path: &str) -> Self {
        self.lock().add(path, true, Vec::new());
        self
    }

    pub fn file(self, path: &str, data: impl Into<Vec<u8>>) -> Self {
        self.lock().add(path, false, data.into());
        self
    }

    pub fn exists(&self, path: &str) -> bool {
        self.lock().nodes.contains_key(path)
    }

    pub fn contents(&self, path: &str) -> Option<Vec<u8>> {
        self.lock().nodes.get(path).map(|n| n.data.clone())
    }

    /// What went to the trash, in order.
    pub fn trash(&self) -> Vec<String> {
        self.lock().trash.clone()
    }

    /// Every call so far ("mkdir /a", "rename /a /b", ...), forgotten on reading.
    pub fn take_calls(&self) -> Vec<String> {
        std::mem::take(&mut self.lock().calls)
    }
}

impl Tree {
    fn add(&mut self, path: &str, is_dir: bool, data: Vec<u8>) {
        let mut at = String::new();
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        for segment in &segments[..segments.len().saturating_sub(1)] {
            at.push('/');
            at.push_str(segment);
            if !self.nodes.contains_key(&at) {
                self.next += 1;
                let id = format!("d{}", self.next);
                self.nodes.insert(
                    at.clone(),
                    Node {
                        is_dir: true,
                        data: Vec::new(),
                        id,
                    },
                );
            }
        }
        self.next += 1;
        let id = format!("{}{}", if is_dir { "d" } else { "f" }, self.next);
        self.nodes.insert(path.into(), Node { is_dir, data, id });
    }

    fn entry(&self, path: &str) -> Option<Entry> {
        let node = self.nodes.get(path)?;
        Some(Entry {
            path: path.into(),
            name: path.rsplit('/').next().unwrap_or("").into(),
            is_dir: node.is_dir,
            size: (!node.is_dir).then_some(node.data.len() as u64),
            modified: Some("2026-10-01T09:00:00Z".into()),
            content_type: None,
            id: Some(node.id.clone()),
            hashes: Vec::new(),
        })
    }

    fn subtree(&self, path: &str) -> Vec<String> {
        self.nodes
            .keys()
            .filter(|p| *p == path || under(p, path))
            .cloned()
            .collect()
    }

    fn job(&mut self) -> JobId {
        self.next += 1;
        self.jobs.insert(self.next, 1);
        JobId(self.next.to_string())
    }
}

impl Storage for MemoryStore {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            trash_on_delete: self.trash_on_delete,
            hashes: vec![HashKind::Sha1],
            server_dir_move: true,
        }
    }

    async fn list(&self, path: &CheckedPath) -> Result<Vec<Entry>, StorageError> {
        let tree = self.lock();
        let dir = path.as_str();
        match tree.nodes.get(dir) {
            None => return Err(StorageError::NotFound(dir.into())),
            Some(n) if !n.is_dir => return Err(StorageError::NotADirectory(dir.into())),
            _ => {}
        }
        Ok(tree
            .nodes
            .keys()
            .filter(|p| *p != dir && parent(p) == dir)
            .filter_map(|p| tree.entry(p))
            .collect())
    }

    async fn walk(
        &self,
        root: &CheckedPath,
        exclude: &[CheckedPath],
    ) -> Result<Vec<Entry>, StorageError> {
        if !exclude.is_empty() {
            return crate::split_walk(self, root, exclude).await;
        }
        let tree = self.lock();
        let dir = root.as_str();
        if !tree.nodes.get(dir).is_some_and(|n| n.is_dir) {
            return Err(StorageError::NotFound(dir.into()));
        }
        Ok(tree
            .nodes
            .keys()
            .filter(|p| *p != dir && under(p, dir))
            .filter_map(|p| tree.entry(p))
            .collect())
    }

    async fn stat(&self, path: &CheckedPath, with_hash: bool) -> Result<Entry, StorageError> {
        let tree = self.lock();
        let mut entry = tree
            .entry(path.as_str())
            .ok_or_else(|| StorageError::NotFound(path.to_string()))?;
        if with_hash && !entry.is_dir {
            let data = &tree.nodes[path.as_str()].data;
            let hex: String = Sha1::digest(data)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            entry.hashes.push((HashKind::Sha1, hex));
        }
        Ok(entry)
    }

    async fn open(
        &self,
        path: &CheckedPath,
        range: Option<ByteRange>,
    ) -> Result<ByteStream, StorageError> {
        let tree = self.lock();
        let node = tree
            .nodes
            .get(path.as_str())
            .ok_or_else(|| StorageError::NotFound(path.to_string()))?;
        if node.is_dir {
            return Err(StorageError::IsADirectory(path.to_string()));
        }
        let data = &node.data;
        let (start, len) = range.map_or((0, data.len() as u64), |r| (r.offset, r.length));
        let start = (start as usize).min(data.len());
        let end = start.saturating_add(len as usize).min(data.len());
        Ok(stream_of(Bytes::copy_from_slice(&data[start..end])))
    }

    async fn health(&self) -> Result<(), StorageError> {
        Ok(())
    }
}

impl StorageWrite for MemoryStore {
    async fn mkdir(&self, path: &CheckedPath) -> Result<Entry, StorageError> {
        let mut tree = self.lock();
        tree.calls.push(format!("mkdir {path}"));
        if !tree.nodes.contains_key(path.as_str()) {
            tree.add(path.as_str(), true, Vec::new());
        }
        tree.entry(path.as_str())
            .ok_or_else(|| StorageError::NotFound(path.to_string()))
    }

    async fn rename(&self, from: &CheckedPath, to: &CheckedPath) -> Result<Done, StorageError> {
        let mut tree = self.lock();
        tree.calls.push(format!("rename {from} {to}"));
        let is_dir = tree
            .nodes
            .get(from.as_str())
            .ok_or_else(|| StorageError::NotFound(from.to_string()))?
            .is_dir;
        for old in tree.subtree(from.as_str()) {
            let node = tree.nodes.remove(&old).expect("listed");
            let new = format!("{to}{}", &old[from.as_str().len()..]);
            tree.nodes.insert(new, node);
        }
        if is_dir {
            return Ok(Done::Job(tree.job()));
        }
        Ok(Done::Now(tree.entry(to.as_str()).expect("just moved")))
    }

    async fn copy(&self, from: &CheckedPath, to: &CheckedPath) -> Result<Done, StorageError> {
        let mut tree = self.lock();
        tree.calls.push(format!("copy {from} {to}"));
        let is_dir = tree
            .nodes
            .get(from.as_str())
            .ok_or_else(|| StorageError::NotFound(from.to_string()))?
            .is_dir;
        for old in tree.subtree(from.as_str()) {
            let node = tree.nodes[&old].clone();
            let new = format!("{to}{}", &old[from.as_str().len()..]);
            tree.add(&new, node.is_dir, node.data);
        }
        if is_dir {
            return Ok(Done::Job(tree.job()));
        }
        Ok(Done::Now(tree.entry(to.as_str()).expect("just copied")))
    }

    async fn trash(&self, path: &CheckedPath) -> Result<Done, StorageError> {
        if !self.trash_on_delete {
            return Err(StorageError::Unsupported("no trash".into()));
        }
        let mut tree = self.lock();
        tree.calls.push(format!("trash {path}"));
        let entry = tree
            .entry(path.as_str())
            .ok_or_else(|| StorageError::NotFound(path.to_string()))?;
        for gone in tree.subtree(path.as_str()) {
            tree.nodes.remove(&gone);
        }
        tree.trash.push(path.to_string());
        if entry.is_dir {
            return Ok(Done::Job(tree.job()));
        }
        Ok(Done::Now(entry))
    }

    async fn put_bytes(&self, data: Bytes, to: &CheckedPath) -> Result<Entry, StorageError> {
        let mut tree = self.lock();
        tree.calls.push(format!("put {to}"));
        tree.add(to.as_str(), false, data.to_vec());
        Ok(tree.entry(to.as_str()).expect("just written"))
    }

    async fn put_local(&self, local: &Path, to: &CheckedPath) -> Result<Entry, StorageError> {
        let data = std::fs::read(local)
            .map_err(|e| StorageError::Backend(format!("can't read {}: {e}", local.display())))?;
        let mut tree = self.lock();
        tree.calls.push(format!("put_local {to}"));
        tree.add(to.as_str(), false, data);
        Ok(tree.entry(to.as_str()).expect("just written"))
    }

    async fn job(&self, id: &JobId) -> Result<JobInfo, StorageError> {
        let mut tree = self.lock();
        let n: u64 = id.0.parse().unwrap_or(0);
        let state = match tree.jobs.get_mut(&n) {
            None => JobState::Lost,
            Some(0) => JobState::Done,
            Some(left) => {
                *left -= 1;
                JobState::Running
            }
        };
        Ok(JobInfo {
            state,
            started: Some(crate::time::now()),
            progress: None,
        })
    }
}

/// [`StateStore`] in memory, with the audit log readable.
#[derive(Debug, Default)]
pub struct MemoryState {
    jobs: Mutex<Vec<JobRow>>,
    links: Mutex<Vec<LinkRow>>,
    uses: Mutex<Vec<(String, LinkUse)>>,
    pub audit: Mutex<Vec<(String, String, String)>>,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl MemoryState {
    pub fn add_use(&self, link: &str, u: LinkUse) {
        locked(&self.uses).push((link.into(), u));
    }

    pub fn audit_log(&self) -> Vec<(String, String, String)> {
        locked(&self.audit).clone()
    }
}

impl StateStore for MemoryState {
    fn insert_job(&self, job: &JobRow) -> StateResult<()> {
        locked(&self.jobs).push(job.clone());
        Ok(())
    }
    fn finish_job(
        &self,
        id: &str,
        state: JobStatus,
        error: Option<&str>,
        at: i64,
    ) -> StateResult<()> {
        for job in locked(&self.jobs).iter_mut().filter(|j| j.id == id) {
            job.state = state;
            job.error = error.map(str::to_owned);
            job.finished = Some(at);
        }
        Ok(())
    }
    fn job(&self, id: &str) -> StateResult<Option<JobRow>> {
        Ok(locked(&self.jobs).iter().find(|j| j.id == id).cloned())
    }
    fn jobs(&self, limit: usize) -> StateResult<Vec<JobRow>> {
        Ok(locked(&self.jobs)
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect())
    }
    fn running_jobs(&self) -> StateResult<Vec<JobRow>> {
        Ok(locked(&self.jobs)
            .iter()
            .filter(|j| j.state == JobStatus::Running)
            .cloned()
            .collect())
    }
    fn insert_link(&self, link: &LinkRow) -> StateResult<()> {
        locked(&self.links).push(link.clone());
        Ok(())
    }
    fn link(&self, id: &str) -> StateResult<Option<LinkRow>> {
        Ok(locked(&self.links).iter().find(|l| l.id == id).cloned())
    }
    fn links(&self, limit: usize) -> StateResult<Vec<LinkRow>> {
        Ok(locked(&self.links)
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect())
    }
    fn transition(
        &self,
        id: &str,
        from: &[LinkState],
        to: LinkState,
        note: Option<&str>,
    ) -> StateResult<bool> {
        let mut links = locked(&self.links);
        let Some(link) = links.iter_mut().find(|l| l.id == id) else {
            return Ok(false);
        };
        if !from.contains(&link.state) {
            return Ok(false);
        }
        link.state = to;
        if let Some(note) = note {
            link.note = Some(note.into());
        }
        Ok(true)
    }
    fn uses(&self, link_id: &str, limit: usize) -> StateResult<Vec<LinkUse>> {
        Ok(locked(&self.uses)
            .iter()
            .rev()
            .filter(|(id, _)| id == link_id)
            .take(limit)
            .map(|(_, u)| u.clone())
            .collect())
    }
    fn reserved_bytes(&self, now: i64) -> StateResult<u64> {
        Ok(locked(&self.links)
            .iter()
            .filter(|l| {
                l.kind == crate::state::LinkKind::Upload
                    && matches!(l.state, LinkState::Active | LinkState::Failed)
                    && l.expires > now
            })
            .map(|l| l.size.saturating_sub(l.committed))
            .sum())
    }
    fn audit(&self, _at: i64, tool: &str, args: &str, result: &str) -> StateResult<()> {
        locked(&self.audit).push((tool.into(), args.into(), result.into()));
        Ok(())
    }
}
