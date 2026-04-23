//! Concrete `McpClient` implementation backed by the `rmcp` crate (ADR 0007).
//!
//! No `rmcp::*` types appear in `pub` items — all rmcp interaction is hidden
//! behind `McpClient`. Errors from rmcp are mapped into [`McpError`] variants
//! at this boundary so callers never depend on rmcp's error hierarchy.
//!
//! # Lifecycle
//! 1. Construct with [`new_stdio`][RmcpClient::new_stdio] or
//!    [`new_http`][RmcpClient::new_http].
//! 2. Call [`initialize`][super::McpClient::initialize] once to spawn/connect
//!    and cache tools.
//! 3. Call [`call_tool`][super::McpClient::call_tool] any number of times.
//! 4. Call [`shutdown`][super::McpClient::shutdown] at run end.

use std::collections::BTreeMap;

use async_trait::async_trait;
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParam;
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::transport::child_process::TokioChildProcess;
use serde_json::Value;
use tracing::instrument;

use crate::error::McpError;
use crate::pipeline::schema::{McpAuthConfig, McpHttpConfig, McpStdioConfig};

use super::{McpClient, McpTool, McpToolCallResult};

// ──────────────────────────────────────────────────────────────────────────────
// Config snapshots (private — rmcp types never leak from here)
// ──────────────────────────────────────────────────────────────────────────────

struct StdioConfig {
    command: Vec<String>,
    env: BTreeMap<String, String>,
}

struct HttpConfig {
    url: String,
    auth: Option<McpAuthConfig>,
    headers: BTreeMap<String, String>,
}

// ──────────────────────────────────────────────────────────────────────────────
// State machine
// ──────────────────────────────────────────────────────────────────────────────

enum State {
    Pending(Config),
    Connected {
        /// Cloned from `RunningService` at initialize time; `call_tool` uses it.
        peer: Peer<RoleClient>,
        /// Held so `shutdown` can call `cancel()`. Taken on first shutdown.
        service: Option<RunningService<RoleClient, ()>>,
    },
    Disconnected,
}

enum Config {
    Stdio(StdioConfig),
    Http(HttpConfig),
}

// ──────────────────────────────────────────────────────────────────────────────
// Public struct
// ──────────────────────────────────────────────────────────────────────────────

/// MCP client backed by `rmcp`. The concrete transport (stdio or HTTP) is
/// selected at construction time; connecting is deferred to
/// [`initialize`][McpClient::initialize] so callers can construct the client
/// before the engine starts and then open connections as a batch.
pub struct RmcpClient {
    server: String,
    state: State,
}

impl std::fmt::Debug for RmcpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (transport_label, connected) = match &self.state {
            State::Pending(Config::Stdio(_)) => ("stdio", false),
            State::Pending(Config::Http(_)) => ("http", false),
            State::Connected { .. } => ("connected", true),
            State::Disconnected => ("disconnected", false),
        };
        f.debug_struct("RmcpClient")
            .field("server", &self.server)
            .field("transport", &transport_label)
            .field("connected", &connected)
            .finish()
    }
}

impl RmcpClient {
    /// Construct for a stdio-transport server. Spawning is deferred to `initialize`.
    pub fn new_stdio(server: String, cfg: &McpStdioConfig) -> Self {
        Self {
            server,
            state: State::Pending(Config::Stdio(StdioConfig {
                command: cfg.command.clone(),
                env: cfg.env.clone(),
            })),
        }
    }

    /// Construct for an HTTP-transport server. Connection is deferred to
    /// `initialize`. Returns `McpError::UnsupportedTransport` at initialize
    /// time until the HTTP transport feature is wired (rmcp feature-gating
    /// issues prevent HTTP in v0.1).
    pub fn new_http(server: String, cfg: &McpHttpConfig) -> Self {
        Self {
            server,
            state: State::Pending(Config::Http(HttpConfig {
                url: cfg.url.clone(),
                auth: cfg.auth.clone(),
                headers: cfg.headers.clone(),
            })),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// McpClient impl
// ──────────────────────────────────────────────────────────────────────────────

#[async_trait]
impl McpClient for RmcpClient {
    fn server_name(&self) -> &str {
        &self.server
    }

    #[instrument(skip(self), fields(mcp.server = %self.server))]
    async fn initialize(&mut self) -> Result<Vec<McpTool>, McpError> {
        let config = match &self.state {
            State::Pending(cfg) => cfg,
            State::Connected { .. } => {
                tracing::warn!("initialize called on already-connected client");
                return Ok(vec![]);
            },
            State::Disconnected => {
                return Err(McpError::SpawnFailed {
                    server: self.server.clone(),
                    source: Box::new(std::io::Error::other(
                        "cannot re-initialize a disconnected client",
                    )),
                });
            },
        };

        let service = match config {
            Config::Stdio(stdio) => spawn_stdio_client(&self.server, stdio).await?,
            Config::Http(http) => {
                // auth/headers stored for when HTTP transport is wired; url
                // surfaced in the error so operators know which server failed.
                tracing::debug!(
                    url = %http.url,
                    auth = http.auth.is_some(),
                    extra_headers = http.headers.len(),
                    "http mcp transport not yet supported"
                );
                return Err(McpError::UnsupportedTransport {
                    transport: format!(
                        "http (url={} — HTTP transport deferred past v0.1 due to rmcp feature-gating issues)",
                        http.url
                    ),
                });
            },
        };

        let peer = service.peer().clone();
        let tools = peer
            .list_all_tools()
            .await
            .map_err(|e| McpError::HandshakeFailed {
                server: self.server.clone(),
                source: Box::new(e),
            })?;

        let mcp_tools: Vec<McpTool> = tools
            .into_iter()
            .map(|t| McpTool {
                name: t.name.to_string(),
                description: t.description.as_deref().unwrap_or("").to_string(),
                schema: Value::Object((*t.input_schema).clone()),
            })
            .collect();

        tracing::debug!(tool_count = mcp_tools.len(), "mcp server initialized");
        self.state = State::Connected {
            peer,
            service: Some(service),
        };
        Ok(mcp_tools)
    }

    #[instrument(skip(self, args), fields(mcp.server = %self.server, mcp.tool = %tool))]
    async fn call_tool(&self, tool: &str, args: Value) -> Result<McpToolCallResult, McpError> {
        let State::Connected { peer, .. } = &self.state else {
            return Err(McpError::CallFailed {
                server: self.server.clone(),
                tool: tool.to_string(),
                source: Box::new(std::io::Error::other(
                    "client is not connected — call initialize() first",
                )),
            });
        };

        let param = CallToolRequestParam {
            name: tool.to_owned().into(),
            arguments: args.as_object().cloned(),
        };

        let result = peer
            .call_tool(param)
            .await
            .map_err(|e| McpError::CallFailed {
                server: self.server.clone(),
                tool: tool.to_string(),
                source: Box::new(e),
            })?;

        let ok = !result.is_error.unwrap_or(false);
        let content = result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n");

        Ok(McpToolCallResult { ok, content })
    }

    #[instrument(skip(self), fields(mcp.server = %self.server))]
    async fn shutdown(&mut self) -> Result<(), McpError> {
        let service = match &mut self.state {
            State::Connected { service, .. } => service.take(),
            State::Pending(_) | State::Disconnected => {
                self.state = State::Disconnected;
                return Ok(());
            },
        };
        self.state = State::Disconnected;

        if let Some(svc) = service {
            // cancel() consumes the service; a JoinError means the task
            // panicked. Treat as a crash but don't propagate — shutdown
            // is best-effort per ADR 0007.
            if let Err(e) = svc.cancel().await {
                tracing::warn!(server = %self.server, error = ?e, "mcp shutdown task panicked");
            }
        }
        Ok(())
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Transport helpers (rmcp-specific, stays behind the trait boundary)
// ──────────────────────────────────────────────────────────────────────────────

async fn spawn_stdio_client(
    server_name: &str,
    cfg: &StdioConfig,
) -> Result<RunningService<RoleClient, ()>, McpError> {
    if cfg.command.is_empty() {
        return Err(McpError::SpawnFailed {
            server: server_name.to_string(),
            source: Box::new(std::io::Error::other("command vector is empty")),
        });
    }

    let mut cmd = tokio::process::Command::new(&cfg.command[0]);
    if cfg.command.len() > 1 {
        cmd.args(&cfg.command[1..]);
    }
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }

    let transport = TokioChildProcess::new(cmd).map_err(|e| McpError::SpawnFailed {
        server: server_name.to_string(),
        source: Box::new(e),
    })?;

    let service =
        ().serve(transport)
            .await
            .map_err(|e| McpError::HandshakeFailed {
                server: server_name.to_string(),
                source: Box::new(e),
            })?;

    Ok(service)
}
