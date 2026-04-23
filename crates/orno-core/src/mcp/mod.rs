//! MCP client seam — ADR 0007.
//!
//! Callers speak to [`McpClient`], never to `rmcp` directly. The concrete
//! implementation ([`RmcpClient`]) wraps `rmcp` and maps its errors into
//! [`McpError`]. No `rmcp` types reach orno-core's public surface.
//!
//! The trait surface is intentionally minimal (ADR 0007 §Decision):
//! `initialize` collapses spawn + handshake + `tools/list`; `call_tool`
//! handles invocation; `shutdown` terminates cleanly. Lifecycle events
//! are emitted by the run orchestration in `orno-cli/src/commands/run.rs`,
//! NOT inside the client — the client's job is protocol, not observability.

pub mod rmcp_client;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use rmcp_client::RmcpClient;

use crate::error::McpError;

/// A tool advertised by an MCP server's `tools/list` response.
///
/// Orno-owned DTO — `rmcp::model::Tool` stays behind the trait boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    /// Tool name as advertised by the server (used verbatim in `tools/call`).
    pub name: String,
    /// Human-readable description forwarded to the LLM in tool schemas.
    pub description: String,
    /// JSON Schema for the tool's arguments, as returned by `tools/list`.
    pub schema: Value,
}

/// Result of a `tools/call` invocation.
///
/// `ok: false` carries a protocol-level error string (tool returned an error
/// payload); transport-level failures surface as [`McpError`] from the call
/// itself. This mirrors [`crate::events::Event::NodeFinished`]'s `ok` field —
/// protocol errors are data, transport errors are failures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolCallResult {
    /// `false` when the server returned `isError: true`.
    pub ok: bool,
    /// Tool content serialized as a string. Multiple text blocks from the
    /// server are concatenated with a newline separator.
    pub content: String,
}

/// Protocol client seam. Dispatched as `Arc<dyn McpClient>` for tool
/// invocations (`call_tool` takes `&self`) and as `Box<dyn McpClient>` for
/// lifecycle management (`initialize`/`shutdown` take `&mut self`).
///
/// Exactly one concrete implementation exists in v0.1: [`RmcpClient`], which
/// wraps the `rmcp` crate. Swapping to a hand-rolled client requires touching
/// only `rmcp_client.rs` (ADR 0007).
#[async_trait]
pub trait McpClient: Send + Sync + std::fmt::Debug {
    /// Server identifier this client was constructed against. Used by
    /// lifecycle event emission so the caller does not carry two aligned
    /// strings.
    fn server_name(&self) -> &str;

    /// Spawn the server (stdio: `Command`; http: establish connection),
    /// perform the MCP `initialize` handshake, call `tools/list`, and return
    /// the cached tool list. Called exactly once per run before any
    /// `call_tool` dispatch.
    async fn initialize(&mut self) -> Result<Vec<McpTool>, McpError>;

    /// Invoke `tools/call` for the named tool. `args` is a JSON object
    /// matching the tool's advertised schema. Transport failures surface as
    /// [`McpError`]; protocol-level tool errors (server returned
    /// `isError: true`) surface as [`McpToolCallResult`]`{ ok: false, .. }`.
    async fn call_tool(&self, tool: &str, args: Value) -> Result<McpToolCallResult, McpError>;

    /// Initiate clean shutdown. For stdio: send `notifications/exit`, then
    /// wait up to a bounded timeout for the child to exit, SIGTERM on
    /// timeout. For http: close the connection. Idempotent — safe to call
    /// twice on a clean run.
    async fn shutdown(&mut self) -> Result<(), McpError>;
}
