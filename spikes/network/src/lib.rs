//! Phase 0 network spike: the endpoints `run.sh`, Claude and Duet use to measure the path
//! workers.dev -> spike Worker -> Workers VPC -> tunnel `chance-of-files` -> this server.
//!
//! Nothing here touches pCloud. Every request must carry `X-Spike-Token` (the Worker adds it).
//!
//! - `GET  /sleep?s=N`              quiet for N s (<= 300), then JSON
//! - `GET  /blob?bytes=N`           N deterministic pseudo-random bytes, `Range` supported
//! - `GET  /blob/sha256?bytes=N`    the SHA-256 of that blob, computed by streaming it locally
//! - `PUT  /chunk?upload=ID&part=K` one part (<= 95 MiB), hashed while streaming, nothing stored
//! - `POST /finish?upload=ID`       totals for the upload, and its SHA-256 if parts came in order
//! - `POST /mcp`                    stateless MCP with tools `hold` and `blob_url`

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub const MAX_SLEEP_S: u64 = 300;
pub const MAX_BLOB_BYTES: u64 = 25 * 1024 * 1024 * 1024;
/// Cloudflare caps request bodies at 100 MB on the free plan; parts are 90 MiB, so 95 MiB
/// leaves room without letting anything near the cap through.
pub const MAX_CHUNK_BYTES: u64 = 95 * 1024 * 1024;
/// Blobs up to this size get their SHA-256 inline in `blob_url`; bigger ones get a URL.
const INLINE_SHA_MAX: u64 = 4 * 1024 * 1024 * 1024;
const MAX_OPEN_UPLOADS: usize = 64;
const BLOB_FRAME: u64 = 256 * 1024;
const SEED: u64 = 0x00C0_FFEE_5EED_0001;

pub const TOKEN_HEADER: &str = "x-spike-token";
/// Set by the Worker: its public origin plus the key prefix, e.g. `https://x.workers.dev/<KEY>`.
pub const PUBLIC_BASE_HEADER: &str = "x-spike-public-base";
/// Set by the Worker from CF-Connecting-IP; logged so Duet's egress stability shows in the logs.
pub const CLIENT_IP_HEADER: &str = "x-spike-client-ip";

const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];

// ---------------------------------------------------------------------------------------------
// Deterministic bytes: byte p is byte (p % 8) of splitmix64(p / 8), so any range is cheap.
// ---------------------------------------------------------------------------------------------

fn splitmix64(i: u64) -> u64 {
    let mut z = SEED.wrapping_add(i.wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Fills `buf` with the blob bytes starting at `offset`.
pub fn fill(offset: u64, buf: &mut [u8]) {
    let mut pos = offset;
    let mut i = 0;
    while i < buf.len() {
        let word = splitmix64(pos / 8).to_le_bytes();
        let start = (pos % 8) as usize;
        let n = (8 - start).min(buf.len() - i);
        buf[i..i + n].copy_from_slice(&word[start..start + n]);
        i += n;
        pos += n as u64;
    }
}

/// SHA-256 of the first `len` blob bytes (blocking; call from `spawn_blocking`).
pub fn blob_sha256(len: u64) -> String {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut pos = 0;
    while pos < len {
        let n = (len - pos).min(buf.len() as u64) as usize;
        fill(pos, &mut buf[..n]);
        hasher.update(&buf[..n]);
        pos += n as u64;
    }
    hex::encode(hasher.finalize())
}

#[derive(Debug, PartialEq, Eq)]
pub enum RangeSpec {
    Full,
    /// Inclusive byte positions.
    Partial(u64, u64),
    Unsatisfiable,
}

/// One `bytes=` range; anything else (several ranges, other units, junk) means the whole body.
pub fn parse_range(value: Option<&str>, len: u64) -> RangeSpec {
    let Some(spec) = value.and_then(|v| v.trim().strip_prefix("bytes=")) else {
        return RangeSpec::Full;
    };
    if spec.contains(',') {
        return RangeSpec::Full;
    }
    let Some((a, b)) = spec.trim().split_once('-') else {
        return RangeSpec::Full;
    };
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        // Suffix: the last N bytes.
        return match b.parse::<u64>() {
            Ok(0) => RangeSpec::Unsatisfiable,
            Ok(_) if len == 0 => RangeSpec::Unsatisfiable,
            Ok(n) => RangeSpec::Partial(len.saturating_sub(n), len - 1),
            Err(_) => RangeSpec::Full,
        };
    }
    let Ok(start) = a.parse::<u64>() else {
        return RangeSpec::Full;
    };
    if start >= len {
        return RangeSpec::Unsatisfiable;
    }
    let end = if b.is_empty() {
        len - 1
    } else {
        match b.parse::<u64>() {
            Ok(e) if e >= start => e.min(len - 1),
            _ => return RangeSpec::Full,
        }
    };
    RangeSpec::Partial(start, end)
}

// ---------------------------------------------------------------------------------------------
// State and router
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Upload {
    parts: BTreeMap<u64, (u64, String)>,
    /// The part that would continue the running whole-file hash; set by the first part.
    next_part: Option<u64>,
    /// Running hash over parts received in order. `None` while a part is streaming into it.
    whole: Option<Sha256>,
    in_order: bool,
}

pub struct AppState {
    token_hash: [u8; 32],
    public_base: Option<String>,
    sha_cache: Mutex<HashMap<u64, String>>,
    uploads: Mutex<HashMap<String, Upload>>,
}

impl AppState {
    pub fn new(token: &str, public_base: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            token_hash: Sha256::digest(token.as_bytes()).into(),
            public_base,
            sha_cache: Mutex::default(),
            uploads: Mutex::default(),
        })
    }

    async fn sha_for(&self, len: u64) -> String {
        if let Some(h) = self.sha_cache.lock().unwrap().get(&len) {
            return h.clone();
        }
        let h = tokio::task::spawn_blocking(move || blob_sha256(len))
            .await
            .expect("hash task panicked");
        self.sha_cache.lock().unwrap().insert(len, h.clone());
        h
    }
}

pub fn app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/sleep", get(sleep))
        .route("/blob", get(blob))
        .route("/blob/sha256", get(blob_sha))
        .route("/chunk", put(chunk))
        .route("/finish", post(finish))
        .route(
            "/mcp",
            post(mcp).get(method_not_allowed).delete(method_not_allowed),
        )
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .layer(middleware::from_fn(log))
        .with_state(state)
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

async fn log(req: Request, next: Next) -> Response {
    let started = Instant::now();
    let ip = req
        .headers()
        .get(CLIENT_IP_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    let line = format!("{ip} {} {}", req.method(), req.uri());
    let res = next.run(req).await;
    // Streams keep going after this; the time is to the response head.
    println!(
        "{line} -> {} in {} ms",
        res.status().as_u16(),
        started.elapsed().as_millis()
    );
    res
}

async fn require_token(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let given = req
        .headers()
        .get(TOKEN_HEADER)
        .map(|v| v.as_bytes())
        .unwrap_or_default();
    // Hash first so the comparison doesn't depend on the lengths either.
    let given: [u8; 32] = Sha256::digest(given).into();
    if !bool::from(given.ct_eq(&state.token_hash)) {
        return err(StatusCode::UNAUTHORIZED, "missing or wrong X-Spike-Token");
    }
    next.run(req).await
}

async fn method_not_allowed() -> Response {
    err(
        StatusCode::METHOD_NOT_ALLOWED,
        "POST only; this server keeps no sessions",
    )
}

// ---------------------------------------------------------------------------------------------
// /sleep, /blob
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct SleepQuery {
    s: u64,
}

async fn sleep(Query(q): Query<SleepQuery>) -> Response {
    if q.s > MAX_SLEEP_S {
        return err(
            StatusCode::BAD_REQUEST,
            format!("s must be <= {MAX_SLEEP_S}"),
        );
    }
    let started = now_ms();
    tokio::time::sleep(Duration::from_secs(q.s)).await;
    Json(json!({ "slept_s": q.s, "started_unix_ms": started, "finished_unix_ms": now_ms() }))
        .into_response()
}

#[derive(Deserialize)]
struct BytesQuery {
    bytes: u64,
}

fn size_error(bytes: u64) -> Option<Response> {
    (bytes > MAX_BLOB_BYTES).then(|| {
        err(
            StatusCode::BAD_REQUEST,
            format!("bytes must be <= {MAX_BLOB_BYTES}"),
        )
    })
}

async fn blob(Query(q): Query<BytesQuery>, headers: HeaderMap) -> Response {
    if let Some(e) = size_error(q.bytes) {
        return e;
    }
    let len = q.bytes;
    let range = parse_range(
        headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
        len,
    );
    let (status, start, end_excl) = match range {
        RangeSpec::Full => (StatusCode::OK, 0, len),
        RangeSpec::Partial(a, b) => (StatusCode::PARTIAL_CONTENT, a, b + 1),
        RangeSpec::Unsatisfiable => {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .body(Body::empty())
                .unwrap();
        }
    };
    let stream = futures_util::stream::unfold(start, move |pos| async move {
        if pos >= end_excl {
            return None;
        }
        let n = (end_excl - pos).min(BLOB_FRAME) as usize;
        let mut buf = BytesMut::zeroed(n);
        fill(pos, &mut buf);
        Some((Ok::<Bytes, std::io::Error>(buf.freeze()), pos + n as u64))
    });
    let mut res = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, end_excl - start)
        .header(header::ACCEPT_RANGES, "bytes")
        // Same bytes for the same N forever, so curl -C - may resume against it.
        .header(header::ETAG, format!("\"blob-{len}\""))
        .header(header::CACHE_CONTROL, "no-store")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"blob-{len}.bin\""),
        );
    if status == StatusCode::PARTIAL_CONTENT {
        res = res.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{len}", end_excl - 1),
        );
    }
    res.body(Body::from_stream(stream)).unwrap()
}

async fn blob_sha(State(state): State<Arc<AppState>>, Query(q): Query<BytesQuery>) -> Response {
    if let Some(e) = size_error(q.bytes) {
        return e;
    }
    let started = Instant::now();
    let sha = state.sha_for(q.bytes).await;
    Json(json!({ "bytes": q.bytes, "sha256": sha, "compute_ms": started.elapsed().as_millis() }))
        .into_response()
}

// ---------------------------------------------------------------------------------------------
// /chunk, /finish
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct ChunkQuery {
    upload: String,
    part: u64,
}

#[derive(Deserialize)]
struct FinishQuery {
    upload: String,
}

fn valid_upload_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

async fn chunk(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ChunkQuery>,
    body: Body,
) -> Response {
    if !valid_upload_id(&q.upload) {
        return err(
            StatusCode::BAD_REQUEST,
            "upload must be 1-64 of [A-Za-z0-9_-]",
        );
    }
    // Take the running hash if this part continues it in order.
    let mut whole = {
        let mut uploads = state.uploads.lock().unwrap();
        if !uploads.contains_key(&q.upload) && uploads.len() >= MAX_OPEN_UPLOADS {
            return err(
                StatusCode::TOO_MANY_REQUESTS,
                "too many open uploads; POST /finish some",
            );
        }
        let up = uploads.entry(q.upload.clone()).or_insert_with(|| Upload {
            whole: Some(Sha256::new()),
            in_order: true,
            ..Upload::default()
        });
        let next = *up.next_part.get_or_insert(q.part);
        if up.in_order && next == q.part && up.whole.is_some() {
            up.whole.take()
        } else {
            up.in_order = false;
            None
        }
    };

    let started = Instant::now();
    let mut part = Sha256::new();
    let mut len: u64 = 0;
    let mut stream = body.into_data_stream();
    let mut failure = None;
    while let Some(frame) = stream.next().await {
        match frame {
            Ok(data) => {
                len += data.len() as u64;
                if len > MAX_CHUNK_BYTES {
                    failure = Some(err(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        format!("part bigger than {MAX_CHUNK_BYTES} bytes"),
                    ));
                    break;
                }
                part.update(&data);
                if let Some(w) = whole.as_mut() {
                    w.update(&data);
                }
            }
            Err(e) => {
                failure = Some(err(
                    StatusCode::BAD_REQUEST,
                    format!("body read failed: {e}"),
                ));
                break;
            }
        }
    }
    let elapsed = started.elapsed();
    let sha = hex::encode(part.finalize());

    let mut uploads = state.uploads.lock().unwrap();
    let Some(up) = uploads.get_mut(&q.upload) else {
        return err(
            StatusCode::CONFLICT,
            "upload was finished while this part streamed",
        );
    };
    if let Some(res) = failure {
        // The running hash saw a partial part; it can't describe the file any more.
        up.in_order = false;
        return res;
    }
    if let Some(w) = whole {
        up.whole = Some(w);
        up.next_part = Some(q.part + 1);
    }
    up.parts.insert(q.part, (len, sha.clone()));
    let secs = elapsed.as_secs_f64().max(1e-6);
    Json(json!({
        "upload": q.upload,
        "part": q.part,
        "bytes": len,
        "sha256": sha,
        "elapsed_ms": elapsed.as_millis(),
        "mib_per_s": (len as f64 / 1048576.0 / secs * 10.0).round() / 10.0,
    }))
    .into_response()
}

async fn finish(State(state): State<Arc<AppState>>, Query(q): Query<FinishQuery>) -> Response {
    let Some(up) = state.uploads.lock().unwrap().remove(&q.upload) else {
        return err(StatusCode::NOT_FOUND, "no such upload");
    };
    let total: u64 = up.parts.values().map(|(n, _)| n).sum();
    let numbers: Vec<u64> = up.parts.keys().copied().collect();
    let contiguous = numbers.windows(2).all(|w| w[1] == w[0] + 1);
    let sha = match (up.in_order, up.whole) {
        (true, Some(h)) => Value::String(hex::encode(h.finalize())),
        _ => Value::Null,
    };
    Json(json!({
        "upload": q.upload,
        "parts": numbers.len(),
        "first_part": numbers.first(),
        "last_part": numbers.last(),
        "contiguous": contiguous,
        "bytes": total,
        // Of the concatenated parts, only when each arrived once and in order.
        "sha256": sha,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------------------------
// /mcp: stateless, JSON responses only (no SSE), one message per POST.
// ---------------------------------------------------------------------------------------------

fn rpc_result(id: &Value, result: Value) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
        .into_response()
}

fn tool_text(text: String, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn tools() -> Value {
    json!([
        {
            "name": "hold",
            "title": "Hold the call open",
            "description": "Network spike: waits the given number of seconds without sending anything, then answers \"held N s\". Measures how long a quiet tool call survives the connector's path.",
            "inputSchema": {
                "type": "object",
                "properties": { "seconds": { "type": "integer", "minimum": 1, "maximum": MAX_SLEEP_S } },
                "required": ["seconds"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        },
        {
            "name": "blob_url",
            "title": "Test download URL",
            "description": "Network spike: returns a public URL that streams the given number of deterministic pseudo-random bytes (Range supported), and its SHA-256 (inline up to 4 GiB, else a URL that returns it). Nothing is read from any drive.",
            "inputSchema": {
                "type": "object",
                "properties": { "bytes": { "type": "integer", "minimum": 1, "maximum": MAX_BLOB_BYTES } },
                "required": ["bytes"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": true }
        }
    ])
}

async fn mcp(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            rpc_error(&Value::Null, -32700, "parse error"),
        )
            .into_response();
    };
    let Some(obj) = msg.as_object() else {
        return (
            StatusCode::BAD_REQUEST,
            rpc_error(&Value::Null, -32600, "one JSON-RPC object per POST"),
        )
            .into_response();
    };
    let (Some(id), Some(method)) = (obj.get("id"), obj.get("method").and_then(Value::as_str))
    else {
        // A notification or a response: nothing to answer.
        return StatusCode::ACCEPTED.into_response();
    };
    let params = obj.get("params").cloned().unwrap_or(json!({}));
    match method {
        "initialize" => {
            let asked = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("");
            let version = if PROTOCOL_VERSIONS.contains(&asked) {
                asked
            } else {
                PROTOCOL_VERSIONS[0]
            };
            rpc_result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "chance-of-files-spike", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "Network measurement spike for Chance of Files. It has no access to any files. Call `hold` with the number of seconds you are asked to; call `blob_url` to get a test download URL."
                }),
            )
        }
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({ "tools": tools() })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match name {
                "hold" => {
                    let Some(s) = args
                        .get("seconds")
                        .and_then(Value::as_u64)
                        .filter(|s| (1..=MAX_SLEEP_S).contains(s))
                    else {
                        return rpc_result(
                            id,
                            tool_text(format!("seconds must be an integer 1-{MAX_SLEEP_S}"), true),
                        );
                    };
                    tokio::time::sleep(Duration::from_secs(s)).await;
                    rpc_result(id, tool_text(format!("held {s} s"), false))
                }
                "blob_url" => {
                    let Some(n) = args
                        .get("bytes")
                        .and_then(Value::as_u64)
                        .filter(|n| (1..=MAX_BLOB_BYTES).contains(n))
                    else {
                        return rpc_result(
                            id,
                            tool_text(format!("bytes must be an integer 1-{MAX_BLOB_BYTES}"), true),
                        );
                    };
                    let base = headers
                        .get(PUBLIC_BASE_HEADER)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned)
                        .or_else(|| state.public_base.clone());
                    let Some(base) = base.map(|b| b.trim_end_matches('/').to_owned()) else {
                        return rpc_result(
                            id,
                            tool_text(
                                "no public base URL (the Worker sends X-Spike-Public-Base)".into(),
                                true,
                            ),
                        );
                    };
                    let url = format!("{base}/blob?bytes={n}");
                    let sha_url = format!("{base}/blob/sha256?bytes={n}");
                    let sha = if n <= INLINE_SHA_MAX {
                        state.sha_for(n).await
                    } else {
                        format!("GET {sha_url}")
                    };
                    rpc_result(
                        id,
                        tool_text(
                            format!(
                                "url: {url}\nbytes: {n}\nsha256: {sha}\nRange and resume supported. Verify with:\n  curl -fSL -C - -o blob.bin '{url}' && sha256sum blob.bin"
                            ),
                            false,
                        ),
                    )
                }
                _ => rpc_error(id, -32602, "unknown tool"),
            }
        }
        _ => rpc_error(id, -32601, "method not found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const TOKEN: &str = "test-token-0123456789";

    fn router() -> Router {
        app(AppState::new(TOKEN, None))
    }

    async fn send(r: &Router, req: HttpRequest<Body>) -> (StatusCode, HeaderMap, Bytes) {
        let res = r.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        (
            status,
            headers,
            res.into_body().collect().await.unwrap().to_bytes(),
        )
    }

    fn req(method: &str, uri: &str) -> axum::http::request::Builder {
        HttpRequest::builder()
            .method(method)
            .uri(uri)
            .header(TOKEN_HEADER, TOKEN)
    }

    fn blob_bytes(offset: u64, len: usize) -> Vec<u8> {
        let mut v = vec![0; len];
        fill(offset, &mut v);
        v
    }

    #[test]
    fn fill_is_position_independent() {
        let whole = blob_bytes(0, 1000);
        for start in [0usize, 1, 7, 8, 9, 333] {
            assert_eq!(blob_bytes(start as u64, 1000 - start), whole[start..]);
        }
        assert_ne!(whole[..8], whole[8..16]);
    }

    #[test]
    fn sha_matches_direct_hash() {
        let n = 3 * 1024 * 1024 + 5;
        assert_eq!(
            blob_sha256(n),
            hex::encode(Sha256::digest(blob_bytes(0, n as usize)))
        );
    }

    #[test]
    fn ranges() {
        use RangeSpec::*;
        assert_eq!(parse_range(None, 10), Full);
        assert_eq!(parse_range(Some("bytes=2-4"), 10), Partial(2, 4));
        assert_eq!(parse_range(Some("bytes=2-"), 10), Partial(2, 9));
        assert_eq!(parse_range(Some("bytes=5-100"), 10), Partial(5, 9));
        assert_eq!(parse_range(Some("bytes=-3"), 10), Partial(7, 9));
        assert_eq!(parse_range(Some("bytes=-30"), 10), Partial(0, 9));
        assert_eq!(parse_range(Some("bytes=10-"), 10), Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=0-1,3-4"), 10), Full);
        assert_eq!(parse_range(Some("items=0-1"), 10), Full);
    }

    #[tokio::test]
    async fn token_required() {
        let r = router();
        let (s, _, _) = send(
            &r,
            HttpRequest::get("/sleep?s=0").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, _, _) = send(
            &r,
            HttpRequest::get("/sleep?s=0")
                .header(TOKEN_HEADER, "nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, _, _) = send(&r, req("GET", "/sleep?s=0").body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::OK);
        let (s, _, _) = send(&r, req("GET", "/sleep?s=301").body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn blob_full_and_range() {
        let r = router();
        let n = 600_000u64;
        let (s, h, b) = send(
            &r,
            req("GET", &format!("/blob?bytes={n}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(h[header::CONTENT_LENGTH], n.to_string().as_str());
        assert_eq!(b.as_ref(), blob_bytes(0, n as usize));
        let (s, h, b) = send(
            &r,
            req("GET", &format!("/blob?bytes={n}"))
                .header(header::RANGE, "bytes=300000-")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::PARTIAL_CONTENT);
        assert_eq!(h[header::CONTENT_RANGE], "bytes 300000-599999/600000");
        assert_eq!(b.as_ref(), blob_bytes(300_000, 300_000));
        let (s, _, _) = send(
            &r,
            req("GET", &format!("/blob?bytes={n}"))
                .header(header::RANGE, "bytes=600000-")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::RANGE_NOT_SATISFIABLE);
        let (_, _, b) = send(
            &r,
            req("GET", &format!("/blob/sha256?bytes={n}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let v: Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["sha256"], blob_sha256(n));
    }

    #[tokio::test]
    async fn chunks_in_order_give_whole_hash() {
        let r = router();
        let (a, b) = (vec![1u8; 1000], vec![2u8; 500]);
        for (k, data) in [(0, &a), (1, &b)] {
            let (s, _, body) = send(
                &r,
                req("PUT", &format!("/chunk?upload=u1&part={k}"))
                    .body(Body::from(data.clone()))
                    .unwrap(),
            )
            .await;
            assert_eq!(s, StatusCode::OK);
            let v: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(v["bytes"], data.len());
            assert_eq!(v["sha256"], hex::encode(Sha256::digest(data)));
        }
        let (s, _, body) = send(
            &r,
            req("POST", "/finish?upload=u1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["bytes"], 1500);
        assert_eq!(v["parts"], 2);
        assert_eq!(v["sha256"], hex::encode(Sha256::digest([a, b].concat())));
        let (s, _, _) = send(
            &r,
            req("POST", "/finish?upload=u1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn out_of_order_chunks_have_no_whole_hash() {
        let r = router();
        for k in [1, 0] {
            send(
                &r,
                req("PUT", &format!("/chunk?upload=u2&part={k}"))
                    .body(Body::from("x"))
                    .unwrap(),
            )
            .await;
        }
        let (_, _, body) = send(
            &r,
            req("POST", "/finish?upload=u2")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["bytes"], 2);
        assert!(v["sha256"].is_null());
    }

    async fn rpc(r: &Router, body: Value) -> (StatusCode, Value) {
        let (s, _, b) = send(
            r,
            req("POST", "/mcp")
                .header(header::CONTENT_TYPE, "application/json")
                .header(PUBLIC_BASE_HEADER, "https://spike.example.workers.dev/KEY")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await;
        (
            s,
            if b.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&b).unwrap()
            },
        )
    }

    #[tokio::test]
    async fn mcp_flow() {
        let r = router();
        let (_, v) = rpc(&r, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}})).await;
        assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
        let (s, _) = rpc(
            &r,
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )
        .await;
        assert_eq!(s, StatusCode::ACCEPTED);
        let (_, v) = rpc(&r, json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).await;
        let names: Vec<&str> = v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["hold", "blob_url"]);
        let (_, v) = rpc(&r, json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"hold","arguments":{"seconds":1}}})).await;
        assert_eq!(v["result"]["content"][0]["text"], "held 1 s");
        let (_, v) = rpc(&r, json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"hold","arguments":{"seconds":0}}})).await;
        assert_eq!(v["result"]["isError"], true);
        let (_, v) = rpc(&r, json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"blob_url","arguments":{"bytes":1000}}})).await;
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("https://spike.example.workers.dev/KEY/blob?bytes=1000"),
            "{text}"
        );
        assert!(text.contains(&blob_sha256(1000)));
        let (_, v) = rpc(&r, json!({"jsonrpc":"2.0","id":6,"method":"nope"})).await;
        assert_eq!(v["error"]["code"], -32601);
    }
}
