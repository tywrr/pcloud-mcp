//! What the tools ask of the metadata index (`chance-of-files-index` keeps it in SQLite).
//!
//! The index holds every visible entry of the configured roots, as of each root's last walk,
//! plus per-folder totals, a change log and cached hashes. Its methods are synchronous and
//! fast (SQL over one file); the tools call them on a blocking thread.
//!
//! Paths in and out are absolute and normalized. Lookups forgive case (`/backups` finds
//! `/Backups`); results carry the path as the drive spells it. Every page of rows comes with
//! a cursor per row, so a tool that stops early (its output budget) resumes right after the
//! last row it showed.

use std::fmt;

use crate::storage::HashKind;

/// Recursive totals of a folder: everything below it, at any depth.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    /// Newest modification time of anything below, Unix seconds.
    pub newest: Option<i64>,
}

/// One indexed file or folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexedEntry {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    /// Unix seconds.
    pub mtime: Option<i64>,
    pub ext: Option<String>,
    pub kind: String,
    pub remote_id: Option<String>,
    /// Folders only.
    pub totals: Option<Totals>,
    /// Cached hashes, where a query asks for them and they are still valid.
    pub hashes: Vec<(HashKind, String)>,
    /// Passed back to continue right after this row.
    pub cursor: String,
}

/// Row orders. Every order ends in the path, so it is total and pages never overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// Folders first, then by name ignoring case and accents.
    Name,
    Path,
    Largest,
    Smallest,
    Newest,
    Oldest,
    /// Folders by file count (folder stats).
    Files,
}

impl Sort {
    pub const NAMES: &'static [&'static str] = &[
        "name", "path", "largest", "smallest", "newest", "oldest", "files",
    ];

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "name" => Self::Name,
            "path" => Self::Path,
            "largest" | "size" => Self::Largest,
            "smallest" => Self::Smallest,
            "newest" | "modified" | "recent" => Self::Newest,
            "oldest" => Self::Oldest,
            "files" | "count" => Self::Files,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Path => "path",
            Self::Largest => "largest",
            Self::Smallest => "smallest",
            Self::Newest => "newest",
            Self::Oldest => "oldest",
            Self::Files => "files",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum What {
    #[default]
    Files,
    Folders,
    Any,
}

/// Which entries a query matches. Every bound is optional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    /// Only entries strictly below this folder (`/` is everything).
    pub under: String,
    /// The name contains this, ignoring case and accents.
    pub name: Option<String>,
    pub kind: Option<String>,
    /// Lower-case extensions without the dot; any of them.
    pub ext: Vec<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    /// Modified at or after (Unix seconds).
    pub after: Option<i64>,
    /// Modified before.
    pub before: Option<i64>,
    pub what: What,
    /// Leave out folders the owner turned off for name search.
    pub skip_search_off: bool,
}

impl Filter {
    pub fn under(path: &str) -> Self {
        Self {
            under: path.to_owned(),
            name: None,
            kind: None,
            ext: Vec::new(),
            min_size: None,
            max_size: None,
            after: None,
            before: None,
            what: What::Files,
            skip_search_off: false,
        }
    }
}

/// A walk that would have removed too much, waiting for `refresh_index confirm=true`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldRemoval {
    /// When the walk that found it started.
    pub at: i64,
    pub removals: u64,
    /// The root's entries at the time.
    pub entries: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootStatus {
    pub path: String,
    pub every_s: u64,
    /// When the last full walk started listing.
    pub walk_started: Option<i64>,
    /// When the last successful full walk listed: what the index shows is as of then.
    pub as_of: Option<i64>,
    /// How long that walk took to list (applying adds ~0.1 s per 10k entries).
    pub last_ms: Option<u64>,
    /// Set when the last walk failed; cleared by the next success.
    pub last_error: Option<String>,
    pub entries: u64,
    pub held: Option<HeldRemoval>,
    pub walking: bool,
    pub queued: bool,
    pub next_due: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexStatus {
    pub roots: Vec<RootStatus>,
    pub generation: u64,
    pub db_bytes: u64,
    /// Files of at least `hash_min_size` sharing a size with another file.
    pub hash_candidates: u64,
    /// How many of those have a cached hash.
    pub hashed: u64,
    pub hash_min_size: u64,
}

/// Count and bytes of one extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeRow {
    pub kind: String,
    pub ext: Option<String>,
    pub files: u64,
    pub bytes: u64,
}

/// One entry of the change log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub seq: i64,
    pub at: i64,
    /// `added`, `removed`, `changed` or `moved`.
    pub op: String,
    pub path: String,
    pub old_path: Option<String>,
    pub size: Option<u64>,
    pub is_dir: bool,
    /// `walk` or `own-write`.
    pub source: String,
    pub cursor: String,
}

/// Files of one size (at least two), with whatever hashes are cached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizeGroup {
    pub size: u64,
    pub files: Vec<IndexedEntry>,
    pub cursor: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SizeGroups {
    pub groups: Vec<SizeGroup>,
    /// Across all pages.
    pub total_groups: u64,
    pub total_files: u64,
    pub hashed_files: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexError {
    /// A cursor from another query, or garbled.
    Cursor(String),
    /// A bad argument (an unknown root and so on).
    Invalid(String),
    /// The database failed.
    Db(String),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cursor(why) => write!(f, "{why}"),
            Self::Invalid(why) => write!(f, "{why}"),
            Self::Db(why) => write!(f, "index error: {why}"),
        }
    }
}

impl std::error::Error for IndexError {}

pub type IndexResult<T> = Result<T, IndexError>;

pub trait Index: Send + Sync {
    /// The configured roots and their walks. Cheap: every index tool header uses it.
    fn roots(&self) -> IndexResult<Vec<RootStatus>>;
    fn status(&self) -> IndexResult<IndexStatus>;
    /// One entry, with totals for a folder; `/` is a folder with the drive's totals.
    fn entry(&self, path: &str) -> IndexResult<Option<IndexedEntry>>;
    /// The direct children of a folder.
    fn children(
        &self,
        folder: &str,
        sort: Sort,
        cursor: Option<&str>,
        limit: usize,
    ) -> IndexResult<Vec<IndexedEntry>>;
    /// How many folders and files are directly inside a folder.
    fn child_counts(&self, folder: &str) -> IndexResult<(u64, u64)>;
    fn find(
        &self,
        filter: &Filter,
        sort: Sort,
        cursor: Option<&str>,
        limit: usize,
    ) -> IndexResult<Vec<IndexedEntry>>;
    /// How many entries match, and their bytes.
    fn count(&self, filter: &Filter) -> IndexResult<(u64, u64)>;
    /// Folders 1 to `depth` levels below `under`, with totals.
    fn folders(
        &self,
        under: &str,
        depth: u32,
        sort: Sort,
        cursor: Option<&str>,
        limit: usize,
    ) -> IndexResult<Vec<IndexedEntry>>;
    /// Files below `under` by kind and extension.
    fn types(&self, under: &str) -> IndexResult<Vec<TypeRow>>;
    /// Newest first.
    fn changes(
        &self,
        under: &str,
        since: i64,
        source: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
    ) -> IndexResult<Vec<Change>>;
    /// Size groups below `under`, largest size first, until about `max_files` files.
    fn size_groups(
        &self,
        under: &str,
        min_size: u64,
        cursor: Option<&str>,
        max_files: usize,
    ) -> IndexResult<SizeGroups>;
    /// Remembers hashes fetched for an entry, while its size and mtime stay the same.
    fn store_hashes(&self, entry: &IndexedEntry, hashes: &[(HashKind, String)]) -> IndexResult<()>;
    /// Queues a walk of the root holding `path` (every root when `None`), or of the subtree at
    /// `path` when it is a folder below a root; with `confirm`, first applies a held removal.
    /// Returns what was done, for the model.
    fn refresh(&self, path: Option<&str>, confirm: bool) -> IndexResult<String>;
}
