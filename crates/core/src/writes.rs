//! The write tools (`create_folder`, `move`, `copy`, `write_file`, `delete`) and `job_status`,
//! plus the follow-up of async folder jobs that the server's background loop also runs.
//!
//! Every path goes through [`crate::Policy::check_write`]: hidden paths are "not found", and
//! anything at, under or above a protected folder is refused, for the source of a move or copy
//! as much as for its destination. A folder with a hidden folder somewhere inside is refused
//! too, since moving, copying or deleting it would take the hidden folder along.
//!
//! After pCloud confirms a write, the index is updated at once (`record_write`,
//! `record_delete`); folder jobs re-walk the affected folders when they finish.

use std::sync::Arc;

use base64::Engine as _;
use bytes::Bytes;
use rmcp::model::{ContentBlock, Tool};
use serde_json::{Map, Value, json};

use crate::index::Index;
use crate::policy::{CheckedPath, Policy};
use crate::server::ServerConfig;
use crate::state::{JobRow, JobStatus, StateStore};
use crate::storage::{Done, Entry, JobId, JobInfo, JobState, StorageError, StorageWrite};
use crate::time;
use crate::tools::{
    Args, COPY, CREATE_FOLDER, Ctx, DELETE, JOB_STATUS, MOVE, Structured, WRITE_FILE,
    destructive_tool, human_size, storage_error, tool,
};

/// A backend job whose start time is this far from ours is someone else's (rclone restarted
/// and reused the id).
const JOB_CLOCK_SLACK: i64 = 120;

pub(crate) fn definition(config: &ServerConfig, name: &str) -> Option<Tool> {
    let path = |what: &str| json!({"type": "string", "description": what});
    let overwrite = json!({"type": "boolean", "default": false,
        "description": "Replace an existing file at the destination (never a folder)."});
    Some(match name {
        CREATE_FOLDER => destructive_tool(
            CREATE_FOLDER,
            "Create folder",
            "Create a folder on the pCloud drive, with any missing parent folders. Creating one \
             that already exists does nothing.",
            json!({
                "type": "object",
                "properties": {"path": path("The new folder, e.g. \"/Projects/2026\".")},
                "required": ["path"],
                "additionalProperties": false
            }),
            true,
            false,
        ),
        MOVE => destructive_tool(
            MOVE,
            "Move or rename",
            "Move or rename a file or folder. `to` is the new full path; if `to` is an existing \
             folder, the item moves into it under its own name. The destination's parent folder \
             must exist. A file moves at once; a folder move runs as a job (on pCloud it is \
             usually one rename): the result gives a job id for job_status.",
            json!({
                "type": "object",
                "properties": {
                    "from": path("The file or folder to move, e.g. \"/Inbox/report.pdf\"."),
                    "to": path("The new path, e.g. \"/Archive/2026/report.pdf\", or an existing folder to move into."),
                    "overwrite": overwrite
                },
                "required": ["from", "to"],
                "additionalProperties": false
            }),
            false,
            false,
        ),
        COPY => destructive_tool(
            COPY,
            "Copy",
            "Copy a file or folder within the drive (on pCloud's side; nothing is downloaded). \
             `to` is the new full path; if `to` is an existing folder, the copy goes into it under \
             the same name. A file is copied at once; a folder copy runs as a job: the result \
             gives a job id for job_status.",
            json!({
                "type": "object",
                "properties": {
                    "from": path("The file or folder to copy."),
                    "to": path("The copy's path, or an existing folder to copy into."),
                    "overwrite": overwrite
                },
                "required": ["from", "to"],
                "additionalProperties": false
            }),
            false,
            false,
        ),
        WRITE_FILE => destructive_tool(
            WRITE_FILE,
            "Write file",
            &format!(
                "Create a file (or replace one, with overwrite=true) from text or base64 \
                 content, up to {}. The parent folder must exist (create_folder makes it). For \
                 bigger files use create_upload_link.",
                human_size(config.max_inline_bytes)
            ),
            json!({
                "type": "object",
                "properties": {
                    "path": path("The file to write, e.g. \"/Notes/todo.md\"."),
                    "text": {"type": "string", "description": "The contents as text (written as UTF-8)."},
                    "base64": {"type": "string", "description": "The contents as base64, for binary files. Give text or base64, not both."},
                    "overwrite": overwrite
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            false,
            false,
        ),
        DELETE => destructive_tool(
            DELETE,
            "Delete to trash",
            "Delete a file or folder by moving it to pCloud's trash, where the owner can restore \
             it (nothing is deleted for good). A folder is deleted as a job: the result gives a \
             job id for job_status.",
            json!({
                "type": "object",
                "properties": {"path": path("The file or folder to delete.")},
                "required": ["path"],
                "additionalProperties": false
            }),
            false,
            false,
        ),
        JOB_STATUS => tool(
            JOB_STATUS,
            "Job status",
            "How a folder move, copy or delete is going: running (with progress), done, failed \
             (with the error), or lost (the file server restarted; check the result with \
             list_folder and retry). Without an id, the recent jobs.",
            json!({
                "type": "object",
                "properties": {"id": {"type": "string", "description": "The job id a write tool returned, e.g. \"j1a2b3c4d5e6f\"."}},
                "additionalProperties": false
            }),
        ),
        _ => return None,
    })
}

pub(crate) async fn call<S: StorageWrite>(
    ctx: &Ctx<'_, S>,
    name: &str,
    args: &Args<'_>,
) -> Option<Structured> {
    let plain = |out: Result<String, String>| out.map(|t| (vec![ContentBlock::text(t)], None));
    Some(match name {
        CREATE_FOLDER => plain(create_folder(ctx, args).await),
        MOVE => plain(move_or_copy(ctx, args, Op::Move).await),
        COPY => plain(move_or_copy(ctx, args, Op::Copy).await),
        WRITE_FILE => plain(write_file(ctx, args).await),
        DELETE => plain(delete(ctx, args).await),
        JOB_STATUS => plain(job_status(ctx, args).await),
        _ => return None,
    })
}

/// The arguments for the audit log, with file contents replaced by their length.
pub(crate) fn audit_args(args: &Map<String, Value>) -> String {
    let mut shown = args.clone();
    for key in ["text", "base64"] {
        if let Some(Value::String(content)) = shown.get(key) {
            let len = content.len();
            shown.insert(key.into(), json!(format!("({len} characters)")));
        }
    }
    Value::Object(shown).to_string()
}

fn checked_write(policy: &Policy, path: &str) -> Result<CheckedPath, String> {
    if policy.check(path).map_err(storage_error)?.is_root() {
        return Err("The drive's root \"/\" itself can't be changed.".into());
    }
    policy.check_write(path).map_err(storage_error)
}

/// Refuses a folder that holds a hidden path: moving, copying or deleting it would take that
/// along. The message doesn't say what is inside.
fn no_hidden_inside(policy: &Policy, path: &CheckedPath) -> Result<(), String> {
    if policy.hidden().any_below(path.as_str()) {
        return Err(format!(
            "{path} can't be moved, copied or deleted as a whole here: it contains something \
             this server doesn't manage. Work on the files inside it instead."
        ));
    }
    Ok(())
}

fn parent(path: &CheckedPath) -> &str {
    match path.as_str().rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path.as_str()[..i],
    }
}

fn name_of(path: &CheckedPath) -> &str {
    path.as_str().rsplit('/').next().unwrap_or("")
}

/// `Some(entry)` when it exists, `None` when it doesn't.
async fn lookup<S: StorageWrite>(
    ctx: &Ctx<'_, S>,
    path: &CheckedPath,
) -> Result<Option<Entry>, String> {
    let at = path.clone();
    match ctx
        .timed(async { stat_or_none(ctx.storage, &at).await })
        .await
    {
        Ok(found) => Ok(found),
        Err(message) => Err(message),
    }
}

async fn stat_or_none<S: StorageWrite>(
    storage: &S,
    path: &CheckedPath,
) -> Result<Option<Entry>, StorageError> {
    match storage.stat(path, false).await {
        Ok(entry) => Ok(Some(entry)),
        Err(StorageError::NotFound(_)) => Ok(None),
        Err(err) => Err(err),
    }
}

/// The destination's parent must be an existing folder, so a typo doesn't grow a new tree.
async fn parent_exists<S: StorageWrite>(
    ctx: &Ctx<'_, S>,
    path: &CheckedPath,
) -> Result<(), String> {
    let dir = parent(path);
    if dir == "/" {
        return Ok(());
    }
    let dir = ctx.check(dir)?;
    match lookup(ctx, &dir).await? {
        Some(entry) if entry.is_dir => Ok(()),
        Some(_) => Err(format!("{dir} is a file, not a folder.")),
        None => Err(format!(
            "The folder {dir} doesn't exist. Create it first with create_folder."
        )),
    }
}

async fn create_folder<S: StorageWrite>(
    ctx: &Ctx<'_, S>,
    args: &Args<'_>,
) -> Result<String, String> {
    let path = checked_write(ctx.policy, args.required_path("path")?)?;
    match lookup(ctx, &path).await? {
        Some(entry) if entry.is_dir => return Ok(format!("{path} already exists.")),
        Some(_) => return Err(format!("{path} exists and is a file.")),
        None => {}
    }
    let entry = ctx.timed(ctx.storage.mkdir(&path)).await?;
    let note = record_write(ctx, &entry).await;
    Ok(format!("Created the folder {path}.{note}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Move,
    Copy,
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Self::Move => "move",
            Self::Copy => "copy",
        }
    }
}

async fn move_or_copy<S: StorageWrite>(
    ctx: &Ctx<'_, S>,
    args: &Args<'_>,
    op: Op,
) -> Result<String, String> {
    let from = checked_write(ctx.policy, args.required_path("from")?)?;
    // The destination is checked for writing once it is final: moving a file into /Backups is
    // fine even while /Backups/Immich is protected.
    let named = ctx.check(args.required_path("to")?)?;
    let overwrite = args.bool("overwrite")?.unwrap_or(false);
    no_hidden_inside(ctx.policy, &from)?;
    let source = lookup(ctx, &from)
        .await?
        .ok_or_else(|| storage_error(StorageError::NotFound(from.to_string())))?;
    let into = named.is_root()
        || (crate::fold(named.as_str()) != crate::fold(from.as_str())
            && lookup(ctx, &named).await?.is_some_and(|e| e.is_dir));
    let to = if into {
        // Into an existing folder, under the same name.
        let joined = if named.is_root() {
            format!("/{}", name_of(&from))
        } else {
            format!("{named}/{}", name_of(&from))
        };
        checked_write(ctx.policy, &joined)?
    } else {
        checked_write(ctx.policy, named.as_str())?
    };
    let existing = lookup(ctx, &to).await?;
    let (f, t) = (crate::fold(from.as_str()), crate::fold(to.as_str()));
    if f == t {
        return Err(format!("{from} and {to} are the same path."));
    }
    if source.is_dir && t.starts_with(&format!("{f}/")) {
        return Err(format!("{from} can't go inside itself ({to})."));
    }
    if let Some(found) = existing
        && (found.is_dir || source.is_dir || !overwrite)
    {
        return Err(storage_error(StorageError::AlreadyExists(to.to_string())));
    }
    parent_exists(ctx, &to).await?;
    let done = match op {
        Op::Move => ctx.timed(ctx.storage.rename(&from, &to)).await?,
        Op::Copy => ctx.timed(ctx.storage.copy(&from, &to)).await?,
    };
    match done {
        Done::Now(entry) => {
            let mut note = String::new();
            if op == Op::Move {
                note.push_str(&record_delete(ctx, from.as_str()).await);
            }
            note.push_str(&record_write(ctx, &entry).await);
            let verb = if op == Op::Move { "Moved" } else { "Copied" };
            let size = entry
                .size
                .map(|s| format!(" ({})", human_size(s)))
                .unwrap_or_default();
            Ok(format!("{verb} {from} to {to}{size}.{note}"))
        }
        Done::Job(id) => {
            let job = start_job(ctx, &id, op.name(), &from, Some(&to)).await?;
            Ok(format!(
                "Started a job to {} the folder {from} to {to}: {job}. Check it with \
                 job_status id=\"{job}\"; folder moves on pCloud usually finish in seconds, \
                 copies take as long as the data.",
                op.name()
            ))
        }
    }
}

async fn write_file<S: StorageWrite>(ctx: &Ctx<'_, S>, args: &Args<'_>) -> Result<String, String> {
    let path = checked_write(ctx.policy, args.required_path("path")?)?;
    let overwrite = args.bool("overwrite")?.unwrap_or(false);
    let data: Vec<u8> = match (args.string("text")?, args.string("base64")?) {
        (Some(text), None) => text.as_bytes().to_vec(),
        (None, Some(b64)) => {
            let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
            base64::engine::general_purpose::STANDARD
                .decode(cleaned.trim_end_matches('='))
                .or_else(|_| {
                    base64::engine::general_purpose::STANDARD_NO_PAD
                        .decode(cleaned.trim_end_matches('='))
                })
                .or_else(|_| {
                    base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(cleaned.trim_end_matches('='))
                })
                .map_err(|_| "base64 is not valid base64.".to_owned())?
        }
        (Some(_), Some(_)) => return Err("Give text or base64, not both.".into()),
        (None, None) => return Err("Give the contents as text or base64.".into()),
    };
    let max = ctx.config.max_inline_bytes;
    if data.len() as u64 > max {
        return Err(format!(
            "That is {}, more than the {} write_file takes. Use create_upload_link for big files.",
            human_size(data.len() as u64),
            human_size(max)
        ));
    }
    match lookup(ctx, &path).await? {
        Some(entry) if entry.is_dir => {
            return Err(storage_error(StorageError::IsADirectory(path.to_string())));
        }
        Some(_) if !overwrite => {
            return Err(storage_error(StorageError::AlreadyExists(path.to_string())));
        }
        _ => {}
    }
    parent_exists(ctx, &path).await?;
    let len = data.len() as u64;
    let entry = ctx
        .timed(ctx.storage.put_bytes(Bytes::from(data), &path))
        .await?;
    if entry.size.is_some_and(|s| s != len) {
        return Err(format!(
            "pCloud reports {} for {path}, but {len} bytes were sent; check the file.",
            human_size(entry.size.unwrap_or(0))
        ));
    }
    let note = record_write(ctx, &entry).await;
    Ok(format!("Wrote {path} ({}).{note}", human_size(len)))
}

async fn delete<S: StorageWrite>(ctx: &Ctx<'_, S>, args: &Args<'_>) -> Result<String, String> {
    let path = checked_write(ctx.policy, args.required_path("path")?)?;
    no_hidden_inside(ctx.policy, &path)?;
    if !ctx.storage.capabilities().trash_on_delete {
        return Err(
            "This storage backend has no trash to restore from, so delete is turned off.".into(),
        );
    }
    let entry = lookup(ctx, &path)
        .await?
        .ok_or_else(|| storage_error(StorageError::NotFound(path.to_string())))?;
    match ctx.timed(ctx.storage.trash(&path)).await? {
        Done::Now(_) => {
            let note = record_delete(ctx, path.as_str()).await;
            let what = if entry.is_dir { "folder" } else { "file" };
            Ok(format!(
                "Moved the {what} {path} to pCloud's trash; the owner can restore it there.{note}"
            ))
        }
        Done::Job(id) => {
            let job = start_job(ctx, &id, "delete", &path, None).await?;
            Ok(format!(
                "Started a job to move the folder {path} to pCloud's trash: {job}. Check it with \
                 job_status id=\"{job}\"."
            ))
        }
    }
}

/// Index bookkeeping after a write; a note for the model only when it failed.
async fn record_write<S: StorageWrite>(ctx: &Ctx<'_, S>, entry: &Entry) -> String {
    let entry = entry.clone();
    match ctx.ix_quiet(move |index| index.record_write(&entry)).await {
        None => String::new(),
        Some(e) => format!(" (The index wasn't updated: {e}; the next walk will catch up.)"),
    }
}

async fn record_delete<S: StorageWrite>(ctx: &Ctx<'_, S>, path: &str) -> String {
    let path = path.to_owned();
    match ctx.ix_quiet(move |index| index.record_delete(&path)).await {
        None => String::new(),
        Some(e) => format!(" (The index wasn't updated: {e}; the next walk will catch up.)"),
    }
}

pub(crate) fn new_job_id() -> String {
    let bytes = crate::links::random_bytes::<6>();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("j{hex}")
}

async fn start_job<S: StorageWrite>(
    ctx: &Ctx<'_, S>,
    backend: &JobId,
    op: &str,
    src: &CheckedPath,
    dst: Option<&CheckedPath>,
) -> Result<String, String> {
    let row = JobRow {
        id: new_job_id(),
        backend_id: backend.0.clone(),
        op: op.to_owned(),
        src: src.to_string(),
        dst: dst.map(ToString::to_string),
        started: time::now(),
        finished: None,
        state: JobStatus::Running,
        error: None,
    };
    let id = row.id.clone();
    ctx.st(move |state| state.insert_job(&row))
        .await
        .map_err(|e| {
            format!(
                "The {op} was started on the file server (rclone job {backend}) but couldn't be \
             recorded: {e}. Check the result with list_folder."
            )
        })?;
    Ok(id)
}

async fn job_status<S: StorageWrite>(ctx: &Ctx<'_, S>, args: &Args<'_>) -> Result<String, String> {
    let state = ctx.state.ok_or("This server keeps no jobs.")?;
    match args.string("id")?.map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => {
            let wanted = id.to_owned();
            let row = ctx.st(move |s| s.job(&wanted)).await?.ok_or_else(|| {
                format!("There is no job {id}. job_status without an id lists recent jobs.")
            })?;
            let (row, progress) = settle(ctx.storage, state, ctx.index, row).await;
            Ok(describe_job(&row, progress.as_deref()))
        }
        None => {
            let rows = ctx.st(|s| s.jobs(20)).await?;
            if rows.is_empty() {
                return Ok("No jobs yet.".into());
            }
            let mut lines = vec![format!("{} most recent jobs, newest first:", rows.len())];
            for row in rows {
                let (row, progress) = settle(ctx.storage, state, ctx.index, row).await;
                lines.push(format!("- {}", describe_job(&row, progress.as_deref())));
            }
            Ok(lines.join("\n"))
        }
    }
}

fn describe_job(row: &JobRow, progress: Option<&str>) -> String {
    let what = match &row.dst {
        Some(dst) => format!("{} {} → {}", row.op, row.src, dst),
        None => format!("{} {}", row.op, row.src),
    };
    let when = format!("started {}", time::short(row.started, time::now()));
    let state = match row.state {
        JobStatus::Running => match progress {
            Some(p) => format!("running ({p})"),
            None => "running".into(),
        },
        JobStatus::Done => format!(
            "done{}",
            row.finished
                .map(|t| format!(" at {}", time::short(t, time::now())))
                .unwrap_or_default()
        ),
        JobStatus::Failed => format!(
            "failed: {}. Check what arrived with list_folder live=true, then retry",
            row.error.as_deref().unwrap_or("unknown error")
        ),
        JobStatus::Lost => "lost: the file server restarted while it ran, so the result is \
                            unknown. Check with list_folder live=true and retry what's missing"
            .into(),
    };
    format!("{}: {what}, {when}, {state}.", row.id)
}

/// Asks the backend about a running job and, when it has ended, records that and updates the
/// index. Finished jobs come back as they are. Returns the row as it now stands and progress.
pub async fn settle<S: StorageWrite>(
    storage: &S,
    state: &Arc<dyn StateStore>,
    index: Option<&Arc<dyn Index>>,
    mut row: JobRow,
) -> (JobRow, Option<String>) {
    if row.state != JobStatus::Running {
        return (row, None);
    }
    let info = match storage.job(&JobId(row.backend_id.clone())).await {
        Ok(info) => info,
        // Can't tell right now; ask again later.
        Err(_) => return (row, None),
    };
    let JobInfo {
        state: job_state,
        started,
        progress,
    } = info;
    let reused = started.is_some_and(|t| (t - row.started).abs() > JOB_CLOCK_SLACK);
    let (status, error) = match job_state {
        _ if reused => (JobStatus::Lost, None),
        JobState::Running => return (row, progress),
        JobState::Done => (JobStatus::Done, None),
        JobState::Failed(e) => (JobStatus::Failed, Some(e)),
        JobState::Lost => (JobStatus::Lost, None),
    };
    let now = time::now();
    let (id, err) = (row.id.clone(), error.clone());
    let store = state.clone();
    let saved =
        tokio::task::spawn_blocking(move || store.finish_job(&id, status, err.as_deref(), now))
            .await;
    if !matches!(saved, Ok(Ok(()))) {
        return (row, None);
    }
    row.state = status;
    row.error = error;
    row.finished = Some(now);
    if let Some(index) = index {
        follow_up(index.clone(), &row).await;
    }
    (row, None)
}

/// What the index learns from a finished job: a move or delete took the source away; any
/// destination, and the folders around a job that didn't end cleanly, get walked again.
async fn follow_up(index: Arc<dyn Index>, row: &JobRow) {
    let parent_of = |p: &str| -> String {
        match p.rfind('/') {
            Some(0) | None => "/".into(),
            Some(i) => p[..i].into(),
        }
    };
    let mut gone = Vec::new();
    let mut walk = Vec::new();
    match row.state {
        JobStatus::Done => {
            if row.op == "move" || row.op == "delete" {
                gone.push(row.src.clone());
            }
            if let Some(dst) = &row.dst {
                walk.push(parent_of(dst));
            }
        }
        _ => {
            walk.push(parent_of(&row.src));
            if let Some(dst) = &row.dst {
                walk.push(parent_of(dst));
            }
        }
    }
    let _ = tokio::task::spawn_blocking(move || {
        for path in gone {
            let _ = index.record_delete(&path);
        }
        walk.dedup();
        for path in walk {
            let _ = index.rewalk(&path);
        }
    })
    .await;
}

/// Settles every running job; the server calls this every few seconds while any runs.
pub async fn poll_jobs<S: StorageWrite>(
    storage: &S,
    state: &Arc<dyn StateStore>,
    index: Option<&Arc<dyn Index>>,
) -> usize {
    let store = state.clone();
    let running = match tokio::task::spawn_blocking(move || store.running_jobs()).await {
        Ok(Ok(rows)) => rows,
        _ => return 0,
    };
    let mut still = 0;
    for row in running {
        let (row, _) = settle(storage, state, index, row).await;
        if row.state == JobStatus::Running {
            still += 1;
        }
    }
    still
}
