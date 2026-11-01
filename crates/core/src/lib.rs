//! MCP server logic for Chance of Files: the Streamable HTTP endpoint, JSON-RPC dispatch, the
//! file tools, the [`Storage`] traits and the path [`Policy`]. No I/O of its own: callers
//! supply a [`Storage`] and turn their HTTP types into [`HttpRequest`] / [`HttpResponse`].
//! Storage calls run under tokio timers, so callers run it on a tokio runtime.

mod audit;
pub mod health;
pub mod http;
pub mod index;
pub mod kind;
mod matching;
pub mod policy;
pub mod scope;
pub mod server;
pub mod storage;
pub mod time;
mod tools;
mod walk;

pub use health::{Health, Unhealthy};
pub use http::{HttpRequest, HttpResponse};
pub use index::{
    Change, Filter, HeldRemoval, Index, IndexError, IndexResult, IndexStatus, IndexedEntry,
    RootStatus, SizeGroup, SizeGroups, Sort, Totals, TypeRow, What,
};
pub use matching::{NameMatcher, fold, name_matches};
pub use policy::{CheckedPath, PathSet, Policy};
pub use scope::{SCOPES_HEADER, Scope, Scopes};
pub use server::{CLAUDE_ORIGINS, DEFAULT_GUIDE_PATH, McpServer, ServerConfig};
pub use storage::{
    ByteRange, ByteStream, Capabilities, Entry, HashKind, Storage, StorageError, normalize_path,
    read_capped, stream_of,
};
pub use walk::{split_walk, split_walk_each};
