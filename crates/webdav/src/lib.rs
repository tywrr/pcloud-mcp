//! A [`pcloud_mcp_core::Storage`] backed by a WebDAV server (in practice `rclone serve webdav`
//! over the pCloud Drive mount). HTTP goes through the [`Transport`] trait: `worker::Fetch` or
//! a Workers VPC binding on Workers, a blocking client in native tests.
//!
//! Only read methods are used: `PROPFIND` (Depth 0, 1 and infinity) and `GET` with `Range`.

mod multistatus;
mod paths;
mod time;

use std::cell::Cell;
use std::collections::VecDeque;
use std::fmt;
use std::ops::ControlFlow;

use base64::Engine as _;
use pcloud_mcp_core::{ByteRange, Entry, NameMatcher, Storage, StorageError};

use crate::multistatus::parse_multistatus;
use crate::paths::{encode_path, is_direct_child};

/// One HTTP request to the WebDAV server. `path_and_query` is already percent-encoded and
/// relative to the server root; the transport adds its base URL (or binding) in front.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransportRequest {
    pub method: String,
    pub path_and_query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Most body bytes the caller can use. A transport should stop reading the body once it
    /// passes this, close the connection so the server stops sending, and set
    /// [`TransportResponse::truncated`]. One that reads everything is still correct, only
    /// slower: a `Depth: infinity` answer for a big tree can run to tens of megabytes. `None`
    /// means the whole body.
    pub max_response_bytes: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransportResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The body was longer than the request's `max_response_bytes`; `body` holds only its
    /// start.
    pub truncated: bool,
}

impl TransportResponse {
    /// First value of a header, matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Sends HTTP requests to the WebDAV server. An `Err` means no response arrived at all
/// (connection refused, DNS, a runtime error); HTTP error statuses are `Ok`.
///
/// No `Send` bound, like [`Storage`]: Workers futures aren't `Send`.
#[allow(async_fn_in_trait)]
pub trait Transport {
    async fn send(&self, request: TransportRequest) -> Result<TransportResponse, String>;

    /// The URL path the WebDAV server is mounted at, such as `/dav` when the base URL is
    /// `http://host:8080/dav`. Hrefs in PROPFIND answers carry it and it gets stripped.
    fn base_path(&self) -> &str {
        ""
    }
}

/// Most PROPFIND requests a fallback search walk sends. The Workers free plan allows 50
/// subrequests per request; this leaves room for the Depth: infinity attempt and the rest.
pub const DEFAULT_WALK_BUDGET: usize = 40;

/// Most PROPFIND answer bytes one storage parses, its listings and searches together. Parsing
/// is the Worker's main CPU cost: 400 kB is about 1,000 entries from rclone, which take a
/// search about 4 ms and a listing about 7 ms in V8, of the 10 ms the Workers free plan allows
/// a request.
pub const DEFAULT_MAX_LISTING_BYTES: usize = 400_000;

/// Asks only for what [`Entry`] needs. Against rclone this makes answers about 30% smaller
/// than the default "allprop" (no lock, etag or display name), which matters for a
/// Depth: infinity search over a big tree.
const PROPFIND_BODY: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<D:propfind xmlns:D="DAV:"><D:prop>"#,
    r#"<D:resourcetype/><D:getcontentlength/><D:getlastmodified/><D:getcontenttype/>"#,
    r#"</D:prop></D:propfind>"#
);

pub struct WebDavStorage<T: Transport> {
    transport: T,
    authorization: String,
    walk_budget: usize,
    max_listing_bytes: usize,
    /// PROPFIND answer bytes taken in so far, against `max_listing_bytes`.
    parsed: Cell<usize>,
    /// PROPFIND answer bytes received so far, parsed or not (cut-off and oversized answers
    /// too), against [`RECEIVE_FACTOR`] times `max_listing_bytes`.
    received: Cell<usize>,
}

/// How many times `max_listing_bytes` one storage may receive in PROPFIND answers, the ones
/// too big to parse included. Without it, a split search whose subfolders are all too big
/// would read up to `max_listing_bytes` for each of them before giving up.
const RECEIVE_FACTOR: usize = 2;

impl<T: Transport> fmt::Debug for WebDavStorage<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the Authorization header: it is the password in base64.
        f.debug_struct("WebDavStorage")
            .field("walk_budget", &self.walk_budget)
            .field("max_listing_bytes", &self.max_listing_bytes)
            .field("parsed", &self.parsed.get())
            .field("received", &self.received.get())
            .finish_non_exhaustive()
    }
}

impl<T: Transport> WebDavStorage<T> {
    /// Every request carries HTTP Basic auth with `user` and `password`.
    pub fn new(transport: T, user: &str, password: &str) -> Self {
        let credentials =
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        Self {
            transport,
            authorization: format!("Basic {credentials}"),
            walk_budget: DEFAULT_WALK_BUDGET,
            max_listing_bytes: DEFAULT_MAX_LISTING_BYTES,
            parsed: Cell::new(0),
            received: Cell::new(0),
        }
    }

    /// Caps the requests a search sends when the server refuses `Depth: infinity`.
    pub fn with_walk_budget(mut self, requests: usize) -> Self {
        self.walk_budget = requests;
        self
    }

    /// Caps the PROPFIND answer bytes this storage parses, all its listings and searches
    /// together (see [`DEFAULT_MAX_LISTING_BYTES`]); past it they fail with
    /// [`StorageError::TooManyEntries`], and the transport is asked to stop reading there. The
    /// cap is for CPU time, which Workers limits per request, so build one storage per request,
    /// as the Worker does: a split search then can't parse a full listing for every subfolder.
    /// Answers too big to parse still cost the time to receive them, so the storage also stops
    /// asking once it has received twice this much in all.
    pub fn with_max_listing_bytes(mut self, bytes: usize) -> Self {
        self.max_listing_bytes = bytes;
        self
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        mut headers: Vec<(String, String)>,
        body: Vec<u8>,
        max_response_bytes: Option<usize>,
    ) -> Result<TransportResponse, StorageError> {
        headers.push(("Authorization".into(), self.authorization.clone()));
        let request = TransportRequest {
            method: method.into(),
            path_and_query: encode_path(path),
            headers,
            body,
            max_response_bytes,
        };
        self.transport.send(request).await.map_err(|e| {
            StorageError::Backend(format!(
                "could not reach the WebDAV server ({e}); check that rclone is running and the tunnel is up"
            ))
        })
    }

    async fn propfind(
        &self,
        path: &str,
        depth: &str,
        max_bytes: usize,
    ) -> Result<TransportResponse, StorageError> {
        let headers = vec![
            ("Depth".into(), depth.into()),
            (
                "Content-Type".into(),
                "application/xml; charset=utf-8".into(),
            ),
        ];
        let response = self
            .send(
                "PROPFIND",
                path,
                headers,
                PROPFIND_BODY.into(),
                Some(max_bytes),
            )
            .await?;
        self.received
            .set(self.received.get().saturating_add(response.body.len()));
        Ok(response)
    }

    fn parse<W, F>(&self, body: &[u8], wanted: W, visit: F) -> Result<(), StorageError>
    where
        W: FnMut(&str, &str) -> bool,
        F: FnMut(Entry) -> ControlFlow<()>,
    {
        parse_multistatus(body, self.transport.base_path(), wanted, visit)
            .map_err(StorageError::Backend)
    }

    /// How many more listing bytes this storage may take in, or the error when none are left
    /// (any answer has some, so there's no point asking): what is left to parse, or to
    /// receive, whichever is less.
    fn room(&self, path: &str) -> Result<usize, StorageError> {
        let max = self.max_listing_bytes;
        let receivable = max
            .saturating_mul(RECEIVE_FACTOR)
            .saturating_sub(self.received.get());
        if receivable == 0 {
            return Err(StorageError::TooManyEntries(format!(
                "{path} (not asked for: this request already received {} bytes of WebDAV \
                 listings, its limit)",
                self.received.get()
            )));
        }
        match max.saturating_sub(self.parsed.get()) {
            0 => Err(self.too_many_entries(path, None, 0)),
            room => Ok(room.min(receivable)),
        }
    }

    /// The body of a 207 answer that fits in `room`, which it then uses up.
    fn take_listing(
        &self,
        path: &str,
        response: TransportResponse,
        room: usize,
    ) -> Result<Vec<u8>, StorageError> {
        if response.truncated || response.body.len() > room {
            let size = (!response.truncated).then_some(response.body.len());
            return Err(self.too_many_entries(path, size, room));
        }
        self.parsed.set(self.parsed.get() + response.body.len());
        Ok(response.body)
    }

    /// The error for a listing of `bytes` (unknown when the transport stopped reading) that
    /// doesn't fit in `room`.
    fn too_many_entries(&self, path: &str, bytes: Option<usize>, room: usize) -> StorageError {
        let max = self.max_listing_bytes;
        let limit = if room < max {
            format!("the {room} bytes left of the {max}-byte limit")
        } else {
            format!("the {max}-byte limit")
        };
        let listing = match bytes {
            Some(bytes) => format!("a {bytes}-byte WebDAV listing"),
            None => "a WebDAV listing".into(),
        };
        StorageError::TooManyEntries(format!("{path} ({listing}, over {limit})"))
    }

    /// A Depth 1 PROPFIND answer for `path`, not yet parsed.
    async fn fetch_listing(&self, path: &str) -> Result<Vec<u8>, StorageError> {
        let room = self.room(path)?;
        let response = self.propfind(path, "1", room).await?;
        if response.status != 207 {
            return Err(status_error(response.status, "PROPFIND", path));
        }
        self.take_listing(path, response, room)
    }

    /// The direct children of `path` in its Depth 1 PROPFIND answer.
    fn children(&self, path: &str, body: &[u8]) -> Result<Vec<Entry>, StorageError> {
        let mut own_entry_is_file = false;
        let mut children = Vec::new();
        self.parse(
            body,
            |_, _| true,
            |entry| {
                if entry.path == path {
                    own_entry_is_file = !entry.is_dir;
                } else if is_direct_child(path, &entry.path) {
                    children.push(entry);
                }
                ControlFlow::Continue(())
            },
        )?;
        if own_entry_is_file {
            return Err(StorageError::NotADirectory(path.into()));
        }
        Ok(children)
    }

    /// Breadth-first search with Depth 1 listings, for servers that refuse Depth: infinity.
    /// The listings draw on the same `max_listing_bytes` as the single answer would.
    async fn walk(
        &self,
        root: &str,
        matcher: &mut NameMatcher,
        limit: usize,
    ) -> Result<Vec<Entry>, StorageError> {
        let mut queue = VecDeque::from([root.to_owned()]);
        let mut found = Vec::new();
        let mut requests = 0;
        while let Some(dir) = queue.pop_front() {
            if requests >= self.walk_budget {
                break;
            }
            requests += 1;
            let listing = self.fetch_listing(&dir).await;
            let children = match listing.and_then(|body| self.children(&dir, &body)) {
                Ok(children) => children,
                Err(e) if dir == root => return Err(e),
                Err(StorageError::TooManyEntries(_)) => {
                    return Err(StorageError::TooManyEntries(format!(
                        "{root} (its WebDAV listings together are over the {}-byte limit)",
                        self.max_listing_bytes
                    )));
                }
                // An unreadable subfolder shouldn't sink the whole search.
                Err(_) => continue,
            };
            for child in children {
                if child.is_dir {
                    queue.push_back(child.path.clone());
                }
                if matcher.matches(&child.name) {
                    found.push(child);
                    if found.len() >= limit {
                        return Ok(found);
                    }
                }
            }
        }
        Ok(found)
    }
}

impl<T: Transport> Storage for WebDavStorage<T> {
    async fn list(&self, path: &str) -> Result<Vec<Entry>, StorageError> {
        let body = self.fetch_listing(path).await?;
        self.children(path, &body)
    }

    async fn stat(&self, path: &str) -> Result<Entry, StorageError> {
        // One entry: tiny, so it doesn't count against the listing budget, but bounded all the
        // same in case a server answers with far more.
        let response = self.propfind(path, "0", self.max_listing_bytes).await?;
        if response.status != 207 {
            return Err(status_error(response.status, "PROPFIND", path));
        }
        if response.truncated {
            return Err(StorageError::Backend(format!(
                "the WebDAV answer for {path} is over {} bytes",
                self.max_listing_bytes
            )));
        }
        let mut found = None;
        self.parse(
            &response.body,
            |_, _| true,
            |entry| {
                if entry.path == path {
                    found = Some(entry);
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            },
        )?;
        found.ok_or_else(|| {
            StorageError::Backend(format!(
                "the WebDAV answer for {path} did not describe it; does BACKEND_URL point at the root rclone serves?"
            ))
        })
    }

    async fn read(&self, path: &str, range: ByteRange) -> Result<Vec<u8>, StorageError> {
        if range.length == 0 {
            return Ok(Vec::new());
        }
        let last = range.offset.saturating_add(range.length - 1);
        let headers = vec![("Range".into(), format!("bytes={}-{last}", range.offset))];
        let response = self.send("GET", path, headers, Vec::new(), None).await?;
        match response.status {
            206 => {
                // Servers may answer with a range that starts earlier than asked; anything
                // starting later would leave a gap.
                let start = response
                    .header("Content-Range")
                    .and_then(content_range_start)
                    .unwrap_or(range.offset);
                if start > range.offset {
                    return Err(StorageError::Backend(format!(
                        "GET {path} returned bytes from {start}, not {}",
                        range.offset
                    )));
                }
                Ok(take_range(
                    response.body,
                    range.offset - start,
                    range.length,
                ))
            }
            // The server ignored Range and sent the whole file.
            200 => Ok(take_range(response.body, range.offset, range.length)),
            // Range Not Satisfiable: the offset is at or past the end.
            416 => Ok(Vec::new()),
            // rclone answers GET on a folder (without a trailing slash) with 405.
            405 => match self.stat(path).await {
                Ok(entry) if entry.is_dir => Err(StorageError::IsADirectory(path.into())),
                _ => Err(status_error(405, "GET", path)),
            },
            status => Err(status_error(status, "GET", path)),
        }
    }

    async fn search(
        &self,
        root: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Entry>, StorageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut matcher = NameMatcher::new(query);
        // One request for the whole tree when the server allows it (rclone does).
        let room = self.room(root)?;
        let response = self.propfind(root, "infinity", room).await?;
        match response.status {
            207 => {}
            // RFC 4918 lets servers refuse infinite depth with 403; others say 400 or 501.
            400 | 403 | 501 => return self.walk(root, &mut matcher, limit).await,
            status => return Err(status_error(status, "PROPFIND", root)),
        }
        let body = self.take_listing(root, response, room)?;
        let mut root_is_file = false;
        let mut found = Vec::new();
        self.parse(
            &body,
            // Only matches (and the root, to tell a file from a folder) become entries.
            |path, name| path == root || matcher.matches(name),
            |entry| {
                if entry.path == root {
                    root_is_file = !entry.is_dir;
                    return ControlFlow::Continue(());
                }
                found.push(entry);
                if found.len() >= limit {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            },
        )?;
        if root_is_file {
            return Err(StorageError::NotADirectory(root.into()));
        }
        Ok(found)
    }
}

/// `bytes 10-19/268` to 10.
fn content_range_start(value: &str) -> Option<u64> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    rest.split('-').next()?.trim().parse().ok()
}

fn take_range(mut body: Vec<u8>, skip: u64, length: u64) -> Vec<u8> {
    let start = usize::try_from(skip).unwrap_or(usize::MAX).min(body.len());
    let end = start
        .saturating_add(usize::try_from(length).unwrap_or(usize::MAX))
        .min(body.len());
    body.truncate(end);
    body.drain(..start);
    body
}

/// Maps an HTTP error status to a [`StorageError`]. Messages name the path, never credentials.
fn status_error(status: u16, method: &str, path: &str) -> StorageError {
    match status {
        404 | 410 => StorageError::NotFound(path.into()),
        401 => StorageError::PermissionDenied(format!(
            "{path} (the WebDAV server rejected the login; check that BACKEND_USER and \
             BACKEND_PASSWORD match the user and password rclone was started with)"
        )),
        403 => StorageError::PermissionDenied(format!(
            "{path} (the WebDAV server refused access; check BACKEND_USER and BACKEND_PASSWORD, \
             and that the folder is readable: a locked pCloud Crypto folder is not)"
        )),
        502 | 503 | 504 | 530 => StorageError::Backend(format!(
            "{method} {path} returned HTTP {status}; check that rclone is running and the tunnel is up"
        )),
        _ => StorageError::Backend(format!("{method} {path} returned HTTP {status}")),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// Answers from a script and records what was sent. It honours `max_response_bytes` as
    /// the Worker's transports do.
    #[derive(Default)]
    struct FakeTransport {
        answers: RefCell<VecDeque<TransportResponse>>,
        sent: RefCell<Vec<TransportRequest>>,
    }

    impl FakeTransport {
        fn answering(answers: Vec<TransportResponse>) -> Self {
            Self {
                answers: RefCell::new(answers.into()),
                sent: RefCell::default(),
            }
        }
    }

    impl Transport for FakeTransport {
        async fn send(&self, request: TransportRequest) -> Result<TransportResponse, String> {
            let max = request.max_response_bytes;
            self.sent.borrow_mut().push(request);
            let mut answer = self
                .answers
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| "connection refused".to_owned())?;
            if let Some(max) = max
                && answer.body.len() > max
            {
                answer.body.truncate(max);
                answer.truncated = true;
            }
            Ok(answer)
        }
    }

    /// A transport that reads whole bodies whatever the limit, as the contract allows.
    struct Unbounded(FakeTransport);

    impl Transport for Unbounded {
        async fn send(&self, mut request: TransportRequest) -> Result<TransportResponse, String> {
            request.max_response_bytes = None;
            self.0.send(request).await
        }
    }

    fn answer(status: u16, headers: &[(&str, &str)], body: &[u8]) -> TransportResponse {
        TransportResponse {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
            body: body.to_vec(),
            truncated: false,
        }
    }

    fn storage(answers: Vec<TransportResponse>) -> WebDavStorage<FakeTransport> {
        WebDavStorage::new(FakeTransport::answering(answers), "mcp", "s3cret-pw")
    }

    fn header<'a>(request: &'a TransportRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn block<F: Future>(f: F) -> F::Output {
        pollster::block_on(f)
    }

    const ROOT_DEPTH1: &[u8] = include_bytes!("../tests/data/rclone-root-depth1.xml");
    const ROOT_INFINITY: &[u8] = include_bytes!("../tests/data/rclone-root-infinity.xml");
    const FILE_DEPTH0: &[u8] = include_bytes!("../tests/data/rclone-file-depth0.xml");

    #[test]
    fn list_sends_propfind_depth_1_with_basic_auth() {
        let s = storage(vec![answer(207, &[], ROOT_DEPTH1)]);
        let entries = block(s.list("/")).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Café menu.txt",
                "README.txt",
                "binary.bin",
                "deep",
                "images",
                "notes"
            ]
        );
        let sent = s.transport().sent.borrow();
        let req = &sent[0];
        assert_eq!(req.method, "PROPFIND");
        assert_eq!(req.path_and_query, "/");
        assert_eq!(header(req, "Depth"), Some("1"));
        // base64("mcp:s3cret-pw")
        assert_eq!(
            header(req, "Authorization"),
            Some("Basic bWNwOnMzY3JldC1wdw==")
        );
        assert!(String::from_utf8_lossy(&req.body).contains("getcontentlength"));
    }

    #[test]
    fn list_encodes_the_path_and_rejects_files() {
        let s = storage(vec![answer(207, &[], FILE_DEPTH0)]);
        let err = block(s.list("/Café menu.txt")).unwrap_err();
        assert_eq!(err, StorageError::NotADirectory("/Café menu.txt".into()));
        assert_eq!(
            s.transport().sent.borrow()[0].path_and_query,
            "/Caf%C3%A9%20menu.txt"
        );
    }

    #[test]
    fn stat_uses_depth_0() {
        let s = storage(vec![answer(207, &[], FILE_DEPTH0)]);
        let entry = block(s.stat("/Café menu.txt")).unwrap();
        assert_eq!(entry.size, Some(100));
        assert_eq!(header(&s.transport().sent.borrow()[0], "Depth"), Some("0"));
    }

    #[test]
    fn status_mapping_never_leaks_the_password() {
        let s = storage(vec![
            answer(404, &[], b"Not Found"),
            answer(401, &[], b"401 Unauthorized"),
            answer(403, &[], b""),
            answer(500, &[], b""),
            answer(530, &[], b""),
        ]);
        assert_eq!(
            block(s.stat("/nope")).unwrap_err(),
            StorageError::NotFound("/nope".into())
        );
        let unauthorized = block(s.stat("/a")).unwrap_err();
        assert!(matches!(unauthorized, StorageError::PermissionDenied(_)));
        assert!(unauthorized.to_string().contains("BACKEND_PASSWORD"));
        let forbidden = block(s.list("/a")).unwrap_err();
        assert!(matches!(forbidden, StorageError::PermissionDenied(_)));
        assert!(matches!(
            block(s.stat("/a")).unwrap_err(),
            StorageError::Backend(_)
        ));
        let tunnel = block(s.stat("/a")).unwrap_err();
        assert!(tunnel.to_string().contains("tunnel"), "{tunnel}");
        // The script is exhausted: the fake now fails like a refused connection.
        let unreachable = block(s.stat("/a")).unwrap_err();
        assert!(matches!(unreachable, StorageError::Backend(_)));
        for err in [unauthorized, forbidden, unreachable] {
            let text = err.to_string();
            assert!(!text.contains("s3cret"), "{text}");
            assert!(!text.contains("bWNwOnMzY3JldC1wdw"), "{text}");
        }
        assert!(!format!("{s:?}").contains("bWNw"));
    }

    #[test]
    fn read_asks_for_an_inclusive_byte_range() {
        let s = storage(vec![answer(
            206,
            &[("Content-Range", "bytes 10-14/268")],
            b"01234",
        )]);
        assert_eq!(
            block(s.read(
                "/README.txt",
                ByteRange {
                    offset: 10,
                    length: 5
                }
            ))
            .unwrap(),
            b"01234"
        );
        let sent = s.transport().sent.borrow();
        assert_eq!(sent[0].method, "GET");
        assert_eq!(header(&sent[0], "Range"), Some("bytes=10-14"));
        assert!(sent[0].body.is_empty());
    }

    #[test]
    fn read_handles_short_and_shifted_partial_content() {
        // Near the end of the file the server sends fewer bytes than asked.
        let s = storage(vec![
            answer(206, &[("Content-Range", "bytes 260-267/268")], b"e them.\n"),
            // A server that rounds the range down to a block boundary.
            answer(206, &[("content-range", "bytes 0-9/268")], b"0123456789"),
            // One that starts later than asked can't be used.
            answer(206, &[("Content-Range", "bytes 8-9/268")], b"89"),
        ]);
        assert_eq!(
            block(s.read(
                "/f",
                ByteRange {
                    offset: 260,
                    length: 100
                }
            ))
            .unwrap(),
            b"e them.\n"
        );
        assert_eq!(
            block(s.read(
                "/f",
                ByteRange {
                    offset: 4,
                    length: 3
                }
            ))
            .unwrap(),
            b"456"
        );
        assert!(matches!(
            block(s.read(
                "/f",
                ByteRange {
                    offset: 4,
                    length: 3
                }
            ))
            .unwrap_err(),
            StorageError::Backend(_)
        ));
    }

    #[test]
    fn read_slices_a_full_200_answer() {
        let s = storage(vec![
            answer(200, &[], b"0123456789"),
            answer(200, &[], b"0123456789"),
            answer(200, &[], b"0123456789"),
        ]);
        let r = |offset, length| block(s.read("/f", ByteRange { offset, length })).unwrap();
        assert_eq!(r(2, 3), b"234");
        assert_eq!(r(8, 100), b"89");
        assert_eq!(r(50, 5), b"");
    }

    #[test]
    fn read_past_the_end_is_empty() {
        let s = storage(vec![answer(416, &[("Content-Range", "bytes */268")], b"")]);
        assert_eq!(
            block(s.read(
                "/f",
                ByteRange {
                    offset: 268,
                    length: 10
                }
            ))
            .unwrap(),
            b""
        );
    }

    #[test]
    fn zero_length_read_sends_nothing() {
        let s = storage(vec![]);
        assert_eq!(
            block(s.read(
                "/f",
                ByteRange {
                    offset: 0,
                    length: 0
                }
            ))
            .unwrap(),
            b""
        );
        assert!(s.transport().sent.borrow().is_empty());
    }

    #[test]
    fn huge_ranges_saturate() {
        let s = storage(vec![answer(
            206,
            &[("Content-Range", "bytes 5-9/10")],
            b"56789",
        )]);
        assert_eq!(
            block(s.read(
                "/f",
                ByteRange {
                    offset: 5,
                    length: u64::MAX
                }
            ))
            .unwrap(),
            b"56789"
        );
        assert_eq!(
            header(&s.transport().sent.borrow()[0], "Range"),
            Some(format!("bytes=5-{}", u64::MAX).as_str())
        );
    }

    #[test]
    fn reading_a_folder_is_reported_as_such() {
        let s = storage(vec![
            answer(405, &[], b"Method Not Allowed"),
            answer(
                207,
                &[],
                include_bytes!("../tests/data/rclone-allprop-depth1.xml"),
            ),
        ]);
        assert_eq!(
            block(s.read(
                "/notes",
                ByteRange {
                    offset: 0,
                    length: 10
                }
            ))
            .unwrap_err(),
            StorageError::IsADirectory("/notes".into())
        );
    }

    #[test]
    fn search_uses_one_infinite_propfind() {
        let s = storage(vec![answer(207, &[], ROOT_INFINITY)]);
        let hits = block(s.search("/", "LEAF", 10)).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/deep/a/b/c/leaf.txt");
        let sent = s.transport().sent.borrow();
        assert_eq!(sent.len(), 1);
        assert_eq!(header(&sent[0], "Depth"), Some("infinity"));
    }

    #[test]
    fn search_matches_folders_and_respects_the_limit() {
        let s = storage(vec![
            answer(207, &[], ROOT_INFINITY),
            answer(207, &[], ROOT_INFINITY),
        ]);
        // "e" is in most names; the root itself never matches.
        assert_eq!(block(s.search("/", "e", 3)).unwrap().len(), 3);
        let folders = block(s.search("/", "DEEP", 10)).unwrap();
        assert_eq!(folders.len(), 1);
        assert!(folders[0].is_dir);
        assert!(block(s.search("/", "x", 0)).unwrap().is_empty());
    }

    /// A multistatus answer for `href` (a folder) and its children; hrefs ending in a slash
    /// are folders.
    fn depth1(href: &str, children: &[&str]) -> TransportResponse {
        let mut xml = String::from(r#"<D:multistatus xmlns:D="DAV:">"#);
        for (i, h) in std::iter::once(&href).chain(children).enumerate() {
            let dir = h.ends_with('/');
            xml.push_str(&format!(
                "<D:response><D:href>{h}</D:href><D:propstat><D:prop><D:resourcetype>{}</D:resourcetype><D:getlastmodified>Fri, 30 Oct 2026 00:32:52 GMT</D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
                if dir || i == 0 { "<D:collection/>" } else { "" }
            ));
        }
        xml.push_str("</D:multistatus>");
        answer(207, &[], xml.as_bytes())
    }

    #[test]
    fn search_matches_names_in_either_normalization_form() {
        // macOS writes names decomposed: "Café" as "Cafe" + U+0301 COMBINING ACUTE ACCENT.
        let tree = || {
            depth1(
                "/",
                &[
                    "/Cafe%CC%81%20menu.txt",
                    "/Caf%C3%A9%20bill.txt",
                    "/cafeteria/",
                    "/Other.txt",
                ],
            )
        };
        let s = storage(vec![tree(), tree(), tree()]);
        let names = |query: &str| -> Vec<String> {
            block(s.search("/", query, 10))
                .unwrap()
                .into_iter()
                .map(|e| e.name)
                .collect()
        };
        // A composed query finds the decomposed name, and the other way round.
        assert_eq!(names("caf\u{e9} m"), ["Cafe\u{301} menu.txt"]);
        assert_eq!(names("CAFE\u{301} B"), ["Caf\u{e9} bill.txt"]);
        // Accents are ignored altogether.
        assert_eq!(
            names("cafe"),
            ["Cafe\u{301} menu.txt", "Caf\u{e9} bill.txt", "cafeteria"]
        );
        // Matches keep the name as the server spelled it, with full details.
        let hit = &block(storage(vec![tree()]).search("/", "menu", 10)).unwrap()[0];
        assert_eq!(hit.path, "/Cafe\u{301} menu.txt");
        assert_eq!(hit.modified.as_deref(), Some("2026-10-30T00:32:52Z"));
    }

    #[test]
    fn listings_over_the_byte_limit_are_not_parsed() {
        let max = ROOT_DEPTH1.len() - 1;
        let s = storage(vec![
            answer(207, &[], ROOT_DEPTH1),
            answer(207, &[], ROOT_INFINITY),
            answer(207, &[], ROOT_DEPTH1),
        ])
        .with_max_listing_bytes(max);
        let err = block(s.list("/")).unwrap_err();
        assert_eq!(
            err,
            StorageError::TooManyEntries(format!(
                "/ (a WebDAV listing, over the {max}-byte limit)"
            ))
        );
        let err = block(s.search("/", "leaf", 10)).unwrap_err();
        assert!(matches!(err, StorageError::TooManyEntries(_)), "{err:?}");
        // The transport was asked to stop reading at the limit.
        let sent = s.transport().sent.borrow();
        assert!(sent.iter().all(|r| r.max_response_bytes == Some(max)));
        drop(sent);
        // At the limit is fine.
        let s =
            storage(vec![answer(207, &[], ROOT_DEPTH1)]).with_max_listing_bytes(ROOT_DEPTH1.len());
        assert_eq!(block(s.list("/")).unwrap().len(), 6);
    }

    #[test]
    fn a_truncated_answer_is_too_many_entries_whatever_its_size() {
        // A transport that stopped reading says so; the start it kept is never parsed.
        let mut cut = answer(207, &[], &ROOT_INFINITY[..100]);
        cut.truncated = true;
        let s = storage(vec![cut.clone(), cut]);
        let err = block(s.search("/", "leaf", 10)).unwrap_err();
        assert_eq!(
            err,
            StorageError::TooManyEntries(format!(
                "/ (a WebDAV listing, over the {DEFAULT_MAX_LISTING_BYTES}-byte limit)"
            ))
        );
        assert!(matches!(
            block(s.list("/")).unwrap_err(),
            StorageError::TooManyEntries(_)
        ));
        // A transport that ignores the limit is caught by the size check instead.
        let s = WebDavStorage::new(
            Unbounded(FakeTransport::answering(vec![answer(
                207,
                &[],
                ROOT_INFINITY,
            )])),
            "mcp",
            "pw",
        )
        .with_max_listing_bytes(100);
        let err = block(s.search("/", "leaf", 10)).unwrap_err();
        assert_eq!(
            err,
            StorageError::TooManyEntries(format!(
                "/ (a {}-byte WebDAV listing, over the 100-byte limit)",
                ROOT_INFINITY.len()
            ))
        );
    }

    #[test]
    fn only_propfind_bodies_are_bounded() {
        let s = storage(vec![
            answer(207, &[], FILE_DEPTH0),
            answer(206, &[("Content-Range", "bytes 0-4/100")], b"01234"),
        ]);
        block(s.stat("/Café menu.txt")).unwrap();
        block(s.read(
            "/Café menu.txt",
            ByteRange {
                offset: 0,
                length: 5,
            },
        ))
        .unwrap();
        let sent = s.transport().sent.borrow();
        assert_eq!(sent[0].max_response_bytes, Some(DEFAULT_MAX_LISTING_BYTES));
        // GET bodies are bounded by their Range instead.
        assert_eq!(sent[1].max_response_bytes, None);
    }

    #[test]
    fn one_storage_parses_at_most_the_limit_in_all() {
        let listing = || answer(207, &[], ROOT_DEPTH1);
        let max = ROOT_DEPTH1.len() * 2 - 1;
        let s = storage(vec![listing(), listing(), listing()]).with_max_listing_bytes(max);
        assert_eq!(block(s.list("/")).unwrap().len(), 6);
        // The second listing gets only what is left, and doesn't fit in it.
        let left = max - ROOT_DEPTH1.len();
        let err = block(s.list("/")).unwrap_err();
        assert_eq!(
            err,
            StorageError::TooManyEntries(format!(
                "/ (a WebDAV listing, over the {left} bytes left of the {max}-byte limit)"
            ))
        );
        assert_eq!(
            s.transport().sent.borrow()[1].max_response_bytes,
            Some(left)
        );
        // Stats don't count.
        let s = storage(vec![listing(), answer(207, &[], FILE_DEPTH0), listing()])
            .with_max_listing_bytes(ROOT_DEPTH1.len());
        block(s.list("/")).unwrap();
        block(s.stat("/Café menu.txt")).unwrap();
        // With nothing left, a listing isn't even asked for.
        assert!(matches!(
            block(s.search("/", "x", 10)).unwrap_err(),
            StorageError::TooManyEntries(_)
        ));
        assert_eq!(s.transport().sent.borrow().len(), 2);
    }

    #[test]
    fn answers_too_big_to_parse_still_use_up_a_receive_budget() {
        // A split search asks for one oversized subfolder after another; each answer is cut
        // off and refused, but what came in counts.
        let max = 1000;
        let big = || answer(207, &[], &vec![b' '; max * 3]);
        let s = storage((0..10).map(|_| big()).collect()).with_max_listing_bytes(max);
        for i in 0..10 {
            let err = block(s.search(&format!("/sub{i}"), "x", 10)).unwrap_err();
            assert!(matches!(err, StorageError::TooManyEntries(_)), "{err:?}");
        }
        let sent = s.transport().sent.borrow();
        let asked: usize = sent.iter().map(|r| r.max_response_bytes.unwrap()).sum();
        assert_eq!(sent.len(), 2, "only two answers fit in the receive budget");
        assert!(asked <= RECEIVE_FACTOR * max, "asked for {asked} bytes");
        drop(sent);
        // Once spent, nothing more is asked for, a listing included.
        let err = block(s.list("/")).unwrap_err();
        assert_eq!(
            err,
            StorageError::TooManyEntries(
                "/ (not asked for: this request already received 2000 bytes of WebDAV \
                 listings, its limit)"
                    .into()
            )
        );
        assert_eq!(s.transport().sent.borrow().len(), 2);

        // A transport that ignores the limit is charged what it actually read.
        let s = WebDavStorage::new(
            Unbounded(FakeTransport::answering((0..10).map(|_| big()).collect())),
            "mcp",
            "pw",
        )
        .with_max_listing_bytes(max);
        for i in 0..10 {
            let _ = block(s.search(&format!("/sub{i}"), "x", 10));
        }
        assert_eq!(s.transport().0.sent.borrow().len(), 1);
    }

    #[test]
    fn a_walk_shares_one_byte_limit() {
        let refuse = || answer(403, &[], b"");
        let root = depth1("/", &["/a/", "/b/"]);
        let a = depth1("/a/", &["/a/x.txt"]);
        let limit = root.body.len() + a.body.len();
        let b = depth1("/b/", &["/b/match.txt"]);
        let s = storage(vec![refuse(), root.clone(), a.clone(), b.clone()])
            .with_max_listing_bytes(limit);
        let err = block(s.search("/", "match", 10)).unwrap_err();
        assert_eq!(
            err,
            StorageError::TooManyEntries(format!(
                "/ (its WebDAV listings together are over the {limit}-byte limit)"
            ))
        );
        // The budget was spent on / and /a, so /b was never asked for.
        assert_eq!(s.transport().sent.borrow().len(), 3);
        let s = storage(vec![refuse(), root, a, b]).with_max_listing_bytes(limit + 1000);
        assert_eq!(block(s.search("/", "match", 10)).unwrap().len(), 1);
    }

    #[test]
    fn search_matches_names_exactly_as_the_core_does() {
        // search_files matches names it lists itself with pcloud_mcp_core::name_matches, so a
        // search split around a turned-off folder must agree with this backend's search.
        let hrefs = [
            "/Cafe%CC%81%20menu.txt",
            "/Caf%C3%A9%20bill.txt",
            "/CAFETERIA/",
            "/stra%C3%9Fe.txt",
            "/%E3%81%8B%E3%82%99%E3%82%93.txt",
            "/Na%C3%AFve%20notes.md",
            "/plain.txt",
            "/e%CC%81.txt",
        ];
        let names: Vec<String> = hrefs
            .iter()
            .map(|h| {
                let path = paths::href_to_path(h, "").unwrap();
                paths::file_name(&path).to_owned()
            })
            .collect();
        for query in [
            "cafe",
            "CAF\u{c9}",
            "e\u{301}",
            "\u{1e9e}",
            "\u{304c}",
            "naive",
            "NA\u{cf}V",
            ".TXT",
            "\u{301}",
            "zzz",
        ] {
            let s = storage(vec![depth1("/", &hrefs)]);
            let mut backend: Vec<String> = block(s.search("/", query, 100))
                .unwrap()
                .into_iter()
                .map(|e| e.name)
                .collect();
            let mut core: Vec<String> = names
                .iter()
                .filter(|name| pcloud_mcp_core::name_matches(query, name))
                .cloned()
                .collect();
            backend.sort();
            core.sort();
            assert_eq!(backend, core, "query {query:?}");
        }
    }

    #[test]
    fn search_falls_back_to_a_bounded_walk() {
        let s = storage(vec![
            answer(
                403,
                &[],
                b"<D:error xmlns:D=\"DAV:\"><D:propfind-finite-depth/></D:error>",
            ),
            depth1("/", &["/a/", "/b/", "/match-root.txt"]),
            depth1("/a/", &["/a/match-a.txt", "/a/deeper/"]),
            depth1("/b/", &["/b/other.txt"]),
            depth1("/a/deeper/", &["/a/deeper/match-deep.txt"]),
        ])
        .with_walk_budget(3);
        let hits = block(s.search("/", "match", 10)).unwrap();
        // The budget of three listings stops the walk before /a/deeper.
        let paths: Vec<_> = hits.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["/match-root.txt", "/a/match-a.txt"]);
        let sent = s.transport().sent.borrow();
        assert_eq!(sent.len(), 4);
        assert!(sent[1..].iter().all(|r| header(r, "Depth") == Some("1")));
    }

    #[test]
    fn content_range_parsing() {
        assert_eq!(content_range_start("bytes 0-9/268"), Some(0));
        assert_eq!(content_range_start(" bytes 260-267/*"), Some(260));
        assert_eq!(content_range_start("bytes */268"), None);
        assert_eq!(content_range_start("items 1-2/3"), None);
    }
}
