//! A [`Storage`] over rclone's remote-control API: `rclone rcd --rc-serve` on loopback, in the
//! same pod, with the pCloud remote configured from env vars.
//!
//! What it relies on (measured in docs/design/phase0-rclone.md):
//!
//! - **No filters, ever.** Any directory filter makes rclone drop pCloud's one-call recursive
//!   listing, so this client never sends `_filter`; [`chance_of_files_core::Policy`] decides
//!   what is visible, and walks split the tree around excluded folders instead.
//! - **rc-serve reads local files too** (`/[/abs/dir]/file`), so a read URL is built only from
//!   the configured remote name and a [`CheckedPath`], never from caller input.
//! - **rc-serve answers an unsatisfiable range with a broken, empty `206`**, so ranges are
//!   checked against a fresh `stat` and the answer's `Content-Range` is checked against the
//!   request.
//! - Only the rc calls in [`Method`] can be sent: no `operations/cleanup` (empties the trash)
//!   and no `operations/publiclink`. Deletes are `deletefile` and `purge`, which pCloud puts in
//!   its trash; other backends delete for good, so there [`Capabilities::trash_on_delete`] is
//!   false and the core refuses deletes.
//! - Folder moves, copies and purges are sent with `_async` and followed with `job/status`.
//!   A folder move with no filter active is one server-side rename on pCloud.
//! - Uploads go through a local staging folder that the rclone process can read at the same
//!   path (`operations/copyfile` from it): pCloud has no streaming upload, so rclone would
//!   otherwise spool every upload to its own small temp folder first.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use base64::Engine as _;
use bytes::Bytes;
use chance_of_files_core::{
    ByteRange, ByteStream, Capabilities, CheckedPath, Done, Entry, HashKind, JobId, JobInfo,
    JobState, PathSet, Storage, StorageError, StorageWrite, split_walk_each, time,
};
use futures_util::StreamExt;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::header::{AUTHORIZATION, CONTENT_RANGE, CONTENT_TYPE, HeaderValue, RANGE};
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// The rc calls this client can make. A closed set, so nothing else can be sent by mistake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    List,
    Stat,
    About,
    FsInfo,
    Mkdir,
    MoveFile,
    CopyFile,
    /// To pCloud's trash (pCloud's own `deletefile`).
    DeleteFile,
    /// A folder to pCloud's trash (`deletefolderrecursive`).
    Purge,
    SyncMove,
    SyncCopy,
    JobStatus,
    /// Progress of one job (`group: job/<id>`).
    Stats,
}

impl Method {
    pub const ALL: [Self; 13] = [
        Self::List,
        Self::Stat,
        Self::About,
        Self::FsInfo,
        Self::Mkdir,
        Self::MoveFile,
        Self::CopyFile,
        Self::DeleteFile,
        Self::Purge,
        Self::SyncMove,
        Self::SyncCopy,
        Self::JobStatus,
        Self::Stats,
    ];

    pub fn path(self) -> &'static str {
        match self {
            Self::List => "operations/list",
            Self::Stat => "operations/stat",
            Self::About => "operations/about",
            Self::FsInfo => "operations/fsinfo",
            Self::Mkdir => "operations/mkdir",
            Self::MoveFile => "operations/movefile",
            Self::CopyFile => "operations/copyfile",
            Self::DeleteFile => "operations/deletefile",
            Self::Purge => "operations/purge",
            Self::SyncMove => "sync/move",
            Self::SyncCopy => "sync/copy",
            Self::JobStatus => "job/status",
            Self::Stats => "core/stats",
        }
    }
}

/// Where rclone is and which remote to use.
#[derive(Clone)]
pub struct RcloneConfig {
    /// The rc address, e.g. `http://127.0.0.1:5572`. Plain HTTP on loopback only: the rc
    /// password is root-equivalent inside the rclone container, so it never leaves the pod.
    pub url: String,
    pub user: String,
    pub password: String,
    /// The remote's name, with or without the trailing colon (`pcloud` or `pcloud:`).
    pub remote: String,
    /// Most bytes of one rc answer taken in (a recursive listing of 133k entries is 35 MB).
    pub max_listing_bytes: u64,
    /// A local folder both this process and rclone see at this same path, for uploads. Without
    /// it, `put_bytes` and `put_local` are refused.
    pub staging: Option<PathBuf>,
}

impl fmt::Debug for RcloneConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RcloneConfig")
            .field("url", &self.url)
            .field("user", &self.user)
            .field("remote", &self.remote)
            .field("max_listing_bytes", &self.max_listing_bytes)
            .field("staging", &self.staging)
            .finish_non_exhaustive()
    }
}

pub struct RcloneStorage {
    http: reqwest::Client,
    /// The rc URL without a trailing slash.
    base: String,
    authorization: HeaderValue,
    /// `pcloud:`, as rc calls name the remote.
    fs: String,
    /// The rc-serve URL prefix for this remote, `<base>/[pcloud:]`.
    serve_prefix: String,
    max_listing_bytes: u64,
    staging: Option<PathBuf>,
    capabilities: OnceLock<Capabilities>,
}

impl fmt::Debug for RcloneStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the Authorization header: it is the rc password in base64.
        f.debug_struct("RcloneStorage")
            .field("base", &self.base)
            .field("fs", &self.fs)
            .finish_non_exhaustive()
    }
}

/// Characters left as they are in a path segment of an rc-serve URL: RFC 3986 unreserved.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

impl RcloneStorage {
    pub fn new(config: RcloneConfig) -> Result<Self, String> {
        let url = Url::parse(config.url.trim())
            .map_err(|e| format!("rclone url {:?} is not a URL: {e}", config.url))?;
        if url.scheme() != "http" {
            return Err(format!(
                "rclone url must be plain http on loopback (got scheme {:?})",
                url.scheme()
            ));
        }
        let host = url.host_str().unwrap_or("");
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if !loopback {
            return Err(
                "rclone url must point at loopback (127.0.0.1, [::1] or localhost): the rc \
                 password must never leave the pod"
                    .into(),
            );
        }
        if !url.username().is_empty() || url.password().is_some() || url.query().is_some() {
            return Err("rclone url must not carry credentials or a query".into());
        }
        let remote = config.remote.trim().trim_end_matches(':');
        if remote.is_empty()
            || !remote
                .chars()
                .all(|c| c.is_alphanumeric() || "_-.+@ ".contains(c))
            || remote.starts_with(['-', ' '])
            || remote.ends_with(' ')
        {
            return Err(format!(
                "rclone remote {:?} is not a remote name (letters, digits, _ - . + @ and spaces)",
                config.remote
            ));
        }
        let fs = format!("{remote}:");
        let base = url.as_str().trim_end_matches('/').to_owned();
        let serve_prefix = format!("{base}/[{}]", utf8_percent_encode(&fs, SEGMENT));
        let credentials = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", config.user, config.password));
        let mut authorization = HeaderValue::from_str(&format!("Basic {credentials}"))
            .map_err(|_| "rclone user or password has characters a header can't carry")?;
        authorization.set_sensitive(true);
        if let Some(staging) = &config.staging {
            let text = staging.to_string_lossy();
            // rclone reads "x:y" as remote x; a staging path must stay a local path.
            if !staging.is_absolute() || text.contains(':') || text.contains(',') {
                return Err(format!(
                    "the staging folder {text:?} must be an absolute path without ':' or ','"
                ));
            }
        }
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            // Loopback: a proxy from the environment must never see the rc password.
            .no_proxy()
            .build()
            .map_err(|e| format!("HTTP client: {e}"))?;
        Ok(Self {
            http,
            base,
            authorization,
            fs,
            serve_prefix,
            max_listing_bytes: config.max_listing_bytes,
            staging: config.staging,
            capabilities: OnceLock::new(),
        })
    }

    /// The remote as rc calls name it, e.g. `pcloud:`.
    pub fn fs(&self) -> &str {
        &self.fs
    }

    /// Asks rclone what the remote supports and remembers it for
    /// [`Storage::capabilities`]. Until this succeeds, capabilities are empty.
    pub async fn load_capabilities(&self) -> Result<Capabilities, StorageError> {
        if let Some(known) = self.capabilities.get() {
            return Ok(known.clone());
        }
        let info: FsInfo = self.rc(Method::FsInfo, json!({"fs": self.fs}), "/").await?;
        let capabilities = Capabilities {
            // pCloud's deletefile and deletefolderrecursive land in its trash (the owner
            // checked, phase 0); every other backend's deletes are final.
            trash_on_delete: info.string.to_ascii_lowercase().starts_with("pcloud "),
            hashes: info
                .hashes
                .iter()
                .filter_map(|h| HashKind::from_name(h))
                .collect(),
            server_dir_move: info
                .features
                .get("DirMove")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        Ok(self.capabilities.get_or_init(|| capabilities).clone())
    }

    /// Space on the remote. Cheap (one API call), so it doubles as the health check.
    pub async fn about(&self) -> Result<About, StorageError> {
        self.rc(Method::About, json!({"fs": self.fs}), "/").await
    }

    /// One rc call, its answer read up to `max_listing_bytes` and parsed. `path` is only for
    /// error messages.
    async fn rc<T: DeserializeOwned>(
        &self,
        method: Method,
        body: Value,
        path: &str,
    ) -> Result<T, StorageError> {
        let response = self.send(method, body).await?;
        let status = response.status();
        let body = self.read_body(response, path).await?;
        if status.is_success() {
            return serde_json::from_slice(&body).map_err(|e| {
                StorageError::Backend(format!(
                    "rclone {} answered unexpected JSON: {e}",
                    method.path()
                ))
            });
        }
        Err(rc_error(method, status, &body, path))
    }

    async fn send(&self, method: Method, body: Value) -> Result<reqwest::Response, StorageError> {
        let url = format!("{}/{}", self.base, method.path());
        self.http
            .post(url)
            .header(AUTHORIZATION, self.authorization.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| transport_error(method.path(), e))
    }

    /// A whole answer, up to `max_listing_bytes`.
    async fn read_body(
        &self,
        response: reqwest::Response,
        path: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let limit = self.max_listing_bytes;
        let too_big = || {
            StorageError::TooManyEntries(format!(
                "{path} (an rclone answer over the {limit}-byte limit)"
            ))
        };
        if response.content_length().is_some_and(|n| n > limit) {
            return Err(too_big());
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| transport_error("rc answer", e))?;
            if (body.len() + chunk.len()) as u64 > limit {
                return Err(too_big());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

fn entry(item: Item) -> Entry {
    let path = format!("/{}", item.path.trim_start_matches('/'));
    let size = (!item.is_dir)
        .then(|| u64::try_from(item.size).ok())
        .flatten();
    Entry {
        name: if item.name.is_empty() {
            path.rsplit('/').next().unwrap_or("").to_owned()
        } else {
            item.name
        },
        path,
        is_dir: item.is_dir,
        size,
        modified: item.mod_time.as_deref().map(utc_seconds),
        content_type: item
            .mime_type
            .filter(|m| !item.is_dir && !m.is_empty() && m != "inode/directory"),
        id: item.id.filter(|id| !id.is_empty()),
        hashes: item
            .hashes
            .into_iter()
            .flatten()
            .filter_map(|(kind, value)| {
                let value = value.as_str()?.to_owned();
                Some((HashKind::from_name(&kind)?, value)).filter(|(_, v)| !v.is_empty())
            })
            .collect(),
    }
}

impl RcloneStorage {
    /// `operations/list` of one folder, plain or recursive, each entry handed to `each` as
    /// it is parsed. Items outside the folder (which rclone never sends) are dropped rather
    /// than trusted.
    async fn list_each(
        &self,
        dir: &CheckedPath,
        recurse: bool,
        each: &mut (dyn FnMut(Entry) + Send),
    ) -> Result<(), StorageError> {
        let mut opt = serde_json::Map::new();
        if recurse {
            opt.insert("recurse".into(), json!(true));
        }
        let body = json!({"fs": self.fs, "remote": dir.relative(), "opt": opt});
        let prefix = if dir.is_root() {
            "/".to_owned()
        } else {
            format!("{}/", dir.as_str())
        };
        let mut keep = |entry: Entry| {
            if entry.path.starts_with(&prefix) && entry.path.len() > prefix.len() {
                each(entry);
            }
        };
        match self.rc_list(body, dir.as_str(), &mut keep).await {
            Err(StorageError::NotFound(_)) => Err(self.why_unlistable(dir).await),
            other => other,
        }
    }

    /// One `operations/list` call whose answer is parsed as it arrives, on a blocking thread:
    /// a recursive listing of Immich is a 35 MB body, and neither it nor a vector of every
    /// item is ever held whole.
    async fn rc_list(
        &self,
        body: Value,
        path: &str,
        each: &mut (dyn FnMut(Entry) + Send),
    ) -> Result<(), StorageError> {
        let method = Method::List;
        let response = self.send(method, body).await?;
        let status = response.status();
        let limit = self.max_listing_bytes;
        let too_big = || {
            StorageError::TooManyEntries(format!(
                "{path} (an rclone answer over the {limit}-byte limit)"
            ))
        };
        if response.content_length().is_some_and(|n| n > limit) {
            return Err(too_big());
        }
        if !status.is_success() {
            let body = self.read_body(response, path).await?;
            return Err(rc_error(method, status, &body, path));
        }

        let (chunks_tx, chunks_rx) = tokio::sync::mpsc::channel::<Result<Bytes, Pump>>(8);
        let (items_tx, mut items_rx) = tokio::sync::mpsc::channel::<Entry>(1024);
        let parser = tokio::task::spawn_blocking(move || {
            let reader = ChunkReader {
                chunks: chunks_rx,
                current: Bytes::new(),
            };
            parse_listing(reader, &items_tx)
        });
        let pump = async move {
            let mut stream = response.bytes_stream();
            let mut seen: u64 = 0;
            while let Some(chunk) = stream.next().await {
                let item = match chunk {
                    Ok(chunk) => {
                        seen += chunk.len() as u64;
                        if seen > limit {
                            Err(Pump::TooBig)
                        } else {
                            Ok(chunk)
                        }
                    }
                    Err(e) => Err(Pump::Transport(transport_error(method.path(), e))),
                };
                let failed = item.is_err();
                // The parser hung up: it failed, and says why itself.
                if chunks_tx.send(item).await.is_err() || failed {
                    return;
                }
            }
        };
        let receive = async {
            while let Some(entry) = items_rx.recv().await {
                each(entry);
            }
        };
        let ((), ()) = tokio::join!(pump, receive);
        match parser.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(ParseError::Pump(Pump::TooBig))) => Err(too_big()),
            Ok(Err(ParseError::Pump(Pump::Transport(err)))) => Err(err),
            Ok(Err(ParseError::Json(e))) => Err(StorageError::Backend(format!(
                "rclone {} answered unexpected JSON: {e}",
                method.path()
            ))),
            Err(e) => Err(StorageError::Backend(format!("listing parser failed: {e}"))),
        }
    }

    /// rclone says "directory not found" for a file too; a stat tells them apart.
    async fn why_unlistable(&self, dir: &CheckedPath) -> StorageError {
        match self.stat(dir, false).await {
            Ok(entry) if !entry.is_dir => StorageError::NotADirectory(dir.to_string()),
            Ok(_) | Err(StorageError::NotFound(_)) => StorageError::NotFound(dir.to_string()),
            Err(err) => err,
        }
    }

    /// The rc-serve URL of a file: the fixed remote plus each segment percent-encoded.
    fn serve_url(&self, path: &CheckedPath) -> String {
        let mut url = self.serve_prefix.clone();
        for segment in path.relative().split('/') {
            url.push('/');
            url.extend(utf8_percent_encode(segment, SEGMENT));
        }
        url
    }
}

impl Storage for RcloneStorage {
    fn capabilities(&self) -> Capabilities {
        self.capabilities.get().cloned().unwrap_or_default()
    }

    async fn list(&self, path: &CheckedPath) -> Result<Vec<Entry>, StorageError> {
        let mut out = Vec::new();
        self.list_each(path, false, &mut |e| out.push(e)).await?;
        Ok(out)
    }

    async fn walk(
        &self,
        root: &CheckedPath,
        exclude: &[CheckedPath],
    ) -> Result<Vec<Entry>, StorageError> {
        let mut out = Vec::new();
        self.walk_each(root, exclude, &mut |e| out.push(e)).await?;
        Ok(out)
    }

    async fn walk_each(
        &self,
        root: &CheckedPath,
        exclude: &[CheckedPath],
        each: &mut (dyn FnMut(Entry) + Send),
    ) -> Result<(), StorageError> {
        let excluded = PathSet::new(exclude.iter().map(CheckedPath::as_str))?;
        if excluded.covers(root.as_str()) {
            return Ok(());
        }
        if excluded.any_below(root.as_str()) {
            return split_walk_each(self, root, exclude, each).await;
        }
        self.list_each(root, true, each).await
    }

    async fn stat(&self, path: &CheckedPath, with_hash: bool) -> Result<Entry, StorageError> {
        if path.is_root() {
            return Ok(Entry {
                path: "/".into(),
                is_dir: true,
                ..Entry::default()
            });
        }
        let body = json!({
            "fs": self.fs,
            "remote": path.relative(),
            "opt": {"showHash": with_hash},
        });
        let stat: Stat = self.rc(Method::Stat, body, path.as_str()).await?;
        let item = stat
            .item
            .ok_or_else(|| StorageError::NotFound(path.to_string()))?;
        let mut entry = entry(item);
        // The path asked for, as the policy checked it, not as rclone spells it back.
        entry.path = path.to_string();
        Ok(entry)
    }

    async fn open(
        &self,
        path: &CheckedPath,
        range: Option<ByteRange>,
    ) -> Result<ByteStream, StorageError> {
        // Also keeps folders away from rc-serve, which would answer with an HTML index.
        let entry = self.stat(path, false).await?;
        if entry.is_dir {
            return Err(StorageError::IsADirectory(path.to_string()));
        }
        let size = entry.size.unwrap_or(0);
        let range = range.unwrap_or(ByteRange {
            offset: 0,
            length: size,
        });
        if range.length == 0 || range.offset >= size {
            return Ok(chance_of_files_core::stream_of(Bytes::new()));
        }
        let last = range.offset.saturating_add(range.length).min(size) - 1;
        let wanted = last - range.offset + 1;
        let response = self
            .http
            .get(self.serve_url(path))
            .header(AUTHORIZATION, self.authorization.clone())
            .header(RANGE, format!("bytes={}-{last}", range.offset))
            .send()
            .await
            .map_err(|e| transport_error("rc-serve GET", e))?;
        let skip = match response.status() {
            StatusCode::PARTIAL_CONTENT => {
                let content_range = response
                    .headers()
                    .get(CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_content_range);
                match content_range {
                    Some((start, end, _)) if start == range.offset && end >= start => 0,
                    _ => {
                        return Err(StorageError::Backend(format!(
                            "rclone answered a range read of {path} with a Content-Range that \
                             doesn't match the request"
                        )));
                    }
                }
            }
            // The range was ignored; skip to it.
            StatusCode::OK => range.offset,
            // The file shrank since the stat.
            StatusCode::RANGE_NOT_SATISFIABLE => {
                return Ok(chance_of_files_core::stream_of(Bytes::new()));
            }
            StatusCode::NOT_FOUND => return Err(StorageError::NotFound(path.to_string())),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(StorageError::Backend(
                    "rclone refused the rc credentials".into(),
                ));
            }
            status => {
                return Err(StorageError::Backend(format!(
                    "rclone answered a read of {path} with HTTP {status}"
                )));
            }
        };
        Ok(capped(response.bytes_stream(), skip, wanted))
    }

    async fn health(&self) -> Result<(), StorageError> {
        self.about().await.map(|_| ())
    }
}

impl RcloneStorage {
    /// The remote rooted at `path` (`pcloud:a/b`), for `sync/*` calls on folders.
    fn fs_at(&self, path: &CheckedPath) -> String {
        format!("{}{}", self.fs, path.relative())
    }

    /// Sends a call with `_async` and returns rclone's job id.
    async fn start_job(
        &self,
        method: Method,
        mut body: Value,
        path: &str,
    ) -> Result<JobId, StorageError> {
        body["_async"] = json!(true);
        let answer: Value = self.rc(method, body, path).await?;
        answer
            .get("jobid")
            .and_then(Value::as_i64)
            .map(|id| JobId(id.to_string()))
            .ok_or_else(|| {
                StorageError::Backend(format!("rclone {} returned no job id", method.path()))
            })
    }

    /// The staging folder, or why uploads can't work.
    fn staging(&self) -> Result<&Path, StorageError> {
        self.staging.as_deref().ok_or_else(|| {
            StorageError::Unsupported("uploads need a staging folder rclone can read".into())
        })
    }

    async fn progress(&self, jobid: i64) -> Option<String> {
        let stats: Value = self
            .rc(
                Method::Stats,
                json!({"group": format!("job/{jobid}")}),
                "job",
            )
            .await
            .ok()?;
        let n = |key: &str| stats.get(key).and_then(Value::as_u64).unwrap_or(0);
        let (bytes, total) = (n("bytes"), n("totalBytes"));
        let (files, all) = (n("transfers"), n("totalTransfers"));
        if total == 0 && all == 0 {
            return None;
        }
        Some(format!(
            "{} of {}, {files} of {all} files",
            mib(bytes),
            mib(total.max(bytes))
        ))
    }
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

impl StorageWrite for RcloneStorage {
    async fn mkdir(&self, path: &CheckedPath) -> Result<Entry, StorageError> {
        let body = json!({"fs": self.fs, "remote": path.relative()});
        let _: Value = self.rc(Method::Mkdir, body, path.as_str()).await?;
        self.stat(path, false).await
    }

    async fn rename(&self, from: &CheckedPath, to: &CheckedPath) -> Result<Done, StorageError> {
        let source = self.stat(from, false).await?;
        if source.is_dir {
            // No filter is ever active, so rclone tries pCloud's one-call folder rename first.
            let body = json!({
                "srcFs": self.fs_at(from),
                "dstFs": self.fs_at(to),
                "deleteEmptySrcDirs": true,
                "createEmptySrcDirs": true,
            });
            return Ok(Done::Job(
                self.start_job(Method::SyncMove, body, from.as_str())
                    .await?,
            ));
        }
        let body = json!({
            "srcFs": self.fs, "srcRemote": from.relative(),
            "dstFs": self.fs, "dstRemote": to.relative(),
        });
        let _: Value = self.rc(Method::MoveFile, body, from.as_str()).await?;
        Ok(Done::Now(self.stat(to, false).await?))
    }

    async fn copy(&self, from: &CheckedPath, to: &CheckedPath) -> Result<Done, StorageError> {
        let source = self.stat(from, false).await?;
        if source.is_dir {
            let body = json!({
                "srcFs": self.fs_at(from),
                "dstFs": self.fs_at(to),
                "createEmptySrcDirs": true,
            });
            return Ok(Done::Job(
                self.start_job(Method::SyncCopy, body, from.as_str())
                    .await?,
            ));
        }
        let body = json!({
            "srcFs": self.fs, "srcRemote": from.relative(),
            "dstFs": self.fs, "dstRemote": to.relative(),
        });
        let _: Value = self.rc(Method::CopyFile, body, from.as_str()).await?;
        Ok(Done::Now(self.stat(to, false).await?))
    }

    async fn trash(&self, path: &CheckedPath) -> Result<Done, StorageError> {
        if !self.capabilities().trash_on_delete {
            return Err(StorageError::Unsupported(
                "this remote has no trash, and nothing here deletes for good".into(),
            ));
        }
        let entry = self.stat(path, false).await?;
        let body = json!({"fs": self.fs, "remote": path.relative()});
        if entry.is_dir {
            return Ok(Done::Job(
                self.start_job(Method::Purge, body, path.as_str()).await?,
            ));
        }
        let _: Value = self.rc(Method::DeleteFile, body, path.as_str()).await?;
        Ok(Done::Now(entry))
    }

    async fn put_bytes(&self, data: Bytes, to: &CheckedPath) -> Result<Entry, StorageError> {
        let staging = self.staging()?.to_owned();
        let name = format!(
            ".put-{}",
            chance_of_files_core::links::random_bytes::<8>()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let local = staging.join(name);
        let file = local.clone();
        tokio::task::spawn_blocking(move || std::fs::write(&file, &data))
            .await
            .map_err(|e| StorageError::Backend(format!("staging write failed: {e}")))?
            .map_err(|e| StorageError::Backend(format!("can't write the staging file: {e}")))?;
        let result = self.put_local(&local, to).await;
        let _ = std::fs::remove_file(&local);
        result
    }

    async fn put_local(&self, local: &Path, to: &CheckedPath) -> Result<Entry, StorageError> {
        let staging = self.staging()?;
        // Only files directly in the staging folder: rclone can read any local path, so the
        // source is never anything a caller chose.
        let name = local
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|_| local.parent() == Some(staging))
            .ok_or_else(|| {
                StorageError::InvalidPath("uploads come only from the staging folder".into())
            })?;
        let body = json!({
            "srcFs": staging.to_string_lossy(), "srcRemote": name,
            "dstFs": self.fs, "dstRemote": to.relative(),
        });
        let _: Value = self.rc(Method::CopyFile, body, to.as_str()).await?;
        self.stat(to, false).await
    }

    async fn job(&self, id: &JobId) -> Result<JobInfo, StorageError> {
        let jobid: i64 =
            id.0.parse()
                .map_err(|_| StorageError::InvalidPath(format!("{id} is not an rclone job id")))?;
        let response = self
            .send(Method::JobStatus, json!({"jobid": jobid}))
            .await?;
        let status = response.status();
        let body = self.read_body(response, "job").await?;
        if !status.is_success() {
            if String::from_utf8_lossy(&body).contains("job not found") {
                return Ok(JobInfo {
                    state: JobState::Lost,
                    started: None,
                    progress: None,
                });
            }
            return Err(rc_error(Method::JobStatus, status, &body, "job"));
        }
        let answer: JobAnswer = serde_json::from_slice(&body).map_err(|e| {
            StorageError::Backend(format!("rclone job/status answered unexpected JSON: {e}"))
        })?;
        let started = answer.start_time.as_deref().and_then(time::parse_rfc3339);
        let state = match (answer.finished, answer.success) {
            (false, _) => JobState::Running,
            (true, true) => JobState::Done,
            (true, false) => JobState::Failed(if answer.error.is_empty() {
                "rclone reported a failure without a message".into()
            } else {
                answer.error.chars().take(500).collect()
            }),
        };
        let progress = if state == JobState::Running {
            self.progress(jobid).await
        } else {
            None
        };
        Ok(JobInfo {
            state,
            started,
            progress,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobAnswer {
    #[serde(default)]
    finished: bool,
    #[serde(default)]
    success: bool,
    #[serde(default)]
    error: String,
    #[serde(default)]
    start_time: Option<String>,
}

/// Why the body pump stopped feeding the parser.
#[derive(Debug)]
enum Pump {
    TooBig,
    Transport(StorageError),
}

#[derive(Debug)]
enum ParseError {
    Pump(Pump),
    Json(serde_json::Error),
}

/// The answer's chunks as a `Read`, for serde_json on a blocking thread.
struct ChunkReader {
    chunks: tokio::sync::mpsc::Receiver<Result<Bytes, Pump>>,
    current: Bytes,
}

/// Carries a pump failure through serde_json's `io::Error`.
#[derive(Debug)]
struct PumpFailed(Pump);

impl fmt::Display for PumpFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the rclone answer stopped")
    }
}

impl std::error::Error for PumpFailed {}

impl std::io::Read for ChunkReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.current.is_empty() {
            match self.chunks.blocking_recv() {
                Some(Ok(chunk)) => self.current = chunk,
                Some(Err(pump)) => {
                    return Err(std::io::Error::other(PumpFailed(pump)));
                }
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.current.len());
        buf[..n].copy_from_slice(&self.current[..n]);
        self.current = self.current.slice(n..);
        Ok(n)
    }
}

/// Parses `{"list": [item, ...]}` item by item, sending each as an [`Entry`].
fn parse_listing(
    reader: ChunkReader,
    items: &tokio::sync::mpsc::Sender<Entry>,
) -> Result<(), ParseError> {
    use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};

    struct Top<'a>(&'a tokio::sync::mpsc::Sender<Entry>);
    struct List<'a>(&'a tokio::sync::mpsc::Sender<Entry>);

    impl<'de> DeserializeSeed<'de> for Top<'_> {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
            d.deserialize_map(self)
        }
    }

    impl<'de> Visitor<'de> for Top<'_> {
        type Value = ();
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an operations/list answer")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
                if key == "list" {
                    map.next_value_seed(List(self.0))?;
                } else {
                    map.next_value::<IgnoredAny>()?;
                }
            }
            Ok(())
        }
    }

    impl<'de> DeserializeSeed<'de> for List<'_> {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
            d.deserialize_any(self)
        }
    }

    impl<'de> Visitor<'de> for List<'_> {
        type Value = ();
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a list of items")
        }
        fn visit_unit<E>(self) -> Result<(), E> {
            Ok(())
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            while let Some(item) = seq.next_element::<Item>()? {
                if self.0.blocking_send(entry(item)).is_err() {
                    // The receiver is gone (the walk was dropped); stop parsing.
                    return Err(serde::de::Error::custom("listing abandoned"));
                }
            }
            Ok(())
        }
    }

    let mut de = serde_json::Deserializer::from_reader(reader);
    let result = Top(items).deserialize(&mut de).and_then(|()| de.end());
    result.map_err(|e| {
        if e.io_error_kind().is_none() {
            return ParseError::Json(e);
        }
        let io: std::io::Error = e.into();
        match io.into_inner().map(|inner| inner.downcast::<PumpFailed>()) {
            Some(Ok(failed)) => ParseError::Pump(failed.0),
            Some(Err(other)) => {
                ParseError::Json(serde_json::Error::io(std::io::Error::other(other)))
            }
            None => ParseError::Json(serde_json::Error::io(std::io::Error::other(
                "reading the answer failed",
            ))),
        }
    })
}

/// The body from `skip` bytes in, at most `take` bytes, stopping the transfer once it has
/// them.
fn capped<S>(body: S, skip: u64, take: u64) -> ByteStream
where
    S: futures_util::Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
{
    let state = (Box::pin(body), skip, take);
    Box::pin(futures_util::stream::unfold(
        state,
        |(mut body, mut skip, mut take)| async move {
            loop {
                if take == 0 {
                    return None;
                }
                let mut chunk = match body.next().await? {
                    Ok(chunk) => chunk,
                    Err(e) => {
                        return Some((Err(transport_error("rc-serve GET", e)), (body, 0, 0)));
                    }
                };
                if skip > 0 {
                    let drop = skip.min(chunk.len() as u64);
                    skip -= drop;
                    chunk = chunk.slice(drop as usize..);
                }
                if chunk.is_empty() {
                    continue;
                }
                let keep = take.min(chunk.len() as u64);
                take -= keep;
                return Some((Ok(chunk.slice(..keep as usize)), (body, skip, take)));
            }
        },
    ))
}

/// `bytes 10-19/100` as (10, 19, Some(100)).
fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (span, total) = rest.split_once('/')?;
    let (start, end) = span.split_once('-')?;
    let total = match total.trim() {
        "*" => None,
        n => Some(n.parse().ok()?),
    };
    Some((start.trim().parse().ok()?, end.trim().parse().ok()?, total))
}

/// A connection-level failure, without the URL (reqwest puts it in its messages).
fn transport_error(what: &str, err: reqwest::Error) -> StorageError {
    let err = err.without_url();
    let kind = if err.is_connect() {
        "can't connect to rclone"
    } else if err.is_timeout() {
        "rclone timed out"
    } else {
        "rclone connection failed"
    };
    StorageError::Backend(format!("{what}: {kind}: {err}"))
}

/// An rc error answer, `{"error": "...", "status": 404, ...}`, as a storage error.
fn rc_error(method: Method, status: StatusCode, body: &[u8], path: &str) -> StorageError {
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return StorageError::Backend("rclone refused the rc credentials".into());
    }
    let message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| format!("HTTP {status}"));
    let lower = message.to_lowercase();
    if status == StatusCode::NOT_FOUND
        || lower.contains("directory not found")
        || lower.contains("object not found")
    {
        return StorageError::NotFound(path.to_owned());
    }
    if lower.contains("is a file not a directory") || lower.contains("not a directory") {
        return StorageError::NotADirectory(path.to_owned());
    }
    StorageError::Backend(format!("rclone {}: {message}", method.path()))
}

/// rclone's `2026-10-29T12:00:00.123456789Z` as `2026-10-29T12:00:00Z` (pCloud keeps whole
/// seconds anyway). Anything else is passed on as it is.
fn utc_seconds(time: &str) -> String {
    match time.split_once('.') {
        Some((seconds, fraction))
            if fraction.ends_with('Z')
                && fraction[..fraction.len() - 1]
                    .chars()
                    .all(|c| c.is_ascii_digit()) =>
        {
            format!("{seconds}Z")
        }
        _ => time.to_owned(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Item {
    #[serde(default)]
    path: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: i64,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    mod_time: Option<String>,
    #[serde(default)]
    is_dir: bool,
    #[serde(rename = "ID", default)]
    id: Option<String>,
    #[serde(default)]
    hashes: Option<serde_json::Map<String, Value>>,
}

#[derive(Debug, Deserialize)]
struct Stat {
    item: Option<Item>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct FsInfo {
    /// e.g. `pcloud root ''`: the backend type comes first.
    #[serde(default)]
    string: String,
    #[serde(default)]
    hashes: Vec<String>,
    #[serde(default)]
    features: serde_json::Map<String, Value>,
}

/// `operations/about`: bytes on the remote, where it reports them.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct About {
    pub total: Option<u64>,
    pub used: Option<u64>,
    pub free: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_harmless_methods_exist() {
        for method in Method::ALL {
            let path = method.path();
            assert!(
                ["operations/", "sync/", "job/status", "core/stats"]
                    .iter()
                    .any(|p| path.starts_with(p)),
                "{path}"
            );
            // Emptying the trash, public links, two-way sync, config and process control
            // can't be sent at all.
            for forbidden in [
                "cleanup",
                "publiclink",
                "rmdirs",
                "bisync",
                "config/",
                "core/command",
                "core/quit",
                "backend/",
                "job/stop",
            ] {
                assert!(!path.contains(forbidden), "{path}");
            }
        }
    }

    /// Feeds `body` to the streaming parser in `chunk`-byte pieces.
    fn parse_in_pieces(body: &[u8], chunk: usize) -> (Vec<Entry>, Result<(), ParseError>) {
        let (chunks_tx, chunks_rx) = tokio::sync::mpsc::channel(body.len() / chunk + 2);
        for piece in body.chunks(chunk) {
            chunks_tx
                .try_send(Ok(Bytes::copy_from_slice(piece)))
                .unwrap();
        }
        drop(chunks_tx);
        let (items_tx, mut items_rx) = tokio::sync::mpsc::channel(1000);
        let reader = ChunkReader {
            chunks: chunks_rx,
            current: Bytes::new(),
        };
        let result = parse_listing(reader, &items_tx);
        drop(items_tx);
        let mut entries = Vec::new();
        while let Ok(entry) = items_rx.try_recv() {
            entries.push(entry);
        }
        (entries, result)
    }

    #[test]
    fn listings_parse_as_a_stream_whatever_the_chunking() {
        let body = br#"{"other": [1, {"x": "y"}], "list": [
            {"Path": "a/b.txt", "Name": "b.txt", "Size": 3, "ModTime": "2026-10-29T12:00:00.5Z", "IsDir": false, "ID": "f1"},
            {"Path": "a/c", "Name": "c", "Size": -1, "IsDir": true, "ID": "d2", "MimeType": "inode/directory"}
        ], "after": null}"#;
        for chunk in [1, 7, body.len()] {
            let (entries, result) = parse_in_pieces(body, chunk);
            assert!(result.is_ok(), "{chunk}: {result:?}");
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].path, "/a/b.txt");
            assert_eq!(entries[0].modified.as_deref(), Some("2026-10-29T12:00:00Z"));
            assert_eq!(entries[1].size, None);
            assert!(entries[1].is_dir && entries[1].content_type.is_none());
        }
        let (_, empty) = parse_in_pieces(br#"{"list": null}"#, 4);
        assert!(empty.is_ok());
        let cut = body.windows(7).position(|w| w == b"\"a/c\", ").unwrap();
        let (some, truncated) = parse_in_pieces(&body[..cut], 5);
        assert_eq!(some.len(), 1, "what arrived before the cut is passed on");
        assert!(
            matches!(truncated, Err(ParseError::Json(_))),
            "{truncated:?}"
        );
    }

    #[test]
    fn content_ranges() {
        assert_eq!(
            parse_content_range("bytes 0-99/1000"),
            Some((0, 99, Some(1000)))
        );
        assert_eq!(parse_content_range("bytes 5-9/*"), Some((5, 9, None)));
        assert_eq!(parse_content_range("bytes */1000"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
    }

    #[test]
    fn mod_times() {
        assert_eq!(
            utc_seconds("2026-10-29T12:00:00.123456789Z"),
            "2026-10-29T12:00:00Z"
        );
        assert_eq!(utc_seconds("2026-10-29T12:00:00Z"), "2026-10-29T12:00:00Z");
        assert_eq!(
            utc_seconds("2026-10-29T12:00:00.5+02:00"),
            "2026-10-29T12:00:00.5+02:00"
        );
    }

    fn config(url: &str, remote: &str) -> RcloneConfig {
        RcloneConfig {
            url: url.into(),
            user: "core".into(),
            password: "pw".into(),
            remote: remote.into(),
            max_listing_bytes: 1000,
            staging: None,
        }
    }

    #[test]
    fn only_loopback_http_with_a_plain_remote_name() {
        assert!(RcloneStorage::new(config("http://127.0.0.1:5572", "pcloud")).is_ok());
        assert!(RcloneStorage::new(config("http://localhost:5572/", "pcloud:")).is_ok());
        assert!(RcloneStorage::new(config("http://[::1]:5572", "my remote")).is_ok());
        for url in [
            "https://127.0.0.1:5572",
            "http://10.0.0.5:5572",
            "http://example.com",
            "http://u:p@127.0.0.1:5572",
            "not a url",
        ] {
            assert!(RcloneStorage::new(config(url, "pcloud")).is_err(), "{url}");
        }
        for remote in ["", ":", "/etc", "[x]", "a:b", "pcloud:/sub", "-x"] {
            assert!(
                RcloneStorage::new(config("http://127.0.0.1:1", remote)).is_err(),
                "{remote:?}"
            );
        }
    }

    #[test]
    fn debug_output_hides_the_password() {
        let storage = RcloneStorage::new(config("http://127.0.0.1:5572", "pcloud")).unwrap();
        let shown = format!(
            "{storage:?} {:?}",
            config("http://127.0.0.1:5572", "pcloud")
        );
        assert!(!shown.contains("pw") && !shown.contains("Basic"), "{shown}");
    }
}
