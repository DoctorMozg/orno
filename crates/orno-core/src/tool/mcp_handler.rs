//! `McpToolHandler` — bridges the `ToolHandler` seam to a live MCP server.
//! One instance per (server, tool) pair; plugged into the executor's
//! dispatch table at run start by `orno run`.
//!
//! Effect class is conservatively `MutationsAndNetwork` because MCP servers
//! are arbitrary programs that may mutate files and issue network calls; orno
//! cannot inspect their internals at load time.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tracing::instrument;

use super::{ToolEffect, ToolHandler, ToolInvocation};
use crate::error::ToolError;
use crate::events::{Event, EventSink, truncate_excerpt};
use crate::mcp::McpClient;

/// Static metadata for one MCP tool. Grouped so `McpToolHandler::new` stays
/// under the project's four-parameter threshold (clippy.toml §too-many-arguments).
pub struct McpToolHandlerConfig {
    /// Tool name as the LLM sees it: `mcp.<server>.<tool>`. Built by the
    /// caller with `format!("mcp.{server}.{tool}")`.
    pub yaml_name: String,
    /// MCP server name — the key in `Pipeline.mcp_servers`. Echoed verbatim
    /// in `McpToolCallSent / Completed` events for correlation.
    pub server: String,
    /// MCP tool name exactly as advertised by the server's `tools/list`.
    pub tool: String,
    pub description: String,
    /// JSON Schema received from the server's `tools/list` response.
    pub schema: Value,
    /// How many bytes of input/output JSON to include in excerpt events.
    /// Shared from the engine's `max_output_bytes` cap.
    pub body_excerpt_max_bytes: usize,
}

/// One handler per `(mcp_server_name, tool_name)` pair. Constructed by
/// `orno run` after each MCP server returns its `tools/list`. The `client`
/// is shared across all tools from the same server (wrapped in `Arc`) so
/// only one TCP/stdio connection exists per server per run.
pub struct McpToolHandler {
    config: McpToolHandlerConfig,
    /// Shared client for the owning MCP server. `call_tool` takes `&self`
    /// so sharing via `Arc` is sound.
    client: Arc<dyn McpClient>,
    sink: Arc<dyn EventSink>,
}

impl std::fmt::Debug for McpToolHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolHandler")
            .field("yaml_name", &self.config.yaml_name)
            .field("server", &self.config.server)
            .field("tool", &self.config.tool)
            .finish_non_exhaustive()
    }
}

impl McpToolHandler {
    #[must_use]
    pub fn new(
        config: McpToolHandlerConfig,
        client: Arc<dyn McpClient>,
        sink: Arc<dyn EventSink>,
    ) -> Self {
        Self {
            config,
            client,
            sink,
        }
    }
}

#[async_trait]
impl ToolHandler for McpToolHandler {
    fn name(&self) -> &str {
        &self.config.yaml_name
    }

    fn description(&self) -> &str {
        &self.config.description
    }

    fn schema(&self) -> Value {
        self.config.schema.clone()
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::MutationsAndNetwork
    }

    #[instrument(
        skip(self, args),
        fields(
            mcp.server = %self.config.server,
            mcp.tool = %self.config.tool,
            call_id = %inv.call_id
        )
    )]
    async fn invoke(&self, inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        let input_excerpt = truncate_excerpt(
            &serde_json::to_string(&args).unwrap_or_default(),
            self.config.body_excerpt_max_bytes,
        );

        self.sink
            .record(Event::McpToolCallSent {
                run_id: inv.run_id.to_string(),
                node_id: inv.node_id.to_string(),
                server: self.config.server.clone(),
                tool: self.config.tool.clone(),
                call_id: inv.call_id.to_string(),
                input_excerpt,
            })
            .await;

        let call_result = self.client.call_tool(&self.config.tool, args).await;

        match call_result {
            Ok(result) => {
                let output_excerpt =
                    truncate_excerpt(&result.content, self.config.body_excerpt_max_bytes);
                self.sink
                    .record(Event::McpToolCallCompleted {
                        run_id: inv.run_id.to_string(),
                        node_id: inv.node_id.to_string(),
                        server: self.config.server.clone(),
                        tool: self.config.tool.clone(),
                        call_id: inv.call_id.to_string(),
                        ok: result.ok,
                        output_excerpt,
                    })
                    .await;
                Ok(result.content)
            },
            Err(e) => {
                let reason = e.to_string();
                self.sink
                    .record(Event::McpToolCallCompleted {
                        run_id: inv.run_id.to_string(),
                        node_id: inv.node_id.to_string(),
                        server: self.config.server.clone(),
                        tool: self.config.tool.clone(),
                        call_id: inv.call_id.to_string(),
                        ok: false,
                        output_excerpt: truncate_excerpt(
                            &reason,
                            self.config.body_excerpt_max_bytes,
                        ),
                    })
                    .await;
                Err(ToolError::Invocation {
                    name: self.config.yaml_name.clone(),
                    source: Box::new(e),
                })
            },
        }
    }
}

/// Replay-only stub for one MCP tool. Carries just enough metadata
/// (name, description, schema) for the agent loop's outgoing
/// `OrnoChatTool` definition to byte-equal what was recorded — that
/// equality is what makes `ReplayTransport` find the right response
/// (the tape key hashes the entire `LlmRequest`, including its
/// `tools` list).
///
/// The real `McpToolHandler` is unusable in `orno replay` because it
/// requires a live `McpClient`, which is not spawned in replay (no
/// network, no subprocess). This stub fills the registration slot so
/// the agent loop's pre-flight `find_handler` check passes; actual
/// dispatch is intercepted by `ReplayToolHandler` wrapping the stub,
/// so [`Self::invoke`] is unreachable in normal use. If somehow
/// called directly, it returns a clear `ToolError` rather than
/// panicking.
pub struct ReplayMcpStubHandler {
    name: String,
    description: String,
    schema: Value,
}

impl std::fmt::Debug for ReplayMcpStubHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayMcpStubHandler")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl ReplayMcpStubHandler {
    /// Build a stub for one MCP tool. `name` is the YAML-form
    /// (`mcp.<server>.<tool>`) so it matches the agent's
    /// `allowed_tools` entries directly. `description` and `schema`
    /// must echo what the original `McpToolHandler` returned at
    /// recording time — `orno replay` looks them up from the LLM
    /// tape's `OrnoChatTool` entries to guarantee that.
    #[must_use]
    pub fn new(name: String, description: String, schema: Value) -> Self {
        Self {
            name,
            description,
            schema,
        }
    }
}

#[async_trait]
impl ToolHandler for ReplayMcpStubHandler {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::MutationsAndNetwork
    }

    async fn invoke(&self, _inv: ToolInvocation<'_>, _args: Value) -> Result<String, ToolError> {
        Err(ToolError::Invocation {
            name: self.name.clone(),
            source: Box::new(std::io::Error::other(
                "ReplayMcpStubHandler::invoke called directly — must be wrapped in ReplayToolHandler so the tool tape serves the response",
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::error::McpError;
    use crate::events::InMemorySink;
    use crate::mcp::{McpTool, McpToolCallResult};

    struct AlwaysOkClient {
        server_name: String,
    }

    impl std::fmt::Debug for AlwaysOkClient {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("AlwaysOkClient").finish()
        }
    }

    #[async_trait::async_trait]
    impl McpClient for AlwaysOkClient {
        fn server_name(&self) -> &str {
            &self.server_name
        }
        async fn initialize(&mut self) -> Result<Vec<McpTool>, McpError> {
            Ok(vec![])
        }
        async fn call_tool(
            &self,
            _tool: &str,
            _args: Value,
        ) -> Result<McpToolCallResult, McpError> {
            Ok(McpToolCallResult {
                ok: true,
                content: "hello from mcp".to_string(),
            })
        }
        async fn shutdown(&mut self) -> Result<(), McpError> {
            Ok(())
        }
    }

    struct AlwaysErrClient {
        server_name: String,
    }

    impl std::fmt::Debug for AlwaysErrClient {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("AlwaysErrClient").finish()
        }
    }

    #[async_trait::async_trait]
    impl McpClient for AlwaysErrClient {
        fn server_name(&self) -> &str {
            &self.server_name
        }
        async fn initialize(&mut self) -> Result<Vec<McpTool>, McpError> {
            Ok(vec![])
        }
        async fn call_tool(
            &self,
            _tool: &str,
            _args: Value,
        ) -> Result<McpToolCallResult, McpError> {
            Err(McpError::CallFailed {
                server: "fs".to_string(),
                tool: "read_file".to_string(),
                source: Box::new(std::io::Error::other("connection refused")),
            })
        }
        async fn shutdown(&mut self) -> Result<(), McpError> {
            Ok(())
        }
    }

    fn make_handler(client: Arc<dyn McpClient>) -> (McpToolHandler, Arc<InMemorySink>) {
        let sink = Arc::new(InMemorySink::new());
        let handler = McpToolHandler::new(
            McpToolHandlerConfig {
                yaml_name: "mcp.fs.read_file".to_string(),
                server: "fs".to_string(),
                tool: "read_file".to_string(),
                description: "Read a file".to_string(),
                schema: json!({"type": "object"}),
                body_excerpt_max_bytes: 512,
            },
            client,
            sink.clone(),
        );
        (handler, sink)
    }

    #[tokio::test]
    async fn successful_call_emits_paired_events_and_returns_content() {
        let client: Arc<dyn McpClient> = Arc::new(AlwaysOkClient {
            server_name: "fs".to_string(),
        });
        let (handler, sink) = make_handler(client);

        let result = handler
            .invoke(
                ToolInvocation::for_test("call-1"),
                json!({"path": "/tmp/x"}),
            )
            .await
            .unwrap();

        assert_eq!(result, "hello from mcp");

        let events = sink.snapshot();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].event, Event::McpToolCallSent { .. }));
        assert!(matches!(
            events[1].event,
            Event::McpToolCallCompleted { ok: true, .. }
        ));
    }

    #[tokio::test]
    async fn transport_error_emits_completed_ok_false_and_returns_tool_error() {
        let client: Arc<dyn McpClient> = Arc::new(AlwaysErrClient {
            server_name: "fs".to_string(),
        });
        let (handler, sink) = make_handler(client);

        let err = handler
            .invoke(ToolInvocation::for_test("call-2"), json!({}))
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::Invocation { .. }));

        let events = sink.snapshot();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].event, Event::McpToolCallSent { .. }));
        assert!(matches!(
            events[1].event,
            Event::McpToolCallCompleted { ok: false, .. }
        ));
    }

    #[test]
    fn effect_is_mutations_and_network() {
        let client: Arc<dyn McpClient> = Arc::new(AlwaysOkClient {
            server_name: "fs".to_string(),
        });
        let (handler, _sink) = make_handler(client);
        assert_eq!(handler.effect(), ToolEffect::MutationsAndNetwork);
    }

    #[test]
    fn replay_stub_exposes_yaml_name_and_recorded_metadata() {
        let stub = ReplayMcpStubHandler::new(
            "mcp.fs.read_file".to_string(),
            "Read a file".to_string(),
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        );
        assert_eq!(stub.name(), "mcp.fs.read_file");
        assert_eq!(stub.description(), "Read a file");
        assert_eq!(stub.schema()["type"], "object");
        // Effect must match the live `McpToolHandler` so the agent
        // loop's policy gate behaves identically in replay vs. record.
        assert_eq!(stub.effect(), ToolEffect::MutationsAndNetwork);
    }

    #[tokio::test]
    async fn replay_stub_invoke_called_directly_returns_clear_error() {
        // Direct invocation should never happen in practice — every
        // stub gets wrapped in `ReplayToolHandler`. But if it does,
        // we want a readable error, not a panic.
        let stub =
            ReplayMcpStubHandler::new("mcp.fs.read_file".to_string(), String::new(), json!({}));
        let err = stub
            .invoke(ToolInvocation::for_test("c1"), json!({}))
            .await
            .unwrap_err();
        match err {
            ToolError::Invocation { name, source } => {
                assert_eq!(name, "mcp.fs.read_file");
                assert!(source.to_string().contains("ReplayToolHandler"));
            },
            other => panic!("expected ToolError::Invocation, got {other:?}"),
        }
    }
}
