//! MCP server logic for Chance of Files: the Streamable HTTP endpoint, JSON-RPC dispatch, the
//! file tools, the [`Storage`] traits and the path [`Policy`]. No I/O of its own: callers
//! supply a [`Storage`] and turn their HTTP types into [`HttpRequest`] / [`HttpResponse`].
//! Storage calls run under tokio timers, so callers run it on a tokio runtime.

pub mod http;
mod matching;
pub mod policy;
pub mod scope;
pub mod server;
pub mod storage;
mod tools;
mod walk;

pub use http::{HttpRequest, HttpResponse};
pub use matching::{NameMatcher, name_matches};
pub use policy::{CheckedPath, PathSet, Policy};
pub use scope::{SCOPES_HEADER, Scope, Scopes};
pub use server::{CLAUDE_ORIGINS, DEFAULT_GUIDE_PATH, McpServer, ServerConfig};
pub use storage::{
    ByteRange, ByteStream, Capabilities, Entry, HashKind, Storage, StorageError, normalize_path,
    read_capped, stream_of,
};
pub use walk::split_walk;
