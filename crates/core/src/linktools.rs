//! The link tools: `create_download_link`, `create_upload_link`, `list_links` and
//! `revoke_link`. The links themselves are served by the server (`/dl`, `/ul`), which checks
//! every token again; see [`crate::links`] for the format.
//!
//! Both create tools carry `destructiveHint`, so Claude asks the owner before each link: a
//! download link lets data leave, and a prompt injection in an indexed file could otherwise
//! ask for one quietly.

use std::fmt::Write as _;

use rmcp::model::{ContentBlock, Tool};
use serde_json::{Value, json};

use crate::links::LinkSettings;
use crate::policy::CheckedPath;
use crate::state::{LinkKind, LinkRow, LinkState};
use crate::storage::{Entry, Storage, StorageError, StorageWrite};
use crate::time;
use crate::tools::{
    Args, CREATE_DOWNLOAD_LINK, CREATE_UPLOAD_LINK, Ctx, LIST_LINKS, REVOKE_LINK, Structured,
    destructive_tool, human_size, storage_error, tool,
};
use crate::zip::{ZipItem, zip_size};

/// Files whose SHA-1 goes into a manifest (one API call each on pCloud).
const MANIFEST_HASHES: usize = 20;
/// Smallest byte allowance of a link, so a tiny file can still be fetched a few times.
const MIN_LINK_BYTES: u64 = 1024 * 1024;

pub(crate) fn definition(links: &LinkSettings, name: &str) -> Option<Tool> {
    let ttl = json!({"type": ["string", "integer"],
        "description": format!("How long the link works: seconds, or e.g. \"15m\", \"2h\" (default {} min, at most {} h).",
            links.default_ttl.as_secs() / 60, links.max_ttl.as_secs() / 3600)});
    Some(match name {
        CREATE_DOWNLOAD_LINK => destructive_tool(
            CREATE_DOWNLOAD_LINK,
            "Create download link",
            &format!(
                "Make expiring HTTPS links to fetch files from the drive with curl, for files too \
                 big for a tool result or to move data to a computer. Without zip: one link per \
                 file (folders are expanded), each resumable with curl -C -, plus a manifest and \
                 a ready-to-run shell snippet. With zip=true: one streamed zip of everything (up \
                 to {}; not resumable). A link may move 3x its size in all and can be revoked \
                 with revoke_link; list_links shows its use. Anyone holding the URL can fetch \
                 until it expires, so share it only with the computer that needs it.",
                human_size(links.max_zip_bytes)
            ),
            json!({
                "type": "object",
                "properties": {
                    "paths": {"type": ["array", "string"], "items": {"type": "string"},
                              "description": "Files or folders, e.g. [\"/Photos/2019\"]."},
                    "zip": {"type": "boolean", "default": false, "description": "One zip of everything instead of a link per file."},
                    "expires_in": ttl
                },
                "required": ["paths"],
                "additionalProperties": false
            }),
            false,
            true,
        ),
        CREATE_UPLOAD_LINK => destructive_tool(
            CREATE_UPLOAD_LINK,
            "Create upload link",
            &format!(
                "Make an expiring HTTPS link for sending one big file to the drive from a \
                 computer, in parts of up to {} (the result has a ready-to-run split + curl \
                 snippet). When the last part arrives the file goes to pCloud and is checked \
                 (size and SHA-1); GET on the link shows progress. The destination folder must \
                 exist. Space for `size` is reserved when the link is made.",
                human_size(links.part_bytes)
            ),
            json!({
                "type": "object",
                "properties": {
                    "folder": {"type": "string", "description": "Existing folder to put the file in, e.g. \"/Uploads\"."},
                    "name": {"type": "string", "description": "The file's name, e.g. \"dataset.tar\"."},
                    "size": {"type": "integer", "minimum": 1, "description": "The file's exact size in bytes (wc -c < file)."},
                    "overwrite": {"type": "boolean", "default": false, "description": "Replace an existing file of that name."},
                    "expires_in": ttl
                },
                "required": ["folder", "name", "size"],
                "additionalProperties": false
            }),
            false,
            true,
        ),
        LIST_LINKS => tool(
            LIST_LINKS,
            "List links",
            "The links made recently, newest first: what each serves, its state and expiry, bytes \
             moved of its allowance, the network it is bound to, and its last uses (address, \
             range, bytes, status).",
            json!({
                "type": "object",
                "properties": {"id": {"type": "string", "description": "Only this link, with all its uses."}},
                "additionalProperties": false
            }),
        ),
        REVOKE_LINK => destructive_tool(
            REVOKE_LINK,
            "Revoke link",
            "End a download or upload link now. An upload's partial data is deleted.",
            json!({
                "type": "object",
                "properties": {"id": {"type": "string", "description": "The link's id from list_links or from creating it."}},
                "required": ["id"],
                "additionalProperties": false
            }),
            true,
            false,
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
        CREATE_DOWNLOAD_LINK => plain(create_download_link(ctx, args).await),
        CREATE_UPLOAD_LINK => plain(create_upload_link(ctx, args).await),
        LIST_LINKS => plain(list_links(ctx, args).await),
        REVOKE_LINK => plain(revoke_link(ctx, args).await),
        _ => return None,
    })
}

fn settings<'a, S>(ctx: &Ctx<'a, S>) -> Result<&'a LinkSettings, String> {
    ctx.links
        .ok_or_else(|| "Links are turned off on this server.".into())
}

/// Where links are reached: the configured address, else the auth Worker's.
fn base_url<S>(ctx: &Ctx<'_, S>, links: &LinkSettings) -> Result<String, String> {
    links
        .public_url
        .as_deref()
        .or(ctx.public_url)
        .map(|u| u.trim().trim_end_matches('/').to_owned())
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .ok_or_else(|| {
            "This server doesn't know its public address, so it can't make links: the auth \
             Worker sends it with each request, or the owner sets [links] public_url."
                .into()
        })
}

fn ttl(args: &Args<'_>, links: &LinkSettings) -> Result<i64, String> {
    let max = links.max_ttl.as_secs() as i64;
    let seconds = match args.0.get("expires_in") {
        None | Some(Value::Null) => links.default_ttl.as_secs() as i64,
        Some(Value::Number(n)) => n.as_i64().ok_or("expires_in must be whole seconds.")?,
        Some(Value::String(s)) => time::parse_age(s)
            .ok_or("expires_in must be seconds or a length such as \"15m\" or \"2h\".")?,
        Some(_) => return Err("expires_in must be seconds or a length such as \"15m\".".into()),
    };
    if seconds < 60 {
        return Err("expires_in must be at least a minute.".into());
    }
    if seconds > max {
        return Err(format!(
            "expires_in can be at most {} hours.",
            links.max_ttl.as_secs() / 3600
        ));
    }
    Ok(seconds)
}

fn paths_arg(args: &Args<'_>) -> Result<Vec<String>, String> {
    let paths: Vec<String> = match args.0.get("paths") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or("paths must be strings.")
            })
            .collect::<Result<_, _>>()?,
        _ => return Err("paths is required: the files or folders to link to.".into()),
    };
    if paths.is_empty() {
        return Err("paths is empty.".into());
    }
    Ok(paths)
}

/// A file name for a URL path segment.
fn url_name(name: &str) -> String {
    const SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(name, SEGMENT).to_string()
}

/// Single-quoted for a POSIX shell.
fn sh(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn byte_allowance(size: u64) -> u64 {
    size.saturating_mul(3).max(MIN_LINK_BYTES)
}

/// One file to link, with its name relative to the requested folder (for the snippet).
struct Linked {
    entry: Entry,
    relative: String,
}

/// The visible files (and, for zips, folders) under each requested path, live from the drive.
/// Hidden subtrees are never listed.
async fn collect<S: StorageWrite>(
    ctx: &Ctx<'_, S>,
    paths: &[String],
    with_folders: bool,
) -> Result<Vec<Linked>, String> {
    let mut out = Vec::new();
    let mut tops: Vec<String> = Vec::new();
    for raw in paths {
        let path = ctx.check(raw)?;
        let entry = ctx.timed(ctx.storage.stat(&path, false)).await?;
        let mut top = if path.is_root() {
            "drive".to_owned()
        } else {
            path.as_str()
                .rsplit('/')
                .next()
                .unwrap_or("file")
                .to_owned()
        };
        // Two requested paths with the same name get distinct folders in a zip.
        let base = top.clone();
        let mut n = 2;
        while tops.iter().any(|t| crate::fold(t) == crate::fold(&top)) {
            top = format!("{base} ({n})");
            n += 1;
        }
        tops.push(top.clone());
        if !entry.is_dir {
            out.push(Linked {
                entry,
                relative: top,
            });
            continue;
        }
        if with_folders {
            out.push(Linked {
                entry: entry.clone(),
                relative: format!("{top}/"),
            });
        }
        let exclude: Vec<CheckedPath> = ctx
            .policy
            .hidden()
            .paths()
            .iter()
            .filter_map(|h| crate::Policy::default().check(h).ok())
            .collect();
        let mut found = ctx.timed(ctx.storage.walk(&path, &exclude)).await?;
        found.retain(|e| ctx.policy.visible(&e.path) && (with_folders || !e.is_dir));
        found.sort_by(|a, b| a.path.cmp(&b.path));
        let prefix = if path.is_root() {
            "/".to_owned()
        } else {
            format!("{path}/")
        };
        for e in found {
            let rest = e.path.strip_prefix(&prefix).unwrap_or(&e.name).to_owned();
            let slash = if e.is_dir { "/" } else { "" };
            out.push(Linked {
                relative: format!("{top}/{rest}{slash}"),
                entry: e,
            });
        }
    }
    Ok(out)
}

/// The zip's contents for a link's paths, as the server streams them. Used at creation (for
/// the size) and again at each download.
pub async fn zip_items<S: Storage>(
    storage: &S,
    policy: &crate::Policy,
    paths: &[String],
) -> Result<Vec<ZipItem>, StorageError> {
    let exclude: Vec<CheckedPath> = policy
        .hidden()
        .paths()
        .iter()
        .filter_map(|h| crate::Policy::default().check(h).ok())
        .collect();
    let mut items = Vec::new();
    let mut tops: Vec<String> = Vec::new();
    for raw in paths {
        let path = policy.check(raw)?;
        let entry = storage.stat(&path, false).await?;
        let mut top = if path.is_root() {
            "drive".to_owned()
        } else {
            path.as_str()
                .rsplit('/')
                .next()
                .unwrap_or("file")
                .to_owned()
        };
        let base = top.clone();
        let mut n = 2;
        while tops.iter().any(|t| crate::fold(t) == crate::fold(&top)) {
            top = format!("{base} ({n})");
            n += 1;
        }
        tops.push(top.clone());
        let mtime = |e: &Entry| e.modified.as_deref().and_then(time::parse_rfc3339);
        if !entry.is_dir {
            items.push(ZipItem {
                mtime: mtime(&entry),
                path,
                name: top,
                is_dir: false,
                size: entry.size.unwrap_or(0),
            });
            continue;
        }
        items.push(ZipItem {
            mtime: mtime(&entry),
            path: path.clone(),
            name: format!("{top}/"),
            is_dir: true,
            size: 0,
        });
        let mut found = storage.walk(&path, &exclude).await?;
        found.retain(|e| policy.visible(&e.path));
        found.sort_by(|a, b| a.path.cmp(&b.path));
        let prefix = if path.is_root() {
            "/".to_owned()
        } else {
            format!("{path}/")
        };
        for e in found {
            let Ok(checked) = policy.check(&e.path) else {
                continue;
            };
            let rest = e.path.strip_prefix(&prefix).unwrap_or(&e.name).to_owned();
            let slash = if e.is_dir { "/" } else { "" };
            items.push(ZipItem {
                mtime: mtime(&e),
                name: format!("{top}/{rest}{slash}"),
                is_dir: e.is_dir,
                size: e.size.unwrap_or(0),
                path: checked,
            });
        }
    }
    Ok(items)
}

async fn insert<S: StorageWrite>(ctx: &Ctx<'_, S>, row: LinkRow) -> Result<(), String> {
    ctx.st(move |state| state.insert_link(&row)).await
}

async fn create_download_link<S: StorageWrite>(
    ctx: &Ctx<'_, S>,
    args: &Args<'_>,
) -> Result<String, String> {
    let links = settings(ctx)?;
    let base = base_url(ctx, links)?;
    let paths = paths_arg(args)?;
    let zip = args.bool("zip")?.unwrap_or(false);
    let ttl = ttl(args, links)?;
    let now = time::now();
    let expires = now + ttl;
    let until = time::rfc3339(expires);

    if zip {
        let items = ctx
            .timed(zip_items(ctx.storage, ctx.policy, &paths))
            .await?;
        let files = items.iter().filter(|i| !i.is_dir).count();
        let content: u64 = items.iter().map(|i| i.size).sum();
        if content > links.max_zip_bytes {
            return Err(format!(
                "That is {} in {files} files, more than the {} a zip may hold. Use zip=false for \
                 one resumable link per file.",
                human_size(content),
                human_size(links.max_zip_bytes)
            ));
        }
        let size = zip_size(&items);
        let name = if paths.len() == 1 {
            let top = items
                .first()
                .map_or("files", |i| i.name.trim_end_matches('/'));
            format!("{top}.zip")
        } else {
            "files.zip".to_owned()
        };
        let checked: Vec<String> = paths
            .iter()
            .map(|p| ctx.check(p).map(|c| c.to_string()))
            .collect::<Result<_, _>>()?;
        let (id, token) = links.key.mint(LinkKind::Download, expires);
        insert(
            ctx,
            LinkRow {
                id: id.clone(),
                kind: LinkKind::Download,
                paths: checked,
                zip: true,
                name: name.clone(),
                created: now,
                expires,
                bound_net: None,
                size,
                max_bytes: byte_allowance(size),
                bytes_used: 0,
                committed: 0,
                overwrite: false,
                state: LinkState::Active,
                note: None,
            },
        )
        .await?;
        let url = format!("{base}/dl/{token}/{}", url_name(&name));
        return Ok(format!(
            "Zip link {id} for {files} files ({} in all, the zip about {}), valid until {until}. \
             It streams the files live and can't resume: if the transfer drops, fetch it again \
             (the link allows {} in all).\n\nURL: {url}\n\nFetch and check:\n\
             curl -fL -o {} {}\nunzip -t {}",
            human_size(content),
            human_size(size),
            human_size(byte_allowance(size)),
            sh(&name),
            sh(&url),
            sh(&name),
        ));
    }

    let found = collect(ctx, &paths, false).await?;
    if found.is_empty() {
        return Err("There are no files there to link to.".into());
    }
    if found.len() > links.max_links_per_call {
        return Err(format!(
            "That is {} files, more than the {} links one call makes. Link fewer paths, or use \
             zip=true.",
            found.len(),
            links.max_links_per_call
        ));
    }
    let hash = found.len() <= MANIFEST_HASHES
        && ctx
            .storage
            .capabilities()
            .hashes
            .contains(&crate::HashKind::Sha1);
    let mut rows = Vec::new();
    let mut manifest = String::new();
    let mut script = String::from("set -e\n");
    let mut total = 0u64;
    for item in &found {
        let entry = &item.entry;
        let path = ctx.check(&entry.path)?;
        let size = entry.size.unwrap_or(0);
        total += size;
        let sha1 = if hash {
            ctx.timed(ctx.storage.stat(&path, true))
                .await
                .ok()
                .and_then(|e| {
                    e.hashes
                        .into_iter()
                        .find(|(k, _)| *k == crate::HashKind::Sha1)
                        .map(|(_, v)| v)
                })
        } else {
            None
        };
        let name = entry.name.clone();
        let (id, token) = links.key.mint(LinkKind::Download, expires);
        let url = format!("{base}/dl/{token}/{}", url_name(&name));
        let _ = writeln!(
            manifest,
            "- {} ({}{}) link {id}",
            entry.path,
            human_size(size),
            sha1.as_deref()
                .map(|h| format!(", SHA-1 {h}"))
                .unwrap_or_default()
        );
        let local = &item.relative;
        if let Some((dir, _)) = local.rsplit_once('/') {
            let _ = writeln!(script, "mkdir -p {}", sh(dir));
        }
        let _ = writeln!(
            script,
            "curl -fL -C - --retry 3 -o {} {}",
            sh(local),
            sh(&url)
        );
        if let Some(h) = &sha1 {
            let _ = writeln!(script, "echo {}  {} | sha1sum -c -", h, sh(local));
        }
        rows.push(LinkRow {
            id,
            kind: LinkKind::Download,
            paths: vec![path.to_string()],
            zip: false,
            name,
            created: now,
            expires,
            bound_net: None,
            size,
            max_bytes: byte_allowance(size),
            bytes_used: 0,
            committed: 0,
            overwrite: false,
            state: LinkState::Active,
            note: None,
        });
    }
    let count = rows.len();
    ctx.st(move |state| {
        for row in &rows {
            state.insert_link(row)?;
        }
        Ok(())
    })
    .await?;
    let check = if hash {
        "SHA-1 checked after each file"
    } else {
        "check sizes with ls -l"
    };
    Ok(format!(
        "{count} download link{} ({} in all), valid until {until}, each resumable (curl -C -) \
         and good for 3x its size.\n\nFiles:\n{manifest}\nFetch them ({check}):\n{script}",
        if count == 1 { "" } else { "s" },
        human_size(total),
    ))
}

async fn create_upload_link<S: StorageWrite>(
    ctx: &Ctx<'_, S>,
    args: &Args<'_>,
) -> Result<String, String> {
    let links = settings(ctx)?;
    let base = base_url(ctx, links)?;
    let folder = ctx.check(args.string("folder")?.ok_or("folder is required.")?)?;
    let name = args.string("name")?.map(str::trim).unwrap_or("");
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err("name must be a plain file name, without \"/\".".into());
    }
    let size = args
        .uint("size")?
        .ok_or("size is required: the file's exact size in bytes.")?;
    if size == 0 {
        return Err("size must be at least 1 byte; use write_file for an empty file.".into());
    }
    let overwrite = args.bool("overwrite")?.unwrap_or(false);
    let ttl = ttl(args, links)?;
    let joined = if folder.is_root() {
        format!("/{name}")
    } else {
        format!("{folder}/{name}")
    };
    let path = ctx.policy.check_write(&joined).map_err(storage_error)?;
    match ctx.timed(ctx.storage.stat(&folder, false)).await {
        Ok(entry) if entry.is_dir => {}
        Ok(_) => return Err(format!("{folder} is a file, not a folder.")),
        Err(e) if e.contains("not found") => {
            return Err(format!(
                "The folder {folder} doesn't exist. Create it first with create_folder."
            ));
        }
        Err(e) => return Err(e),
    }
    match ctx.timed(ctx.storage.stat(&path, false)).await {
        Ok(entry) if entry.is_dir => {
            return Err(storage_error(StorageError::IsADirectory(path.to_string())));
        }
        Ok(_) if !overwrite => {
            return Err(storage_error(StorageError::AlreadyExists(path.to_string())));
        }
        Ok(_) => {}
        Err(e) if e.contains("not found") => {}
        Err(e) => return Err(e),
    }
    if size > links.staging_max_bytes {
        return Err(format!(
            "{} is more than the {} an upload may be.",
            human_size(size),
            human_size(links.staging_max_bytes)
        ));
    }
    let now = time::now();
    let (reserved, free) = ctx
        .st(move |state| Ok((state.reserved_bytes(now)?, state.staging_free_bytes()?)))
        .await?;
    if reserved.saturating_add(size) > links.staging_max_bytes {
        return Err(format!(
            "Not enough staging space: {} is promised to other uploads, and {} more would pass \
             the {} limit. Wait for them, or revoke one.",
            human_size(reserved),
            human_size(size),
            human_size(links.staging_max_bytes)
        ));
    }
    if let Some(free) = free
        && free.saturating_sub(reserved) < size.saturating_add(links.reserve_bytes)
    {
        return Err(format!(
            "Not enough free disk space for {} (the server keeps {} free).",
            human_size(size),
            human_size(links.reserve_bytes)
        ));
    }
    let expires = now + ttl;
    let (id, token) = links.key.mint(LinkKind::Upload, expires);
    insert(
        ctx,
        LinkRow {
            id: id.clone(),
            kind: LinkKind::Upload,
            paths: vec![path.to_string()],
            zip: false,
            name: name.to_owned(),
            created: now,
            expires,
            bound_net: None,
            size,
            max_bytes: byte_allowance(size),
            bytes_used: 0,
            committed: 0,
            overwrite,
            state: LinkState::Active,
            note: None,
        },
    )
    .await?;
    let url = format!("{base}/ul/{token}");
    let part = links.part_bytes;
    let parts = size.div_ceil(part);
    let mib = part / (1024 * 1024);
    Ok(format!(
        "Upload link {id} for {path} ({}, {parts} part{} of up to {}), valid until {}. Parts go \
         in order; a part already received is skipped, so rerunning the loop resumes. When the \
         last part arrives the server sends the file to pCloud and checks its size and SHA-1: \
         poll the link with GET until it says \"done\".\n\nURL: {url}\n\n\
         f=/path/to/{}  # the local file, exactly {size} bytes\n\
         split -b {mib}M -d -a 4 \"$f\" cof-part.\n\
         i=0; for p in cof-part.*; do curl -fsS --retry 3 -T \"$p\" {}\"?part=$i\" && echo; i=$((i+1)); done\n\
         until curl -fsS {} | grep -Eq '\"state\":\"(done|failed)\"'; do sleep 5; done; curl -fsS {}; echo\n\
         rm -f cof-part.*",
        human_size(size),
        if parts == 1 { "" } else { "s" },
        human_size(part),
        time::rfc3339(expires),
        sh(name).trim_matches('\''),
        sh(&url),
        sh(&url),
        sh(&url),
    ))
}

fn state_of(row: &LinkRow, now: i64) -> LinkState {
    match row.state {
        LinkState::Active | LinkState::Failed if row.expires <= now => LinkState::Expired,
        state => state,
    }
}

async fn list_links<S: StorageWrite>(ctx: &Ctx<'_, S>, args: &Args<'_>) -> Result<String, String> {
    let now = time::now();
    let one = args.string("id")?.map(str::to_owned);
    let (rows, uses_per) = match &one {
        Some(id) => {
            let id = id.clone();
            let row = ctx.st(move |s| s.link(&id)).await?;
            (row.into_iter().collect::<Vec<_>>(), 200)
        }
        None => (ctx.st(|s| s.links(50)).await?, 3),
    };
    if rows.is_empty() {
        return Ok(match one {
            Some(id) => format!("There is no link {id}."),
            None => "No links yet.".into(),
        });
    }
    let mut text = String::new();
    for row in rows {
        let id = row.id.clone();
        let uses = ctx.st(move |s| s.uses(&id, uses_per)).await?;
        let state = state_of(&row, now);
        let kind = match row.kind {
            LinkKind::Download if row.zip => "zip download",
            LinkKind::Download => "download",
            LinkKind::Upload => "upload",
        };
        let _ = write!(
            text,
            "- {} ({kind}, {}): {} · {} · expires {} · used {} of {}",
            row.id,
            state.as_str(),
            row.paths.join(", "),
            human_size(row.size),
            time::short(row.expires, now),
            human_size(row.bytes_used),
            human_size(row.max_bytes),
        );
        if row.kind == LinkKind::Upload {
            let _ = write!(text, " · {} received", human_size(row.committed));
        }
        if let Some(net) = &row.bound_net {
            let _ = write!(text, " · bound to {net}");
        }
        if let Some(note) = &row.note {
            let _ = write!(text, " · {note}");
        }
        text.push('\n');
        for u in &uses {
            let _ = writeln!(
                text,
                "    {} {} {} {} {}",
                time::short(u.at, now),
                u.ip,
                if u.range.is_empty() { "-" } else { &u.range },
                human_size(u.bytes),
                u.status
            );
        }
    }
    Ok(text)
}

/// The answer for a link that revoke_link must leave alone, or None if it can be revoked.
fn not_revocable(id: &str, state: LinkState) -> Option<Result<String, String>> {
    match state {
        LinkState::Active | LinkState::Failed => None,
        LinkState::Revoked => Some(Ok(format!("Link {id} was already revoked."))),
        LinkState::Expired => Some(Ok(format!(
            "Link {id} has expired; it answers 410 already."
        ))),
        LinkState::Done => Some(Ok(format!(
            "Link {id} is finished (its upload is on the drive); nothing to revoke."
        ))),
        LinkState::Finishing => Some(Err(format!(
            "Link {id} is sending its file to pCloud right now; revoke it after that, or \
             delete the file."
        ))),
    }
}

async fn revoke_link<S: StorageWrite>(ctx: &Ctx<'_, S>, args: &Args<'_>) -> Result<String, String> {
    let id = args
        .string("id")?
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("id is required.")?
        .to_owned();
    let wanted = id.clone();
    let row = ctx
        .st(move |s| s.link(&wanted))
        .await?
        .ok_or_else(|| format!("There is no link {id}."))?;
    if let Some(answer) = not_revocable(&id, row.state) {
        return answer;
    }
    // Only from Active or Failed, in one step: an upload that moved on to Finishing meanwhile
    // is mid-copy to pCloud and must not lose its staging file under it.
    let wanted = id.clone();
    let revoked = ctx
        .st(move |s| {
            s.transition(
                &wanted,
                &[LinkState::Active, LinkState::Failed],
                LinkState::Revoked,
                Some("revoked"),
            )
        })
        .await?;
    if !revoked {
        let wanted = id.clone();
        let now = ctx
            .st(move |s| s.link(&wanted))
            .await?
            .ok_or_else(|| format!("There is no link {id}."))?;
        return not_revocable(&id, now.state).unwrap_or_else(|| {
            Err(format!(
                "Link {id} changed while it was being revoked; try again."
            ))
        });
    }
    let extra = if row.kind == LinkKind::Upload {
        " Its partial upload is deleted."
    } else {
        ""
    };
    Ok(format!(
        "Revoked link {id}; it answers 410 from now on.{extra}"
    ))
}
