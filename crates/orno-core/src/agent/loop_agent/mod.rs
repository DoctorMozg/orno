//! `LoopAgent` — iteration-loop implementation of [`Agent`].
//!
//! Enforces the five strictness dimensions in one loop: bounded
//! iteration (`max_iterations`), bounded tool surface (`allowed_tools`
//! plus registered handlers), bounded effects (`allow_mutations` and
//! `allow_network` — denials feed back to the model as tool-result
//! strings, the loop continues), bounded resources (`max_total_tokens`,
//! `max_tool_calls`), and bounded non-determinism (delegated to the
//! transport / recording layer).
//!
//! On transport failure the impl emits a typed `LlmRequestFailed` next
//! to the dangling `LlmRequestStarted` so downstream consumers can
//! classify auth / rate-limit / model-not-found without grepping error
//! strings.
//!
//! **Subagent dispatch.** Entries in `allowed_tools` named
//! `subagent.<child>` correspond to [`SubagentHandler`] instances that
//! hold a `Weak<LoopAgent>` back-pointer into this same loop. Depth is
//! enforced here (not in the handler) so the policy gate runs before
//! any child loop entry, and the denial feeds back as a
//! tool-result string. Wire names are sanitized at the
//! `OrnoChatTool` boundary — the YAML uses dotted names but some
//! providers reject dots in `function.name`, so we translate
//! `subagent.<child>` → `subagent_<child>` before the schema reaches
//! the LLM, and reverse-translate when routing the model's tool call
//! back to a handler.
//!
//! Module layout: `mod.rs` owns the struct and its trivial helpers plus
//! all unit tests; `policy.rs` owns the effect-gate and parse-retry
//! helpers; `run.rs` owns the `impl Agent for LoopAgent` body.

pub(crate) mod policy;
mod run;

use std::sync::Arc;

use crate::events::{EventSink, Redactor, truncate_excerpt};
use crate::llm::LlmTransport;
use crate::tool::ToolHandler;

/// Default cap on the body excerpt captured into `LlmFailure::ApiError`
/// when the agent was constructed without a caller-supplied bound.
/// Mirrors `EngineConfig::default().max_output_bytes` so an embedder
/// that builds the agent in isolation gets the same truncation policy
/// the CLI threads through.
const DEFAULT_BODY_EXCERPT_BYTES: usize = 2048;

/// YAML-facing subagent prefix. A tool name in `allowed_tools` that
/// starts with this string is routed through the recursion-depth gate
/// before dispatch. Kept as a constant so a typo in one place does
/// not silently bypass the gate.
const SUBAGENT_PREFIX: &str = "subagent.";

/// Configuration bundle for [`LoopAgent`]. Keeps the constructor below
/// the four-parameter threshold per the project's config-struct
/// convention (CLAUDE.md). Fields are `pub` so embedders can construct
/// the struct with standard field-init syntax.
pub struct LoopAgentConfig {
    pub transport: Arc<dyn LlmTransport>,
    pub sink: Arc<dyn EventSink>,
    /// Redacts `secrets.*` values out of prompt, response, and tool
    /// excerpts before they reach the wire.
    pub redactor: Arc<Redactor>,
    /// Cap for body excerpts captured into `LlmFailure::ApiError` and
    /// the `prompt_excerpt` / `system_excerpt` / `content_excerpt` /
    /// tool-call excerpt fields. Shared with the engine's
    /// `max_output_bytes` so every truncated field looks alike to log
    /// readers.
    pub body_excerpt_max_bytes: usize,
    /// Handlers for tools the agent is allowed to invoke. An empty
    /// vector means the agent can only converse — it will receive no
    /// tool definitions and any tool-call turn from the model will
    /// route through [`AgentError::UnknownToolCalled`] since every
    /// name validates against this set.
    pub tools: Vec<Arc<dyn ToolHandler>>,
}

pub struct LoopAgent {
    config: LoopAgentConfig,
}

impl LoopAgent {
    #[must_use]
    pub fn new(config: LoopAgentConfig) -> Self {
        Self { config }
    }

    /// Convenience constructor for embedders and tests that do not
    /// thread an `EngineConfig` or a live secret map through. Picks
    /// the same default the engine ships with for the body cap and a
    /// no-op redactor; the wire format stays consistent across
    /// construction sites, and a test without secrets pays no
    /// redaction cost (`Redactor::is_noop() == true`).
    #[must_use]
    pub fn with_defaults(transport: Arc<dyn LlmTransport>, sink: Arc<dyn EventSink>) -> Self {
        Self::new(LoopAgentConfig {
            transport,
            sink,
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: DEFAULT_BODY_EXCERPT_BYTES,
            tools: Vec::new(),
        })
    }

    /// Redact + head-truncate a user-visible string for emission into
    /// an excerpt field on an event envelope. Head truncation because
    /// prompts lead with the instruction and responses lead with the
    /// answer.
    fn excerpt_for_wire(&self, s: &str) -> String {
        truncate_excerpt(
            self.config.redactor.redact(s).as_ref(),
            self.config.body_excerpt_max_bytes,
        )
    }

    /// Locate the handler for a tool by its YAML-facing name. Returns
    /// `None` only when the name slipped past the `allowed_tools`
    /// cross-check — treated as `AgentError::UnknownToolCalled` at the
    /// call site.
    fn find_handler(&self, yaml_name: &str) -> Option<&Arc<dyn ToolHandler>> {
        self.config.tools.iter().find(|h| h.name() == yaml_name)
    }

    /// Translate a YAML-facing tool name (possibly containing dots, as
    /// in `subagent.contributor_vibes`) into the wire-safe form the LLM
    /// schema presents. Dotless names are returned unchanged; a new
    /// allocation only happens for the subagent case.
    fn to_wire_name(yaml_name: &str) -> String {
        if yaml_name.contains('.') {
            yaml_name.replace('.', "_")
        } else {
            yaml_name.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    mod events;
    mod helpers;
    mod parse_retry;
    mod strictness_effects;
    mod strictness_iteration;
    mod strictness_resources;
    mod strictness_tools;
    mod subagent;
}
