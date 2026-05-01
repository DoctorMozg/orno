//! User-facing pipeline schema. Keep every variant explicit; no untagged
//! enums over bool-ish strings (the Norway problem).
//!
//! Shape targets the v0.1.0 YAML documented in `docs/yaml-spec.md`. Two
//! node kinds live here — `agent` and `shell`. The agent kind covers
//! pure LLM calls and tool-using loops alike; HTTP, parsing, and
//! assertions happen inside agent tooling rather than as sibling kinds.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Pipeline {
    /// Schema version. Incremented whenever the schema changes in a
    /// backwards-incompatible way.
    #[serde(default = "default_schema_version")]
    pub version: u32,
    #[serde(default)]
    pub vars: BTreeMap<String, serde_json::Value>,
    /// Names pulled from the process environment into the `env.*`
    /// template namespace at run start. Opt-in only — the
    /// process environment is not auto-inherited. Names not present via
    /// `pass_env`, `-e`, or `--env-file` are a template-render error.
    #[serde(default)]
    pub pass_env: Vec<String>,
    /// User-declared credential names, routed into the redacted
    /// `secrets.*` template namespace. Provider-known names
    /// (e.g. `OPENROUTER_API_KEY`) are auto-pulled by the transport and
    /// do not need to appear here; this list is for additional secrets
    /// such as MCP server tokens.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Named agent configurations, referenced from `kind: agent` nodes
    /// by `agent: <name>`.
    #[serde(default)]
    pub agents: BTreeMap<String, AgentConfig>,
    /// MCP servers spawned at run start, torn down at run end.
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    pub nodes: Vec<Node>,
}

fn default_schema_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Node {
    pub id: String,
    #[serde(flatten)]
    pub kind: NodeKind,
    #[serde(default)]
    pub needs: Vec<String>,
    /// Per-node wall-clock budget in seconds. When elapsed
    /// before the executor returns, `Engine::dispatch_node` cancels via
    /// `tokio::time::timeout`, emits `Event::NodeTimedOut`, and records
    /// `NodeFailure::TimedOut` on the paired `NodeFinished`. `None` means
    /// no wall-clock budget on this node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

/// Built-in node kinds. v0.1 ships two variants — `Agent` and `Shell`.
/// HTTP, parsing, and assertions happen inside agent tooling rather
/// than as sibling kinds.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum NodeKind {
    Agent(AgentNode),
    Shell(ShellNode),
}

/// Agent node — references a named entry in `Pipeline.agents`. Inline
/// agent configuration at the node level is a v0.2+ convenience and is
/// not in the v0.1 schema (see `docs/yaml-spec.md`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentNode {
    /// Name of an entry in the top-level `agents` map.
    pub agent: String,
    /// First user message sent to the agent loop. Rendered with the
    /// `nodes.<id>.*` / `vars.*` / `env.*` template context.
    pub initial_prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ShellNode {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Content to pipe into the child's stdin. Rendered through the
    /// same template context as `command` and `args`. When `None`,
    /// stdin is closed (default) — matching the pre-stdin behavior so
    /// existing pipelines keep working. When `Some`, even an empty
    /// string is delivered so the child sees a pipe that closes at
    /// EOF rather than a closed handle.
    #[serde(default)]
    pub stdin: Option<String>,
    /// Environment variable overrides for the spawned child. The
    /// executor clears the inherited environment, restores a small
    /// allowlist of safe variables (`PATH`, `HOME`, etc.), then
    /// applies these entries on top so per-node values shadow the
    /// allowlist. Operator-supplied — not rendered through the
    /// template engine.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Named agent configuration. Referenced by `kind: agent` nodes and by
/// `subagent.<name>` entries in another agent's `allowed_tools`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentConfig {
    /// Provider-side model identifier. For `OpenRouter` this is the
    /// slash-prefixed route (`openai/gpt-5`); for direct-vendor
    /// providers this is the vendor's own model name.
    pub model: String,
    /// `LlmTransport` provider key. Default in v0.1 is `openrouter`.
    pub provider: String,
    /// Optional system prompt. Rendered with the same template context
    /// as `initial_prompt`.
    #[serde(default)]
    pub system: Option<String>,
    /// Tool allowlist. Builtin names (`Bash`, `Read`, `Edit`, `Write`,
    /// `WebFetch`), `mcp.<server>.<tool>` / `mcp.<server>.*`, and
    /// `subagent.<agent-name>`. Empty list means the agent has no tools.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    pub policy: AgentPolicy,
}

/// Agent-loop strictness knobs. Every field is required in Rust
/// construction; defaults belong in docs and examples, not silently
/// in code. `allow_context_writes` gets `#[serde(default)]` so YAML
/// pipelines that don't opt into `SetState` keep deserializing — the
/// default `false` matches the behavior of "no mutable context."
/// Wall-clock is **not** here — `timeout:` is a universal node-level
/// attribute applicable to every `NodeKind`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentPolicy {
    pub max_iterations: u32,
    pub max_total_tokens: u64,
    pub max_tool_calls: u32,
    pub max_subagent_depth: u32,
    /// Cap on per-call tool output (in bytes) appended to the
    /// conversation history that the next LLM turn sees. A handler may
    /// legally return any size; the loop truncates the head bytes that
    /// fit and appends an ellipsis marker before pushing the
    /// `ToolResult` message. `None` keeps the built-in default (256 KiB)
    /// — large enough for typical Read / `WebFetch` results, small enough
    /// to bound conversation growth on a runaway tool. Set to a small
    /// value in tests to make the truncation observable.
    #[serde(default)]
    pub max_tool_output_bytes: Option<usize>,
    pub allow_mutations: bool,
    pub allow_network: bool,
    /// Gates `ToolEffect::ContextSelf` (the `SetState` builtin).
    /// Defaults to `false` so a pipeline that never references the
    /// tool sees no shape change. When `false`, any `SetState` call
    /// returns a tool-result denial string exactly like
    /// `allow_mutations=false` denies filesystem tools.
    #[serde(default)]
    pub allow_context_writes: bool,
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    #[serde(default)]
    pub blocked_domains: Vec<String>,
    pub on_parse_error: OnParseError,
    /// Filesystem roots the `Read` / `Write` / `Edit` builtins are
    /// jailed to. When the agent's `allowed_tools` list contains any
    /// of those names, this list must be non-empty — `LoopAgent`
    /// rejects with `AgentError::InvalidPolicy` at run start.
    /// Currently only `roots[0]` is consulted; multi-root agents are
    /// a v0.2+ extension.
    #[serde(default)]
    pub roots: Vec<std::path::PathBuf>,
    /// Soft cap on the approximate byte size of the accumulated
    /// conversation history (sum of content lengths across all
    /// `OrnoChatMessage` variants). When the history exceeds this
    /// threshold after a push, the loop evicts the oldest messages
    /// until the history is back under the cap, always retaining the
    /// two most recent messages so the model sees at least one
    /// request-response round.
    ///
    /// `None` disables the cap (not recommended for long-running agents).
    /// The default (4 MiB) is generous enough for normal tool-call loops
    /// and tight enough to prevent multi-gigabyte prompt payloads.
    #[serde(default = "default_max_message_history_bytes")]
    pub max_message_history_bytes: Option<usize>,
}

fn default_max_message_history_bytes() -> Option<usize> {
    Some(4 * 1024 * 1024)
}

/// What the loop does when the model returns malformed JSON for a
/// tool-call argument payload.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OnParseError {
    /// Terminate the node with `AgentError::ParseFailed`.
    Fail,
    /// Feed the parse error back as a tool-result message and loop once
    /// more. Further parse errors terminate.
    RetryOnce,
}

/// MCP server declaration. Two transports in v0.1: stdio subprocess and
/// HTTP.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "transport", rename_all = "snake_case")]
#[non_exhaustive]
pub enum McpServerConfig {
    Stdio(McpStdioConfig),
    Http(McpHttpConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpStdioConfig {
    /// argv vector. Not passed through a shell.
    pub command: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpHttpConfig {
    pub url: String,
    #[serde(default)]
    pub auth: Option<McpAuthConfig>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// MCP HTTP authentication. `bearer` takes a single token; `basic`
/// takes `user`/`password`; `none` is the absence-of-auth sentinel and
/// is equivalent to omitting the `auth` block.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum McpAuthConfig {
    Bearer { token: String },
    Basic { user: String, password: String },
    None,
}
