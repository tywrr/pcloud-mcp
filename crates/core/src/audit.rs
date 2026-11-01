//! The tools that answer from the metadata index: `list_folder`, `search_files` and the
//! `get_file_info` totals when an index is configured, and the audit tools.
//!
//! Every result starts with a header line: which rows these are, how current each
//! contributing root is, the next cursor, and the backend's health when it is failing, e.g.
//! `[rows 1-500 of 13,204 · / as of 14:05Z, /Backups/Immich as of 09:00Z · next cursor=…]`.
//! Text plus `structuredContent` stays under [`BUDGET`] characters; longer answers page.
//!
//! Index queries run on the blocking pool. Results honour the policy like live ones: hidden
//! entries are never indexed, and the index filters again in case the policy changed.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use futures_util::StreamExt;
use rmcp::model::{ContentBlock, Tool};
use serde_json::{Value, json};

use crate::health::Health;
use crate::index::{Filter, Index, IndexResult, IndexedEntry, RootStatus, Sort, What};
use crate::kind::KINDS;
use crate::matching::fold;
use crate::policy::{CheckedPath, is_at_or_under, is_under};
use crate::server::ServerConfig;
use crate::storage::{Entry, HashKind, Storage};
use crate::time;
use crate::tools::{
    Args, Ctx, FIND_DUPLICATES, FOLDER_STATS, INDEX_STATUS, QUERY_FILES, RECENT_CHANGES,
    REFRESH_INDEX, Structured, TYPE_BREAKDOWN, human_size, storage_error, tool,
};

/// Characters of text and structured content together per result: under Claude's ~150k cap
/// with room for the header and notes.
const BUDGET: usize = 110_000;
/// Rows one `list_folder` page asks the index for.
const LIST_PAGE: usize = 500;
/// Files of size groups one `find_duplicates` page takes on.
const DUPLICATE_PAGE_FILES: usize = 300;
/// Hash calls at once (each is one pCloud API call, under rclone's --tpslimit).
const HASH_CONCURRENCY: usize = 4;
/// Most of a call's time `find_duplicates` spends hashing.
const HASH_TIME: std::time::Duration = std::time::Duration::from_secs(90);

/// The tool definitions that change when an index is present, and the audit tools.
pub(crate) fn definition(config: &ServerConfig, name: &str) -> Option<Tool> {
    let search_limit = config.search_limit.max(1);
    let path = |what: &str| json!({"type": "string", "default": "/", "description": what});
    let cursor = json!({"type": "string", "description": "From the previous page's header, to continue it."});
    let limit = |default: u64, max: u64| {
        json!({"type": "integer", "minimum": 1, "maximum": max, "default": default,
               "description": "Most rows to return."})
    };
    let size = |what: &str| {
        json!({"type": ["integer", "string"],
               "description": format!("{what}: bytes, or with a unit such as \"500 MiB\", \"1.5 GB\" (KB/MB/GB are powers of 1000, KiB/MiB/GiB and K/M/G of 1024).")})
    };
    let kinds: Vec<&str> = KINDS.iter().copied().filter(|k| *k != "folder").collect();
    Some(match name {
        "list_folder" => tool(
            "list_folder",
            "List folder",
            &format!(
                "List the files and folders directly inside a folder of the pCloud drive, from the \
                 index (instant, as of the last walk, which the first line states): type, size \
                 (for folders, everything inside them), last-modified time and full path. Sort by \
                 name (folders first), largest, smallest, newest or oldest. Up to {LIST_PAGE} \
                 entries per call; the first line gives a cursor for the next page. live=true asks \
                 the drive itself instead (slower; for something just changed)."
            ),
            json!({
                "type": "object",
                "properties": {
                    "path": path("Folder to list, e.g. \"/\" or \"/Notes/2026\"."),
                    "sort": {"type": "string", "enum": ["name", "largest", "smallest", "newest", "oldest"], "default": "name", "description": "Order: name (folders first), largest, smallest, newest or oldest."},
                    "cursor": cursor,
                    "live": {"type": "boolean", "default": false, "description": "List from the drive, not the index."}
                },
                "additionalProperties": false
            }),
        ),
        "search_files" => tool(
            "search_files",
            "Search files",
            &format!(
                "Find files and folders whose name contains the query, ignoring case and accents, \
                 anywhere under a folder (default the whole drive), from the index. Matches names \
                 only, not contents. Up to {search_limit} results per call, sorted by path; the \
                 first line gives a cursor for more. kind narrows to one type of file. Skips \
                 folders the owner turned off for search, and says which."
            ),
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "minLength": 1, "description": "Part of a file or folder name, e.g. \"invoice\" or \".pdf\"."},
                    "path": path("Folder to search under."),
                    "kind": {"type": "string", "enum": KINDS, "description": "Only this kind of entry."},
                    "limit": limit(search_limit as u64, search_limit as u64),
                    "cursor": cursor
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        ),
        FOLDER_STATS => tool(
            FOLDER_STATS,
            "Folder stats",
            "Where the space goes: the folders 1 to 3 levels below a folder, each with the total \
             size, file count and newest change of everything inside it, largest first. Start \
             here for \"what takes up my drive\" and drill down with path. From the index.",
            json!({
                "type": "object",
                "properties": {
                    "path": path("Folder to break down."),
                    "depth": {"type": "integer", "minimum": 1, "maximum": 3, "default": 1, "description": "How many levels of subfolders to include."},
                    "sort": {"type": "string", "enum": ["largest", "smallest", "files", "newest", "oldest", "name", "path"], "default": "largest", "description": "Order of the folders: by total size, file count, newest change, or name."},
                    "limit": limit(100, 1000),
                    "cursor": cursor
                },
                "additionalProperties": false
            }),
        ),
        QUERY_FILES => tool(
            QUERY_FILES,
            "Query files",
            "Files (or folders) under a folder matching any mix of kind, extensions, size and \
             modification-date bounds, sorted as asked: \"the 50 largest files\", \"all .mov over \
             1 GB\", \"PDFs changed this year\". The first line gives the total count and size of \
             all matches. From the index.",
            json!({
                "type": "object",
                "properties": {
                    "path": path("Folder to look under."),
                    "kind": {"type": "string", "enum": kinds, "description": "Only files of this kind."},
                    "ext": {"type": ["string", "array"], "items": {"type": "string"}, "description": "Extension(s) without the dot, e.g. \"mov\" or [\"jpg\", \"heic\"]."},
                    "min_size": size("Smallest size"),
                    "max_size": size("Largest size"),
                    "modified_after": {"type": "string", "description": "A date (2024-01-31), date-time, or an age such as \"30d\" (modified within)."},
                    "modified_before": {"type": "string", "description": "A date or date-time; or an age such as \"365d\" (not modified within)."},
                    "type": {"type": "string", "enum": ["files", "folders", "any"], "default": "files", "description": "Match files, folders (sizes are their totals) or both."},
                    "sort": {"type": "string", "enum": ["largest", "smallest", "newest", "oldest", "name", "path"], "default": "largest", "description": "Order of the results."},
                    "limit": limit(100, 1000),
                    "cursor": cursor
                },
                "additionalProperties": false
            }),
        ),
        FIND_DUPLICATES => tool(
            FIND_DUPLICATES,
            "Find duplicates",
            "Files with identical contents under a folder: grouped by size from the index, then \
             confirmed by content hash (fetched from the drive and cached, so the first run over \
             a big folder can take a few calls). Largest first, with the space each set wastes.",
            json!({
                "type": "object",
                "properties": {
                    "path": path("Folder to look under."),
                    "min_size": size("Smallest file size to consider (default 1 MiB)"),
                    "cursor": cursor
                },
                "additionalProperties": false
            }),
        ),
        RECENT_CHANGES => tool(
            RECENT_CHANGES,
            "Recent changes",
            "What changed on the drive, newest first: files and folders added, removed, changed \
             or moved, as the index's walks noticed them (a moved or removed folder is one line). \
             From the index's change log.",
            json!({
                "type": "object",
                "properties": {
                    "since": {"type": "string", "default": "7d", "description": "An age such as \"24h\" or \"30d\", or a date."},
                    "path": path("Only changes under this folder."),
                    "source": {"type": "string", "enum": ["walk", "own-write"], "description": "Only changes found by walks, or made through this server."},
                    "limit": limit(200, 1000),
                    "cursor": cursor
                },
                "additionalProperties": false
            }),
        ),
        TYPE_BREAKDOWN => tool(
            TYPE_BREAKDOWN,
            "Type breakdown",
            "Count and total size of the files under a folder by kind (image, video, document \
             and so on) and the main extensions of each. From the index.",
            json!({
                "type": "object",
                "properties": {"path": path("Folder to break down.")},
                "additionalProperties": false
            }),
        ),
        INDEX_STATUS => tool(
            INDEX_STATUS,
            "Index status",
            "How current the index is: each root's last walk, its entries, errors, removals held \
             for confirmation, walks running or queued, duplicate-hashing progress and the \
             storage backend's health.",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
        ),
        REFRESH_INDEX => tool(
            REFRESH_INDEX,
            "Refresh index",
            "Queue a fresh walk of the drive (or of the root or folder holding path), for changes \
             made outside this server; it runs in the background, and index_status shows when it \
             is done. confirm=true first applies a removal that index_status reports as held \
             (a walk that would have removed more than a few percent of a root).",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "A root, or a folder to re-walk; leave out for everything."},
                    "confirm": {"type": "boolean", "default": false, "description": "Apply a held removal."}
                },
                "additionalProperties": false
            }),
        ),
        _ => return None,
    })
}

pub(crate) async fn call<S: Storage>(
    ctx: &Ctx<'_, S>,
    name: &str,
    args: &Args<'_>,
) -> Option<Structured> {
    Some(match name {
        FOLDER_STATS => folder_stats(ctx, args).await,
        QUERY_FILES => query_files(ctx, args).await,
        FIND_DUPLICATES => find_duplicates(ctx, args).await,
        RECENT_CHANGES => recent_changes(ctx, args).await,
        TYPE_BREAKDOWN => type_breakdown(ctx, args).await,
        INDEX_STATUS => index_status(ctx).await,
        REFRESH_INDEX => refresh_index(ctx, args).await,
        _ => return None,
    })
}

/// Runs an index query on the blocking pool.
async fn ix<S, T, F>(ctx: &Ctx<'_, S>, work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&dyn Index) -> IndexResult<T> + Send + 'static,
{
    let index: Arc<dyn Index> = ctx
        .index
        .ok_or("This server has no index configured.")?
        .clone();
    tokio::task::spawn_blocking(move || work(&*index))
        .await
        .map_err(|e| format!("The index query failed: {e}"))?
        .map_err(|e| e.to_string())
}

pub(crate) fn parse_sort(text: &str, allowed: &[Sort]) -> Result<Sort, String> {
    Sort::parse(text)
        .filter(|s| allowed.contains(s))
        .ok_or_else(|| {
            format!(
                "sort must be one of {}.",
                allowed
                    .iter()
                    .map(|s| s.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// Tool cursors are `<rows before>.<index cursor>`, so headers can number rows.
fn wrap_cursor(shown_before: u64, index_cursor: &str) -> String {
    format!("{shown_before}.{index_cursor}")
}

fn unwrap_cursor(cursor: Option<&str>) -> Result<(u64, Option<String>), String> {
    match cursor.map(str::trim).filter(|c| !c.is_empty()) {
        None => Ok((0, None)),
        Some(c) => {
            let (n, rest) = c
                .split_once('.')
                .and_then(|(n, rest)| Some((n.parse().ok()?, rest)))
                .ok_or("cursor is not one this tool gave out; leave it out to start from the first page.")?;
            Ok((n, Some(rest.to_owned())))
        }
    }
}

pub(crate) fn is_index_cursor(cursor: &str) -> bool {
    cursor
        .split_once('.')
        .is_some_and(|(n, _)| n.parse::<u64>().is_ok())
}

/// The root whose walk shows `path`'s contents: the deepest one at or above it.
fn covering<'r>(roots: &'r [RootStatus], path: &str) -> Option<&'r RootStatus> {
    let folded = fold(path);
    roots
        .iter()
        .filter(|r| is_at_or_under(&folded, &fold(&r.path)))
        .max_by_key(|r| r.path.len())
}

/// The roots whose data a result about `path` draws on: the covering root and every root
/// below `path`.
fn contributing<'r>(roots: &'r [RootStatus], path: &str) -> Vec<&'r RootStatus> {
    let folded = fold(path);
    let mut out: Vec<&RootStatus> = covering(roots, path).into_iter().collect();
    out.extend(roots.iter().filter(|r| is_under(&fold(&r.path), &folded)));
    out
}

fn as_of(roots: &[&RootStatus]) -> String {
    let now = time::now();
    roots
        .iter()
        .map(|r| match r.as_of {
            Some(t) => format!("{} as of {}", r.path, time::short(t, now)),
            None => format!("{} not indexed yet", r.path),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `13,204`.
pub(crate) fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

struct Header<'a> {
    /// First and last row numbers shown (1-based), and the total when known.
    rows: Option<(u64, u64, Option<u64>)>,
    roots: Vec<&'a RootStatus>,
    next: Option<String>,
}

impl Header<'_> {
    fn render(&self, health: &Health) -> String {
        let mut parts = Vec::new();
        if let Some((first, last, total)) = self.rows {
            let total = total
                .map(|t| format!(" of {}", thousands(t)))
                .unwrap_or_default();
            if last >= first {
                parts.push(format!(
                    "rows {}-{}{total}",
                    thousands(first),
                    thousands(last)
                ));
            } else {
                parts.push(format!("no rows{total}"));
            }
        }
        if !self.roots.is_empty() {
            parts.push(as_of(&self.roots));
        }
        if let Some(next) = &self.next {
            parts.push(format!("next cursor={next}"));
        }
        let mut text = format!("[{}]\n", parts.join(" · "));
        if let Some(bad) = health.describe() {
            let _ = writeln!(text, "[{bad}]");
        }
        text
    }
}

/// Rows of text and structured content, within [`BUDGET`].
struct Page {
    text: String,
    rows: Vec<Value>,
    used: usize,
}

impl Page {
    fn new() -> Self {
        Self {
            text: String::new(),
            rows: Vec::new(),
            used: 0,
        }
    }

    /// Adds a row unless it would pass the budget.
    fn push(&mut self, line: &str, row: Value) -> bool {
        let cost = line.len() + 1 + row.to_string().len() + 1;
        if self.used + cost > BUDGET {
            return false;
        }
        self.used += cost;
        self.text.push_str(line);
        self.text.push('\n');
        self.rows.push(row);
        true
    }

    fn push_text(&mut self, line: &str) {
        self.used += line.len() + 1;
        self.text.push_str(line);
        self.text.push('\n');
    }
}

fn modified(ts: Option<i64>) -> String {
    ts.map_or_else(|| "unknown".into(), time::rfc3339)
}

fn entry_json(e: &IndexedEntry) -> Value {
    let mut row = json!({
        "path": e.path,
        "type": if e.is_dir { "folder" } else { "file" },
        "kind": e.kind,
        "modified": e.mtime.map(time::rfc3339),
    });
    if let Some(size) = e.size.filter(|_| !e.is_dir) {
        row["size"] = json!(size);
    }
    if let Some(t) = &e.totals {
        row["bytes"] = json!(t.bytes);
        row["files"] = json!(t.files);
        row["folders"] = json!(t.dirs);
        row["newest"] = json!(t.newest.map(time::rfc3339));
    }
    row
}

/// `12.3 GiB in 4,512 files, 31 folders, newest 2026-10-01T09:00:00Z`.
fn totals_text(e: &IndexedEntry) -> String {
    let t = e.totals.unwrap_or_default();
    let mut text = format!(
        "{} in {}",
        human_size(t.bytes),
        count_u64(t.files, "file", "files")
    );
    if t.dirs > 0 {
        let _ = write!(text, ", {}", count_u64(t.dirs, "folder", "folders"));
    }
    if let Some(newest) = t.newest {
        let _ = write!(text, ", newest {}", time::rfc3339(newest));
    }
    text
}

fn count_u64(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", thousands(n), if n == 1 { one } else { many })
}

/// `- name/ (folder, 12.3 GiB in ...) path: /a/name` or `- name (file, 1.2 MiB, modified ...) path: ...`.
fn listing_line(e: &IndexedEntry) -> String {
    if e.is_dir {
        format!(
            "- {}/ (folder, {}) path: {}",
            e.name,
            totals_text(e),
            e.path
        )
    } else {
        format!(
            "- {} (file, {}, modified {}) path: {}",
            e.name,
            human_size(e.size.unwrap_or(0)),
            modified(e.mtime),
            e.path
        )
    }
}

/// `- /a/b.mov (video, 1.2 GiB, modified ...)`.
fn path_line(e: &IndexedEntry) -> String {
    if e.is_dir {
        format!("- {}/ (folder, {})", e.path, totals_text(e))
    } else {
        format!(
            "- {} ({}, {}, modified {})",
            e.path,
            e.kind,
            human_size(e.size.unwrap_or(0)),
            modified(e.mtime)
        )
    }
}

/// The roots, and whether the index can answer for `path` (its covering root has walked).
async fn roots_for<S: Storage>(
    ctx: &Ctx<'_, S>,
    path: &str,
) -> Result<(Vec<RootStatus>, bool), String> {
    let roots = ix(ctx, |i| i.roots()).await?;
    let ready = covering(&roots, path).is_some_and(|r| r.as_of.is_some());
    Ok((roots, ready))
}

fn not_ready(roots: &[RootStatus], path: &str) -> String {
    match covering(roots, path) {
        None => format!(
            "{path} is outside the indexed roots ({}), so the index can't answer for it. Use \
             list_folder or search_files, which ask the drive directly there.",
            roots
                .iter()
                .map(|r| r.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Some(root) => format!(
            "The index of {} isn't ready yet: its first walk hasn't finished{}. index_status \
             shows progress; list_folder and search_files ask the drive directly meanwhile.",
            root.path,
            root.last_error
                .as_deref()
                .map(|e| format!(" (last attempt failed: {e})"))
                .unwrap_or_default()
        ),
    }
}

/// The folder as the index spells it, or an error for a file or unknown path.
async fn indexed_folder<S: Storage>(
    ctx: &Ctx<'_, S>,
    path: &CheckedPath,
) -> Result<IndexedEntry, String> {
    let wanted = path.as_str().to_owned();
    match ix(ctx, move |i| i.entry(&wanted)).await? {
        Some(e) if e.is_dir => Ok(e),
        Some(e) => Err(format!(
            "{} is a file, not a folder. Use get_file_info for it.",
            e.path
        )),
        None => Err(format!(
            "{path} isn't in the index. Check the path with list_folder or search_files; if it \
             was just created, refresh_index picks it up."
        )),
    }
}

pub(crate) async fn list_folder<S: Storage>(
    ctx: &Ctx<'_, S>,
    path: &CheckedPath,
    sort: Sort,
    cursor: Option<&str>,
) -> Result<Option<(Vec<ContentBlock>, Option<Value>)>, String> {
    let (roots, ready) = roots_for(ctx, path.as_str()).await?;
    if !ready {
        return Ok(None);
    }
    let wanted = path.as_str().to_owned();
    let Some(folder) = ix(ctx, move |i| i.entry(&wanted)).await? else {
        // Perhaps created since the last walk: the live listing knows.
        return Ok(None);
    };
    if !folder.is_dir {
        return Err(format!(
            "{} is a file, not a folder. Use get_file_info or read_text_file for it.",
            folder.path
        ));
    }
    let (before, index_cursor) = unwrap_cursor(cursor)?;
    let canonical = folder.path.clone();
    let (rows, (dirs, files)) = ix(ctx, move |i| {
        Ok((
            i.children(&canonical, sort, index_cursor.as_deref(), LIST_PAGE + 1)?,
            i.child_counts(&canonical)?,
        ))
    })
    .await?;
    let total = dirs + files;
    let mut page = Page::new();
    page.push_text(&format!(
        "Folder {}: {}, {}; {} below it in all.",
        folder.path,
        count_u64(dirs, "folder", "folders"),
        count_u64(files, "file", "files"),
        totals_text(&folder)
    ));
    let mut shown = 0usize;
    for e in rows.iter().take(LIST_PAGE) {
        if !page.push(&listing_line(e), Value::Null) {
            break;
        }
        shown += 1;
    }
    let more = shown < rows.len();
    let next = more.then(|| wrap_cursor(before + shown as u64, &rows[shown - 1].cursor));
    if total == 0 {
        page.push_text("(empty)");
    }
    let header = Header {
        rows: Some((before + 1, before + shown as u64, Some(total))),
        roots: contributing(&roots, &folder.path),
        next,
    };
    let text = header.render(ctx.health) + &page.text;
    Ok(Some((vec![ContentBlock::text(text)], None)))
}

pub(crate) async fn search_files<S: Storage>(
    ctx: &Ctx<'_, S>,
    query: &str,
    root: &CheckedPath,
    args: &Args<'_>,
    limit: usize,
) -> Result<Option<(Vec<ContentBlock>, Option<Value>)>, String> {
    let (roots, ready) = roots_for(ctx, root.as_str()).await?;
    if !ready {
        return Ok(None);
    }
    let kind = match args.string("kind")? {
        None => None,
        Some(k) => Some(parse_kind(k, true)?),
    };
    let under = if root.is_root() {
        "/".to_owned()
    } else {
        indexed_folder(ctx, root).await?.path
    };
    let filter = Filter {
        name: Some(query.to_owned()),
        kind,
        what: What::Any,
        skip_search_off: true,
        ..Filter::under(&under)
    };
    let (before, index_cursor) = unwrap_cursor(args.string("cursor")?)?;
    let f = filter.clone();
    let (rows, (total, _)) = ix(ctx, move |i| {
        Ok((
            i.find(&f, Sort::Path, index_cursor.as_deref(), limit + 1)?,
            i.count(&f)?,
        ))
    })
    .await?;
    let mut page = Page::new();
    let mut shown = 0usize;
    for e in rows.iter().take(limit) {
        if !page.push(&path_line(e), Value::Null) {
            break;
        }
        shown += 1;
    }
    let more = shown < rows.len();
    let mut notes = Vec::new();
    if total == 0 {
        notes.push(format!(
            "No files or folders under {under} have names containing \"{query}\"."
        ));
    }
    let folded = fold(&under);
    let skipped: Vec<&str> = ctx
        .policy
        .search_skip()
        .paths()
        .iter()
        .filter(|p| is_under(&fold(p), &folded) || folded == "/")
        .map(String::as_str)
        .collect();
    if !skipped.is_empty() {
        notes.push(format!(
            "Skipped {}: search is turned off there. Browse with list_folder instead.",
            skipped.join(", ")
        ));
    }
    let missing: Vec<&str> = contributing(&roots, &under)
        .iter()
        .filter(|r| r.as_of.is_none())
        .map(|r| r.path.as_str())
        .collect();
    if !missing.is_empty() {
        notes.push(format!(
            "Not indexed yet, so not searched: {}.",
            missing.join(", ")
        ));
    }
    let header = Header {
        rows: Some((before + 1, before + shown as u64, Some(total))),
        roots: contributing(&roots, &under),
        next: more.then(|| wrap_cursor(before + shown as u64, &rows[shown - 1].cursor)),
    };
    let mut text = header.render(ctx.health);
    if total > 0 {
        let _ = writeln!(text, "Names containing \"{query}\" under {under}:");
    }
    text.push_str(&page.text);
    for note in notes {
        text.push_str(&note);
        text.push('\n');
    }
    Ok(Some((vec![ContentBlock::text(text)], None)))
}

/// Index lines for `get_file_info`: folder totals, kind, cached hashes. Also caches hashes
/// the live stat just fetched.
pub(crate) async fn add_index_info<S: Storage>(ctx: &Ctx<'_, S>, live: &Entry, text: &mut String) {
    let path = live.path.clone();
    let Ok(roots) = ix(ctx, |i| i.roots()).await else {
        return;
    };
    let Ok(Some(indexed)) = ix(ctx, move |i| i.entry(&path)).await else {
        if covering(&roots, &live.path).is_some_and(|r| r.as_of.is_some()) {
            text.push_str("Index: not in the index yet (new since the last walk?).\n");
        }
        return;
    };
    let when = as_of(&contributing(&roots, &indexed.path));
    if indexed.is_dir {
        let _ = writeln!(text, "Contains: {} (index, {when})", totals_text(&indexed));
    } else {
        let _ = writeln!(text, "Kind: {}", indexed.kind);
        let live_mtime = live.modified.as_deref().and_then(time::parse_rfc3339);
        if live.size != indexed.size || (live_mtime.is_some() && live_mtime != indexed.mtime) {
            let _ = writeln!(text, "Index: changed since the last walk ({when}).");
        }
        if live.hashes.is_empty() {
            for (kind, value) in &indexed.hashes {
                let _ = writeln!(text, "{} (cached): {value}", kind.name().to_uppercase());
            }
        } else if live.size == indexed.size && live_mtime == indexed.mtime {
            let hashes = live.hashes.clone();
            let _ = ix(ctx, move |i| i.store_hashes(&indexed, &hashes)).await;
        }
    }
}

/// `get_file_info` when the live stat failed: what the index knows, marked as such.
pub(crate) async fn file_info_from_index<S: Storage>(
    ctx: &Ctx<'_, S>,
    path: &CheckedPath,
    failure: &str,
) -> Option<String> {
    ctx.index?;
    let wanted = path.as_str().to_owned();
    let roots = ix(ctx, |i| i.roots()).await.ok()?;
    let e = ix(ctx, move |i| i.entry(&wanted)).await.ok()??;
    let mut text = format!(
        "The live lookup failed ({}); this is from the index, {}.\nPath: {}\nName: {}\n",
        failure.trim_end_matches('.'),
        as_of(&contributing(&roots, &e.path)),
        e.path,
        e.name
    );
    if e.is_dir {
        let _ = writeln!(text, "Type: folder\nContains: {}", totals_text(&e));
    } else {
        let size = e.size.unwrap_or(0);
        let size = if size < 1024 {
            human_size(size)
        } else {
            format!("{size} bytes ({})", human_size(size))
        };
        let _ = writeln!(
            text,
            "Type: file\nSize: {size}\nModified: {}\nKind: {}",
            modified(e.mtime),
            e.kind
        );
        for (kind, value) in &e.hashes {
            let _ = writeln!(text, "{} (cached): {value}", kind.name().to_uppercase());
        }
    }
    Some(text)
}

fn parse_kind(text: &str, folders_ok: bool) -> Result<String, String> {
    let kind = text.trim().to_ascii_lowercase();
    if KINDS.contains(&kind.as_str()) && (folders_ok || kind != "folder") {
        Ok(kind)
    } else {
        Err(format!(
            "kind must be one of {}.",
            KINDS
                .iter()
                .filter(|k| folders_ok || **k != "folder")
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

/// Bytes from a number or text such as `1.5 GB`, `500MiB`, `2g`.
pub(crate) fn parse_size(value: &Value, key: &str) -> Result<Option<u64>, String> {
    let bad =
        || format!("{key} must be a size in bytes or with a unit, such as 1048576 or \"1.5 GB\".");
    let text = match value {
        Value::Null => return Ok(None),
        Value::Number(n) => {
            return n
                .as_u64()
                .or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
                .map(Some)
                .ok_or_else(bad);
        }
        Value::String(s) => s.trim().to_ascii_lowercase(),
        _ => return Err(bad()),
    };
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number.parse().map_err(|_| bad())?;
    let scale: f64 = match unit.trim() {
        "" | "b" | "bytes" | "byte" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "k" | "kib" => 1024.0,
        "m" | "mib" => 1024.0 * 1024.0,
        "g" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tib" => 1024.0f64.powi(4),
        _ => return Err(bad()),
    };
    Ok(Some((number * scale).round() as u64))
}

/// A time bound: a date, date-time, or an age counted back from now.
fn parse_when(text: &str, key: &str) -> Result<i64, String> {
    time::parse_date(text)
        .or_else(|| time::parse_age(text).map(|age| time::now() - age))
        .ok_or_else(|| {
            format!(
                "{key} must be a date such as 2024-01-31, a date-time, or an age such as \"30d\"."
            )
        })
}

fn limit_arg(args: &Args<'_>, default: usize, max: usize) -> Result<usize, String> {
    match args.uint("limit")? {
        None => Ok(default),
        Some(0) => Err("limit must be at least 1.".into()),
        Some(n) => Ok(usize::try_from(n).unwrap_or(max).min(max)),
    }
}

/// The checked folder to work under, as the index spells it, plus the roots, or the reason
/// the index can't answer.
async fn audit_target<S: Storage>(
    ctx: &Ctx<'_, S>,
    args: &Args<'_>,
) -> Result<(String, Vec<RootStatus>), String> {
    let path = ctx.check(args.string("path")?.unwrap_or("/"))?;
    let (roots, ready) = roots_for(ctx, path.as_str()).await?;
    if !ready {
        return Err(not_ready(&roots, path.as_str()));
    }
    let under = if path.is_root() {
        "/".to_owned()
    } else {
        indexed_folder(ctx, &path).await?.path
    };
    Ok((under, roots))
}

fn done(text: String, structured: Value) -> Structured {
    Ok((vec![ContentBlock::text(text)], Some(structured)))
}

async fn folder_stats<S: Storage>(ctx: &Ctx<'_, S>, args: &Args<'_>) -> Structured {
    let (under, roots) = audit_target(ctx, args).await?;
    let depth = match args.uint("depth")? {
        None => 1,
        Some(d @ 1..=3) => d as u32,
        Some(_) => return Err("depth must be 1, 2 or 3.".into()),
    };
    let sort = match args.string("sort")? {
        None => Sort::Largest,
        Some(s) => parse_sort(
            s,
            &[
                Sort::Largest,
                Sort::Smallest,
                Sort::Files,
                Sort::Newest,
                Sort::Oldest,
                Sort::Name,
                Sort::Path,
            ],
        )?,
    };
    let limit = limit_arg(args, 100, 1000)?;
    let (before, index_cursor) = unwrap_cursor(args.string("cursor")?)?;
    let u = under.clone();
    let (top, rows, loose) = ix(ctx, move |i| {
        let top = i.entry(&u)?.unwrap_or_default();
        let rows = i.folders(&u, depth, sort, index_cursor.as_deref(), limit + 1)?;
        let loose = i.count(&Filter {
            what: What::Files,
            ..Filter::under(&u)
        });
        let direct = i.child_counts(&u)?;
        Ok((top, rows, (loose?, direct)))
    })
    .await?;
    let (_, (_, direct_files)) = loose;
    let mut page = Page::new();
    for e in rows.iter().take(limit) {
        let indent = "  ".repeat(depth_below(&under, &e.path).saturating_sub(1));
        let line = format!("{indent}- {}/ {}", e.path, totals_text(e));
        if !page.push(&line, entry_json(e)) {
            break;
        }
    }
    let shown = page.rows.len();
    let more = shown < rows.len();
    let next = more.then(|| wrap_cursor(before + shown as u64, &rows[shown - 1].cursor));
    let header = Header {
        rows: Some((before + 1, before + shown as u64, None)),
        roots: contributing(&roots, &under),
        next: next.clone(),
    };
    let mut text = header.render(ctx.health);
    let _ = writeln!(
        text,
        "{} holds {}; {} directly inside it. Subfolders {} level{} down, {}:",
        under,
        totals_text(&top),
        count_u64(direct_files, "file", "files"),
        depth,
        if depth == 1 { "" } else { "s" },
        match sort {
            Sort::Largest => "largest first",
            Sort::Smallest => "smallest first",
            Sort::Files => "most files first",
            Sort::Newest => "most recently changed first",
            Sort::Oldest => "least recently changed first",
            _ => "by path",
        }
    );
    text.push_str(&page.text);
    if shown == 0 && before == 0 {
        text.push_str("(no subfolders)\n");
    }
    let structured = json!({
        "path": under,
        "totals": entry_json(&top),
        "folders": page.rows,
        "next_cursor": next,
    });
    done(text, structured)
}

fn depth_below(under: &str, path: &str) -> usize {
    let base = if under == "/" {
        0
    } else {
        under.matches('/').count()
    };
    path.matches('/').count().saturating_sub(base)
}

async fn query_files<S: Storage>(ctx: &Ctx<'_, S>, args: &Args<'_>) -> Structured {
    let (under, roots) = audit_target(ctx, args).await?;
    let what = match args.string("type")? {
        None | Some("files") => What::Files,
        Some("folders") => What::Folders,
        Some("any") => What::Any,
        Some(_) => return Err("type must be files, folders or any.".into()),
    };
    let ext = match args.0.get("ext") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(s)) => s
            .split([',', ' '])
            .filter(|e| !e.is_empty())
            .map(|e| e.trim_start_matches('.').to_lowercase())
            .collect(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(|e| e.trim().trim_start_matches('.').to_lowercase())
            })
            .collect::<Option<Vec<_>>>()
            .ok_or("ext must be a string or a list of strings.")?,
        Some(_) => return Err("ext must be a string or a list of strings.".into()),
    };
    let kind = match args.string("kind")? {
        None => None,
        Some(k) => Some(parse_kind(k, false)?),
    };
    let after = match args.string("modified_after")? {
        None => None,
        Some(t) => Some(parse_when(t, "modified_after")?),
    };
    let before_t = match args.string("modified_before")? {
        None => None,
        Some(t) => Some(parse_when(t, "modified_before")?),
    };
    let filter = Filter {
        kind,
        ext,
        min_size: parse_size(args.0.get("min_size").unwrap_or(&Value::Null), "min_size")?,
        max_size: parse_size(args.0.get("max_size").unwrap_or(&Value::Null), "max_size")?,
        after,
        before: before_t,
        what,
        ..Filter::under(&under)
    };
    let sort = match args.string("sort")? {
        None => Sort::Largest,
        Some(s) => parse_sort(
            s,
            &[
                Sort::Largest,
                Sort::Smallest,
                Sort::Newest,
                Sort::Oldest,
                Sort::Name,
                Sort::Path,
            ],
        )?,
    };
    let limit = limit_arg(args, 100, 1000)?;
    let (before, index_cursor) = unwrap_cursor(args.string("cursor")?)?;
    let f = filter.clone();
    let (rows, (total, bytes)) = ix(ctx, move |i| {
        Ok((
            i.find(&f, sort, index_cursor.as_deref(), limit + 1)?,
            i.count(&f)?,
        ))
    })
    .await?;
    let mut page = Page::new();
    for e in rows.iter().take(limit) {
        if !page.push(&path_line(e), entry_json(e)) {
            break;
        }
    }
    let shown = page.rows.len();
    let more = shown < rows.len();
    let next = more.then(|| wrap_cursor(before + shown as u64, &rows[shown - 1].cursor));
    let header = Header {
        rows: Some((before + 1, before + shown as u64, Some(total))),
        roots: contributing(&roots, &under),
        next: next.clone(),
    };
    let mut text = header.render(ctx.health);
    let noun = match what {
        What::Files => ("file", "files"),
        What::Folders => ("folder", "folders"),
        What::Any => ("entry", "entries"),
    };
    let in_all = if what == What::Folders {
        String::new()
    } else {
        format!(" ({} in all)", human_size(bytes))
    };
    let _ = writeln!(
        text,
        "{} under {under} match{in_all}; sorted {}:",
        count_u64(total, noun.0, noun.1),
        sort.name()
    );
    text.push_str(&page.text);
    let structured = json!({
        "path": under,
        "total": total,
        "total_bytes": bytes,
        "rows": page.rows,
        "next_cursor": next,
    });
    done(text, structured)
}

/// The hash to compare files by: SHA-1 where known, else MD5, else SHA-256.
fn hash_key(hashes: &[(HashKind, String)]) -> Option<String> {
    [HashKind::Sha1, HashKind::Md5, HashKind::Sha256]
        .iter()
        .find_map(|kind| {
            hashes
                .iter()
                .find(|(k, _)| k == kind)
                .map(|(k, v)| format!("{}:{}", k.name(), v.to_lowercase()))
        })
}

/// One duplicate set's line and row, listing at most `cap` paths (the rest as "… and M more").
fn duplicate_set(size: u64, key: &str, files: &[&IndexedEntry], cap: usize) -> (String, Value) {
    let extra = size * (files.len() as u64 - 1);
    let mut line = format!(
        "- {} copies of {} ({} reclaimable), {}:",
        files.len(),
        human_size(size),
        human_size(extra),
        key.split_once(':').map_or_else(
            || key.to_owned(),
            |(k, v)| format!("{k} {}", &v[..v.len().min(12)])
        )
    );
    let shown = files.len().min(cap);
    for f in &files[..shown] {
        let _ = write!(line, "\n    {}", f.path);
    }
    let hidden = files.len() - shown;
    if hidden > 0 {
        let _ = write!(line, "\n    … and {hidden} more");
    }
    let mut row = json!({
        "size": size,
        "copies": files.len(),
        "reclaimable": extra,
        "hash": key,
        "paths": files[..shown].iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
    });
    if hidden > 0 {
        row["more_paths"] = json!(hidden);
    }
    (line, row)
}

async fn find_duplicates<S: Storage>(ctx: &Ctx<'_, S>, args: &Args<'_>) -> Structured {
    let (under, roots) = audit_target(ctx, args).await?;
    let min_size = parse_size(args.0.get("min_size").unwrap_or(&Value::Null), "min_size")?
        .unwrap_or(1024 * 1024)
        .max(1);
    let (before, index_cursor) = unwrap_cursor(args.string("cursor")?)?;
    let u = under.clone();
    let mut found = ix(ctx, move |i| {
        i.size_groups(&u, min_size, index_cursor.as_deref(), DUPLICATE_PAGE_FILES)
    })
    .await?;

    // Hash what isn't cached, within this call's time.
    let todo: Vec<IndexedEntry> = found
        .groups
        .iter()
        .flat_map(|g| g.files.iter().filter(|f| f.hashes.is_empty()).cloned())
        .collect();
    let mut fetched: HashMap<String, Vec<(HashKind, String)>> = HashMap::new();
    let mut unreachable = None;
    if !todo.is_empty() {
        ctx.gate().await?;
        let budget = tokio::time::Instant::now() + HASH_TIME;
        let stop_at = budget.min(ctx.deadline - std::time::Duration::from_secs(10));
        let storage = ctx.storage;
        let policy = ctx.policy;
        let mut stream = futures_util::stream::iter(todo)
            .map(|entry| async move {
                let result = async {
                    let checked = policy.check(&entry.path)?;
                    storage.stat(&checked, true).await
                }
                .await;
                (entry, result)
            })
            .buffer_unordered(HASH_CONCURRENCY);
        loop {
            let next = tokio::time::timeout_at(stop_at, stream.next()).await;
            let Ok(Some((entry, result))) = next else {
                break;
            };
            match result {
                Ok(live) => {
                    let mtime = live.modified.as_deref().and_then(time::parse_rfc3339);
                    let same =
                        live.size == entry.size && (entry.mtime.is_none() || mtime == entry.mtime);
                    let hashes = if same { live.hashes } else { Vec::new() };
                    let (e, h) = (entry.clone(), hashes.clone());
                    let _ = ix(ctx, move |i| i.store_hashes(&e, &h)).await;
                    fetched.insert(entry.path, hashes);
                }
                Err(crate::storage::StorageError::NotFound(_)) => {
                    let e = entry.clone();
                    let _ = ix(ctx, move |i| i.store_hashes(&e, &[])).await;
                    fetched.insert(entry.path, Vec::new());
                }
                Err(err) => {
                    ctx.health.poke();
                    unreachable = Some(storage_error(err));
                    break;
                }
            }
        }
    }
    for group in &mut found.groups {
        for file in &mut group.files {
            if let Some(hashes) = fetched.get(&file.path) {
                file.hashes = hashes.clone();
            }
        }
    }

    // Groups in order until one still has files waiting for a hash.
    let mut page = Page::new();
    let mut groups_done = 0usize;
    let mut sets = 0u64;
    let mut wasted = 0u64;
    let mut unverified = 0u64;
    let mut stopped_hashing = false;
    for group in &found.groups {
        let waiting = group
            .files
            .iter()
            .filter(|f| f.hashes.is_empty() && !fetched.contains_key(&f.path))
            .count();
        if waiting > 0 {
            stopped_hashing = true;
            break;
        }
        let mut by_hash: HashMap<String, Vec<&IndexedEntry>> = HashMap::new();
        let mut no_hash = 0u64;
        for file in &group.files {
            match hash_key(&file.hashes) {
                Some(key) => by_hash.entry(key).or_default().push(file),
                None => no_hash += 1,
            }
        }
        let mut copies: Vec<(String, Vec<&IndexedEntry>)> =
            by_hash.into_iter().filter(|(_, v)| v.len() > 1).collect();
        copies.sort_by(|a, b| a.1[0].path.cmp(&b.1[0].path));
        let full: Vec<(String, Value)> = copies
            .iter()
            .map(|(key, files)| duplicate_set(group.size, key, files, usize::MAX))
            .collect();
        let cost: usize = full
            .iter()
            .map(|(l, r)| l.len() + r.to_string().len() + 2)
            .sum();
        if page.used + cost > BUDGET && groups_done > 0 {
            // The next page starts with it.
            break;
        }
        // Alone on the page and still too big: show each set with fewer paths ("… and M
        // more"), as many sets as fit, and count only those.
        let lines = if page.used + cost > BUDGET {
            let longest = copies.iter().map(|(_, f)| f.len()).max().unwrap_or(2);
            let mut cap = longest;
            loop {
                cap = (cap / 2).max(2);
                let lines: Vec<(String, Value)> = copies
                    .iter()
                    .map(|(key, files)| duplicate_set(group.size, key, files, cap))
                    .collect();
                let cost: usize = lines
                    .iter()
                    .map(|(l, r)| l.len() + r.to_string().len() + 2)
                    .sum();
                if page.used + cost <= BUDGET || cap == 2 {
                    break lines;
                }
            }
        } else {
            full
        };
        let mut shown_sets = 0usize;
        for ((line, row), (_, files)) in lines.into_iter().zip(&copies) {
            if !page.push(&line, row) {
                break;
            }
            shown_sets += 1;
            sets += 1;
            wasted += group.size * (files.len() as u64 - 1);
        }
        if shown_sets < copies.len() {
            if shown_sets == 0 {
                // Nothing of it shown: don't move the cursor past it.
                break;
            }
            page.push_text(&format!(
                "  (… and {} more duplicate sets of {} that don't fit on this page)",
                copies.len() - shown_sets,
                human_size(group.size)
            ));
        }
        unverified += no_hash;
        groups_done += 1;
    }
    let more = before + (groups_done as u64) < found.total_groups;
    let next = if groups_done == 0 {
        // Nothing finished: the next call carries on hashing from the same place.
        args.string("cursor")?.map(str::to_owned)
    } else {
        more.then(|| {
            wrap_cursor(
                before + groups_done as u64,
                &found.groups[groups_done - 1].cursor,
            )
        })
    };
    let header = Header {
        rows: None,
        roots: contributing(&roots, &under),
        next: next.clone(),
    };
    let mut text = header.render(ctx.health);
    let _ = writeln!(
        text,
        "Under {under}: {} sizes ≥ {} are shared by {} files; {} of them hashed so far.",
        thousands(found.total_groups),
        human_size(min_size),
        thousands(found.total_files),
        thousands((found.hashed_files + fetched.len() as u64).min(found.total_files))
    );
    if found.total_groups == 0 {
        text.push_str("No two files share a size, so there are no duplicates.\n");
    } else if groups_done > 0 {
        let _ = writeln!(
            text,
            "This page: {} size groups, {} duplicate sets, {} reclaimable{}:",
            groups_done,
            sets,
            human_size(wasted),
            if sets == 0 {
                " (same sizes, different contents)"
            } else {
                ""
            }
        );
    }
    text.push_str(&page.text);
    if unverified > 0 {
        let _ = writeln!(
            text,
            "{unverified} files on this page have no hash from the drive (changed since the last walk, or gone), so they aren't compared."
        );
    }
    if let Some(why) = &unreachable {
        let _ = writeln!(text, "Hashing stopped: {why}");
    } else if stopped_hashing {
        let _ = writeln!(
            text,
            "Still hashing (one API call per file); call again{} to continue.",
            if next.is_some() {
                " with the cursor"
            } else {
                ""
            }
        );
    }
    let structured = json!({
        "path": under,
        "min_size": min_size,
        "size_groups": found.total_groups,
        "files_sharing_a_size": found.total_files,
        "sets": page.rows,
        "reclaimable_on_page": wasted,
        "next_cursor": next,
    });
    done(text, structured)
}

async fn recent_changes<S: Storage>(ctx: &Ctx<'_, S>, args: &Args<'_>) -> Structured {
    let path = ctx.check(args.string("path")?.unwrap_or("/"))?;
    let roots = ix(ctx, |i| i.roots()).await?;
    let since = parse_when(args.string("since")?.unwrap_or("7d"), "since")?;
    let source = match args.string("source")? {
        None => None,
        Some(s @ ("walk" | "own-write")) => Some(s.to_owned()),
        Some(_) => return Err("source must be walk or own-write.".into()),
    };
    let limit = limit_arg(args, 200, 1000)?;
    let (before, index_cursor) = unwrap_cursor(args.string("cursor")?)?;
    // The range bounds compare stored paths byte for byte, so use the index's spelling; a
    // folder the index no longer has (removed or moved away) keeps the spelling given.
    let wanted = path.as_str().to_owned();
    let under = ix(ctx, move |i| {
        Ok(i.entry(&wanted)?.map_or(wanted, |e| e.path))
    })
    .await?;
    let u = under.clone();
    let rows = ix(ctx, move |i| {
        i.changes(
            &u,
            since,
            source.as_deref(),
            index_cursor.as_deref(),
            limit + 1,
        )
    })
    .await?;
    let now = time::now();
    let mut page = Page::new();
    for c in rows.iter().take(limit) {
        let what = if c.is_dir { "folder" } else { "file" };
        let size = c
            .size
            .map(|s| format!(", {}", human_size(s)))
            .unwrap_or_default();
        let line = match &c.old_path {
            Some(old) => format!(
                "- {} {} {what} {old} → {}{size}",
                time::short(c.at, now),
                c.op,
                c.path
            ),
            None => format!(
                "- {} {} {what} {}{size}",
                time::short(c.at, now),
                c.op,
                c.path
            ),
        };
        let row = json!({"at": time::rfc3339(c.at), "op": c.op, "path": c.path, "old_path": c.old_path,
                         "type": what, "size": c.size, "source": c.source});
        if !page.push(&line, row) {
            break;
        }
    }
    let shown = page.rows.len();
    let more = shown < rows.len();
    let next = more.then(|| wrap_cursor(before + shown as u64, &rows[shown - 1].cursor));
    let header = Header {
        rows: Some((before + 1, before + shown as u64, None)),
        roots: contributing(&roots, &under),
        next: next.clone(),
    };
    let mut text = header.render(ctx.health);
    let _ = writeln!(
        text,
        "Changes under {under} since {}, newest first:",
        time::rfc3339(since)
    );
    text.push_str(&page.text);
    if shown == 0 && before == 0 {
        text.push_str("(none recorded; the first walk of a root records no changes)\n");
    }
    done(
        text,
        json!({"path": under, "since": time::rfc3339(since), "changes": page.rows, "next_cursor": next}),
    )
}

async fn type_breakdown<S: Storage>(ctx: &Ctx<'_, S>, args: &Args<'_>) -> Structured {
    let (under, roots) = audit_target(ctx, args).await?;
    let u = under.clone();
    let rows = ix(ctx, move |i| i.types(&u)).await?;
    type Ext<'a> = (&'a str, u64, u64);
    let mut kinds: HashMap<&str, (u64, u64, Vec<Ext<'_>>)> = HashMap::new();
    for row in &rows {
        let k = kinds.entry(row.kind.as_str()).or_default();
        k.0 += row.files;
        k.1 += row.bytes;
        k.2.push((row.ext.as_deref().unwrap_or("(none)"), row.files, row.bytes));
    }
    let mut kinds: Vec<_> = kinds.into_iter().collect();
    kinds.sort_by(|a, b| b.1.1.cmp(&a.1.1).then(a.0.cmp(b.0)));
    let (files, bytes) = kinds
        .iter()
        .fold((0, 0), |acc, (_, k)| (acc.0 + k.0, acc.1 + k.1));
    let header = Header {
        rows: None,
        roots: contributing(&roots, &under),
        next: None,
    };
    let mut text = header.render(ctx.health);
    let _ = writeln!(
        text,
        "{} under {under}, {} in all, by kind:",
        count_u64(files, "file", "files"),
        human_size(bytes)
    );
    let mut structured = Vec::new();
    for (kind, (n, size, mut exts)) in kinds {
        exts.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(b.0)));
        let share = if bytes > 0 {
            size as f64 * 100.0 / bytes as f64
        } else {
            0.0
        };
        let top: Vec<String> = exts
            .iter()
            .take(6)
            .map(|(e, n, b)| format!("{e} {} · {}", thousands(*n), human_size(*b)))
            .collect();
        let rest = exts.len().saturating_sub(6);
        let _ = writeln!(
            text,
            "- {kind}: {}, {} ({share:.1}%): {}{}",
            count_u64(n, "file", "files"),
            human_size(size),
            top.join("; "),
            if rest > 0 {
                format!("; {rest} more extensions")
            } else {
                String::new()
            }
        );
        structured.push(json!({
            "kind": kind, "files": n, "bytes": size,
            "extensions": exts.iter().take(20).map(|(e, n, b)| json!({"ext": e, "files": n, "bytes": b})).collect::<Vec<_>>(),
        }));
    }
    done(
        text,
        json!({"path": under, "files": files, "bytes": bytes, "kinds": structured}),
    )
}

async fn index_status<S: Storage>(ctx: &Ctx<'_, S>) -> Structured {
    let status = ix(ctx, |i| i.status()).await?;
    let now = time::now();
    let entries: u64 = status.roots.iter().map(|r| r.entries).sum();
    let mut text = format!(
        "Index: {} entries in {} {}, {} on disk (generation {}).\n",
        thousands(entries),
        status.roots.len(),
        if status.roots.len() == 1 {
            "root"
        } else {
            "roots"
        },
        human_size(status.db_bytes),
        status.generation
    );
    let mut roots = Vec::new();
    for r in &status.roots {
        let every = if r.every_s % 3600 == 0 {
            format!("every {} h", r.every_s / 3600)
        } else {
            format!("every {} min", r.every_s / 60)
        };
        let mut line = format!("- {} ({every}): ", r.path);
        match r.as_of {
            Some(t) => {
                let _ = write!(
                    line,
                    "as of {}, {}",
                    time::short(t, now),
                    count_u64(r.entries, "entry", "entries")
                );
                if let Some(ms) = r.last_ms {
                    let _ = write!(line, ", its listing took {:.1} s", ms as f64 / 1000.0);
                }
            }
            None => line.push_str("not indexed yet"),
        }
        if r.walking {
            line.push_str("; walking now");
        } else if r.queued {
            line.push_str("; a walk is queued");
        } else if let Some(due) = r.next_due {
            let _ = write!(line, "; next walk {}", time::short(due, now));
        }
        line.push('.');
        if let Some(err) = &r.last_error {
            let _ = write!(line, "\n  Last walk failed: {err}");
        }
        if let Some(held) = &r.held {
            let share = if held.entries > 0 {
                held.removals as f64 * 100.0 / held.entries as f64
            } else {
                100.0
            };
            let _ = write!(
                line,
                "\n  Walk of {} at {} would remove {} entries ({share:.1}%); held. If that is right, refresh_index path={} confirm=true applies it.",
                r.path,
                time::short(held.at, now),
                thousands(held.removals),
                r.path
            );
        }
        text.push_str(&line);
        text.push('\n');
        roots.push(json!({
            "path": r.path, "every_s": r.every_s, "as_of": r.as_of.map(time::rfc3339),
            "entries": r.entries, "last_ms": r.last_ms, "last_error": r.last_error,
            "walking": r.walking, "queued": r.queued, "next_due": r.next_due.map(time::rfc3339),
            "held_removals": r.held.as_ref().map(|h| json!({"at": time::rfc3339(h.at), "removals": h.removals, "of": h.entries})),
        }));
    }
    let _ = writeln!(
        text,
        "Duplicate hashing: {} of {} files ≥ {} that share a size are hashed.",
        thousands(status.hashed),
        thousands(status.hash_candidates),
        human_size(status.hash_min_size)
    );
    let health = ctx.health.describe();
    let _ = writeln!(text, "Backend: {}.", health.as_deref().unwrap_or("healthy"));
    text.push_str("Mirror and full-text search: not in this version.\n");
    done(
        text,
        json!({
            "roots": roots, "generation": status.generation, "db_bytes": status.db_bytes,
            "hash_candidates": status.hash_candidates, "hashed": status.hashed,
            "backend": health.unwrap_or_else(|| "healthy".into()),
        }),
    )
}

async fn refresh_index<S: Storage>(ctx: &Ctx<'_, S>, args: &Args<'_>) -> Structured {
    let path = match args.string("path")? {
        None => None,
        Some(p) => Some(ctx.check(p)?.as_str().to_owned()),
    };
    let confirm = args.bool("confirm")?.unwrap_or(false);
    let message = ix(ctx, move |i| i.refresh(path.as_deref(), confirm)).await?;
    Ok((vec![ContentBlock::text(message)], None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_thousands() {
        assert_eq!(parse_size(&json!(5), "s"), Ok(Some(5)));
        assert_eq!(parse_size(&json!("1.5 GB"), "s"), Ok(Some(1_500_000_000)));
        assert_eq!(parse_size(&json!("2MiB"), "s"), Ok(Some(2 * 1024 * 1024)));
        assert_eq!(parse_size(&json!("1g"), "s"), Ok(Some(1 << 30)));
        assert!(parse_size(&json!("lots"), "s").is_err());
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(1234), "1,234");
        assert_eq!(thousands(139_266), "139,266");
        assert_eq!(thousands(1_000_000), "1,000,000");
    }

    #[test]
    fn cursors_wrap() {
        let c = wrap_cursor(500, "abc");
        assert!(is_index_cursor(&c));
        assert_eq!(unwrap_cursor(Some(&c)), Ok((500, Some("abc".into()))));
        assert!(unwrap_cursor(Some("nonsense")).is_err());
        assert!(!is_index_cursor("bzoxMDAw"));
    }
}
