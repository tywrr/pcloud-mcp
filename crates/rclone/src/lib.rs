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
//!   and no `operations/publiclink`.

use std::fmt;
use std::sync::OnceLock;

use base64::Engine as _;
use bytes::Bytes;
use chance_of_files_core::{
    ByteRange, ByteStream, Capabilities, CheckedPath, Entry, HashKind, PathSet, Storage,
    StorageError, split_walk,
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
}

impl Method {
    pub const ALL: [Self; 4] = [Self::List, Self::Stat, Self::About, Self::FsInfo];

    pub fn path(self) -> &'static str {
        match self {
            Self::List => "operations/list",
            Self::Stat => "operations/stat",
            Self::About => "operations/about",
            Self::FsInfo => "operations/fsinfo",
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
}

impl fmt::Debug for RcloneConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RcloneConfig")
            .field("url", &self.url)
            .field("user", &self.user)
            .field("remote", &self.remote)
            .field("max_listing_bytes", &self.max_listing_bytes)
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
            // Decided per backend when writes arrive (phase 3); nothing deletes yet.
            trash_on_delete: false,
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
        let url = format!("{}/{}", self.base, method.path());
        let response = self
            .http
            .post(url)
            .header(AUTHORIZATION, self.authorization.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| transport_error(method.path(), e))?;
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
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| transport_error(method.path(), e))?;
            if (body.len() + chunk.len()) as u64 > limit {
                return Err(too_big());
            }
            body.extend_from_slice(&chunk);
        }
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

    fn entry(&self, item: Item) -> Entry {
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

    /// `operations/list` of one folder, plain or recursive. Items outside the folder (which
    /// rclone never sends) are dropped rather than trusted.
    async fn list_items(
        &self,
        dir: &CheckedPath,
        recurse: bool,
    ) -> Result<Vec<Entry>, StorageError> {
        let mut opt = serde_json::Map::new();
        if recurse {
            opt.insert("recurse".into(), json!(true));
        }
        let body = json!({"fs": self.fs, "remote": dir.relative(), "opt": opt});
        let listed: Listing = match self.rc(Method::List, body, dir.as_str()).await {
            Ok(listed) => listed,
            Err(StorageError::NotFound(_)) => return Err(self.why_unlistable(dir).await),
            Err(err) => return Err(err),
        };
        let prefix = if dir.is_root() {
            "/".to_owned()
        } else {
            format!("{}/", dir.as_str())
        };
        Ok(listed
            .list
            .into_iter()
            .map(|item| self.entry(item))
            .filter(|e| e.path.starts_with(&prefix) && e.path.len() > prefix.len())
            .collect())
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
        self.list_items(path, false).await
    }

    async fn walk(
        &self,
        root: &CheckedPath,
        exclude: &[CheckedPath],
    ) -> Result<Vec<Entry>, StorageError> {
        let excluded = PathSet::new(exclude.iter().map(CheckedPath::as_str))?;
        if excluded.covers(root.as_str()) {
            return Ok(Vec::new());
        }
        if excluded.any_below(root.as_str()) {
            return split_walk(self, root, exclude).await;
        }
        self.list_items(root, true).await
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
        let mut entry = self.entry(item);
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
struct Listing {
    #[serde(default)]
    list: Vec<Item>,
}

#[derive(Debug, Deserialize)]
struct Stat {
    item: Option<Item>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct FsInfo {
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
            assert!(path.starts_with("operations/"), "{path}");
            for forbidden in [
                "cleanup",
                "publiclink",
                "delete",
                "purge",
                "core/",
                "config/",
            ] {
                assert!(!path.contains(forbidden), "{path}");
            }
        }
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
