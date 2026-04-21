//! User-facing pipeline schema. Keep every variant explicit; no untagged
//! enums over bool-ish strings (the Norway problem).
//!
//! Shape targets the v0.1.0 YAML documented in `docs/yaml-spec.md`. Two
//! node kinds live here — `agent` and `shell` — per ADR 0009 (collapse
//! `llm` into `agent`) and ADR 0017 (remove `external` entirely).

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
    /// template namespace at run start (ADR 0020). Opt-in only — the
    /// process environment is not auto-inherited. Names not present via
    /// `pass_env`, `-e`, or `--env-file` are a template-render error.
    #[serde(default)]
    pub pass_env: Vec<String>,
    /// User-declared credential names, routed into the redacted
    /// `secrets.*` template namespace (ADR 0020). Provider-known names
    /// (e.g. `OPENROUTER_API_KEY`) are auto-pulled by the transport and
    /// do not need to appear here; this list is for additional secrets
    /// such as MCP server tokens.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Named agent configurations, referenced from `kind: agent` nodes
    /// by `agent: <name>` (ADR 0009).
    #[serde(default)]
    pub agents: BTreeMap<String, AgentConfig>,
    /// MCP servers spawned at run start, torn down at run end (ADR 0007).
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
}

/// Built-in node kinds. v0.1 ships two variants — `Agent` and `Shell` —
/// per ADR 0017 §1. The former `Llm` variant was collapsed into `Agent`
/// (ADR 0009); the former `External` variant was removed entirely and
/// will return as a `transport:` axis on existing kinds post-v0.1
/// (ADR 0017 §3).
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

/// Agent-loop strictness knobs (ADR 0005, narrowed by ADR 0017). Every
/// field is required; defaults belong in docs and examples, not silently
/// in code. Wall-clock is **not** here — ADR 0017 promotes it to a
/// universal node-level `timeout:` attribute applicable to every
/// `NodeKind`; the attribute is not yet modeled on `Node` and lands with
/// the Phase 4–5 executor work.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentPolicy {
    pub max_iterations: u32,
    pub max_total_tokens: u64,
    pub max_tool_calls: u32,
    pub max_subagent_depth: u32,
    pub allow_mutations: bool,
    pub allow_network: bool,
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    #[serde(default)]
    pub blocked_domains: Vec<String>,
    pub on_parse_error: OnParseError,
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
/// HTTP. See ADR 0007.
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
