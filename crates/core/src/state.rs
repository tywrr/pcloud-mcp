//! What the server keeps that can't be rebuilt from pCloud: async jobs, links and their use,
//! and the audit log of writes. The server keeps it in SQLite (`state.db`); the tools see this
//! trait. Methods are synchronous (one small SQLite file); tools call them on a blocking thread.

use std::fmt;

/// An async folder operation, as the server tracks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRow {
    /// Our id, shown to the model (`j` and 12 hex digits).
    pub id: String,
    /// The backend's id (rclone's jobid).
    pub backend_id: String,
    /// `move`, `copy` or `delete`.
    pub op: String,
    pub src: String,
    pub dst: Option<String>,
    pub started: i64,
    pub finished: Option<i64>,
    pub state: JobStatus,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Done,
    Failed,
    /// The backend forgot it (rclone restarted); the result is unknown.
    Lost,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Lost => "lost",
        }
    }

    pub fn parse(text: &str) -> Self {
        match text {
            "running" => Self::Running,
            "done" => Self::Done,
            "failed" => Self::Failed,
            _ => Self::Lost,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Download,
    Upload,
}

impl LinkKind {
    /// Also the route (`/dl/…`, `/ul/…`) and the first part of the signed message.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Download => "dl",
            Self::Upload => "ul",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "dl" => Some(Self::Download),
            "ul" => Some(Self::Upload),
            _ => None,
        }
    }
}

/// A link's life: `active` until it expires, is revoked, or (uploads) completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Active,
    /// Upload: every byte arrived and the file is going to pCloud.
    Finishing,
    /// Upload: the file is on pCloud; the link is dead.
    Done,
    /// Upload: the last step failed; `?finish` retries it.
    Failed,
    Revoked,
    Expired,
}

impl LinkState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Finishing => "finishing",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
        }
    }

    pub fn parse(text: &str) -> Self {
        match text {
            "active" => Self::Active,
            "finishing" => Self::Finishing,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "revoked" => Self::Revoked,
            _ => Self::Expired,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkRow {
    /// 128 random bits, base64url: the first part of the token.
    pub id: String,
    pub kind: LinkKind,
    /// Download: the files or folders it serves. Upload: the one destination file.
    pub paths: Vec<String>,
    /// Download: one streamed zip of everything in `paths`.
    pub zip: bool,
    /// The file name in the URL and in `Content-Disposition`.
    pub name: String,
    pub created: i64,
    pub expires: i64,
    /// The network of the first client (`160.79.106.0/24`), once one used it.
    pub bound_net: Option<String>,
    /// Payload bytes: the file, the zip's contents, or the upload's announced size.
    pub size: u64,
    /// Bytes this link may move in all, retries included (3× `size`).
    pub max_bytes: u64,
    /// Download: bytes sent. Upload: bytes received.
    pub bytes_used: u64,
    /// Upload: bytes committed to the staging file, in order.
    pub committed: u64,
    pub overwrite: bool,
    pub state: LinkState,
    /// Upload: the error of a failed completion, or the final path.
    pub note: Option<String>,
}

/// One request made with a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkUse {
    pub at: i64,
    pub ip: String,
    /// `bytes=0-99`, `part 3`, `finish`, or empty.
    pub range: String,
    pub bytes: u64,
    pub status: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateError(pub String);

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "state database error: {}", self.0)
    }
}

impl std::error::Error for StateError {}

pub type StateResult<T> = Result<T, StateError>;

pub trait StateStore: Send + Sync {
    fn insert_job(&self, job: &JobRow) -> StateResult<()>;
    fn finish_job(
        &self,
        id: &str,
        state: JobStatus,
        error: Option<&str>,
        at: i64,
    ) -> StateResult<()>;
    fn job(&self, id: &str) -> StateResult<Option<JobRow>>;
    /// Newest first.
    fn jobs(&self, limit: usize) -> StateResult<Vec<JobRow>>;
    fn running_jobs(&self) -> StateResult<Vec<JobRow>>;

    fn insert_link(&self, link: &LinkRow) -> StateResult<()>;
    fn link(&self, id: &str) -> StateResult<Option<LinkRow>>;
    /// Newest first; expired ones too, for a while.
    fn links(&self, limit: usize) -> StateResult<Vec<LinkRow>>;
    /// Moves a link to `to` (setting `note` when given) only if it is in one of `from`, in one
    /// step; false when it wasn't (or there is no such link).
    fn transition(
        &self,
        id: &str,
        from: &[LinkState],
        to: LinkState,
        note: Option<&str>,
    ) -> StateResult<bool>;
    /// Newest first.
    fn uses(&self, link_id: &str, limit: usize) -> StateResult<Vec<LinkUse>>;
    /// Staging space promised to upload links still in flight: their size minus what has
    /// arrived (that part is already on disk).
    fn reserved_bytes(&self, now: i64) -> StateResult<u64>;
    /// Free bytes on the volume upload parts are staged on, when known.
    fn staging_free_bytes(&self) -> StateResult<Option<u64>> {
        Ok(None)
    }

    /// Records a write or link tool call: the tool, its arguments (file contents left out) and
    /// how it ended.
    fn audit(&self, at: i64, tool: &str, args: &str, result: &str) -> StateResult<()>;
}
