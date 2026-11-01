//! The tools: definitions for `tools/list` and the read tools' implementations over a
//! [`Storage`]; the write tools are in [`crate::writes`], the link tools in `linktools`, the
//! index tools in `audit`.
//!
//! Problems the model can fix (a wrong path, a missing argument, a file that is too big) come
//! back as tool results with `isError: true`, as MCP 2025-11-25 recommends, so the model sees
//! the message and can retry. Only an unknown tool name is a protocol error.
//!
//! Every path from a client goes through [`Policy::check`] (writes: [`Policy::check_write`])
//! before any storage call, and every entry a backend returns goes through
//! [`Policy::visible`] before it is shown.

use std::fmt::Write as _;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use rmcp::model::{CallToolResult, ContentBlock, JsonObject, Tool, ToolAnnotations, object};
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::audit;
use crate::health::Health;
use crate::index::{Index, Sort};
use crate::links::LinkSettings;
use crate::linktools;
use crate::matching::NameMatcher;
use crate::policy::{CheckedPath, PathSet, Policy};
use crate::scope::{Scope, Scopes};
use crate::server::ServerConfig;
use crate::state::StateStore;
use crate::storage::{ByteRange, Entry, Storage, StorageError, StorageWrite, read_capped};
use crate::writes;

const LIST_FOLDER: &str = "list_folder";
const GET_FILE_INFO: &str = "get_file_info";
const READ_TEXT_FILE: &str = "read_text_file";
const READ_IMAGE: &str = "read_image";
const SEARCH_FILES: &str = "search_files";
pub(crate) const FOLDER_STATS: &str = "folder_stats";
pub(crate) const QUERY_FILES: &str = "query_files";
pub(crate) const FIND_DUPLICATES: &str = "find_duplicates";
pub(crate) const RECENT_CHANGES: &str = "recent_changes";
pub(crate) const TYPE_BREAKDOWN: &str = "type_breakdown";
pub(crate) const INDEX_STATUS: &str = "index_status";
pub(crate) const REFRESH_INDEX: &str = "refresh_index";
pub(crate) const JOB_STATUS: &str = "job_status";
pub(crate) const CREATE_FOLDER: &str = "create_folder";
pub(crate) const MOVE: &str = "move";
pub(crate) const COPY: &str = "copy";
pub(crate) const WRITE_FILE: &str = "write_file";
pub(crate) const DELETE: &str = "delete";
pub(crate) const CREATE_UPLOAD_LINK: &str = "create_upload_link";
pub(crate) const CREATE_DOWNLOAD_LINK: &str = "create_download_link";
pub(crate) const LIST_LINKS: &str = "list_links";
pub(crate) const REVOKE_LINK: &str = "revoke_link";

/// What a tool needs besides its scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Needs {
    Nothing,
    Index,
    /// `state.db` (jobs).
    State,
    /// `state.db` and the server's `[writes] tools` naming it.
    Write,
    /// `state.db`, `LINK_KEY` and `[links]`.
    Links,
    /// Links, on a server that allows writes at all.
    UploadLinks,
}

/// Every tool, the scope it needs and what else, in `tools/list` order (2026-07-28 asks for a
/// deterministic list so clients can cache it).
const TOOLS: &[(&str, Scope, Needs)] = &[
    (LIST_FOLDER, Scope::Read, Needs::Nothing),
    (GET_FILE_INFO, Scope::Read, Needs::Nothing),
    (READ_TEXT_FILE, Scope::Read, Needs::Nothing),
    (READ_IMAGE, Scope::Read, Needs::Nothing),
    (SEARCH_FILES, Scope::Read, Needs::Nothing),
    (FOLDER_STATS, Scope::Read, Needs::Index),
    (QUERY_FILES, Scope::Read, Needs::Index),
    (FIND_DUPLICATES, Scope::Read, Needs::Index),
    (RECENT_CHANGES, Scope::Read, Needs::Index),
    (TYPE_BREAKDOWN, Scope::Read, Needs::Index),
    (INDEX_STATUS, Scope::Read, Needs::Index),
    (REFRESH_INDEX, Scope::Read, Needs::Index),
    (JOB_STATUS, Scope::Read, Needs::State),
    (CREATE_FOLDER, Scope::Write, Needs::Write),
    (MOVE, Scope::Write, Needs::Write),
    (COPY, Scope::Write, Needs::Write),
    (WRITE_FILE, Scope::Write, Needs::Write),
    (DELETE, Scope::Write, Needs::Write),
    (CREATE_UPLOAD_LINK, Scope::Write, Needs::UploadLinks),
    (CREATE_DOWNLOAD_LINK, Scope::Read, Needs::Links),
    (LIST_LINKS, Scope::Read, Needs::Links),
    (REVOKE_LINK, Scope::Read, Needs::Links),
];

/// Which optional parts the server has.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Parts {
    pub(crate) index: bool,
    pub(crate) state: bool,
    pub(crate) links: bool,
}

/// Whether the server offers `name` at all (whatever the client's scopes).
fn offered(config: &ServerConfig, parts: Parts, name: &str, needs: Needs) -> bool {
    match needs {
        Needs::Nothing => true,
        Needs::Index => parts.index,
        Needs::State => parts.state,
        Needs::Write => parts.state && config.write_tools.iter().any(|t| t == name),
        Needs::Links => parts.state && parts.links,
        Needs::UploadLinks => parts.state && parts.links && !config.write_tools.is_empty(),
    }
}

/// Write and link tools, whose calls go into the audit log.
pub(crate) fn audited(name: &str) -> bool {
    matches!(
        TOOLS.iter().find(|(tool, _, _)| *tool == name),
        Some((_, _, Needs::Write | Needs::UploadLinks))
    ) || matches!(name, CREATE_DOWNLOAD_LINK | REVOKE_LINK)
}

/// Most entries one `list_folder` page shows.
const MAX_LIST_ENTRIES: usize = 1000;
/// Most characters of entry lines in one result. Claude truncates tool results at about
/// 150,000 characters; a folder of long paths could pass that before 1000 entries.
const MAX_LISTING_CHARS: usize = 120_000;
/// How much of a file is checked for NUL bytes before it is treated as binary (git does the same).
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

const PNG: &str = "image/png";
const JPEG: &str = "image/jpeg";
const GIF: &str = "image/gif";
const WEBP: &str = "image/webp";

/// What a tool returns: its output, or a message for the model.
type ToolOutput = Result<Vec<ContentBlock>, String>;
/// Output with optional `structuredContent`, for the audit tools.
pub(crate) type Structured = Result<(Vec<ContentBlock>, Option<Value>), String>;

/// The outcome of `tools/call`.
pub(crate) enum Called {
    Done(CallToolResult),
    Unknown,
}

/// Everything a tool call needs, and when it has to be done by.
pub(crate) struct Ctx<'a, S> {
    pub(crate) storage: &'a S,
    pub(crate) config: &'a ServerConfig,
    pub(crate) policy: &'a Policy,
    pub(crate) index: Option<&'a Arc<dyn Index>>,
    pub(crate) health: &'a Health,
    pub(crate) state: Option<&'a Arc<dyn StateStore>>,
    pub(crate) links: Option<&'a LinkSettings>,
    /// Where the auth Worker is reached, from its trusted `X-CoF-Public-Url` header.
    pub(crate) public_url: Option<&'a str>,
    pub(crate) deadline: Instant,
}

/// A storage call that didn't finish before the call's deadline.
pub(crate) struct Late;

/// How long a live call waits for a re-check of a backend marked unhealthy.
const RECHECK_TIMEOUT: Duration = Duration::from_secs(5);

impl<'a, S: Storage> Ctx<'a, S> {
    pub(crate) fn new(
        storage: &'a S,
        config: &'a ServerConfig,
        policy: &'a Policy,
        index: Option<&'a Arc<dyn Index>>,
        health: &'a Health,
    ) -> Self {
        Self {
            storage,
            config,
            policy,
            index,
            health,
            state: None,
            links: None,
            public_url: None,
            deadline: Instant::now() + config.deadline,
        }
    }

    /// `work`, unless the deadline passes first.
    pub(crate) async fn within<T>(&self, work: impl Future<Output = T>) -> Result<T, Late> {
        tokio::time::timeout_at(self.deadline, work)
            .await
            .map_err(|_| Late)
    }

    /// Lets a live call through unless the backend is known to be failing: then it checks
    /// once more (a cheap call), so a recovered backend works at once and a dead one fails
    /// fast instead of using up the call's deadline.
    pub(crate) async fn gate(&self) -> Result<(), String> {
        if self.health.is_healthy() {
            return Ok(());
        }
        match tokio::time::timeout(RECHECK_TIMEOUT, self.storage.health()).await {
            Ok(Ok(())) => {
                self.health.set_ok();
                Ok(())
            }
            Ok(Err(err)) => {
                self.health.set_failed(&redact_urls(&err.to_string()));
                Err(self.unhealthy_message())
            }
            Err(_) => {
                self.health.set_failed("the health check timed out");
                Err(self.unhealthy_message())
            }
        }
    }

    fn unhealthy_message(&self) -> String {
        let what = self
            .health
            .describe()
            .unwrap_or_else(|| "backend unhealthy".into());
        let rest = if self.index.is_some() {
            " Tools that answer from the index (list_folder, search_files, query_files, \
             folder_stats and the rest) still work, as of their last walk."
        } else {
            " Try again later."
        };
        format!("The file server can't be reached right now: {what}.{rest}")
    }

    /// `work` for a tool that can't return a partial answer: past the deadline it fails.
    pub(crate) async fn timed<T>(
        &self,
        work: impl Future<Output = Result<T, StorageError>>,
    ) -> Result<T, String> {
        self.gate().await?;
        match self.within(work).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => {
                if matches!(err, StorageError::Backend(_) | StorageError::Unhealthy(_)) {
                    self.health.poke();
                }
                Err(storage_error(err))
            }
            Err(Late) => Err(format!(
                "The storage backend didn't answer within the {} time limit. Try again later.",
                seconds(self.config.deadline)
            )),
        }
    }

    pub(crate) fn check(&self, path: &str) -> Result<CheckedPath, String> {
        self.policy.check(path).map_err(storage_error)
    }

    /// A state database call on the blocking pool.
    pub(crate) async fn st<T: Send + 'static>(
        &self,
        work: impl FnOnce(&dyn StateStore) -> crate::state::StateResult<T> + Send + 'static,
    ) -> Result<T, String> {
        let state = self
            .state
            .ok_or(
                "This server keeps no state (no [server] state_dir), so it has no jobs or links.",
            )?
            .clone();
        tokio::task::spawn_blocking(move || work(&*state))
            .await
            .map_err(|e| format!("state database task failed: {e}"))?
            .map_err(|e| e.to_string())
    }

    /// An index call on the blocking pool; failures are logged by the caller's result only.
    pub(crate) async fn ix_quiet(
        &self,
        work: impl FnOnce(&dyn Index) -> crate::index::IndexResult<()> + Send + 'static,
    ) -> Option<String> {
        let index = self.index?.clone();
        match tokio::task::spawn_blocking(move || work(&*index)).await {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e.to_string()),
            Err(e) => Some(e.to_string()),
        }
    }
}

/// Tool definitions the client's scopes allow, in a fixed order. Limits in the descriptions
/// and schemas come from `config`.
pub(crate) fn definitions(
    config: &ServerConfig,
    scopes: Scopes,
    parts: Parts,
    links: Option<&LinkSettings>,
) -> Vec<Tool> {
    TOOLS
        .iter()
        .filter(|(name, scope, needs)| {
            scopes.contains(*scope) && offered(config, parts, name, *needs)
        })
        .map(|(name, _, _)| {
            if let Some(tool) = writes::definition(config, name) {
                return tool;
            }
            if let Some(tool) = links.and_then(|l| linktools::definition(l, name)) {
                return tool;
            }
            if parts.index {
                audit::definition(config, name).unwrap_or_else(|| definition(config, name))
            } else {
                definition(config, name)
            }
        })
        .collect()
}

/// Whether any write tool is offered to a client with `scopes`.
pub(crate) fn can_write(config: &ServerConfig, scopes: Scopes, parts: Parts) -> bool {
    scopes.contains(Scope::Write)
        && TOOLS
            .iter()
            .any(|(name, _, needs)| *needs == Needs::Write && offered(config, parts, name, *needs))
}

fn definition(config: &ServerConfig, name: &str) -> Tool {
    let max_read = config.max_read_bytes.max(1);
    let search_limit = config.search_limit.max(1);
    match name {
        LIST_FOLDER => tool(
            LIST_FOLDER,
            "List folder",
            &format!(
                "List the files and folders directly inside a folder of the pCloud drive. Folders \
                 come first, then files, each sorted by name, with type, size, last-modified time \
                 and full path. Paths start at the drive's root \"/\". Shows up to \
                 {MAX_LIST_ENTRIES} entries per call; a longer folder ends with a cursor to pass \
                 back for the next page."
            ),
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Folder to list, e.g. \"/\" or \"/Notes/2026\".",
                        "default": "/"
                    },
                    "sort": {
                        "type": "string",
                        "enum": ["name", "largest", "smallest", "newest", "oldest"],
                        "default": "name",
                        "description": "Order of the entries."
                    },
                    "cursor": {
                        "type": "string",
                        "description": "From the previous page of the same folder, to continue it."
                    }
                },
                "additionalProperties": false
            }),
        ),
        GET_FILE_INFO => tool(
            GET_FILE_INFO,
            "Get file info",
            "Get the details of one file or folder: type, exact size in bytes, last-modified \
             time and content type.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File or folder, e.g. \"/Notes/todo.md\"."
                    },
                    "hash": {
                        "type": "boolean",
                        "default": false,
                        "description": "Also fetch the file's content hashes (SHA-1, MD5); slower."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        READ_TEXT_FILE => tool(
            READ_TEXT_FILE,
            "Read text file",
            &format!(
                "Read a text file (notes, Markdown, code, CSV, JSON, logs and so on) as UTF-8. \
                 Returns at most {max_read} bytes per call. When that is not the whole file, the \
                 text starts with a line such as \"[bytes 0-99999 of 523456; continue with \
                 offset=100000]\"; call again with that offset to read on. Binary files are \
                 refused: use read_image for pictures."
            ),
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File to read, e.g. \"/Notes/todo.md\"."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "default": 0,
                        "description": "Byte to start at, from the previous call's header line."
                    },
                    "max_bytes": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": max_read,
                        "default": max_read,
                        "description": "Most bytes to return."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        READ_IMAGE => tool(
            READ_IMAGE,
            "Read image",
            &format!(
                "Look at an image file: PNG, JPEG, GIF or WebP, up to {}. Returns the picture \
                 itself.",
                human_size(config.max_image_bytes)
            ),
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Image file, e.g. \"/Photos/receipt.jpg\"."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        SEARCH_FILES => tool(
            SEARCH_FILES,
            "Search files",
            &format!(
                "Find files and folders whose name contains the query, ignoring case and accents, \
                 anywhere under a folder. Matches names only, not file contents. Returns up to \
                 {search_limit} results with their full paths. Skips folders the owner turned \
                 off for search, and says which."
            ),
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Part of a file or folder name, e.g. \"invoice\" or \".pdf\"."
                    },
                    "path": {
                        "type": "string",
                        "description": "Folder to search under.",
                        "default": "/"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": search_limit,
                        "default": search_limit,
                        "description": "Most results to return."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        ),
        _ => unreachable!("every name in TOOLS has a definition"),
    }
}

pub(crate) fn tool(name: &'static str, title: &str, description: &str, schema: Value) -> Tool {
    // The title is repeated in the annotations for 2025-03-26 clients, which predate
    // Tool.title.
    let annotations = ToolAnnotations::with_title(title)
        .read_only(true)
        .destructive(false)
        .idempotent(true)
        .open_world(false);
    annotated(name, title, description, schema, annotations)
}

/// A tool that changes something or lets data leave: `destructiveHint` makes Claude ask the
/// owner before every call.
pub(crate) fn destructive_tool(
    name: &'static str,
    title: &str,
    description: &str,
    schema: Value,
    idempotent: bool,
    open_world: bool,
) -> Tool {
    let annotations = ToolAnnotations::with_title(title)
        .read_only(false)
        .destructive(true)
        .idempotent(idempotent)
        .open_world(open_world);
    annotated(name, title, description, schema, annotations)
}

fn annotated(
    name: &'static str,
    title: &str,
    description: &str,
    schema: Value,
    annotations: ToolAnnotations,
) -> Tool {
    let schema: Arc<JsonObject> = Arc::new(object(schema));
    Tool::new(name, description.to_owned(), schema)
        .with_title(title)
        .with_annotations(annotations)
}

/// Runs a tool, if the server offers it and the client's scopes allow it.
pub(crate) async fn call<S: StorageWrite>(
    ctx: &Ctx<'_, S>,
    name: &str,
    args: &Map<String, Value>,
    scopes: Scopes,
) -> Called {
    let Some(&(_, scope, needs)) = TOOLS.iter().find(|(tool, _, _)| *tool == name) else {
        return Called::Unknown;
    };
    let parts = Parts {
        index: ctx.index.is_some(),
        state: ctx.state.is_some(),
        links: ctx.links.is_some(),
    };
    if !offered(ctx.config, parts, name, needs) {
        return Called::Unknown;
    }
    if !scopes.contains(scope) {
        return Called::Done(CallToolResult::error(vec![ContentBlock::text(format!(
            "{name} needs the {scope} permission, which this connection wasn't granted. The \
             owner can grant it by reconnecting the connector."
        ))]));
    }
    let args = Args(args);
    let plain = |out: ToolOutput| out.map(|content| (content, None));
    let output = match name {
        LIST_FOLDER => list_folder(ctx, &args).await,
        GET_FILE_INFO => plain(get_file_info(ctx, &args).await),
        READ_TEXT_FILE => plain(read_text_file(ctx, &args).await),
        READ_IMAGE => plain(read_image(ctx, &args).await),
        SEARCH_FILES => search_files(ctx, &args).await,
        name => match writes::call(ctx, name, &args).await {
            Some(output) => output,
            None => match linktools::call(ctx, name, &args).await {
                Some(output) => output,
                None => match audit::call(ctx, name, &args).await {
                    Some(output) => output,
                    None => return Called::Unknown,
                },
            },
        },
    };
    if audited(name) && ctx.state.is_some() {
        let result = match &output {
            Ok((content, _)) => format!("ok: {}", first_line(content)),
            Err(message) => format!("error: {message}"),
        };
        let shown = writes::audit_args(args.0);
        let (tool, at) = (name.to_owned(), crate::time::now());
        let _ = ctx
            .st(move |state| state.audit(at, &tool, &shown, &result))
            .await;
    }
    Called::Done(match output {
        Ok((content, structured)) => {
            let mut result = CallToolResult::success(content);
            result.structured_content = structured;
            result
        }
        Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
    })
}

/// The first line of a result's text, for the audit log.
fn first_line(content: &[ContentBlock]) -> String {
    content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
        .unwrap_or_default()
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(300)
        .collect()
}

/// Folders first, then files, each by name ignoring case.
fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by_cached_key(|e| (!e.is_dir, e.name.to_lowercase(), e.name.clone()));
}

async fn list_folder<S: Storage>(ctx: &Ctx<'_, S>, args: &Args<'_>) -> Structured {
    let path = ctx.check(args.string("path")?.unwrap_or("/"))?;
    let sort = match args.string("sort")? {
        None => Sort::Name,
        Some(s) => audit::parse_sort(
            s,
            &[
                Sort::Name,
                Sort::Largest,
                Sort::Smallest,
                Sort::Newest,
                Sort::Oldest,
            ],
        )?,
    };
    if ctx.index.is_some()
        && !args.bool("live")?.unwrap_or(false)
        && let Some(done) = audit::list_folder(ctx, &path, sort, args.string("cursor")?).await?
    {
        return Ok(done);
    }
    let cursor = args.string("cursor")?;
    if cursor.is_some_and(audit::is_index_cursor) {
        return Err(
            "That cursor came from an index listing; repeat the call without live=true, \
                    or start the live listing again without the cursor."
                .into(),
        );
    }
    list_folder_live(ctx, &path, sort, cursor)
        .await
        .map(|c| (c, None))
}

async fn list_folder_live<S: Storage>(
    ctx: &Ctx<'_, S>,
    path: &CheckedPath,
    sort: Sort,
    cursor: Option<&str>,
) -> ToolOutput {
    let path = path.clone();
    let offset = match cursor {
        None | Some("") => 0,
        Some(cursor) => decode_cursor(cursor).ok_or(
            "cursor is not one list_folder gave out; leave it out to start from the first page.",
        )?,
    };
    let mut entries = ctx.timed(ctx.storage.list(&path)).await?;
    entries.retain(|e| ctx.policy.visible(&e.path));
    sort_entries(&mut entries);
    match sort {
        Sort::Largest => entries.sort_by_key(|e| std::cmp::Reverse(e.size.unwrap_or(0))),
        Sort::Smallest => entries.sort_by_key(|e| e.size.unwrap_or(0)),
        Sort::Newest => entries.sort_by(|a, b| b.modified.cmp(&a.modified)),
        Sort::Oldest => entries.sort_by(|a, b| a.modified.cmp(&b.modified)),
        _ => {}
    }

    let total = entries.len();
    if total == 0 {
        return Ok(vec![ContentBlock::text(format!("Folder {path} is empty."))]);
    }
    if offset >= total {
        return Err(format!(
            "The cursor is past the end of {path}, which has {total} entries now."
        ));
    }
    let folders = entries.iter().filter(|e| e.is_dir).count();
    let mut text = format!(
        "Folder {path}: {}, {}.\n",
        count(folders, "folder", "folders"),
        count(total - folders, "file", "files")
    );
    let shown = push_entry_lines(
        &mut text,
        entries.iter().skip(offset).take(MAX_LIST_ENTRIES),
    );
    let end = offset + shown;
    if end < total {
        let range = if offset == 0 {
            format!("the first {shown}")
        } else {
            format!("entries {}-{end}", offset + 1)
        };
        let _ = writeln!(
            text,
            "Showing {range} of {total} entries. For the next page call list_folder again with \
             cursor=\"{}\", or use search_files to find a specific name.",
            encode_cursor(end)
        );
    } else if offset > 0 {
        let _ = writeln!(text, "Showing entries {}-{end} of {total}.", offset + 1);
    }
    Ok(vec![ContentBlock::text(text)])
}

/// Cursors are opaque to the model, so the format can change; today it's the next offset.
pub(crate) fn encode_cursor(offset: usize) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("o:{offset}"))
}

pub(crate) fn decode_cursor(cursor: &str) -> Option<usize> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor.trim())
        .ok()?;
    std::str::from_utf8(&raw)
        .ok()?
        .strip_prefix("o:")?
        .parse()
        .ok()
}

async fn get_file_info<S: Storage>(ctx: &Ctx<'_, S>, args: &Args<'_>) -> ToolOutput {
    let path = ctx.check(args.required_path("path")?)?;
    let with_hash = args.bool("hash")?.unwrap_or(false);
    let entry = match ctx.timed(ctx.storage.stat(&path, with_hash)).await {
        Ok(entry) => entry,
        // The index may still know it while the backend is down (not when it is just gone).
        Err(message) if backend_failed(&message) => {
            let why = ctx.health.describe().unwrap_or_else(|| message.clone());
            return match audit::file_info_from_index(ctx, &path, &why).await {
                Some(text) => Ok(vec![ContentBlock::text(text)]),
                None => Err(message),
            };
        }
        Err(message) => return Err(message),
    };
    let mut text = format!("Path: {}\n", entry.path);
    if !entry.name.is_empty() {
        text.push_str(&format!("Name: {}\n", entry.name));
    }
    text.push_str(if entry.is_dir {
        "Type: folder\n"
    } else {
        "Type: file\n"
    });
    if let Some(size) = entry.size {
        if size < 1024 {
            text.push_str(&format!("Size: {}\n", human_size(size)));
        } else {
            text.push_str(&format!("Size: {size} bytes ({})\n", human_size(size)));
        }
    }
    if let Some(modified) = &entry.modified {
        text.push_str(&format!("Modified: {modified}\n"));
    }
    if let Some(content_type) = &entry.content_type {
        text.push_str(&format!("Content type: {content_type}\n"));
    }
    for (kind, value) in &entry.hashes {
        text.push_str(&format!("{}: {value}\n", kind.name().to_uppercase()));
    }
    if with_hash && entry.hashes.is_empty() && !entry.is_dir {
        text.push_str("Hashes: the storage backend reported none for this file.\n");
    }
    if ctx.index.is_some() {
        audit::add_index_info(ctx, &entry, &mut text).await;
    }
    Ok(vec![ContentBlock::text(text)])
}

async fn read_text_file<S: Storage>(ctx: &Ctx<'_, S>, args: &Args<'_>) -> ToolOutput {
    let path = ctx.check(args.required_path("path")?)?;
    let offset = args.uint("offset")?.unwrap_or(0);
    let max_read = ctx.config.max_read_bytes.max(1);
    let limit = match args.uint("max_bytes")? {
        Some(0) => return Err("max_bytes must be at least 1.".into()),
        Some(n) => n.min(max_read),
        None => max_read,
    };

    let entry = ctx.timed(ctx.storage.stat(&path, false)).await?;
    if entry.is_dir {
        return Err(format!(
            "{path} is a folder, not a file. Use list_folder to see what it contains."
        ));
    }
    if let Some(mime) = image_type(&entry) {
        return Err(format!(
            "{path} is an image ({mime}), not text. Use read_image to look at it."
        ));
    }
    match entry.size {
        Some(0) => return Ok(vec![ContentBlock::text(format!("({path} is empty.)"))]),
        Some(size) if offset >= size => {
            return Err(format!(
                "offset {offset} is past the end of {path}, which has {size} bytes."
            ));
        }
        _ => {}
    }

    let range = ByteRange {
        offset,
        length: limit,
    };
    let bytes = ctx
        .timed(async {
            let stream = ctx.storage.open(&path, Some(range)).await?;
            read_capped(stream, limit).await
        })
        .await?;
    if bytes.is_empty() {
        return if offset == 0 {
            Ok(vec![ContentBlock::text(format!("({path} is empty.)"))])
        } else {
            Err(format!("offset {offset} is past the end of {path}."))
        };
    }
    if bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0) {
        return Err(format!(
            "{path} looks like a binary file (it contains NUL bytes), so it is not returned as \
             text. Use get_file_info for its details, or read_image if it is a picture."
        ));
    }

    let read_to = offset.saturating_add(bytes.len() as u64);
    let at_eof = match entry.size {
        Some(size) => read_to >= size,
        None => (bytes.len() as u64) < limit,
    };
    // Start and end on character boundaries: an offset inside a multi-byte character skips
    // the rest of it, and a character cut off by the byte limit is left for the next call.
    let start = leading_continuation_bytes(&bytes);
    let end = if at_eof {
        bytes.len()
    } else {
        complete_prefix_len(&bytes)
    };
    if end <= start {
        return Err(format!(
            "max_bytes={limit} is too small to hold the character at offset {offset}; ask for \
             at least 4 bytes."
        ));
    }
    let text = String::from_utf8_lossy(&bytes[start..end]);

    let first = offset.saturating_add(start as u64);
    let next = offset.saturating_add(end as u64);
    if first == 0 && at_eof {
        return Ok(vec![ContentBlock::text(text)]);
    }
    let total = entry
        .size
        .map_or_else(|| "unknown size".to_owned(), |size| size.to_string());
    let rest = if at_eof {
        "end of file".to_owned()
    } else {
        format!("continue with offset={next}")
    };
    Ok(vec![ContentBlock::text(format!(
        "[bytes {first}-{} of {total}; {rest}]\n{text}",
        next - 1
    ))])
}

async fn read_image<S: Storage>(ctx: &Ctx<'_, S>, args: &Args<'_>) -> ToolOutput {
    let path = ctx.check(args.required_path("path")?)?;
    let entry = ctx.timed(ctx.storage.stat(&path, false)).await?;
    if entry.is_dir {
        return Err(format!(
            "{path} is a folder, not a file. Use list_folder to see what it contains."
        ));
    }
    let Some(declared) = image_type(&entry) else {
        return Err(format!(
            "{path} is not a PNG, JPEG, GIF or WebP image. Use read_text_file for text, or \
             get_file_info for its details."
        ));
    };
    let max = ctx.config.max_image_bytes;
    let too_large = |size: u64| {
        format!(
            "{path} is {}, larger than the {} read_image can return.",
            human_size(size),
            human_size(max)
        )
    };
    if let Some(size) = entry.size
        && size > max
    {
        return Err(too_large(size));
    }
    // One byte more than allowed, to tell "fits" from "too big" when the size was wrong.
    let length = max.saturating_add(1);
    let bytes = ctx
        .timed(async {
            let stream = ctx
                .storage
                .open(&path, Some(ByteRange { offset: 0, length }))
                .await?;
            read_capped(stream, length).await
        })
        .await?;
    if bytes.len() as u64 > max {
        return Err(too_large(bytes.len() as u64));
    }
    if bytes.is_empty() {
        return Err(format!("{path} is empty."));
    }
    // Trust the bytes over the name: Claude rejects an image whose data doesn't match its
    // declared type, and a .png that is really a JPEG is common enough.
    let Some(mime) = sniff_image(&bytes) else {
        return Err(format!(
            "{path} is named like a {declared} image, but its contents are not PNG, JPEG, GIF \
             or WebP."
        ));
    };
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(vec![
        ContentBlock::text(format!(
            "{path} ({mime}, {})",
            human_size(bytes.len() as u64)
        )),
        ContentBlock::image(data, mime),
    ])
}

async fn search_files<S: Storage>(ctx: &Ctx<'_, S>, args: &Args<'_>) -> Structured {
    let query = args
        .string("query")?
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .ok_or("query is required: part of a file or folder name to look for.")?;
    let root = ctx.check(args.string("path")?.unwrap_or("/"))?;
    let max = ctx.config.search_limit.max(1);
    let limit = match args.uint("limit")? {
        Some(0) => return Err("limit must be at least 1.".into()),
        Some(n) => usize::try_from(n).unwrap_or(usize::MAX).min(max),
        None => max,
    };

    // Compared folded, like names in search: "/backups/immich" turns off "/Backups/Immich"
    // as list_folder shows it.
    if let Some(off) = ctx.policy.search_skip().covering(root.as_str()) {
        return Err(format!(
            "Search is turned off under {off} because it's too large. Browse it with list_folder \
             instead."
        ));
    }
    if ctx.index.is_some()
        && let Some(done) = audit::search_files(ctx, query, &root, args, limit).await?
    {
        return Ok(done);
    }
    if args.string("cursor")?.is_some() {
        return Err(
            "cursor only works while the index covers the search; repeat the search \
                    without it."
                .into(),
        );
    }
    ctx.gate().await?;
    let excludes = ctx.policy.search_excludes();
    let mut search = Search::new(ctx, query, &excludes, limit);
    if excludes.any_below(root.as_str()) {
        search.folder(&root).await.map_err(storage_error)?;
    } else {
        search.subtree(&root).await?;
    }
    let (found, more, notes) = search.finish(&root)?;

    let mut text = if found.is_empty() {
        let partly = if notes.is_empty() {
            ""
        } else {
            " in the folders that were searched"
        };
        format!("No files or folders under {root} have names containing \"{query}\"{partly}.")
    } else {
        format!(
            "{} for \"{query}\" under {root}:\n",
            count(found.len(), "match", "matches")
        )
    };
    let shown = push_entry_lines(&mut text, found.iter());
    if more || shown < found.len() {
        text.push_str(&format!(
            "Showing the first {shown} matches; there may be more. Narrow the query or the path.\n"
        ));
    }
    for note in &notes {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(note);
    }
    Ok((vec![ContentBlock::text(text)], None))
}

/// A name search. Where nothing below a folder is excluded it walks the folder whole; above
/// an excluded folder (hidden, or turned off for search) it lists one level at a time, so
/// excluded folders are never fetched (see [`crate::split_walk`]). A subfolder that fails
/// doesn't sink the rest, and the deadline ends the search with what it has; the result says
/// what was left out.
struct Search<'a, 'c, S> {
    ctx: &'c Ctx<'a, S>,
    matcher: NameMatcher,
    excludes: &'c PathSet,
    limit: usize,
    found: Vec<Entry>,
    /// More matched than `limit` allowed, or the search stopped at `limit`.
    more: bool,
    /// Subtrees walked whole without failing.
    searched: usize,
    /// Turned-off folders passed on the way.
    skipped: Vec<String>,
    /// Subfolders with too many entries to take in at once.
    too_large: Vec<String>,
    /// Other subfolders that failed, each with the reason.
    failed: Vec<String>,
    /// Why the search ended before going through everything.
    stopped: Option<String>,
}

impl<'a, 'c, S: Storage> Search<'a, 'c, S> {
    fn new(ctx: &'c Ctx<'a, S>, query: &str, excludes: &'c PathSet, limit: usize) -> Self {
        Self {
            ctx,
            matcher: NameMatcher::new(query),
            excludes,
            limit,
            found: Vec::new(),
            more: false,
            searched: 0,
            skipped: Vec::new(),
            too_large: Vec::new(),
            failed: Vec::new(),
            stopped: None,
        }
    }

    fn done(&self) -> bool {
        self.found.len() >= self.limit || self.stopped.is_some()
    }

    fn late(&mut self) {
        self.stopped = Some(format!(
            "it reached the {} time limit",
            seconds(self.ctx.config.deadline)
        ));
    }

    /// Walks the search root whole. Its failure is the search's failure.
    async fn subtree(&mut self, dir: &CheckedPath) -> Result<(), String> {
        let ctx = self.ctx;
        match ctx.within(ctx.storage.walk(dir, &[])).await {
            Ok(Ok(entries)) => self.take_hits(entries),
            Ok(Err(err)) => return Err(storage_error(err)),
            Err(Late) => self.late(),
        }
        Ok(())
    }

    /// Walks a subfolder whole; a failure becomes a note.
    async fn subtree_or_note(&mut self, dir: &CheckedPath) {
        let ctx = self.ctx;
        match ctx.within(ctx.storage.walk(dir, &[])).await {
            Ok(Ok(entries)) => self.take_hits(entries),
            Ok(Err(err)) => self.left_out(dir.as_str().to_owned(), err),
            Err(Late) => self.late(),
        }
    }

    /// Keeps the matching, visible entries of one walked subtree, in path order.
    fn take_hits(&mut self, entries: Vec<Entry>) {
        self.searched += 1;
        let policy = self.ctx.policy;
        let mut hits: Vec<Entry> = entries
            .into_iter()
            .filter(|e| policy.visible(&e.path) && self.matcher.matches(&e.name))
            .collect();
        hits.sort_by_cached_key(|e| (e.path.to_lowercase(), e.path.clone()));
        let room = self.limit - self.found.len();
        if hits.len() > room {
            self.more = true;
        }
        self.found.extend(hits.into_iter().take(room));
    }

    /// Goes through the children of `dir`, which has an excluded folder inside. Fails only
    /// when `dir` itself can't be listed.
    fn folder<'f>(
        &'f mut self,
        dir: &'f CheckedPath,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), StorageError>> + Send + 'f>>
    where
        'a: 'f,
        'c: 'f,
    {
        Box::pin(async move {
            let ctx = self.ctx;
            let mut children = match ctx.within(ctx.storage.list(dir)).await {
                Ok(children) => children?,
                Err(Late) => {
                    self.late();
                    return Ok(());
                }
            };
            children.retain(|e| ctx.policy.visible(&e.path));
            sort_entries(&mut children);
            for child in children {
                if self.done() {
                    if self.found.len() >= self.limit {
                        self.more = true;
                    }
                    break;
                }
                let (name, is_dir) = (child.name.clone(), child.is_dir);
                if self.matcher.matches(&child.name) {
                    self.found.push(child);
                }
                if !is_dir {
                    continue;
                }
                let path = dir.child(&name);
                if self.excludes.covers(path.as_str()) {
                    // Hidden folders were dropped above, so this one is turned off for search.
                    self.skipped.push(path.as_str().to_owned());
                    continue;
                }
                if self.done() {
                    self.more = true;
                    break;
                }
                if self.excludes.any_below(path.as_str()) {
                    if let Err(err) = self.folder(&path).await {
                        self.left_out(path.as_str().to_owned(), err);
                    }
                } else {
                    self.subtree_or_note(&path).await;
                }
            }
            Ok(())
        })
    }

    fn left_out(&mut self, path: String, err: StorageError) {
        match err {
            StorageError::TooManyEntries(_) => self.too_large.push(path),
            err => {
                if matches!(err, StorageError::Backend(_) | StorageError::Unhealthy(_)) {
                    // Most likely down for every other folder too.
                    self.stopped = Some("the file server stopped answering".into());
                }
                self.failed
                    .push(format!("{path} ({})", redact_urls(&err.to_string())));
            }
        }
    }

    /// The matches, whether there may be more, and notes on what was left out; or an error
    /// when nothing could be searched and nothing matched.
    #[allow(clippy::type_complexity)]
    fn finish(self, root: &CheckedPath) -> Result<(Vec<Entry>, bool, Vec<String>), String> {
        let mut notes = Vec::new();
        if !self.too_large.is_empty() {
            notes.push(format!(
                "Not searched, too many entries to go through in one call: {}. Search inside {} \
                 directly, with a narrower path.",
                self.too_large.join(", "),
                if self.too_large.len() == 1 {
                    "it"
                } else {
                    "each"
                }
            ));
        }
        for failure in &self.failed {
            notes.push(format!("Could not search {failure}."));
        }
        if !self.skipped.is_empty() {
            notes.push(format!(
                "Skipped {}: search is turned off there. Browse with list_folder instead.",
                self.skipped.join(", ")
            ));
        }
        if let Some(why) = &self.stopped {
            notes.push(format!(
                "The search stopped early because {why}; there may be more matches. Search a \
                 narrower path to go through the rest."
            ));
        }
        let failed_everywhere = self.searched == 0
            && !(self.too_large.is_empty() && self.failed.is_empty() && self.stopped.is_none());
        if failed_everywhere && self.found.is_empty() {
            return Err(format!(
                "Nothing under {root} could be searched.\n{}",
                notes.join("\n")
            ));
        }
        let notes = notes.into_iter().map(|note| note + "\n").collect();
        Ok((self.found, self.more, notes))
    }
}

/// Appends one line per entry while the listing stays under [`MAX_LISTING_CHARS`]; returns
/// how many were written.
fn push_entry_lines<'a>(text: &mut String, entries: impl Iterator<Item = &'a Entry>) -> usize {
    let budget = text.len() + MAX_LISTING_CHARS;
    let mut shown = 0;
    for entry in entries {
        let before = text.len();
        push_entry_line(text, entry);
        if text.len() > budget {
            text.truncate(before);
            break;
        }
        shown += 1;
    }
    shown
}

/// `- notes.md (file, 1.2 KiB, modified 2026-10-01T09:00:00Z) path: /Notes/notes.md`, written
/// straight into `text`: a folder can have a thousand of these.
pub(crate) fn push_entry_line(text: &mut String, entry: &Entry) {
    let (slash, kind) = if entry.is_dir {
        ("/", "folder")
    } else {
        ("", "file")
    };
    let _ = write!(text, "- {}{slash} ({kind}", entry.name);
    if !entry.is_dir
        && let Some(size) = entry.size
    {
        let _ = write!(text, ", {}", human_size(size));
    }
    if let Some(modified) = &entry.modified {
        let _ = write!(text, ", modified {modified}");
    }
    let _ = writeln!(text, ") path: {}", entry.path);
}

/// The tool arguments, with lenient readers: `null` counts as absent, and numbers may arrive
/// as whole floats or numeric strings, which models sometimes send.
pub(crate) struct Args<'a>(pub(crate) &'a Map<String, Value>);

impl Args<'_> {
    pub(crate) fn bool(&self, key: &str) -> Result<Option<bool>, String> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(Value::String(s)) if s.eq_ignore_ascii_case("true") => Ok(Some(true)),
            Some(Value::String(s)) if s.eq_ignore_ascii_case("false") => Ok(Some(false)),
            Some(_) => Err(format!("{key} must be true or false.")),
        }
    }

    pub(crate) fn string(&self, key: &str) -> Result<Option<&str>, String> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(format!("{key} must be a string.")),
        }
    }

    pub(crate) fn uint(&self, key: &str) -> Result<Option<u64>, String> {
        let invalid = || format!("{key} must be a whole number, 0 or more.");
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(n)) => match (n.as_u64(), n.as_f64()) {
                (Some(n), _) => Ok(Some(n)),
                (None, Some(f)) if f >= 0.0 && f.fract() == 0.0 && f <= u64::MAX as f64 => {
                    Ok(Some(f as u64))
                }
                _ => Err(invalid()),
            },
            Some(Value::String(s)) => s.trim().parse().map(Some).map_err(|_| invalid()),
            Some(_) => Err(invalid()),
        }
    }

    pub(crate) fn required_path(&self, key: &str) -> Result<&str, String> {
        self.string(key)?
            .ok_or_else(|| format!("{key} is required, e.g. \"/Notes/todo.md\"."))
    }
}

/// The error text for the model, with a hint at what to do next. Backend messages are
/// scrubbed of URLs, which could carry credentials.
pub(crate) fn storage_error(err: StorageError) -> String {
    let hint = match &err {
        StorageError::NotFound(_) => " Check the path with list_folder or search_files.",
        StorageError::NotADirectory(_) => " Use get_file_info or read_text_file for a file.",
        StorageError::IsADirectory(_) => " Use list_folder to see what it contains.",
        StorageError::InvalidPath(_) => {
            " Paths look like \"/Folder/file.txt\", starting at the drive's root."
        }
        StorageError::TooManyEntries(_) => " Search a narrower path, such as a subfolder.",
        StorageError::Backend(_) | StorageError::Unhealthy(_) => {
            " The file server may be offline; try again later."
        }
        StorageError::AlreadyExists(_) => {
            " Pass overwrite=true to replace a file, or choose another name."
        }
        StorageError::Protected(_) => {
            " The owner made it read-only; nothing at, under or around it can be changed here."
        }
        StorageError::PermissionDenied(_) | StorageError::Unsupported(_) => "",
    };
    let message = redact_urls(&err.to_string());
    let stop = if message.ends_with('.') { "" } else { "." };
    format!("{message}{stop}{hint}")
}

/// Whether a tool error came from the backend failing (unreachable, erroring, too slow) rather
/// than from the path.
fn backend_failed(message: &str) -> bool {
    [
        "storage backend",
        "The file server can't",
        "The storage backend didn't",
    ]
    .iter()
    .any(|start| message.starts_with(start))
}

pub(crate) fn redact_urls(text: &str) -> String {
    text.split(' ')
        .map(|word| if word.contains("://") { "[url]" } else { word })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn seconds(duration: Duration) -> String {
    format!("{} s", duration.as_secs())
}

/// The image type of an entry, from its content type or else its extension.
fn image_type(entry: &Entry) -> Option<&'static str> {
    if let Some(content_type) = &entry.content_type {
        let media = content_type.split(';').next().unwrap_or("").trim();
        let by_type = match media.to_ascii_lowercase().as_str() {
            "image/png" => Some(PNG),
            "image/jpeg" | "image/jpg" | "image/pjpeg" => Some(JPEG),
            "image/gif" => Some(GIF),
            "image/webp" => Some(WEBP),
            _ => None,
        };
        if by_type.is_some() {
            return by_type;
        }
    }
    let file_name = entry.path.rsplit('/').next().unwrap_or(&entry.name);
    let (_, extension) = file_name.rsplit_once('.')?;
    match extension.to_ascii_lowercase().as_str() {
        "png" => Some(PNG),
        "jpg" | "jpeg" | "jpe" | "jfif" => Some(JPEG),
        "gif" => Some(GIF),
        "webp" => Some(WEBP),
        _ => None,
    }
}

/// The image type the bytes themselves announce.
fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(PNG)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(JPEG)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(GIF)
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(WEBP)
    } else {
        None
    }
}

/// How many bytes at the start belong to a character that began before them (at most 3).
fn leading_continuation_bytes(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .take(3)
        .take_while(|&&b| b & 0xC0 == 0x80)
        .count()
}

/// Length of `bytes` without a multi-byte character cut off at the end. Invalid sequences
/// are left alone (the lossy decode shows them as U+FFFD).
fn complete_prefix_len(bytes: &[u8]) -> usize {
    let len = bytes.len();
    for back in 1..=len.min(4) {
        let byte = bytes[len - back];
        if byte & 0xC0 == 0x80 {
            continue;
        }
        let char_len = match byte {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        return if back < char_len { len - back } else { len };
    }
    len
}

/// `512 bytes`, `1.5 KiB`, `3.0 MiB`. Rounds down so a limit is never overstated.
pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return count(bytes as usize, "byte", "bytes");
    }
    let mut unit = 0;
    let mut scale: u64 = 1024;
    while unit + 1 < UNITS.len() && bytes / scale >= 1024 {
        scale *= 1024;
        unit += 1;
    }
    let tenths = u128::from(bytes) * 10 / u128::from(scale);
    format!("{}.{} {}", tenths / 10, tenths % 10, UNITS[unit])
}

pub(crate) fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_prefix_trims_a_cut_character() {
        let text = "a€".as_bytes(); // € is 3 bytes
        assert_eq!(complete_prefix_len(text), 4);
        assert_eq!(complete_prefix_len(&text[..3]), 1);
        assert_eq!(complete_prefix_len(&text[..2]), 1);
        let emoji = "😀".as_bytes();
        assert_eq!(complete_prefix_len(&emoji[..3]), 0);
        assert_eq!(complete_prefix_len(emoji), 4);
        // Invalid trailing bytes are not this function's business.
        assert_eq!(complete_prefix_len(&[0x80, 0x80]), 2);
        assert_eq!(complete_prefix_len(b""), 0);
    }

    #[test]
    fn leading_continuation_bytes_are_counted() {
        let euro = "€x".as_bytes();
        assert_eq!(leading_continuation_bytes(euro), 0);
        assert_eq!(leading_continuation_bytes(&euro[1..]), 2);
        assert_eq!(leading_continuation_bytes(&euro[2..]), 1);
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_size(0), "0 bytes");
        assert_eq!(human_size(1), "1 byte");
        assert_eq!(human_size(1023), "1023 bytes");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MiB");
        assert_eq!(human_size(u64::MAX), "16383.9 PiB");
    }

    #[test]
    fn cursors_round_trip() {
        assert_eq!(decode_cursor(&encode_cursor(1000)), Some(1000));
        assert_eq!(decode_cursor("nonsense"), None);
        assert_eq!(decode_cursor(""), None);
    }

    #[test]
    fn urls_are_redacted() {
        assert_eq!(
            redact_urls("GET http://mcp:secret@127.0.0.1:8080/x failed"),
            "GET [url] failed"
        );
    }
}
