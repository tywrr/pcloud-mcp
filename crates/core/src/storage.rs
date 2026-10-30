//! The storage contract the tools are written against. Backends: WebDAV (rclone serving the
//! pCloud Drive mount, `pcloud-mcp-webdav`) today; the pCloud HTTP API once an app is approved.

use std::fmt;

/// One file or folder. `path` is absolute and normalized (see [`normalize_path`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    /// Bytes; `None` for folders or when the backend doesn't report it.
    pub size: Option<u64>,
    /// Last modified time as RFC 3339 (UTC), when known.
    pub modified: Option<String>,
    pub content_type: Option<String>,
}

/// A byte range to read: `length` bytes starting at `offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    NotFound(String),
    NotADirectory(String),
    IsADirectory(String),
    PermissionDenied(String),
    InvalidPath(String),
    /// A listing or search would have to go through more entries than the backend handles in
    /// one call (a CPU limit, not a fault); the text names the path and the limit.
    TooManyEntries(String),
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
            Self::Backend(why) => write!(f, "storage backend error: {why}"),
        }
    }
}

impl std::error::Error for StorageError {}

/// Read access to a file tree. Paths passed in are always already normalized by the caller.
///
/// No `Send` bound: on Workers everything runs on one thread and JS futures aren't `Send`.
#[allow(async_fn_in_trait)]
pub trait Storage {
    /// Direct children of a folder (not the folder itself).
    async fn list(&self, path: &str) -> Result<Vec<Entry>, StorageError>;
    async fn stat(&self, path: &str) -> Result<Entry, StorageError>;
    /// Bytes of a file in `range`; fewer bytes than asked means the end of the file was reached.
    async fn read(&self, path: &str, range: ByteRange) -> Result<Vec<u8>, StorageError>;
    /// Files and folders under `root` (any depth, not `root` itself) whose name matches `query`
    /// by [`crate::name_matches`] (contains it, ignoring case, accents and Unicode
    /// normalization form); at most `limit` results. `search_files` also matches names it
    /// lists itself, so a backend must use the same rule.
    async fn search(
        &self,
        root: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Entry>, StorageError>;
}

/// Turns user input into an absolute path such as `/` or `/Library/notes.md`: collapses
/// repeated slashes, drops `.` segments and a trailing slash, and rejects `..`, NUL and
/// other control characters.
///
/// Empty input means the root, `/`, and a missing leading slash is added (`Notes` is
/// `/Notes`). Only `/` separates segments: a backslash is an ordinary character, as it is in
/// file names on the Linux side of the pCloud Drive mount, so `a\b` names one entry. Nothing is
/// percent-decoded either (`%2e%2e` is a literal name); backends must escape each segment
/// when they build a URL. Other text is kept as given, spaces included.
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
                    "\"..\" segments are not allowed; paths start at the shared folder's root"
                        .into(),
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
