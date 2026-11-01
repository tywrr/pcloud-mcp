//! MCP server logic for Chance of Files: the Streamable HTTP endpoint, JSON-RPC dispatch, the
//! file tools, the [`Storage`] traits and the path [`Policy`]. No I/O of its own: callers
//! supply a [`Storage`] and turn their HTTP types into [`HttpRequest`] / [`HttpResponse`].
//! Storage calls run under tokio timers, so callers run it on a tokio runtime.

mod audit;
pub mod health;
pub mod http;
pub mod index;
pub mod kind;
pub mod links;
mod linktools;
mod matching;
#[cfg(feature = "testing")]
pub mod memory;
pub mod policy;
pub mod scope;
pub mod server;
pub mod state;
pub mod storage;
pub mod time;
mod tools;
mod walk;
pub mod writes;
pub mod zip;

pub use health::{Health, Unhealthy};
pub use http::{HttpRequest, HttpResponse};
pub use index::{
    Change, Filter, HeldRemoval, Index, IndexError, IndexResult, IndexStatus, IndexedEntry,
    RootStatus, SizeGroup, SizeGroups, Sort, Totals, TypeRow, What,
};
pub use links::{Bind, LinkKey, LinkSettings, RangeAnswer, Token, TokenError, parse_range};
pub use linktools::zip_items;
pub use matching::{NameMatcher, fold, name_matches};
pub use policy::{CheckedPath, PathSet, Policy};
pub use scope::{SCOPES_HEADER, Scope, Scopes};
pub use server::{
    CLAUDE_ORIGINS, Caller, DEFAULT_GUIDE_PATH, McpServer, ServerConfig, WRITE_TOOLS,
};
pub use state::{
    JobRow, JobStatus, LinkKind, LinkRow, LinkState, LinkUse, StateError, StateResult, StateStore,
};
pub use storage::{
    ByteRange, ByteStream, Capabilities, Done, Entry, HashKind, JobId, JobInfo, JobState, Storage,
    StorageError, StorageWrite, normalize_path, read_capped, stream_of,
};
pub use walk::{split_walk, split_walk_each};
