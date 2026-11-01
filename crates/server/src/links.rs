//! The link routes, behind the auth Worker (docs/architecture.md, "Links"):
//!
//! - `GET`/`HEAD /dl/<token>/<name>`: a file, with `Range`, or a streamed zip.
//! - `/ul/<token>`: `PUT ?part=K` (or with `Content-Range`) adds a part, in order; `HEAD` gives
//!   the committed offset in `Upload-Offset`; `GET` the state as JSON; `POST ?finish` retries
//!   a completion that failed. The last part starts the completion: the file goes to pCloud
//!   from the staging folder and is checked (size, SHA-1).
//!
//! The Worker checks the token first, but every check is repeated here, where it counts: the
//! MAC, expiry, the link's row and state, the client network it is bound to, and the byte
//! allowance. Every request that names a real link is logged in `link_log`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::Request;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use chance_of_files_core::links::content_disposition;
use chance_of_files_core::zip::{zip_size, zip_stream};
use chance_of_files_core::{
    ByteRange, ByteStream, Capabilities, CheckedPath, Entry, HashKind, LinkKind, LinkRow,
    LinkSettings, LinkState, LinkUse, RangeAnswer, StateStore, Storage, StorageError, StorageWrite,
    TokenError, parse_range, time, zip_items,
};
use futures_util::{Stream, StreamExt};
use serde_json::json;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::App;
use crate::state::{Bound, State};

/// The client's address, set by the auth Worker from `CF-Connecting-IP`; trusted only on a
/// request that carried the internal token.
pub const CLIENT_IP_HEADER: &str = "x-cof-client-ip";
/// The committed offset of an upload (tus calls it the same).
const UPLOAD_OFFSET: &str = "upload-offset";
const UPLOAD_LENGTH: &str = "upload-length";

pub struct LinkServer {
    pub state: Arc<State>,
    pub settings: LinkSettings,
    /// Upload links with a part or a completion in progress: one at a time per link.
    busy: Mutex<HashSet<String>>,
}

impl LinkServer {
    pub fn new(state: Arc<State>, settings: LinkSettings) -> Self {
        Self {
            state,
            settings,
            busy: Mutex::new(HashSet::new()),
        }
    }

    fn claim(&self, id: &str) -> Option<Claim<'_>> {
        let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        busy.insert(id.to_owned()).then(|| Claim {
            server: self,
            id: id.to_owned(),
        })
    }
}

/// Releases an upload link when dropped.
struct Claim<'a> {
    server: &'a LinkServer,
    id: String,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let mut busy = self.server.busy.lock().unwrap_or_else(|e| e.into_inner());
        busy.remove(&self.id);
    }
}

/// A state database call off the async threads.
async fn db<T: Send + 'static>(
    state: &Arc<State>,
    work: impl FnOnce(&State) -> T + Send + 'static,
) -> T {
    let state = state.clone();
    tokio::task::spawn_blocking(move || work(&state))
        .await
        .expect("state work doesn't panic")
}

fn text(status: StatusCode, body: &'static str) -> Response {
    let mut response = (status, body).into_response();
    secure(response.headers_mut());
    response
}

fn json_answer(status: StatusCode, value: serde_json::Value) -> Response {
    let mut response = (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        value.to_string(),
    )
        .into_response();
    secure(response.headers_mut());
    response
}

/// Headers on every link answer: the links share an origin with the OAuth pages, so nothing
/// served here may run or be sniffed as anything.
fn secure(headers: &mut HeaderMap) {
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
}

/// The token from `/dl/<token>/<name>` or `/ul/<token>`.
fn token_of(path: &str, prefix: &str) -> Option<String> {
    let rest = path.strip_prefix(prefix)?;
    let token = rest.split('/').next()?;
    (!token.is_empty()).then(|| token.to_owned())
}

fn query_param<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (k == key).then_some(v)
    })
}

/// What every link request goes through before its own handling.
struct Checked {
    row: LinkRow,
    ip: String,
}

async fn check<S: StorageWrite>(
    app: &Arc<App<S>>,
    links: &LinkServer,
    request: &Parts,
    kind: LinkKind,
) -> Result<Checked, Box<Response>> {
    if !app.authorized(&request.headers) {
        return Err(Box::new(text(StatusCode::UNAUTHORIZED, "unauthorized")));
    }
    let ip = request
        .headers
        .get(CLIENT_IP_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Box::new(text(StatusCode::BAD_REQUEST, "missing client address")))?
        .to_owned();
    let prefix = if kind == LinkKind::Download {
        "/dl/"
    } else {
        "/ul/"
    };
    let token = token_of(request.uri.path(), prefix)
        .ok_or_else(|| Box::new(text(StatusCode::NOT_FOUND, "not found")))?;
    let now = time::now();
    let token = match links.settings.key.verify(kind, &token, now) {
        Ok(token) => token,
        Err(TokenError::Invalid) => return Err(Box::new(text(StatusCode::NOT_FOUND, "not found"))),
        Err(TokenError::Expired) => {
            return Err(Box::new(text(StatusCode::GONE, "this link has expired")));
        }
    };
    let id = token.id.clone();
    let row = db(&links.state, move |s| s.link(&id))
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "state.db read failed");
            Box::new(text(StatusCode::SERVICE_UNAVAILABLE, "try again later"))
        })?
        .filter(|row| row.kind == kind && row.expires == token.expires)
        .ok_or_else(|| Box::new(text(StatusCode::NOT_FOUND, "not found")))?;
    let refuse = |status: StatusCode, why: &'static str| (status, why);
    let refusal = match row.state {
        LinkState::Revoked => Some(refuse(StatusCode::GONE, "this link was revoked")),
        LinkState::Expired => Some(refuse(StatusCode::GONE, "this link has expired")),
        _ => None,
    };
    let refusal = match refusal {
        Some(r) => Some(r),
        None => match links.settings.bind.network(&ip) {
            None => None,
            Some(Err(())) => Some(refuse(StatusCode::FORBIDDEN, "unusable client address")),
            Some(Ok(net)) => {
                let id = row.id.clone();
                match db(&links.state, move |s| s.bind(&id, &net)).await {
                    Ok(Bound::Ok) => None,
                    Ok(Bound::Elsewhere(_)) => Some(refuse(
                        StatusCode::FORBIDDEN,
                        "this link is bound to another network",
                    )),
                    Err(_) => Some(refuse(StatusCode::SERVICE_UNAVAILABLE, "try again later")),
                }
            }
        },
    };
    if let Some((status, why)) = refusal {
        log(links, &row.id, &ip, "", 0, status).await;
        tracing::warn!(link = %row.id, %ip, status = status.as_u16(), reason = why, "link refused");
        return Err(Box::new(text(status, why)));
    }
    Ok(Checked { row, ip })
}

async fn log(links: &LinkServer, id: &str, ip: &str, range: &str, bytes: u64, status: StatusCode) {
    let entry = LinkUse {
        at: time::now(),
        ip: ip.to_owned(),
        range: range.to_owned(),
        bytes,
        status: status.as_u16(),
    };
    let id = id.to_owned();
    if let Err(e) = db(&links.state, move |s| s.log_use(&id, &entry)).await {
        tracing::warn!(error = %e, "can't log a link use");
    }
}

/// The storage behind the app, owned, for a zip stream that outlives the request handler.
struct AppStorage<S: StorageWrite>(Arc<App<S>>);

impl<S: StorageWrite + 'static> Storage for AppStorage<S> {
    fn capabilities(&self) -> Capabilities {
        self.0.mcp.storage().capabilities()
    }
    async fn list(&self, path: &CheckedPath) -> Result<Vec<Entry>, StorageError> {
        self.0.mcp.storage().list(path).await
    }
    async fn walk(
        &self,
        root: &CheckedPath,
        exclude: &[CheckedPath],
    ) -> Result<Vec<Entry>, StorageError> {
        self.0.mcp.storage().walk(root, exclude).await
    }
    async fn stat(&self, path: &CheckedPath, with_hash: bool) -> Result<Entry, StorageError> {
        self.0.mcp.storage().stat(path, with_hash).await
    }
    async fn open(
        &self,
        path: &CheckedPath,
        range: Option<ByteRange>,
    ) -> Result<ByteStream, StorageError> {
        self.0.mcp.storage().open(path, range).await
    }
    async fn health(&self) -> Result<(), StorageError> {
        self.0.mcp.storage().health().await
    }
}

/// A response body that counts what goes out, and when it ends (or the client goes away)
/// gives back the unsent part of the bytes taken from the link's allowance and logs the use.
struct Metered {
    inner: ByteStream,
    sent: u64,
    planned: u64,
    done: Option<Box<dyn FnOnce(u64, bool) + Send>>,
}

impl Metered {
    fn finish(&mut self, complete: bool) {
        if let Some(done) = self.done.take() {
            done(self.sent, complete);
        }
    }
}

impl Stream for Metered {
    type Item = Result<Bytes, StorageError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(mut chunk))) => {
                // Never past what was taken from the allowance.
                let room = self.planned.saturating_sub(self.sent);
                if chunk.len() as u64 > room {
                    chunk.truncate(room as usize);
                }
                self.sent += chunk.len() as u64;
                // hyper stops polling once Content-Length bytes are out and never sees the
                // end of the stream, so a full body counts as complete here.
                if self.sent >= self.planned {
                    self.finish(true);
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                tracing::warn!(error = %e, "a link transfer failed");
                self.finish(false);
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                self.finish(true);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for Metered {
    fn drop(&mut self) {
        self.finish(false);
    }
}

fn metered(
    links: &LinkServer,
    id: &str,
    ip: &str,
    range: String,
    status: StatusCode,
    inner: ByteStream,
    planned: u64,
) -> Body {
    let state = links.state.clone();
    let (id, ip) = (id.to_owned(), ip.to_owned());
    let done = move |sent: u64, complete: bool| {
        let entry = LinkUse {
            at: time::now(),
            ip,
            range,
            bytes: sent,
            // A transfer cut short is logged as 499, the way proxies log a client that left.
            status: if complete { status.as_u16() } else { 499 },
        };
        let work = move || {
            let _ = state.give_back(&id, planned.saturating_sub(sent));
            let _ = state.log_use(&id, &entry);
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(work);
            }
            Err(_) => work(),
        }
    };
    Body::from_stream(Metered {
        inner,
        sent: 0,
        planned,
        done: Some(Box::new(done)),
    })
}

pub(crate) async fn download<S: StorageWrite + 'static>(
    app: Arc<App<S>>,
    request: Request,
) -> Response {
    let Some(links) = app.links.as_ref() else {
        return text(StatusCode::NOT_FOUND, "not found");
    };
    let (request, _) = request.into_parts();
    if request.method != Method::GET && request.method != Method::HEAD {
        let mut response = text(StatusCode::METHOD_NOT_ALLOWED, "use GET");
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return response;
    }
    let Checked { row, ip } = match check(&app, links, &request, LinkKind::Download).await {
        Ok(checked) => checked,
        Err(response) => return *response,
    };
    let head = request.method == Method::HEAD;
    let mut headers = HeaderMap::new();
    secure(&mut headers);
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(value) = HeaderValue::from_str(&content_disposition(&row.name)) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    let policy = app.mcp.policy();
    let storage = app.mcp.storage();

    if row.zip {
        let items = match zip_items(storage, policy, &row.paths).await {
            Ok(items) => items,
            Err(e) => return storage_failure(links, &row, &ip, e).await,
        };
        let content: u64 = items.iter().map(|i| i.size).sum();
        if content > links.settings.max_zip_bytes {
            log(links, &row.id, &ip, "", 0, StatusCode::PAYLOAD_TOO_LARGE).await;
            return text(
                StatusCode::PAYLOAD_TOO_LARGE,
                "the folder has grown past the zip size limit",
            );
        }
        let len = zip_size(&items);
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
        headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));
        if head {
            log(links, &row.id, &ip, "HEAD", 0, StatusCode::OK).await;
            return (StatusCode::OK, headers).into_response();
        }
        if !take(links, &row.id, len).await {
            log(links, &row.id, &ip, "", 0, StatusCode::GONE).await;
            return text(StatusCode::GONE, "this link has used up its byte allowance");
        }
        tracing::info!(link = %row.id, %ip, files = items.len(), bytes = len, "zip download");
        let stream = zip_stream(AppStorage(app.clone()), items);
        let body = metered(
            links,
            &row.id,
            &ip,
            "zip".into(),
            StatusCode::OK,
            stream,
            len,
        );
        return (StatusCode::OK, headers, body).into_response();
    }

    let Some(path) = row.paths.first().and_then(|p| policy.check(p).ok()) else {
        return text(StatusCode::NOT_FOUND, "not found");
    };
    let entry = match storage.stat(&path, false).await {
        Ok(entry) if !entry.is_dir => entry,
        Ok(_) => {
            return storage_failure(links, &row, &ip, StorageError::NotFound(path.to_string()))
                .await;
        }
        Err(e) => return storage_failure(links, &row, &ip, e).await,
    };
    let size = entry.size.unwrap_or(0);
    let range_header = request
        .headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok());
    let (status, start, len) = match parse_range(range_header, size) {
        RangeAnswer::Full => (StatusCode::OK, 0, size),
        RangeAnswer::Part { start, end } => (StatusCode::PARTIAL_CONTENT, start, end - start + 1),
        RangeAnswer::Unsatisfiable => {
            log(
                links,
                &row.id,
                &ip,
                range_header.unwrap_or(""),
                0,
                StatusCode::RANGE_NOT_SATISFIABLE,
            )
            .await;
            let mut response = text(StatusCode::RANGE_NOT_SATISFIABLE, "range not satisfiable");
            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{size}")) {
                response.headers_mut().insert(header::CONTENT_RANGE, value);
            }
            return response;
        }
    };
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    if status == StatusCode::PARTIAL_CONTENT
        && let Ok(value) =
            HeaderValue::from_str(&format!("bytes {start}-{}/{size}", start + len - 1))
    {
        headers.insert(header::CONTENT_RANGE, value);
    }
    if let Some(modified) = entry.modified.as_deref().and_then(time::parse_rfc3339)
        && let Ok(value) = HeaderValue::from_str(&http_date(modified))
    {
        headers.insert(header::LAST_MODIFIED, value);
    }
    let range_text = range_header.unwrap_or("").to_owned();
    if head {
        log(links, &row.id, &ip, "HEAD", 0, status).await;
        return (status, headers).into_response();
    }
    if !take(links, &row.id, len).await {
        log(links, &row.id, &ip, &range_text, 0, StatusCode::GONE).await;
        return text(StatusCode::GONE, "this link has used up its byte allowance");
    }
    let stream = match storage
        .open(
            &path,
            Some(ByteRange {
                offset: start,
                length: len,
            }),
        )
        .await
    {
        Ok(stream) => stream,
        Err(e) => {
            let id = row.id.clone();
            let _ = db(&links.state, move |s| s.give_back(&id, len)).await;
            return storage_failure(links, &row, &ip, e).await;
        }
    };
    tracing::info!(link = %row.id, %ip, range = %range_text, bytes = len, "download");
    let body = metered(links, &row.id, &ip, range_text, status, stream, len);
    (status, headers, body).into_response()
}

async fn take(links: &LinkServer, id: &str, bytes: u64) -> bool {
    let id = id.to_owned();
    db(&links.state, move |s| s.take_bytes(&id, bytes))
        .await
        .unwrap_or(false)
}

async fn storage_failure(
    links: &LinkServer,
    row: &LinkRow,
    ip: &str,
    err: StorageError,
) -> Response {
    let (status, body) = match err {
        StorageError::NotFound(_) | StorageError::NotADirectory(_) => {
            (StatusCode::NOT_FOUND, "the file is gone")
        }
        other => {
            tracing::warn!(link = %row.id, error = %other, "link read failed");
            (StatusCode::BAD_GATEWAY, "the file server failed; try again")
        }
    };
    log(links, &row.id, ip, "", 0, status).await;
    text(status, body)
}

/// `Sun, 06 Nov 1994 08:49:37 GMT`.
fn http_date(ts: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (y, mo, d, h, mi, s) = time::civil(ts);
    let weekday = DAYS[ts.div_euclid(86_400).rem_euclid(7) as usize];
    format!(
        "{weekday}, {d:02} {} {y} {h:02}:{mi:02}:{s:02} GMT",
        MONTHS[(mo - 1) as usize]
    )
}

fn upload_status(row: &LinkRow) -> serde_json::Value {
    let state = match row.state {
        LinkState::Active | LinkState::Failed if row.expires <= time::now() => "expired",
        state => state.as_str(),
    };
    json!({
        "id": row.id,
        "state": state,
        "path": row.paths.first(),
        "size": row.size,
        "committed": row.committed,
        "expires": time::rfc3339(row.expires),
        "note": row.note,
    })
}

fn with_offset(mut response: Response, row: &LinkRow, committed: u64) -> Response {
    let headers = response.headers_mut();
    headers.insert(UPLOAD_OFFSET, HeaderValue::from(committed));
    headers.insert(UPLOAD_LENGTH, HeaderValue::from(row.size));
    response
}

pub(crate) async fn upload<S: StorageWrite + 'static>(
    app: Arc<App<S>>,
    request: Request,
) -> Response {
    let Some(links) = app.links.as_ref() else {
        return text(StatusCode::NOT_FOUND, "not found");
    };
    let (request, body) = request.into_parts();
    let Checked { row, ip } = match check(&app, links, &request, LinkKind::Upload).await {
        Ok(checked) => checked,
        Err(response) => return *response,
    };
    let query = request.uri.query().map(str::to_owned);
    let finish_asked = query_param(query.as_deref(), "finish").is_some();
    match request.method {
        Method::HEAD => {
            log(links, &row.id, &ip, "HEAD", 0, StatusCode::OK).await;
            let committed = row.committed;
            with_offset(StatusCode::OK.into_response(), &row, committed)
        }
        Method::GET => {
            let committed = row.committed;
            with_offset(
                json_answer(StatusCode::OK, upload_status(&row)),
                &row,
                committed,
            )
        }
        Method::POST => finish_request(&app, links, row, &ip).await,
        Method::PUT if finish_asked => finish_request(&app, links, row, &ip).await,
        Method::PUT => put_part(&app, links, row, &ip, &request, body).await,
        _ => {
            let mut response = text(StatusCode::METHOD_NOT_ALLOWED, "use PUT, HEAD, GET or POST");
            response.headers_mut().insert(
                header::ALLOW,
                HeaderValue::from_static("PUT, HEAD, GET, POST"),
            );
            response
        }
    }
}

/// Where a part goes and how long it is: `?part=K` (parts of the link's fixed size, so
/// `split -b` output maps straight onto it) or `Content-Range: bytes a-b/total`.
fn part_span(
    request: &Parts,
    row: &LinkRow,
    part_bytes: u64,
) -> Result<(u64, u64, String), &'static str> {
    let query = request.uri.query();
    if let Some(k) = query_param(query, "part") {
        let k: u64 = k.parse().map_err(|_| "part must be a number from 0")?;
        let offset = k.checked_mul(part_bytes).ok_or("part is too big")?;
        if offset >= row.size {
            return Err("that part is past the end of the file");
        }
        return Ok((
            offset,
            part_bytes.min(row.size - offset),
            format!("part {k}"),
        ));
    }
    let range = request
        .headers
        .get(header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .ok_or("send ?part=K or a Content-Range header")?;
    let rest = range
        .trim()
        .strip_prefix("bytes ")
        .ok_or("Content-Range must look like bytes 0-99/1000")?;
    let (span, total) = rest.split_once('/').ok_or("Content-Range needs a total")?;
    let (a, b) = span.split_once('-').ok_or("Content-Range needs a span")?;
    let (a, b, total): (u64, u64, u64) = match (a.parse(), b.parse(), total.parse()) {
        (Ok(a), Ok(b), Ok(t)) => (a, b, t),
        _ => return Err("Content-Range must look like bytes 0-99/1000"),
    };
    if total != row.size || b < a || b >= total {
        return Err("Content-Range doesn't fit the upload's size");
    }
    Ok((a, b - a + 1, format!("bytes {a}-{b}")))
}

async fn put_part<S: StorageWrite + 'static>(
    app: &Arc<App<S>>,
    links: &LinkServer,
    row: LinkRow,
    ip: &str,
    request: &Parts,
    body: Body,
) -> Response {
    if row.state != LinkState::Active {
        let status = StatusCode::CONFLICT;
        log(links, &row.id, ip, "PUT", 0, status).await;
        return json_answer(status, upload_status(&row));
    }
    let part_bytes = links.settings.part_bytes;
    let (offset, len, label) = match part_span(request, &row, part_bytes) {
        Ok(span) => span,
        Err(why) => {
            log(links, &row.id, ip, "PUT", 0, StatusCode::BAD_REQUEST).await;
            return text(StatusCode::BAD_REQUEST, why);
        }
    };
    if len > part_bytes {
        log(links, &row.id, ip, &label, 0, StatusCode::PAYLOAD_TOO_LARGE).await;
        return text(
            StatusCode::PAYLOAD_TOO_LARGE,
            "parts may be at most the part size",
        );
    }
    let declared = request
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    if declared.is_some_and(|n| n != len) {
        log(links, &row.id, ip, &label, 0, StatusCode::BAD_REQUEST).await;
        return text(
            StatusCode::BAD_REQUEST,
            "the body's length doesn't match the part (every part but the last is the full part size)",
        );
    }
    let Some(_claim) = links.claim(&row.id) else {
        return text(
            StatusCode::CONFLICT,
            "another part of this upload is arriving; send parts one at a time",
        );
    };
    // Read the row again under the claim: another request may have moved it on.
    let id = row.id.clone();
    let row = match db(&links.state, move |s| s.link(&id)).await {
        Ok(Some(row)) => row,
        _ => return text(StatusCode::SERVICE_UNAVAILABLE, "try again later"),
    };
    if offset + len <= row.committed {
        log(links, &row.id, ip, &label, 0, StatusCode::OK).await;
        let committed = row.committed;
        let mut status = upload_status(&row);
        status["skipped"] = json!(true);
        return with_offset(json_answer(StatusCode::OK, status), &row, committed);
    }
    if offset != row.committed {
        log(links, &row.id, ip, &label, 0, StatusCode::CONFLICT).await;
        let committed = row.committed;
        return with_offset(
            json_answer(
                StatusCode::CONFLICT,
                json!({"error": "parts must arrive in order", "committed": committed,
                       "next_part": committed / part_bytes}),
            ),
            &row,
            committed,
        );
    }
    if !take(links, &row.id, len).await {
        log(links, &row.id, ip, &label, 0, StatusCode::GONE).await;
        return text(StatusCode::GONE, "this link has used up its byte allowance");
    }
    let file = links.state.part_file(&row.id);
    let written = write_part(&file, offset, len, body).await;
    let written = match written {
        Ok(()) => len,
        Err(why) => {
            // Nothing was kept, so the part's bytes go back to the allowance for its retry.
            let id = row.id.clone();
            let _ = db(&links.state, move |s| s.give_back(&id, len)).await;
            log(links, &row.id, ip, &label, 0, StatusCode::BAD_REQUEST).await;
            return text(StatusCode::BAD_REQUEST, why);
        }
    };
    let committed = offset + written;
    let id = row.id.clone();
    if let Err(e) = db(&links.state, move |s| s.set_committed(&id, committed)).await {
        tracing::error!(error = %e, "can't record an upload part");
        return text(StatusCode::SERVICE_UNAVAILABLE, "try again later");
    }
    let mut row = row;
    row.committed = committed;
    if committed < row.size {
        log(links, &row.id, ip, &label, written, StatusCode::OK).await;
        return with_offset(
            json_answer(StatusCode::OK, upload_status(&row)),
            &row,
            committed,
        );
    }
    let id = row.id.clone();
    let _ = db(&links.state, move |s| {
        s.transition(&id, &[LinkState::Active], LinkState::Finishing, None)
    })
    .await;
    log(links, &row.id, ip, &label, written, StatusCode::ACCEPTED).await;
    drop(_claim);
    spawn_finish(app.clone(), row.id.clone());
    row.state = LinkState::Finishing;
    with_offset(
        json_answer(StatusCode::ACCEPTED, upload_status(&row)),
        &row,
        committed,
    )
}

/// Writes a part at `offset` of the staging file, which must then be exactly `offset + len`
/// bytes long; anything short or long is cut back to `offset`, so a dropped part leaves no
/// trace and is simply sent again.
async fn write_part(file: &PathBuf, offset: u64, len: u64, body: Body) -> Result<(), &'static str> {
    let mut out = tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(file)
        .await
        .map_err(|_| "the staging file can't be opened")?;
    let reset = async |out: &mut tokio::fs::File| {
        let _ = out.set_len(offset).await;
    };
    if out.set_len(offset).await.is_err()
        || out.seek(std::io::SeekFrom::Start(offset)).await.is_err()
    {
        return Err("the staging file can't be prepared");
    }
    let mut stream = body.into_data_stream();
    let mut got: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            reset(&mut out).await;
            return Err("the part didn't arrive whole");
        };
        got += chunk.len() as u64;
        if got > len {
            reset(&mut out).await;
            return Err("the part is longer than its span");
        }
        if out.write_all(&chunk).await.is_err() {
            reset(&mut out).await;
            return Err("the staging disk failed");
        }
    }
    if got != len {
        reset(&mut out).await;
        return Err("the part is shorter than its span");
    }
    if out.flush().await.is_err() || out.sync_all().await.is_err() {
        reset(&mut out).await;
        return Err("the staging disk failed");
    }
    Ok(())
}

async fn finish_request<S: StorageWrite + 'static>(
    app: &Arc<App<S>>,
    links: &LinkServer,
    row: LinkRow,
    ip: &str,
) -> Response {
    if row.committed < row.size || !matches!(row.state, LinkState::Failed | LinkState::Finishing) {
        log(links, &row.id, ip, "finish", 0, StatusCode::CONFLICT).await;
        return json_answer(StatusCode::CONFLICT, upload_status(&row));
    }
    let id = row.id.clone();
    let moved = db(&links.state, move |s| {
        s.transition(&id, &[LinkState::Failed], LinkState::Finishing, None)
    })
    .await
    .unwrap_or(false);
    if moved || row.state == LinkState::Finishing {
        spawn_finish(app.clone(), row.id.clone());
    }
    log(links, &row.id, ip, "finish", 0, StatusCode::ACCEPTED).await;
    let mut row = row;
    row.state = LinkState::Finishing;
    json_answer(StatusCode::ACCEPTED, upload_status(&row))
}

/// Sends a complete upload to pCloud in the background (a big file takes minutes, longer than
/// a request through Cloudflare should be held). Does nothing if it is already running.
pub(crate) fn spawn_finish<S: StorageWrite + 'static>(app: Arc<App<S>>, id: String) {
    tokio::spawn(async move {
        let Some(links) = app.links.as_ref() else {
            return;
        };
        let Some(_claim) = links.claim(&id) else {
            return;
        };
        let lookup = id.clone();
        let Ok(Some(row)) = db(&links.state, move |s| s.link(&lookup)).await else {
            return;
        };
        if row.state != LinkState::Finishing {
            return;
        }
        let started = std::time::Instant::now();
        let outcome = finish(&app, links, &row).await;
        let (state, note) = match &outcome {
            Ok(note) => (LinkState::Done, note.clone()),
            Err(why) => (
                LinkState::Failed,
                format!("{why}; POST the link with ?finish to retry"),
            ),
        };
        match &outcome {
            Ok(_) => {
                tracing::info!(link = %id, ms = started.elapsed().as_millis() as u64, "upload finished")
            }
            Err(why) => tracing::warn!(link = %id, error = %why, "upload failed"),
        }
        let (i, n) = (id.clone(), note.clone());
        let _ = db(&links.state, move |s| {
            s.transition(&i, &[LinkState::Finishing], state, Some(&n))
        })
        .await;
        let args = json!({"id": id, "path": row.paths.first()}).to_string();
        let result = match outcome {
            Ok(_) => format!("ok: {note}"),
            Err(_) => format!("error: {note}"),
        };
        let _ = db(&links.state, move |s| {
            s.audit(time::now(), "upload_link", &args, &result)
        })
        .await;
    });
}

async fn finish<S: StorageWrite + 'static>(
    app: &Arc<App<S>>,
    links: &LinkServer,
    row: &LinkRow,
) -> Result<String, String> {
    let storage = app.mcp.storage();
    let target = row.paths.first().ok_or("the link has no destination")?;
    // Checked again at completion: the policy may have changed since the link was made.
    let dest = app
        .mcp
        .policy()
        .check_write(target)
        .map_err(|e| e.to_string())?;
    let staged = links.state.part_file(&row.id);
    let on_disk = tokio::fs::metadata(&staged)
        .await
        .map(|m| m.len())
        .map_err(|_| "the staged upload is missing".to_owned())?;
    if on_disk != row.size {
        return Err(format!(
            "the staged upload has {on_disk} bytes, not {}",
            row.size
        ));
    }
    if !row.overwrite {
        match storage.stat(&dest, false).await {
            Ok(_) => return Err(format!("{dest} exists now, and the link doesn't overwrite")),
            Err(StorageError::NotFound(_)) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    let file = staged.clone();
    let sha1 = tokio::task::spawn_blocking(move || sha1_of(&file))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("can't read the staged upload: {e}"))?;
    storage
        .put_local(&staged, &dest)
        .await
        .map_err(|e| e.to_string())?;
    let entry = storage.stat(&dest, true).await.map_err(|e| e.to_string())?;
    if entry.size != Some(row.size) {
        return Err(format!(
            "pCloud has {} bytes for {dest}, not {}",
            entry.size.unwrap_or(0),
            row.size
        ));
    }
    let remote_sha1 = entry
        .hashes
        .iter()
        .find(|(k, _)| *k == HashKind::Sha1)
        .map(|(_, v)| v.to_ascii_lowercase());
    if remote_sha1.as_ref().is_some_and(|h| *h != sha1) {
        return Err(format!(
            "pCloud's SHA-1 of {dest} doesn't match what arrived; the file there is suspect"
        ));
    }
    if let Some(index) = app.mcp.index() {
        let index = index.clone();
        let mut written = entry.clone();
        written.hashes.clear();
        let _ = tokio::task::spawn_blocking(move || index.record_write(&written)).await;
    }
    let _ = tokio::fs::remove_file(&staged).await;
    let checked = if remote_sha1.is_some() {
        "size and SHA-1 checked"
    } else {
        "size checked"
    };
    Ok(format!(
        "uploaded to {dest}, {} bytes, SHA-1 {sha1}, {checked}",
        row.size
    ))
}

fn sha1_of(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Every minute: deletes the staging files of revoked and expired uploads, and restarts
/// completions a restart interrupted.
pub async fn housekeeping<S: StorageWrite + 'static>(app: Arc<App<S>>) {
    let Some(links) = app.links.as_ref() else {
        return;
    };
    loop {
        let now = time::now();
        match db(&links.state, move |s| s.sweep(now)).await {
            Ok(ids) => {
                for id in ids {
                    let file = links.state.part_file(&id);
                    if tokio::fs::remove_file(&file).await.is_ok() {
                        tracing::info!(link = %id, "removed an unfinished upload");
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "link sweep failed"),
        }
        if let Ok(ids) = db(&links.state, |s| s.finishing()).await {
            for id in ids {
                spawn_finish(app.clone(), id);
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_dates() {
        assert_eq!(http_date(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    #[test]
    fn tokens_and_queries() {
        assert_eq!(token_of("/dl/abc/x.txt", "/dl/"), Some("abc".into()));
        assert_eq!(token_of("/ul/abc", "/ul/"), Some("abc".into()));
        assert_eq!(token_of("/dl/", "/dl/"), None);
        assert_eq!(query_param(Some("part=3"), "part"), Some("3"));
        assert_eq!(query_param(Some("a=1&finish"), "finish"), Some(""));
        assert_eq!(query_param(None, "part"), None);
    }

    #[tokio::test]
    async fn a_full_body_is_complete_even_if_the_stream_is_never_drained() {
        // hyper drops the body after Content-Length bytes without polling for the end.
        let (tx, rx) = std::sync::mpsc::channel();
        let inner: ByteStream = Box::pin(
            futures_util::stream::iter([Ok(Bytes::from_static(b"abcd"))])
                .chain(futures_util::stream::pending()),
        );
        let mut body = Metered {
            inner,
            sent: 0,
            planned: 4,
            done: Some(Box::new(move |sent, complete| {
                tx.send((sent, complete)).unwrap();
            })),
        };
        assert_eq!(body.next().await.unwrap().unwrap().len(), 4);
        drop(body);
        assert_eq!(rx.recv().unwrap(), (4, true));

        let (tx, rx) = std::sync::mpsc::channel();
        let inner: ByteStream = Box::pin(
            futures_util::stream::iter([Ok(Bytes::from_static(b"ab"))])
                .chain(futures_util::stream::pending()),
        );
        let mut body = Metered {
            inner,
            sent: 0,
            planned: 4,
            done: Some(Box::new(move |sent, complete| {
                tx.send((sent, complete)).unwrap();
            })),
        };
        body.next().await.unwrap().unwrap();
        drop(body);
        assert_eq!(rx.recv().unwrap(), (2, false));
    }
}
