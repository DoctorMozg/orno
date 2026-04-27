//! Concrete `McpClient` implementation backed by the `rmcp` crate.
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

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, Transport};
use serde_json::Value;
use tracing::instrument;

use crate::error::McpError;
use crate::pipeline::schema::{McpAuthConfig, McpHttpConfig, McpStdioConfig};

use super::{McpClient, McpTool, McpToolCallResult};

/// Maximum time a single MCP handshake step (transport `serve`,
/// `list_all_tools`, shutdown `cancel`) may run before the client
/// surfaces a timeout. A misbehaving stdio child or a slow HTTP
/// endpoint must not be able to hang the whole pipeline indefinitely.
const MCP_HANDSHAKE_TIMEOUT_SECS: u64 = 30;
/// Maximum time a single `tools/call` may run before the client
/// surfaces a timeout. Mirrors [`MCP_HANDSHAKE_TIMEOUT_SECS`]; a
/// per-tool override is intentionally out of scope for v0.1.
const MCP_CALL_TIMEOUT_SECS: u64 = 30;

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
    /// `initialize`. Uses rmcp's streamable-HTTP client (the current MCP
    /// transport — SSE-only is gone). Bearer auth and custom headers are
    /// applied via the rmcp transport config; `Basic` auth still returns
    /// `McpError::UnsupportedTransport` at initialize time (deferred).
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
            Config::Http(http) => connect_http_client(&self.server, http).await?,
        };

        let peer = service.peer().clone();
        let tools = tokio::time::timeout(
            Duration::from_secs(MCP_HANDSHAKE_TIMEOUT_SECS),
            peer.list_all_tools(),
        )
        .await
        .map_err(|_| McpError::HandshakeFailed {
            server: self.server.clone(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("list_tools timed out after {MCP_HANDSHAKE_TIMEOUT_SECS}s"),
            )),
        })?
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

        let mut param = CallToolRequestParams::new(tool.to_owned());
        if let Some(obj) = args.as_object() {
            param = param.with_arguments(obj.clone());
        }

        let result = tokio::time::timeout(
            Duration::from_secs(MCP_CALL_TIMEOUT_SECS),
            peer.call_tool(param),
        )
        .await
        .map_err(|_| McpError::CallFailed {
            server: self.server.clone(),
            tool: tool.to_string(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("call_tool timed out after {MCP_CALL_TIMEOUT_SECS}s"),
            )),
        })?
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
            // is best-effort. The bounded timeout protects against a
            // wedged child task that never returns from `cancel()`.
            match tokio::time::timeout(
                Duration::from_secs(MCP_HANDSHAKE_TIMEOUT_SECS),
                svc.cancel(),
            )
            .await
            {
                Ok(Err(e)) => {
                    tracing::warn!(server = %self.server, error = ?e, "mcp shutdown task panicked");
                },
                Err(_) => {
                    // On timeout, the cancel() future is dropped, which
                    // drops the rmcp `RunningService` and its underlying
                    // `TokioChildProcess`. rmcp's `ChildWithCleanup::drop`
                    // spawns a fire-and-forget kill task — the child
                    // *should* die as a result, but we cannot await that
                    // kill from here because rmcp does not expose the
                    // child handle. Escalate to `error!` so the orphan
                    // is loud in operator logs even though the process
                    // table will reflect the kill on a sub-second delay.
                    tracing::error!(
                        server = %self.server,
                        timeout_secs = MCP_HANDSHAKE_TIMEOUT_SECS,
                        "MCP shutdown timed out; relying on rmcp's drop-time kill — \
                         child process may briefly outlive this call",
                    );
                },
                Ok(Ok(_quit_reason)) => {},
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
    // Hard-isolate the child env from the parent's: the parent process
    // may carry secrets, build-artifact paths, or test harness state that
    // an MCP server has no business reading. Strip everything, then
    // re-introduce only the keys the operator declared in
    // `mcp_servers.<name>.env`. `PATH` is the one mandatory exception —
    // without it the kernel cannot resolve a bare command name like
    // `npx` or `uvx`, which is how every reference MCP server ships.
    cmd.env_clear();
    if let Ok(path_val) = std::env::var("PATH") {
        cmd.env("PATH", path_val);
    }
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }

    let transport = TokioChildProcess::new(cmd).map_err(|e| McpError::SpawnFailed {
        server: server_name.to_string(),
        source: Box::new(e),
    })?;

    let service = tokio::time::timeout(
        Duration::from_secs(MCP_HANDSHAKE_TIMEOUT_SECS),
        ().serve(transport),
    )
    .await
    .map_err(|_| McpError::HandshakeFailed {
        server: server_name.to_string(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("stdio handshake timed out after {MCP_HANDSHAKE_TIMEOUT_SECS}s"),
        )),
    })?
    .map_err(|e| McpError::HandshakeFailed {
        server: server_name.to_string(),
        source: Box::new(e),
    })?;

    Ok(service)
}

/// Open a streamable-HTTP MCP session against `cfg.url` and complete the
/// MCP handshake. Bearer tokens are passed as the rmcp transport's
/// `auth_header` (rmcp prepends `Bearer ` itself); user-supplied headers
/// in `cfg.headers` are forwarded as `custom_headers` on every request.
///
/// `Basic` auth is intentionally not wired in v0.1: the schema accepts
/// it for forward compatibility, but the streamable-HTTP transport only
/// exposes a single `auth_header` slot and basic-auth interop with MCP
/// servers is not part of the v0.1 demo scope. Returns
/// `McpError::UnsupportedTransport` so operators get a loud, scoped
/// failure rather than silent omission of credentials.
async fn connect_http_client(
    server_name: &str,
    cfg: &HttpConfig,
) -> Result<RunningService<RoleClient, ()>, McpError> {
    let mut transport_cfg = StreamableHttpClientTransportConfig::with_uri(cfg.url.as_str());

    match &cfg.auth {
        Some(McpAuthConfig::Bearer { token }) => {
            transport_cfg = transport_cfg.auth_header(token.clone());
        },
        Some(McpAuthConfig::Basic { .. }) => {
            return Err(McpError::UnsupportedTransport {
                transport: format!(
                    "http (url={} — `auth: basic` not supported on the streamable-HTTP transport in v0.1; use bearer or a custom Authorization header)",
                    cfg.url
                ),
            });
        },
        Some(McpAuthConfig::None) | None => {},
    }

    if !cfg.headers.is_empty() {
        let custom = build_custom_headers(server_name, &cfg.headers)?;
        transport_cfg = transport_cfg.custom_headers(custom);
    }

    let transport = build_http_transport(transport_cfg);

    tokio::time::timeout(
        Duration::from_secs(MCP_HANDSHAKE_TIMEOUT_SECS),
        ().serve(transport),
    )
    .await
    .map_err(|_| McpError::HandshakeFailed {
        server: server_name.to_string(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("http handshake timed out after {MCP_HANDSHAKE_TIMEOUT_SECS}s"),
        )),
    })?
    .map_err(|e| McpError::HandshakeFailed {
        server: server_name.to_string(),
        source: Box::new(e),
    })
}

/// Convert the schema's `BTreeMap<String, String>` headers into rmcp's
/// `HashMap<HeaderName, HeaderValue>`. Either an invalid header name or
/// a non-ASCII / control-character value surfaces as a single
/// `HandshakeFailed` so the caller sees the same failure shape as a
/// real connection-time rejection — handlers don't need a separate
/// "bad config" branch.
fn build_custom_headers(
    server_name: &str,
    headers: &BTreeMap<String, String>,
) -> Result<HashMap<HeaderName, HeaderValue>, McpError> {
    let mut out = HashMap::with_capacity(headers.len());
    for (k, v) in headers {
        let name = HeaderName::try_from(k.as_str()).map_err(|e| McpError::HandshakeFailed {
            server: server_name.to_string(),
            source: Box::new(std::io::Error::other(format!(
                "invalid http header name `{k}`: {e}"
            ))),
        })?;
        let value = HeaderValue::try_from(v.as_str()).map_err(|e| McpError::HandshakeFailed {
            server: server_name.to_string(),
            source: Box::new(std::io::Error::other(format!(
                "invalid http header value for `{k}`: {e}"
            ))),
        })?;
        out.insert(name, value);
    }
    Ok(out)
}

/// Concrete reqwest-backed transport construction kept in its own
/// function so the generic parameter on `StreamableHttpClientTransport`
/// stays out of `connect_http_client`'s signature.
fn build_http_transport(
    config: StreamableHttpClientTransportConfig,
) -> impl Transport<RoleClient> + 'static {
    StreamableHttpClientTransport::<reqwest::Client>::from_config(config)
}
