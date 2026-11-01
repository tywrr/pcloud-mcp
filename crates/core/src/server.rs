//! The MCP endpoint: Streamable HTTP in stateless mode, JSON-RPC dispatch and the tools.
//!
//! Two protocol eras share the one endpoint. Clients of 2025-03-26 to 2025-11-25 (claude.ai
//! among them) open with `initialize`; the server answers it but keeps no session, so every
//! later request stands alone. Clients of 2026-07-28 skip the handshake and put the protocol
//! version and their capabilities in each request's `_meta`; those requests also get the
//! stricter header checks and result fields that revision asks for.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use rmcp::model::{
    CacheScope, DiscoverResult, ErrorCode, ErrorData, Implementation, InitializeResult,
    ListToolsResult, ProtocolVersion, ServerCapabilities, ToolsCapability,
};
use serde_json::{Map, Value, json};

use crate::health::Health;
use crate::http::{HttpRequest, HttpResponse};
use crate::index::Index;
use crate::links::LinkSettings;
use crate::policy::Policy;
use crate::scope::Scopes;
use crate::state::StateStore;
use crate::storage::{ByteRange, StorageWrite, read_capped};
use crate::tools::{self, Called, Parts};

/// Limits and identity. Build with struct-update syntax over [`ServerConfig::default`].
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub name: String,
    pub version: String,
    /// Most bytes `read_text_file` returns per call (Claude caps a tool result at ~150k chars).
    pub max_read_bytes: u64,
    /// Largest image `read_image` will return (base64 grows it by a third). Encoding and
    /// sending the default 1.5 MiB takes about 5 ms of CPU, half the Workers free plan's
    /// allowance per request.
    pub max_image_bytes: u64,
    /// Most results `search_files` returns.
    pub search_limit: usize,
    /// A text file, as an absolute path, whose first 8 KiB go into the instructions
    /// so the owner can tell Claude what each folder is for. `None` turns it off; a missing
    /// or unreadable file is left out quietly.
    pub guide_path: Option<String>,
    /// Origins allowed in an `Origin` header; any other gets 403. Requests without the header
    /// (server-to-server) are always allowed. The default is [`CLAUDE_ORIGINS`], in case
    /// Claude's requests carry one.
    pub allowed_origins: Vec<String>,
    /// How long one tool call may take. Past it, tools return what they have and say so.
    /// Phase 0 measured 200 s passing through Cloudflare; Claude gives up at 240 s.
    pub deadline: Duration,
    /// How long `initialize` waits for the guide before answering without it, so a backend
    /// that doesn't answer can't hold up the handshake.
    pub guide_timeout: Duration,
    /// The write tools this server offers (to clients granted `files:write`); empty makes it
    /// read-only. Writes also need a state database.
    pub write_tools: Vec<String>,
    /// Largest file `write_file` takes inline; bigger ones go through an upload link.
    pub max_inline_bytes: u64,
}

/// Every write tool, the default for [`ServerConfig::write_tools`].
pub const WRITE_TOOLS: &[&str] = &["create_folder", "move", "copy", "write_file", "delete"];

/// Claude's web origins, allowed by default.
pub const CLAUDE_ORIGINS: &[&str] = &["https://claude.ai", "https://claude.com"];

/// Where the owner's guide is by default: next to the files it describes, editable from any
/// pCloud client.
pub const DEFAULT_GUIDE_PATH: &str = "/AGENTS.md";

/// Most of the guide that goes into the instructions; Claude reads them on every connection.
const MAX_GUIDE_BYTES: usize = 8 * 1024;

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            name: "chance-of-files".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            max_read_bytes: 100_000,
            max_image_bytes: 1536 * 1024,
            search_limit: 50,
            guide_path: Some(DEFAULT_GUIDE_PATH.into()),
            allowed_origins: CLAUDE_ORIGINS.iter().map(|&o| o.to_owned()).collect(),
            deadline: Duration::from_secs(180),
            guide_timeout: Duration::from_millis(1500),
            write_tools: WRITE_TOOLS.iter().map(|&t| t.to_owned()).collect(),
            max_inline_bytes: 5_000_000,
        }
    }
}

/// Protocol revisions this server speaks, newest first.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2026-07-28", "2025-11-25", "2025-06-18", "2025-03-26"];

/// The same list as rmcp values, for `server/discover`.
const SUPPORTED_VERSIONS: &[ProtocolVersion] = &[
    ProtocolVersion::V_2026_07_28,
    ProtocolVersion::V_2025_11_25,
    ProtocolVersion::V_2025_06_18,
    ProtocolVersion::V_2025_03_26,
];

/// Revisions that negotiate with `initialize`. A client asking for anything else (an unknown
/// version, or 2026-07-28, which has no handshake) is offered the newest of these.
const HANDSHAKE_VERSIONS: &[ProtocolVersion] = &[
    ProtocolVersion::V_2025_11_25,
    ProtocolVersion::V_2025_06_18,
    ProtocolVersion::V_2025_03_26,
];

/// The stateless revision: version and capabilities travel in every request's `_meta`.
const STATELESS_VERSION: &str = "2026-07-28";

const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// How long a 2026-07-28 client may cache `tools/list` and `server/discover`. The tools only
/// change with a new deployment.
const LIST_TTL_MS: u64 = 60 * 60 * 1000;

const INSTRUCTIONS: &str = "Access to the owner's pCloud drive. Paths start at its \
root \"/\" (for example \"/Notes/todo.md\"). \
list_folder shows long folders in pages: call again with the cursor it gives. \
Find a path with search_files or list_folder before reading it. \
search_files skips folders the owner turned off for search and says so; browse those with \
list_folder. \
read_text_file returns long files in pages: its first line says which offset to continue from. \
read_image shows PNG, JPEG, GIF and WebP pictures. \
If the owner wrote a guide to their files, it ends these instructions and says what each folder \
is for.";

const INDEX_INSTRUCTIONS: &str = " \
The server keeps an index of the whole drive, refreshed by periodic walks: list_folder, \
search_files and the audit tools answer from it instantly and their first line says how current \
it is. For storage questions (\"where did my space go\", largest files, duplicates, what \
changed) start with folder_stats or query_files rather than listing folder by folder. \
list_folder with live=true and get_file_info ask the drive itself. index_status shows walks \
and problems; refresh_index re-walks after changes made elsewhere.";

const WRITE_INSTRUCTIONS: &str = " \
This connection may change the drive: create_folder, move, copy, write_file and delete (to \
pCloud's trash, restorable there). The owner confirms each call. Folder moves, copies and \
deletes run as jobs: they return a job id, and job_status says when they are done. Folders the \
owner made read-only refuse every change, including moves and copies into or out of them.";

const LINK_INSTRUCTIONS: &str = " \
For files too big for a tool result, create_download_link makes an expiring HTTPS link (15 \
minutes by default) that a computer can fetch with curl, a single file resumable with \
curl -C -, or a folder as one streamed zip; create_upload_link makes one for sending a big file \
in parts. list_links shows each link's use; revoke_link ends one.";

/// Who is calling: what the auth Worker vouched for.
#[derive(Debug, Clone, Default)]
pub struct Caller {
    pub scopes: Scopes,
    /// The Worker's public origin, from its `X-CoF-Public-Url` header.
    pub public_url: Option<String>,
}

/// Which protocol rules a request follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Era {
    /// 2025-11-25 and earlier: results carry no `resultType`.
    Handshake,
    /// 2026-07-28: results carry `resultType` and `serverInfo`; some errors change HTTP status.
    Stateless,
}

/// A JSON-RPC error plus the HTTP status it travels with.
struct RpcError {
    status: u16,
    error: ErrorData,
}

impl RpcError {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            status: 200,
            error: ErrorData::new(code, message.into(), None),
        }
    }

    fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::INVALID_PARAMS, message)
    }

    fn unsupported_version(requested: &str) -> Self {
        Self {
            status: 400,
            error: ErrorData::new(
                ErrorCode::UNSUPPORTED_PROTOCOL_VERSION,
                "Unsupported protocol version",
                Some(json!({
                    "supported": SUPPORTED_PROTOCOL_VERSIONS,
                    "requested": requested,
                })),
            ),
        }
    }

    fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }
}

/// The part of the owner's guide that goes into the instructions.
struct Guide<'a> {
    path: &'a str,
    text: String,
    /// Where the rest starts, when the file is longer than what is shown.
    continue_at: Option<usize>,
}

/// What a message produced: an HTTP status and a JSON-RPC body.
struct Reply {
    status: u16,
    body: Value,
}

impl Reply {
    fn error(id: Option<Value>, err: RpcError) -> Self {
        Self {
            status: err.status,
            body: error_message(id, err.error),
        }
    }

    /// For input that is not a usable JSON-RPC message at all.
    fn invalid(id: Option<Value>, message: &str) -> Self {
        Self::error(
            id,
            RpcError::new(ErrorCode::INVALID_REQUEST, message).with_status(400),
        )
    }
}

pub struct McpServer<S: StorageWrite> {
    storage: S,
    config: ServerConfig,
    policy: Policy,
    index: Option<Arc<dyn Index>>,
    health: Arc<Health>,
    state: Option<Arc<dyn StateStore>>,
    links: Option<LinkSettings>,
}

impl<S: StorageWrite> McpServer<S> {
    /// A server with nothing hidden; see [`McpServer::with_policy`].
    pub fn new(storage: S, config: ServerConfig) -> Self {
        Self {
            storage,
            config,
            policy: Policy::default(),
            index: None,
            health: Arc::new(Health::new()),
            state: None,
            links: None,
        }
    }

    /// Keeps jobs, links and the audit log, and so offers the write tools (as configured)
    /// and `job_status`.
    pub fn with_state(mut self, state: Arc<dyn StateStore>) -> Self {
        self.state = Some(state);
        self
    }

    /// Offers the link tools (with a state database).
    pub fn with_links(mut self, links: LinkSettings) -> Self {
        self.links = Some(links);
        self
    }

    pub fn state(&self) -> Option<&Arc<dyn StateStore>> {
        self.state.as_ref()
    }

    pub fn links(&self) -> Option<&LinkSettings> {
        self.links.as_ref()
    }

    fn parts(&self) -> Parts {
        Parts {
            index: self.index.is_some(),
            state: self.state.is_some(),
            links: self.links.is_some(),
        }
    }

    pub fn with_policy(mut self, policy: Policy) -> Self {
        self.policy = policy;
        self
    }

    /// Answers from the metadata index where it can, and adds the audit tools.
    pub fn with_index(mut self, index: Arc<dyn Index>) -> Self {
        self.index = Some(index);
        self
    }

    /// Shares the backend's health with the checker that updates it.
    pub fn with_health(mut self, health: Arc<Health>) -> Self {
        self.health = health;
        self
    }

    pub fn health(&self) -> &Arc<Health> {
        &self.health
    }

    pub fn index(&self) -> Option<&Arc<dyn Index>> {
        self.index.as_ref()
    }

    pub fn storage(&self) -> &S {
        &self.storage
    }

    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Handles one HTTP request to the MCP endpoint. Authentication, and setting
    /// [`HttpRequest::scopes`] from what it proved, is the caller's job.
    pub async fn handle_http(&self, req: HttpRequest) -> HttpResponse {
        // Checked first: a browser page on a foreign origin gets nothing, not even a 405.
        if let Some(origin) = req.header("origin")
            && !self.origin_allowed(origin)
        {
            return transport_error(403, ErrorCode::INVALID_REQUEST, "Origin not allowed");
        }
        if !req.method.eq_ignore_ascii_case("POST") {
            // Stateless: no GET stream to open and no session to DELETE.
            return HttpResponse::empty(405).with_header("Allow", "POST");
        }
        if !is_json(req.header("content-type")) {
            return transport_error(
                415,
                ErrorCode::INVALID_REQUEST,
                "Content-Type must be application/json",
            );
        }
        let msg: Value = match serde_json::from_slice(&req.body) {
            Ok(msg) => msg,
            Err(_) => return transport_error(400, ErrorCode::PARSE_ERROR, "Parse error"),
        };
        if msg.is_array() {
            // Batches left the protocol in 2025-06-18; one message per POST.
            return transport_error(
                400,
                ErrorCode::INVALID_REQUEST,
                "Batched messages are not supported; send one message per request",
            );
        }

        if let Some(version) = req.header("mcp-protocol-version")
            && !SUPPORTED_PROTOCOL_VERSIONS.contains(&version.trim())
        {
            let err = RpcError::unsupported_version(version.trim());
            return to_http(Reply::error(request_id(&msg), err));
        }
        if let Err(why) = check_mirrored_headers(&req, &msg) {
            let err = RpcError::new(ErrorCode::HEADER_MISMATCH, why).with_status(400);
            return to_http(Reply::error(request_id(&msg), err));
        }

        let caller = Caller {
            scopes: req.scopes,
            public_url: req.public_url.clone(),
        };
        match self.dispatch(msg, &caller).await {
            Some(reply) => to_http(reply),
            None => HttpResponse::empty(202),
        }
    }

    /// Handles one JSON-RPC message from a read-only client; `None` for notifications and
    /// responses.
    pub async fn handle_jsonrpc(&self, msg: serde_json::Value) -> Option<serde_json::Value> {
        self.handle_jsonrpc_with(msg, Scopes::read_only()).await
    }

    /// Handles one JSON-RPC message from a client granted `scopes`.
    pub async fn handle_jsonrpc_with(
        &self,
        msg: serde_json::Value,
        scopes: Scopes,
    ) -> Option<serde_json::Value> {
        let caller = Caller {
            scopes,
            public_url: None,
        };
        self.handle_jsonrpc_as(msg, &caller).await
    }

    /// Handles one JSON-RPC message from `caller`.
    pub async fn handle_jsonrpc_as(
        &self,
        msg: serde_json::Value,
        caller: &Caller,
    ) -> Option<serde_json::Value> {
        self.dispatch(msg, caller).await.map(|reply| reply.body)
    }

    fn origin_allowed(&self, origin: &str) -> bool {
        let origin = origin.trim().trim_end_matches('/');
        self.config.allowed_origins.iter().any(|allowed| {
            allowed
                .trim()
                .trim_end_matches('/')
                .eq_ignore_ascii_case(origin)
        })
    }

    async fn dispatch(&self, msg: Value, caller: &Caller) -> Option<Reply> {
        let Value::Object(msg) = msg else {
            return Some(Reply::invalid(None, "Expected a JSON-RPC message object"));
        };
        let id = msg.get("id").filter(|id| is_valid_id(id)).cloned();
        if msg.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Some(Reply::invalid(id, "jsonrpc must be \"2.0\""));
        }
        match (msg.get("method"), msg.get("id")) {
            (Some(Value::String(method)), Some(_)) => match id {
                Some(id) => Some(
                    self.handle_request(id, method, msg.get("params"), caller)
                        .await,
                ),
                None => Some(Reply::invalid(None, "id must be a string or a number")),
            },
            // A notification. None of them (initialized, cancelled, ...) needs anything from
            // a stateless server.
            (Some(Value::String(_)), None) => None,
            (Some(_), _) => Some(Reply::invalid(id, "method must be a string")),
            // A response to a server request. This server never sends any, so there is
            // nothing to match it with; accept and drop it.
            (None, Some(_)) if msg.contains_key("result") || msg.contains_key("error") => None,
            (None, _) => Some(Reply::invalid(
                id,
                "Not a JSON-RPC request, notification or response",
            )),
        }
    }

    async fn handle_request(
        &self,
        id: Value,
        method: &str,
        params: Option<&Value>,
        caller: &Caller,
    ) -> Reply {
        let scopes = caller.scopes;
        let empty = Map::new();
        let params = match params {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(params)) => params,
            Some(_) => {
                return Reply::error(
                    Some(id),
                    RpcError::invalid_params("params must be an object"),
                );
            }
        };
        let era = match request_era(params) {
            Ok(era) => era,
            Err(err) => return Reply::error(Some(id), err),
        };

        let result = match method {
            "initialize" => self.initialize(params, scopes).await,
            "ping" => Ok(json!({})),
            "server/discover" => Ok(to_json(&self.discover(scopes).await)),
            "tools/list" => Ok(self.list_tools(era, scopes)),
            "tools/call" => self.call_tool(params, caller).await,
            _ => {
                let err = RpcError::new(
                    ErrorCode::METHOD_NOT_FOUND,
                    format!("Method not found: {method}"),
                );
                // 2026-07-28 tells an unknown method apart from a missing endpoint by status.
                Err(match era {
                    Era::Stateless => err.with_status(404),
                    Era::Handshake => err,
                })
            }
        };
        match result {
            Ok(result) => Reply {
                status: 200,
                body: json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": self.finish(result, era),
                }),
            },
            Err(err) => Reply::error(Some(id), err),
        }
    }

    async fn initialize(
        &self,
        params: &Map<String, Value>,
        scopes: Scopes,
    ) -> Result<Value, RpcError> {
        let requested = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("initialize needs params.protocolVersion"))?;
        // Echo the client's version when we speak it; otherwise offer our newest and let the
        // client decide whether it can live with that.
        let version = HANDSHAKE_VERSIONS
            .iter()
            .find(|v| v.as_str() == requested)
            .unwrap_or(&HANDSHAKE_VERSIONS[0])
            .clone();
        let result = InitializeResult::new(self.capabilities())
            .with_protocol_version(version)
            .with_server_info(self.implementation())
            .with_instructions(self.instructions(scopes).await);
        Ok(to_json(&result))
    }

    async fn discover(&self, scopes: Scopes) -> DiscoverResult {
        let mut result = DiscoverResult::new(SUPPORTED_VERSIONS.to_vec(), self.capabilities())
            .with_ttl_ms(LIST_TTL_MS)
            .with_cache_scope(CacheScope::Private)
            .with_server_info(self.implementation());
        result.instructions = Some(self.instructions(scopes).await);
        result
    }

    /// The fixed instructions, what this connection may change, the folders search skips and
    /// the read-only folders, and the owner's guide if there is one.
    async fn instructions(&self, scopes: Scopes) -> String {
        let mut text = String::from(INSTRUCTIONS);
        if self.index.is_some() {
            text.push_str(INDEX_INSTRUCTIONS);
        }
        let parts = self.parts();
        if tools::can_write(&self.config, scopes, parts) {
            text.push_str(WRITE_INSTRUCTIONS);
            let protected = self.policy.protected().paths();
            if !protected.is_empty() {
                text.push_str(" Read-only folders: ");
                text.push_str(&protected.join(", "));
                text.push('.');
            }
        } else {
            text.push_str(" This connection can't change anything on the drive.");
        }
        if parts.state && parts.links {
            text.push_str(LINK_INSTRUCTIONS);
        }
        let skipped = self.policy.search_skip().paths();
        if !skipped.is_empty() {
            text.push_str(" Search is turned off under ");
            text.push_str(&skipped.join(", "));
            text.push('.');
        }
        if let Some(guide) = self.guide().await {
            text.push_str(&format!(
                "\n\nThe owner's guide to these files ({}):\n\n{}",
                guide.path, guide.text
            ));
            if let Some(next) = guide.continue_at {
                text.push_str(&format!(
                    "\n\n[The guide is longer than this; read the rest with read_text_file at {} \
                     with offset={next}.]",
                    guide.path
                ));
            }
        }
        text
    }

    /// The start of the owner's guide, or `None` when there is none to show. Any failure, or
    /// the guide timeout passing, means none: the handshake has to work while the backend is
    /// down.
    async fn guide(&self) -> Option<Guide<'_>> {
        let path = self.config.guide_path.as_deref()?;
        let checked = self.policy.check(path).ok()?;
        // One byte more than shown, to tell a cut guide from one that fits exactly.
        let length = MAX_GUIDE_BYTES as u64 + 1;
        let read = async {
            let stream = self
                .storage
                .open(&checked, Some(ByteRange { offset: 0, length }))
                .await?;
            read_capped(stream, length).await
        };
        let bytes = tokio::time::timeout(self.config.guide_timeout, read)
            .await
            .ok()?
            .ok()?;
        let cut = bytes.len() > MAX_GUIDE_BYTES;
        let bytes = &bytes[..bytes.len().min(MAX_GUIDE_BYTES)];
        if bytes.contains(&0) {
            return None;
        }
        let text = match std::str::from_utf8(bytes) {
            Ok(text) => text,
            // Only a character cut off by the limit is forgiven; anything else isn't UTF-8.
            Err(e) if cut && e.error_len().is_none() => {
                std::str::from_utf8(&bytes[..e.valid_up_to()]).ok()?
            }
            Err(_) => return None,
        };
        let shown = text.len();
        let text = text.trim_start_matches('\u{feff}').trim();
        if text.is_empty() {
            return None;
        }
        Some(Guide {
            path,
            text: text.to_owned(),
            continue_at: cut.then_some(shown),
        })
    }

    fn list_tools(&self, era: Era, scopes: Scopes) -> Value {
        let mut result = ListToolsResult::with_all_items(tools::definitions(
            &self.config,
            scopes,
            self.parts(),
            self.links.as_ref(),
        ));
        if era == Era::Stateless {
            result = result
                .with_ttl_ms(LIST_TTL_MS)
                .with_cache_scope(CacheScope::Private);
        }
        to_json(&result)
    }

    async fn call_tool(
        &self,
        params: &Map<String, Value>,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("tools/call needs params.name"))?;
        let empty = Map::new();
        let args = match params.get("arguments") {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(args)) => args,
            Some(_) => {
                return Err(RpcError::invalid_params(
                    "params.arguments must be an object",
                ));
            }
        };
        let mut ctx = tools::Ctx::new(
            &self.storage,
            &self.config,
            &self.policy,
            self.index.as_ref(),
            &self.health,
        );
        ctx.state = self.state.as_ref();
        ctx.links = self.links.as_ref();
        ctx.public_url = caller.public_url.as_deref();
        match tools::call(&ctx, name, args, caller.scopes).await {
            Called::Done(result) => Ok(to_json(&result)),
            Called::Unknown => Err(RpcError::invalid_params(format!("Unknown tool: {name}"))),
        }
    }

    fn capabilities(&self) -> ServerCapabilities {
        let mut tools = ToolsCapability::default();
        // The tool set is fixed for the life of a deployment.
        tools.list_changed = Some(false);
        let mut capabilities = ServerCapabilities::default();
        capabilities.tools = Some(tools);
        capabilities
    }

    fn implementation(&self) -> Implementation {
        Implementation::new(&self.config.name, &self.config.version)
    }

    /// Shapes a result for the request's era. rmcp's result types always carry `resultType`;
    /// older clients never heard of it, so it comes off for them.
    fn finish(&self, result: Value, era: Era) -> Value {
        let Value::Object(mut result) = result else {
            return result;
        };
        match era {
            Era::Handshake => {
                result.remove("resultType");
            }
            Era::Stateless => {
                result
                    .entry("resultType")
                    .or_insert_with(|| json!("complete"));
                if let Value::Object(meta) = result.entry("_meta").or_insert_with(|| json!({})) {
                    meta.insert(META_SERVER_INFO.into(), to_json(&self.implementation()));
                }
            }
        }
        Value::Object(result)
    }
}

/// Reads the per-request protocol fields. A request without them is an older client's.
fn request_era(params: &Map<String, Value>) -> Result<Era, RpcError> {
    let Some(meta) = params.get("_meta").and_then(Value::as_object) else {
        return Ok(Era::Handshake);
    };
    let Some(version) = meta.get(META_PROTOCOL_VERSION) else {
        return Ok(Era::Handshake);
    };
    let Some(version) = version.as_str() else {
        return Err(RpcError::invalid_params(format!(
            "_meta {META_PROTOCOL_VERSION} must be a string"
        ))
        .with_status(400));
    };
    if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
        return Err(RpcError::unsupported_version(version));
    }
    if version != STATELESS_VERSION {
        return Ok(Era::Handshake);
    }
    if !meta
        .get(META_CLIENT_CAPABILITIES)
        .is_some_and(Value::is_object)
    {
        return Err(RpcError::invalid_params(format!(
            "_meta {META_CLIENT_CAPABILITIES} is required"
        ))
        .with_status(400));
    }
    Ok(Era::Stateless)
}

/// 2026-07-28 copies the version, method and tool name into headers so proxies can route
/// without reading the body, and requires servers to reject a request whose headers and body
/// disagree (otherwise a proxy and the server could act on different things). Older clients
/// don't send these headers, so only stateless requests are checked.
fn check_mirrored_headers(req: &HttpRequest, msg: &Value) -> Result<(), String> {
    let (Some(method), Some(_)) = (msg.get("method").and_then(Value::as_str), msg.get("id")) else {
        // Notifications and responses: the revision defines no header rules for them.
        return Ok(());
    };
    let params = msg.get("params");
    let body_version = params
        .and_then(|p| p.get("_meta"))
        .and_then(|m| m.get(META_PROTOCOL_VERSION))
        .and_then(Value::as_str);
    let header_version = req.header("mcp-protocol-version").map(str::trim);
    if body_version != Some(STATELESS_VERSION) && header_version != Some(STATELESS_VERSION) {
        return Ok(());
    }

    match header_version {
        None => return Err("Missing MCP-Protocol-Version header".into()),
        Some(header) if Some(header) != body_version => {
            return Err(format!(
                "MCP-Protocol-Version header {header} does not match _meta {META_PROTOCOL_VERSION} {}",
                body_version.unwrap_or("(missing)")
            ));
        }
        Some(_) => {}
    }
    match req.header("mcp-method") {
        None => return Err("Missing Mcp-Method header".into()),
        Some(header) if header != method => {
            return Err(format!(
                "Mcp-Method header {header} does not match method {method}"
            ));
        }
        Some(_) => {}
    }
    if method == "tools/call" {
        let name = params.and_then(|p| p.get("name")).and_then(Value::as_str);
        let header = match req.header("mcp-name") {
            None => return Err("Missing Mcp-Name header".into()),
            Some(raw) => decode_header_value(raw)
                .ok_or_else(|| format!("Mcp-Name header {raw} is not valid"))?,
        };
        if Some(header.as_str()) != name {
            return Err(format!(
                "Mcp-Name header {header} does not match params.name {}",
                name.unwrap_or("(missing)")
            ));
        }
    }
    Ok(())
}

/// Undoes the `=?base64?...?=` wrapping clients use for header values that aren't plain ASCII.
fn decode_header_value(raw: &str) -> Option<String> {
    match raw
        .strip_prefix("=?base64?")
        .and_then(|rest| rest.strip_suffix("?="))
    {
        Some(encoded) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()?;
            String::from_utf8(bytes).ok()
        }
        None => Some(raw.to_owned()),
    }
}

/// `application/json`, with or without parameters such as `charset`.
fn is_json(content_type: Option<&str>) -> bool {
    content_type
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
}

/// MCP request ids are strings or numbers, never null.
fn is_valid_id(id: &Value) -> bool {
    id.is_string() || id.is_number()
}

/// The id of a request, when `msg` is one, so an early error can still name it.
fn request_id(msg: &Value) -> Option<Value> {
    msg.get("method")?;
    msg.get("id").filter(|id| is_valid_id(id)).cloned()
}

/// A JSON-RPC error response. With no id, the field is left out: MCP ids can't be null.
fn error_message(id: Option<Value>, error: ErrorData) -> Value {
    let mut msg = Map::new();
    msg.insert("jsonrpc".into(), json!("2.0"));
    if let Some(id) = id {
        msg.insert("id".into(), id);
    }
    msg.insert("error".into(), to_json(&error));
    Value::Object(msg)
}

/// An error found before any message could be read; the body has no id.
fn transport_error(status: u16, code: ErrorCode, message: &str) -> HttpResponse {
    HttpResponse::json(
        status,
        &error_message(None, ErrorData::new(code, message.to_owned(), None)),
    )
}

fn to_http(reply: Reply) -> HttpResponse {
    HttpResponse::json(reply.status, &reply.body)
}

fn to_json<T: serde::Serialize>(value: &T) -> Value {
    // rmcp's model types are plain structs with string keys; serializing them can't fail.
    serde_json::to_value(value).expect("MCP model types serialize to JSON")
}
