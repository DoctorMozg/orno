//! User-facing pipeline schema. Keep every variant explicit; no untagged
//! enums over bool-ish strings (the Norway problem).
//!
//! Two node kinds live here — `agent` and `shell`. The agent kind covers
//! pure LLM calls and tool-using loops alike; HTTP, parsing, and
//! assertions happen inside agent tooling rather than as sibling kinds.

use std::collections::BTreeMap;
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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

/// One DAG node: the fields every kind shares plus the kind-specific body.
///
/// Deserialization is hand-written because serde cannot combine
/// `flatten` with `deny_unknown_fields`.
#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub id: String,
    #[serde(flatten)]
    pub kind: NodeKind,
    pub needs: Vec<String>,
    /// Per-node wall-clock budget in seconds. When elapsed
    /// before the executor returns, `Engine::dispatch_node` cancels via
    /// `tokio::time::timeout`, emits `Event::NodeTimedOut`, and records
    /// `NodeFailure::TimedOut` on the paired `NodeFinished`. `None` means
    /// no wall-clock budget on this node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

/// The fields common to every node kind, split off before the
/// kind-specific body is parsed.
#[derive(Deserialize, JsonSchema)]
struct NodeCommon {
    id: String,
    #[serde(default)]
    needs: Vec<String>,
    #[serde(default)]
    timeout: Option<u64>,
}

impl<'de> Deserialize<'de> for Node {
    /// Splits the common fields off the mapping, then hands the rest to
    /// `NodeKind`, whose structs reject unknown keys. Errors carry the
    /// node id so a typo like `need:` points at the right node.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;

        let mut body = serde_json::Map::deserialize(deserializer)?;
        let mut common = serde_json::Map::new();
        for key in ["id", "needs", "timeout"] {
            if let Some(value) = body.remove(key) {
                common.insert(key.to_string(), value);
            }
        }
        let common = NodeCommon::deserialize(serde_json::Value::Object(common))
            .map_err(|e| D::Error::custom(format!("node: {e}")))?;
        let kind = NodeKind::deserialize(serde_json::Value::Object(body))
            .map_err(|e| D::Error::custom(format!("node `{}`: {e}", common.id)))?;
        Ok(Self {
            id: common.id,
            kind,
            needs: common.needs,
            timeout: common.timeout,
        })
    }
}

/// One closed object per kind with the common fields merged in, because
/// `additionalProperties: false` on the kind bodies would otherwise
/// reject `id` and `needs`.
impl JsonSchema for Node {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Node".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        node_schema(generator)
    }
}

fn node_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let common = NodeCommon::json_schema(generator);
    let variants: Vec<serde_json::Value> = [
        ("agent", AgentNode::json_schema(generator)),
        ("shell", ShellNode::json_schema(generator)),
    ]
    .into_iter()
    .map(|(kind, body)| {
        let mut schema = body.to_value();
        let object = schema.as_object_mut().expect("kind schemas are objects");
        let properties = object
            .entry("properties")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .expect("properties is an object");
        properties.insert("kind".into(), serde_json::json!({ "const": kind }));
        for (name, value) in common
            .as_object()
            .and_then(|c| c.get("properties"))
            .and_then(|p| p.as_object())
            .into_iter()
            .flatten()
        {
            properties.insert(name.clone(), value.clone());
        }
        let required = object
            .entry("required")
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .expect("required is an array");
        required.push("kind".into());
        required.push("id".into());
        schema
    })
    .collect();
    schemars::json_schema!({ "type": "object", "oneOf": variants })
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
/// not in the schema.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentNode {
    /// Name of an entry in the top-level `agents` map.
    pub agent: String,
    /// First user message sent to the agent loop. Rendered with the
    /// `nodes.<id>.*` / `vars.*` / `env.*` template context.
    pub initial_prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
    /// allowlist. Values are rendered through the template engine
    /// (the same context as `command`, `args`, and `stdin`); keys
    /// are not rendered — they are identifiers.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Named agent configuration. Referenced by `kind: agent` nodes and by
/// `subagent.<name>` entries in another agent's `allowed_tools`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    ///
    /// Each entry is rendered through the template engine at dispatch
    /// time (the same context as `initial_prompt` and `system`), so
    /// `"{{ env.WORKSPACE }}"` is valid and resolves to the current
    /// value of that variable.
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
    /// Acknowledges that `Bash` escapes `allowed_domains` and `roots`:
    /// a shell can open any connection and touch any path orno cannot
    /// see. Pipeline validation rejects an agent that lists `Bash`
    /// next to either confinement unless this is `true`.
    #[serde(default)]
    pub unconfined_shell: bool,
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

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpStdioConfig {
    /// argv vector. Not passed through a shell.
    pub command: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Per-call `tools/call` timeout in seconds. Defaults to 30 when omitted.
    #[serde(default)]
    pub call_timeout_secs: Option<u64>,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpHttpConfig {
    pub url: String,
    #[serde(default)]
    pub auth: Option<McpAuthConfig>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Per-call `tools/call` timeout in seconds. Defaults to 30 when omitted.
    #[serde(default)]
    pub call_timeout_secs: Option<u64>,
}

/// MCP HTTP authentication. `bearer` takes a single token; `basic`
/// takes `user`/`password`; `none` is the absence-of-auth sentinel and
/// is equivalent to omitting the `auth` block.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum McpAuthConfig {
    Bearer { token: String },
    Basic { user: String, password: String },
    None,
}

/// Prints map keys but hides values, which carry credentials for MCP
/// `env` and `headers`.
struct RedactedValues<'a>(&'a BTreeMap<String, String>);

impl fmt::Debug for RedactedValues<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.0.keys().map(|k| (k, "<redacted>")))
            .finish()
    }
}

impl fmt::Debug for McpStdioConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpStdioConfig")
            .field("command", &self.command)
            .field("env", &RedactedValues(&self.env))
            .field("call_timeout_secs", &self.call_timeout_secs)
            .finish()
    }
}

impl fmt::Debug for McpHttpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpHttpConfig")
            .field("url", &self.url)
            .field("auth", &self.auth)
            .field("headers", &RedactedValues(&self.headers))
            .field("call_timeout_secs", &self.call_timeout_secs)
            .finish()
    }
}

impl fmt::Debug for McpAuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bearer { .. } => f
                .debug_struct("Bearer")
                .field("token", &"<redacted>")
                .finish(),
            Self::Basic { user, .. } => f
                .debug_struct("Basic")
                .field("user", user)
                .field("password", &"<redacted>")
                .finish(),
            Self::None => f.write_str("None"),
        }
    }
}

#[cfg(test)]
mod credential_debug_tests {
    use super::*;

    #[test]
    fn debug_hides_every_credential_value() {
        let cfg = McpHttpConfig {
            url: "https://mcp.example".to_string(),
            auth: Some(McpAuthConfig::Basic {
                user: "bob".to_string(),
                password: "pw-secret".to_string(),
            }),
            headers: BTreeMap::from([(
                "Authorization".to_string(),
                "Bearer hdr-secret".to_string(),
            )]),
            call_timeout_secs: None,
        };
        let stdio = McpStdioConfig {
            command: vec!["srv".to_string()],
            env: BTreeMap::from([("TOKEN".to_string(), "env-secret".to_string())]),
            call_timeout_secs: None,
        };
        let bearer = McpAuthConfig::Bearer {
            token: "tok-secret".to_string(),
        };
        let out = format!("{cfg:?} {stdio:?} {bearer:?}");
        for leaked in ["pw-secret", "hdr-secret", "env-secret", "tok-secret"] {
            assert!(!out.contains(leaked), "{leaked} leaked: {out}");
        }
        assert!(out.contains("<redacted>"));
        assert!(out.contains("Authorization"));
    }
}

#[cfg(test)]
mod unknown_field_tests {
    use super::*;

    const AGENT_BLOCK: &str = "agents:\n  a:\n    model: m\n    provider: p\n    policy:\n      max_iterations: 1\n      max_total_tokens: 1\n      max_tool_calls: 0\n      max_subagent_depth: 0\n      allow_mutations: false\n      allow_network: false\n      on_parse_error: fail\n";

    #[rstest::rstest]
    #[case::node_needs(
        "nodes:\n  - id: n\n    kind: shell\n    command: x\n    need: [a]\n",
        "need",
        "n"
    )]
    #[case::node_timeout_secs(
        "nodes:\n  - id: n\n    kind: shell\n    command: x\n    timeout_secs: 5\n",
        "timeout_secs",
        "n"
    )]
    #[case::agent_node(
        "nodes:\n  - id: n\n    kind: agent\n    agent: a\n    initial_prompt: x\n    prompt: y\n",
        "prompt",
        "n"
    )]
    #[case::top_level("nodes: []\nvars_: {}\n", "vars_", "")]
    fn unknown_key_is_rejected(#[case] yaml: &str, #[case] key: &str, #[case] node: &str) {
        let err = serde_yaml_ng::from_str::<Pipeline>(yaml)
            .expect_err("unknown key must fail to parse")
            .to_string();
        assert!(err.contains(key), "{err}");
        assert!(err.contains(node), "{err}");
    }

    #[test]
    fn unknown_policy_key_is_rejected() {
        let yaml = format!("{AGENT_BLOCK}      blocked_domain: [x.com]\nnodes: []\n");
        let err = serde_yaml_ng::from_str::<Pipeline>(&yaml)
            .expect_err("typo'd policy key must fail")
            .to_string();
        assert!(err.contains("blocked_domain"), "{err}");
    }

    #[test]
    fn unknown_mcp_key_is_rejected() {
        let yaml =
            "mcp_servers:\n  s:\n    transport: stdio\n    command: [x]\n    cwd: /\nnodes: []\n";
        let err = serde_yaml_ng::from_str::<Pipeline>(yaml)
            .expect_err("unknown mcp key must fail")
            .to_string();
        assert!(err.contains("cwd"), "{err}");
    }

    #[test]
    fn well_formed_node_keeps_common_fields() {
        let yaml =
            "nodes:\n  - id: n\n    kind: shell\n    command: x\n    needs: [m]\n    timeout: 7\n";
        let p: Pipeline = serde_yaml_ng::from_str(yaml).expect("valid node parses");
        assert_eq!(p.nodes[0].needs, ["m"]);
        assert_eq!(p.nodes[0].timeout, Some(7));
    }
}
