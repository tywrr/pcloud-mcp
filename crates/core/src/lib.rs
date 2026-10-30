//! MCP server logic for pcloud-mcp: the Streamable HTTP endpoint, JSON-RPC dispatch and the
//! file tools. It knows nothing about Workers or WebDAV; callers supply a [`Storage`] and turn
//! their runtime's HTTP types into [`HttpRequest`] / [`HttpResponse`].

pub mod http;
pub mod server;
pub mod storage;
mod tools;

pub use http::{HttpRequest, HttpResponse};
pub use server::{CLAUDE_ORIGINS, McpServer, ServerConfig};
pub use storage::{ByteRange, Entry, Storage, StorageError, normalize_path};
