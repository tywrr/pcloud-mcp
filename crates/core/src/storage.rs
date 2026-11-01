//! The storage contract the tools are written against. Backends: rclone's remote-control API
//! (`chance-of-files-rclone`) today; the pCloud HTTP API once an app is approved.
//!
//! Every path a backend receives is a [`CheckedPath`], which only [`crate::Policy`] can make,
//! so a hidden path never reaches a backend.

use std::fmt;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use crate::policy::CheckedPath;

/// One file or folder. `path` is absolute and normalized (see [`normalize_path`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    /// Bytes; `None` for folders or when the backend doesn't report it.
    pub size: Option<u64>,
    /// Last modified time as RFC 3339 (UTC), when known.
    pub modified: Option<String>,
    pub content_type: Option<String>,
    /// The backend's own id, when it has one (pCloud: `f123` for files, `d456` for folders).
    pub id: Option<String>,
    /// Content hashes, only when asked for: on pCloud each one costs an API call.
    pub hashes: Vec<(HashKind, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HashKind {
    Md5,
    Sha1,
    Sha256,
}

impl HashKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Md5 => "md5",
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "md5" => Some(Self::Md5),
            "sha1" => Some(Self::Sha1),
            "sha256" => Some(Self::Sha256),
            _ => None,
        }
    }
}

/// What a backend can do. Read-only phases only look at `hashes`; the rest is for writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// Deletes land in a trash the owner can restore from.
    pub trash_on_delete: bool,
    pub hashes: Vec<HashKind>,
    /// A folder move is one server-side rename (rclone: only while no filter is active).
    pub server_dir_move: bool,
}

/// A byte range to read: `length` bytes starting at `offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub offset: u64,
    pub length: u64,
}

/// File contents as they arrive. A backend yields at most the requested range's length, and
/// nothing for a range that starts at or past the end of the file.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, StorageError>> + Send>>;

/// A stream of the given bytes, for backends that have them in memory.
pub fn stream_of(bytes: impl Into<Bytes>) -> ByteStream {
    let bytes = bytes.into();
    let items = if bytes.is_empty() {
        Vec::new()
    } else {
        vec![Ok(bytes)]
    };
    Box::pin(futures_util::stream::iter(items))
}

/// Reads a stream into memory, keeping at most `cap` bytes, and stops reading once it has
/// them (so the backend stops sending).
pub async fn read_capped(mut stream: ByteStream, cap: u64) -> Result<Vec<u8>, StorageError> {
    let cap = usize::try_from(cap).unwrap_or(usize::MAX);
    let mut out = Vec::new();
    while out.len() < cap {
        match stream.next().await {
            Some(chunk) => {
                let chunk = chunk?;
                let room = cap - out.len();
                out.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            None => break,
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    NotFound(String),
    NotADirectory(String),
    IsADirectory(String),
    PermissionDenied(String),
    InvalidPath(String),
    /// A listing is bigger than the server is configured to take in one call; the text names
    /// the path and the limit.
    TooManyEntries(String),
    AlreadyExists(String),
    /// A write that touches a protected path.
    Protected(String),
    Unsupported(String),
    /// The backend is known to be failing (a health check), so the call wasn't tried.
    Unhealthy(String),
    /// The backend could not be reached or answered something unexpected.
    Backend(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(p) => write!(f, "not found: {p}"),
            Self::NotADirectory(p) => write!(f, "not a folder: {p}"),
            Self::IsADirectory(p) => write!(f, "is a folder, not a file: {p}"),
            Self::PermissionDenied(p) => write!(f, "permission denied: {p}"),
            Self::InvalidPath(why) => write!(f, "invalid path: {why}"),
            Self::TooManyEntries(why) => write!(f, "too many entries to go through at once: {why}"),
            Self::AlreadyExists(p) => write!(f, "already exists: {p}"),
            Self::Protected(p) => write!(f, "protected, read-only: {p}"),
            Self::Unsupported(why) => write!(f, "not supported: {why}"),
            Self::Unhealthy(why) => write!(f, "storage backend unhealthy: {why}"),
            Self::Backend(why) => write!(f, "storage backend error: {why}"),
        }
    }
}

impl std::error::Error for StorageError {}

/// Read access to a file tree.
#[trait_variant::make(Send)]
pub trait Storage: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    /// Direct children of a folder (not the folder itself).
    async fn list(&self, path: &CheckedPath) -> Result<Vec<Entry>, StorageError>;
    /// Everything under `root` at any depth (not `root` itself), leaving out each path in
    /// `exclude` and everything under it. An excluded folder's own entry is still returned
    /// when its parent is walked; callers drop hidden entries through [`crate::Policy`].
    /// Backends whose recursive listing can't skip subtrees use [`crate::split_walk`].
    async fn walk(
        &self,
        root: &CheckedPath,
        exclude: &[CheckedPath],
    ) -> Result<Vec<Entry>, StorageError>;
    /// Like [`Storage::walk`], handing each entry to `each` as it arrives instead of
    /// collecting them, so a walk of 130k entries never holds the whole listing twice.
    /// Backends that can stream a listing override it.
    fn walk_each<'a>(
        &'a self,
        root: &'a CheckedPath,
        exclude: &'a [CheckedPath],
        each: &'a mut (dyn FnMut(Entry) + Send),
    ) -> impl Future<Output = Result<(), StorageError>> {
        async move {
            for entry in self.walk(root, exclude).await? {
                each(entry);
            }
            Ok(())
        }
    }
    async fn stat(&self, path: &CheckedPath, with_hash: bool) -> Result<Entry, StorageError>;
    /// The bytes of a file, all of it or `range`. A range past the end is empty, not an error.
    async fn open(
        &self,
        path: &CheckedPath,
        range: Option<ByteRange>,
    ) -> Result<ByteStream, StorageError>;
    /// A cheap check that the backend works, for readiness probes.
    async fn health(&self) -> Result<(), StorageError>;
}

/// So the server and the background indexer can share one backend.
impl<T: Storage> Storage for Arc<T> {
    fn capabilities(&self) -> Capabilities {
        (**self).capabilities()
    }

    async fn list(&self, path: &CheckedPath) -> Result<Vec<Entry>, StorageError> {
        (**self).list(path).await
    }

    async fn walk(
        &self,
        root: &CheckedPath,
        exclude: &[CheckedPath],
    ) -> Result<Vec<Entry>, StorageError> {
        (**self).walk(root, exclude).await
    }

    async fn walk_each(
        &self,
        root: &CheckedPath,
        exclude: &[CheckedPath],
        each: &mut (dyn FnMut(Entry) + Send),
    ) -> Result<(), StorageError> {
        (**self).walk_each(root, exclude, each).await
    }

    async fn stat(&self, path: &CheckedPath, with_hash: bool) -> Result<Entry, StorageError> {
        (**self).stat(path, with_hash).await
    }

    async fn open(
        &self,
        path: &CheckedPath,
        range: Option<ByteRange>,
    ) -> Result<ByteStream, StorageError> {
        (**self).open(path, range).await
    }

    async fn health(&self) -> Result<(), StorageError> {
        (**self).health().await
    }
}

/// A backend's id for a long-running operation (rclone: the rc job id).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JobId(pub String);

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a write did: finished, or started a job to follow with [`StorageWrite::job`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Done {
    Now(Entry),
    Job(JobId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Running,
    Done,
    Failed(String),
    /// The backend doesn't know the job (it restarted, or forgot it).
    Lost,
}

/// A job as the backend reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobInfo {
    pub state: JobState,
    /// When the backend started it (Unix seconds), to tell a reused id from ours.
    pub started: Option<i64>,
    /// Progress for people, e.g. "1.2 GiB of 4.0 GiB, 31 of 120 files".
    pub progress: Option<String>,
}

/// Changes to a file tree. Every method has a default that refuses, so a read-only backend
/// implements this with an empty `impl`.
///
/// Folder operations may run long and return [`Done::Job`]; files finish at once. Paths are
/// [`CheckedPath`]s: the caller has already applied [`crate::Policy::check_write`].
#[trait_variant::make(Send)]
pub trait StorageWrite: Storage {
    /// Creates a folder and any missing parents; an existing folder is fine.
    fn mkdir<'a>(
        &'a self,
        path: &'a CheckedPath,
    ) -> impl Future<Output = Result<Entry, StorageError>> {
        let _ = path;
        async { Err(read_only()) }
    }
    /// Moves or renames `from` to the full path `to`, which must not exist.
    fn rename<'a>(
        &'a self,
        from: &'a CheckedPath,
        to: &'a CheckedPath,
    ) -> impl Future<Output = Result<Done, StorageError>> {
        let _ = (from, to);
        async { Err(read_only()) }
    }
    /// Copies `from` to the full path `to` (server-side where the backend can).
    fn copy<'a>(
        &'a self,
        from: &'a CheckedPath,
        to: &'a CheckedPath,
    ) -> impl Future<Output = Result<Done, StorageError>> {
        let _ = (from, to);
        async { Err(read_only()) }
    }
    /// Moves a file or folder to the backend's trash. Refused unless
    /// [`Capabilities::trash_on_delete`]: nothing here deletes for good.
    fn trash<'a>(
        &'a self,
        path: &'a CheckedPath,
    ) -> impl Future<Output = Result<Done, StorageError>> {
        let _ = path;
        async { Err(read_only()) }
    }
    /// Writes a small file whole, replacing any file at `to`.
    fn put_bytes<'a>(
        &'a self,
        data: Bytes,
        to: &'a CheckedPath,
    ) -> impl Future<Output = Result<Entry, StorageError>> {
        let _ = (data, to);
        async { Err(read_only()) }
    }
    /// Uploads a finished local file (an upload link's staging file) to `to`, replacing any
    /// file there.
    fn put_local<'a>(
        &'a self,
        local: &'a Path,
        to: &'a CheckedPath,
    ) -> impl Future<Output = Result<Entry, StorageError>> {
        let _ = (local, to);
        async { Err(read_only()) }
    }
    fn job<'a>(&'a self, id: &'a JobId) -> impl Future<Output = Result<JobInfo, StorageError>> {
        let _ = id;
        async { Err(read_only()) }
    }
}

fn read_only() -> StorageError {
    StorageError::Unsupported("this storage backend is read-only".into())
}

impl<T: StorageWrite> StorageWrite for Arc<T> {
    async fn mkdir(&self, path: &CheckedPath) -> Result<Entry, StorageError> {
        (**self).mkdir(path).await
    }
    async fn rename(&self, from: &CheckedPath, to: &CheckedPath) -> Result<Done, StorageError> {
        (**self).rename(from, to).await
    }
    async fn copy(&self, from: &CheckedPath, to: &CheckedPath) -> Result<Done, StorageError> {
        (**self).copy(from, to).await
    }
    async fn trash(&self, path: &CheckedPath) -> Result<Done, StorageError> {
        (**self).trash(path).await
    }
    async fn put_bytes(&self, data: Bytes, to: &CheckedPath) -> Result<Entry, StorageError> {
        (**self).put_bytes(data, to).await
    }
    async fn put_local(&self, local: &Path, to: &CheckedPath) -> Result<Entry, StorageError> {
        (**self).put_local(local, to).await
    }
    async fn job(&self, id: &JobId) -> Result<JobInfo, StorageError> {
        (**self).job(id).await
    }
}

/// Turns user input into an absolute path such as `/` or `/Library/notes.md`: collapses
/// repeated slashes, drops `.` segments and a trailing slash, and rejects `..`, NUL and
/// other control characters.
///
/// Empty input means the root, `/`, and a missing leading slash is added (`Notes` is
/// `/Notes`). Only `/` separates segments: a backslash is an ordinary character, as it is in
/// pCloud names, so `a\b` names one entry. Nothing is percent-decoded either (`%2e%2e` is a
/// literal name); backends must escape each segment when they build a URL. Other text is kept
/// as given, spaces included.
pub fn normalize_path(input: &str) -> Result<String, StorageError> {
    if input.chars().any(char::is_control) {
        return Err(StorageError::InvalidPath(
            "control characters (including NUL) are not allowed".into(),
        ));
    }
    let mut out = String::with_capacity(input.len() + 1);
    for segment in input.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                return Err(StorageError::InvalidPath(
                    "\"..\" segments are not allowed; paths start at the drive's root".into(),
                ));
            }
            name => {
                out.push('/');
                out.push_str(name);
            }
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    Ok(out)
}
