//! `LoopAgent` — Phase 5 iteration-loop implementation of [`Agent`].
//!
//! Enforces the five strictness dimensions of ADR 0005 in one loop:
//! bounded iteration (`max_iterations`), bounded tool surface
//! (`allowed_tools` + registered handlers), bounded effects
//! (`allow_mutations` / `allow_network` — denials feed back to the
//! model as tool-result strings per §3, the loop continues), bounded
//! resources (`max_total_tokens`, `max_tool_calls`), and bounded
//! non-determinism (delegated to the transport / recording layer).
//!
//! On transport failure the impl emits a typed `LlmRequestFailed` next
//! to the dangling `LlmRequestStarted` so downstream consumers can
//! classify auth / rate-limit / model-not-found without grepping error
//! strings.
//!
//! **Subagent dispatch (ADR 0006).** Entries in `allowed_tools` named
//! `subagent.<child>` correspond to [`SubagentHandler`] instances that
//! hold a `Weak<LoopAgent>` back-pointer into this same loop. Depth is
//! enforced here (not in the handler) so the policy gate runs before
//! any child loop entry, and the denial feeds back as a
//! tool-result string per §3. Wire names are sanitized at the
//! `OrnoChatTool` boundary — the YAML uses dotted names but some
//! providers reject dots in `function.name`, so we translate
//! `subagent.<child>` → `subagent_<child>` before the schema reaches
//! the LLM, and reverse-translate when routing the model's tool call
//! back to a handler.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Map, Value};
use tracing::instrument;

use crate::error::{AgentError, ToolError};
use crate::events::{BudgetKind, Event, EventSink, LlmFailure, Redactor, truncate_excerpt};
use crate::llm::{LlmRequest, LlmTransport, OrnoChatMessage, OrnoChatTool, OrnoChatToolCall};
use crate::tool::{StateHandle, ToolEffect, ToolHandler, ToolInvocation};

use super::{Agent, AgentOutput, AgentRequest};

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
    /// excerpts before they reach the wire (ADR 0020 / 0024).
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
    /// answer (ADR 0024).
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

    /// Apply the effect-based policy gate and invoke the handler. Per
    /// ADR 0005 §3 a policy denial is *not* a terminal error — it is
    /// fed back to the model as a `ToolResult` denial string so the
    /// model can adapt. A handler error still terminates the loop via
    /// [`AgentError::Tool`].
    async fn check_policy_and_invoke(
        &self,
        handler: &Arc<dyn ToolHandler>,
        tool_call: &OrnoChatToolCall,
        policy: &crate::pipeline::AgentPolicy,
        inv: ToolInvocation<'_>,
    ) -> Result<String, AgentError> {
        match handler.effect() {
            ToolEffect::Mutations => {
                if !policy.allow_mutations {
                    return Ok(format!(
                        "denied: tool `{}` blocked by allow_mutations=false",
                        tool_call.fn_name,
                    ));
                }
            }
            ToolEffect::Network => {
                if !policy.allow_network {
                    return Ok(format!(
                        "denied: tool `{}` blocked by allow_network=false",
                        tool_call.fn_name,
                    ));
                }
            }
            ToolEffect::MutationsAndNetwork => {
                if !policy.allow_mutations {
                    return Ok(format!(
                        "denied: tool `{}` blocked by allow_mutations=false",
                        tool_call.fn_name,
                    ));
                }
                if !policy.allow_network {
                    return Ok(format!(
                        "denied: tool `{}` blocked by allow_network=false",
                        tool_call.fn_name,
                    ));
                }
            }
            ToolEffect::ContextSelf => {
                if !policy.allow_context_writes {
                    return Ok(format!(
                        "denied: tool `{}` blocked by allow_context_writes=false",
                        tool_call.fn_name,
                    ));
                }
            }
            ToolEffect::ReadOnly => {}
        }

        match handler.invoke(inv, tool_call.fn_arguments.clone()).await {
            Ok(output) => Ok(output),
            // A stub handler that declared itself NotImplemented is a
            // predictable condition — feed it back to the model rather
            // than terminating the loop, matching the denial semantics.
            Err(ToolError::NotImplemented { name, feature }) => Ok(format!(
                "error: tool `{name}` not yet implemented: {feature}",
            )),
            Err(source) => Err(AgentError::Tool {
                name: tool_call.fn_name.clone(),
                source,
            }),
        }
    }
}

/// Snapshot the per-node state buffer for `AgentOutput.state`. Returns
/// `None` when no `SetState` call landed — keeps the wire shape of
/// `nodes.<id>` unchanged for pipelines that never opt into the feature
/// (ADR 0025 §2). A poisoned mutex also reports `None` since the
/// offending panic has already terminated the relevant tool call; a
/// partial buffer would be worse than no state.
fn final_state(buf: &Mutex<Value>) -> Option<Value> {
    let guard = buf.lock().ok()?;
    match &*guard {
        Value::Object(m) if m.is_empty() => None,
        other => Some(other.clone()),
    }
}

#[async_trait]
impl Agent for LoopAgent {
    #[instrument(
        skip(self, req),
        fields(
            node.id = %node_id,
            pipeline.run_id = %run_id,
            llm.provider = %req.provider,
            llm.model = %req.model,
            agent.depth = req.depth,
        ),
    )]
    async fn run(
        &self,
        run_id: &str,
        node_id: &str,
        req: AgentRequest,
    ) -> Result<AgentOutput, AgentError> {
        if req.policy.max_iterations == 0 {
            return Err(AgentError::InvalidPolicy(
                "max_iterations must be >= 1".to_string(),
            ));
        }

        // Cross-check: every entry in `allowed_tools` must correspond
        // to a registered handler. Catching mismatches up-front means
        // the model never sees a tool schema it can't actually call.
        for name in &req.allowed_tools {
            if self.find_handler(name).is_none() {
                return Err(AgentError::UnknownToolCalled { name: name.clone() });
            }
        }

        // Per-node state buffer for the `SetState` builtin (ADR 0025).
        // One object per `run()` call, visible only to tool dispatches
        // inside this loop. Subagent recursion calls `run()` again and
        // gets a fresh buffer — child state never leaks into the parent.
        let state_buffer: Mutex<Value> = Mutex::new(Value::Object(Map::new()));
        let state_handle = StateHandle::new(&state_buffer);

        // Build the tool definitions the LLM sees on each request —
        // intersection of `allowed_tools` and the registered handler
        // set. Empty vector when the agent declared no tools.
        let declared_tools: Vec<OrnoChatTool> = self
            .config
            .tools
            .iter()
            .filter(|h| req.allowed_tools.iter().any(|n| n == h.name()))
            .map(|h| OrnoChatTool {
                name: Self::to_wire_name(h.name()),
                description: h.description().to_string(),
                schema: h.schema(),
            })
            .collect();

        // Reverse map: wire name the LLM sends back → YAML name we
        // registered under. Only dotted YAML names appear here with a
        // non-identity mapping, but we populate the full allowed set so
        // an unexpected wire format still resolves correctly.
        let wire_to_yaml: HashMap<String, String> = self
            .config
            .tools
            .iter()
            .filter(|h| req.allowed_tools.iter().any(|n| n == h.name()))
            .map(|h| {
                let yaml = h.name().to_string();
                (Self::to_wire_name(&yaml), yaml)
            })
            .collect();

        let prompt_excerpt = self.excerpt_for_wire(&req.initial_prompt);
        let system_excerpt = req.system.as_deref().map(|s| self.excerpt_for_wire(s));

        let max_tokens = (req.policy.max_total_tokens > 0)
            .then(|| u32::try_from(req.policy.max_total_tokens).unwrap_or(u32::MAX));

        // Growing conversation history across iterations. The initial
        // user turn rides in `LlmRequest.prompt`; this vector captures
        // assistant tool-call turns and their paired `ToolResult`s so
        // the model can reason over what it already did.
        let mut messages: Vec<OrnoChatMessage> = Vec::new();
        let mut tool_call_count: u32 = 0;
        let mut total_tokens: u64 = 0;

        for iteration in 0..req.policy.max_iterations {
            self.config
                .sink
                .record(Event::AgentIterationStarted {
                    run_id: run_id.to_string(),
                    node_id: node_id.to_string(),
                    iteration,
                })
                .await;

            let llm_req = LlmRequest {
                provider: req.provider.clone(),
                model: req.model.clone(),
                prompt: req.initial_prompt.clone(),
                system: req.system.clone(),
                temperature: None,
                max_tokens,
                messages: messages.clone(),
                tools: declared_tools.clone(),
            };

            self.config
                .sink
                .record(Event::LlmRequestStarted {
                    run_id: run_id.to_string(),
                    node_id: node_id.to_string(),
                    provider: req.provider.clone(),
                    model: req.model.clone(),
                    prompt_excerpt: prompt_excerpt.clone(),
                    system_excerpt: system_excerpt.clone(),
                })
                .await;

            let response = match self.config.transport.complete(llm_req).await {
                Ok(resp) => resp,
                Err(err) => {
                    let failure =
                        LlmFailure::from_llm_error(&err, self.config.body_excerpt_max_bytes);
                    self.config
                        .sink
                        .record(Event::LlmRequestFailed {
                            run_id: run_id.to_string(),
                            node_id: node_id.to_string(),
                            provider: req.provider.clone(),
                            model: req.model.clone(),
                            failure,
                        })
                        .await;
                    return Err(AgentError::from(err));
                }
            };

            // ADR 0023: every `LlmRequestStarted` must be paired with a
            // terminal envelope. Emit `LlmResponseReceived` BEFORE any
            // post-response budget check so a token-budget breach at the
            // end of an iteration does not leave the `LlmRequestStarted`
            // dangling on the wire. The consumer's pairing logic can
            // then rely on the invariant unconditionally.
            let content_excerpt = self.excerpt_for_wire(&response.content);
            self.config
                .sink
                .record(Event::LlmResponseReceived {
                    run_id: run_id.to_string(),
                    node_id: node_id.to_string(),
                    finish_reason: response.finish_reason.clone(),
                    usage: response.usage.clone(),
                    content_excerpt,
                })
                .await;

            if let Some(usage) = &response.usage {
                total_tokens = total_tokens.saturating_add(u64::from(usage.total_tokens));
                if req.policy.max_total_tokens > 0 && total_tokens > req.policy.max_total_tokens {
                    return Err(AgentError::BudgetExceeded {
                        kind: BudgetKind::Tokens,
                    });
                }
            }

            // No tool calls → the model produced a final text answer.
            if response.tool_calls.is_empty() {
                // ADR 0025 §2: the `state` field is `None` when the
                // agent made no `SetState` calls. An empty buffer maps
                // to `None` so pipelines that never use the feature see
                // no shape change on `nodes.<id>`.
                let state = final_state(&state_buffer);
                return Ok(AgentOutput {
                    content: response.content,
                    finish_reason: response.finish_reason,
                    usage: response.usage,
                    iterations: iteration,
                    total_tokens,
                    state,
                });
            }

            // Record the assistant's tool-call turn so the next LLM
            // request carries the full causal chain.
            messages.push(OrnoChatMessage::ToolCalls {
                calls: response.tool_calls.clone(),
            });

            for tool_call in &response.tool_calls {
                tool_call_count = tool_call_count.saturating_add(1);
                if req.policy.max_tool_calls > 0 && tool_call_count > req.policy.max_tool_calls {
                    return Err(AgentError::BudgetExceeded {
                        kind: BudgetKind::ToolCalls,
                    });
                }

                let yaml_name = wire_to_yaml
                    .get(&tool_call.fn_name)
                    .cloned()
                    .unwrap_or_else(|| tool_call.fn_name.clone());

                // ADR 0006: subagent calls are routed through the
                // recursion-depth gate before dispatch. If the gate
                // fires, the child loop is never entered; the parent's
                // next LLM turn carries a denial string as the tool's
                // result so the model can adapt (ADR 0005 §3).
                let result_content = if yaml_name.starts_with(SUBAGENT_PREFIX) {
                    let child_depth = req.depth.saturating_add(1);
                    let child_agent = yaml_name
                        .strip_prefix(SUBAGENT_PREFIX)
                        .unwrap_or(&yaml_name)
                        .to_string();
                    if child_depth > req.policy.max_subagent_depth {
                        self.config
                            .sink
                            .record(Event::SubagentDepthExceeded {
                                run_id: run_id.to_string(),
                                parent_node_id: node_id.to_string(),
                                attempted_child_agent: child_agent.clone(),
                                depth_attempted: child_depth,
                                max_depth: req.policy.max_subagent_depth,
                            })
                            .await;
                        format!(
                            "denied: subagent `{child_agent}` would run at depth {child_depth}, \
                             exceeding max_subagent_depth={} (ADR 0006)",
                            req.policy.max_subagent_depth,
                        )
                    } else {
                        let handler = self.find_handler(&yaml_name).ok_or_else(|| {
                            AgentError::UnknownToolCalled {
                                name: tool_call.fn_name.clone(),
                            }
                        })?;
                        let inv = ToolInvocation {
                            run_id,
                            node_id,
                            call_id: &tool_call.call_id,
                            depth: req.depth,
                            state_handle: Some(state_handle),
                        };
                        self.check_policy_and_invoke(handler, tool_call, &req.policy, inv)
                            .await?
                    }
                } else {
                    let handler = self.find_handler(&yaml_name).ok_or_else(|| {
                        AgentError::UnknownToolCalled {
                            name: tool_call.fn_name.clone(),
                        }
                    })?;
                    let inv = ToolInvocation {
                        run_id,
                        node_id,
                        call_id: &tool_call.call_id,
                        depth: req.depth,
                        state_handle: Some(state_handle),
                    };
                    self.check_policy_and_invoke(handler, tool_call, &req.policy, inv)
                        .await?
                };

                let input_excerpt = self.excerpt_for_wire(
                    &serde_json::to_string(&tool_call.fn_arguments).unwrap_or_default(),
                );
                let output_excerpt = self.excerpt_for_wire(&result_content);
                self.config
                    .sink
                    .record(Event::ToolCallRecorded {
                        run_id: run_id.to_string(),
                        node_id: node_id.to_string(),
                        tool_name: tool_call.fn_name.clone(),
                        call_id: tool_call.call_id.clone(),
                        input_excerpt,
                        output_excerpt,
                    })
                    .await;

                messages.push(OrnoChatMessage::ToolResult {
                    call_id: tool_call.call_id.clone(),
                    content: result_content,
                });
            }
        }

        Err(AgentError::IterationLimitExceeded {
            max: req.policy.max_iterations,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::LlmError;
    use crate::events::InMemorySink;
    use crate::llm::{DummyTransport, LlmResponse, dummy::ScriptedTransport};
    use crate::pipeline::{AgentPolicy, OnParseError};
    use async_trait::async_trait;

    /// Transport stub that returns a caller-chosen `LlmError`. Lives in
    /// the test module because production code never wants a transport
    /// that always fails — its only purpose is to exercise the
    /// `LlmRequestFailed` emission path.
    struct FailingTransport(LlmError);

    impl FailingTransport {
        fn auth() -> Self {
            Self(LlmError::AuthFailed {
                provider: "openai".into(),
            })
        }
    }

    #[async_trait]
    impl LlmTransport for FailingTransport {
        async fn complete(&self, _req: LlmRequest) -> Result<LlmResponse, LlmError> {
            // Cloning by reconstruction since LlmError isn't Clone — the
            // stub holds a single error and the test calls it once.
            Err(match &self.0 {
                LlmError::AuthFailed { provider } => LlmError::AuthFailed {
                    provider: provider.clone(),
                },
                other => panic!("FailingTransport got an unsupported variant: {other:?}"),
            })
        }
    }

    fn policy() -> AgentPolicy {
        AgentPolicy {
            max_iterations: 1,
            max_total_tokens: 1000,
            max_tool_calls: 0,
            max_subagent_depth: 0,
            allow_mutations: false,
            allow_network: false,
            allow_context_writes: false,
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
            on_parse_error: OnParseError::Fail,
        }
    }

    fn request() -> AgentRequest {
        AgentRequest {
            agent_name: "greeter".into(),
            initial_prompt: "say hi".into(),
            system: None,
            provider: "openai".into(),
            model: "gpt-5".into(),
            policy: policy(),
            allowed_tools: Vec::new(),
            depth: 0,
        }
    }

    #[tokio::test]
    async fn emits_request_and_response_events_in_order() {
        let sink = Arc::new(InMemorySink::new());
        let agent = LoopAgent::with_defaults(Arc::new(DummyTransport), sink.clone());

        let out = agent
            .run("run_test", "n", request())
            .await
            .expect("dummy transport always succeeds");

        assert!(out.content.contains("[dummy]"));

        let events = sink.snapshot();
        let starts = events
            .iter()
            .enumerate()
            .find_map(|(i, e)| matches!(e.event, Event::LlmRequestStarted { .. }).then_some(i))
            .expect("LlmRequestStarted emitted");
        let recvs = events
            .iter()
            .enumerate()
            .find_map(|(i, e)| matches!(e.event, Event::LlmResponseReceived { .. }).then_some(i))
            .expect("LlmResponseReceived emitted");
        assert!(
            starts < recvs,
            "LlmRequestStarted must precede LlmResponseReceived",
        );

        // ADR 0024: the excerpt fields must round-trip the rendered
        // prompt and the model response, not be silently empty. A
        // consumer pairing the two envelopes must see what went in and
        // what came back without folding `NodeResponse.output`.
        if let Event::LlmRequestStarted {
            provider,
            model,
            prompt_excerpt,
            system_excerpt,
            ..
        } = &events[starts].event
        {
            assert_eq!(provider, "openai");
            assert_eq!(model, "gpt-5");
            assert!(
                prompt_excerpt.contains("say hi"),
                "prompt_excerpt must carry the rendered prompt: {prompt_excerpt:?}",
            );
            assert!(
                system_excerpt.is_none(),
                "baseline request has no system prompt — excerpt must be None, got {system_excerpt:?}",
            );
        } else {
            panic!("LlmRequestStarted event not destructurable");
        }

        if let Event::LlmResponseReceived {
            content_excerpt, ..
        } = &events[recvs].event
        {
            assert!(
                content_excerpt.contains("[dummy]"),
                "content_excerpt must carry the transport response: {content_excerpt:?}",
            );
        } else {
            panic!("LlmResponseReceived event not destructurable");
        }
    }

    #[tokio::test]
    async fn unknown_tool_in_allowed_list_is_rejected_before_any_call() {
        // Phase 5 cross-checks `allowed_tools` against registered
        // handlers at the top of `run`. A name absent from the handler
        // set terminates with `UnknownToolCalled` before the LLM is
        // even contacted.
        let sink = Arc::new(InMemorySink::new());
        let agent = LoopAgent::with_defaults(Arc::new(DummyTransport), sink);
        let mut req = request();
        req.allowed_tools = vec!["UnregisteredTool".into()];

        let err = agent
            .run("run_test", "n", req)
            .await
            .expect_err("unknown tool must be rejected");
        match err {
            AgentError::UnknownToolCalled { name } => assert_eq!(name, "UnregisteredTool"),
            other => panic!("expected UnknownToolCalled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn zero_max_iterations_rejected_as_invalid_policy() {
        let sink = Arc::new(InMemorySink::new());
        let agent = LoopAgent::with_defaults(Arc::new(DummyTransport), sink);
        let mut req = request();
        req.policy.max_iterations = 0;

        let err = agent
            .run("run_test", "n", req)
            .await
            .expect_err("zero max_iterations must be rejected");
        match err {
            AgentError::InvalidPolicy(msg) => assert!(msg.contains("max_iterations")),
            other => panic!("expected InvalidPolicy, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_error_emits_llm_request_failed_before_propagating() {
        // The Phase 3 invariant: a transport failure leaves a typed
        // `LlmRequestFailed` on the wire next to the dangling
        // `LlmRequestStarted`. Without this event, a downstream consumer
        // can only see the opaque error chain and cannot tell
        // `auth_failed` from a stray parse error.
        let sink = Arc::new(InMemorySink::new());
        let agent = LoopAgent::with_defaults(Arc::new(FailingTransport::auth()), sink.clone());

        let err = agent
            .run("run_test", "n", request())
            .await
            .expect_err("transport failure must propagate as AgentError");
        match err {
            AgentError::Llm(LlmError::AuthFailed { provider }) => assert_eq!(provider, "openai"),
            other => panic!("expected AgentError::Llm(AuthFailed), got {other:?}"),
        }

        let events = sink.snapshot();
        let mut started_idx = None;
        let mut failed_idx = None;
        for (i, env) in events.iter().enumerate() {
            match &env.event {
                Event::LlmRequestStarted { .. } => started_idx = Some(i),
                Event::LlmRequestFailed { failure, .. } => {
                    assert!(
                        matches!(failure, LlmFailure::AuthFailed),
                        "expected AuthFailed classification, got {failure:?}",
                    );
                    failed_idx = Some(i);
                }
                Event::LlmResponseReceived { .. } => {
                    panic!("LlmResponseReceived must not fire on a transport failure");
                }
                _ => {}
            }
        }
        let started = started_idx.expect("LlmRequestStarted must still fire");
        let failed = failed_idx.expect("LlmRequestFailed must fire on transport error");
        assert!(
            started < failed,
            "LlmRequestFailed must follow LlmRequestStarted in stream order",
        );
    }

    #[tokio::test]
    async fn max_total_tokens_zero_sends_no_cap() {
        // When max_total_tokens is 0 (the default), the impl must NOT
        // send max_tokens: Some(0) to the transport. DummyTransport
        // always succeeds; if the impl panicked or sent Some(0) the
        // test would need a real provider to observe the bad behavior —
        // but at minimum we verify the path completes without error.
        let sink = Arc::new(InMemorySink::new());
        let agent = LoopAgent::with_defaults(Arc::new(DummyTransport), sink);
        let mut req = request();
        req.policy.max_total_tokens = 0;

        agent
            .run("run_test", "n", req)
            .await
            .expect("zero max_total_tokens must not error");
    }

    #[tokio::test]
    async fn system_excerpt_present_when_agent_config_declared_a_system_prompt() {
        // The sibling of the baseline test: when the agent config
        // carries a `system:` block, its redacted excerpt must reach
        // the wire so a consumer pairs request intent with the
        // behavioral contract the operator set.
        let sink = Arc::new(InMemorySink::new());
        let agent = LoopAgent::with_defaults(Arc::new(DummyTransport), sink.clone());
        let mut req = request();
        req.system = Some("You are a terse assistant.".to_string());

        agent
            .run("run_test", "n", req)
            .await
            .expect("dummy transport always succeeds");

        let events = sink.snapshot();
        let got = events.iter().find_map(|e| match &e.event {
            Event::LlmRequestStarted { system_excerpt, .. } => Some(system_excerpt.clone()),
            _ => None,
        });
        assert_eq!(
            got,
            Some(Some("You are a terse assistant.".to_string())),
            "system_excerpt must round-trip the configured system prompt",
        );
    }

    #[tokio::test]
    async fn prompt_excerpt_redacts_known_secret_values() {
        // ADR 0020 + ADR 0024: the agent shares the engine's `Redactor`
        // so a prompt that embedded a rendered `secrets.*` value never
        // reaches the sink in cleartext. Without this, enabling
        // prompt excerpts would regress the secrets-namespace contract.
        use std::collections::BTreeMap;
        let mut secret_map = BTreeMap::new();
        secret_map.insert(
            "OPENROUTER_API_KEY".to_string(),
            "sk-very-secret-12345".to_string(),
        );
        let redactor = Arc::new(crate::events::Redactor::new(&secret_map));

        let sink = Arc::new(InMemorySink::new());
        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(DummyTransport),
            sink: sink.clone(),
            redactor,
            body_excerpt_max_bytes: 2048,
            tools: Vec::new(),
        });
        let mut req = request();
        req.initial_prompt = "Use key sk-very-secret-12345 to authorize this request.".to_string();

        agent
            .run("run_test", "n", req)
            .await
            .expect("dummy transport always succeeds");

        let events = sink.snapshot();
        let prompt = events
            .iter()
            .find_map(|e| match &e.event {
                Event::LlmRequestStarted { prompt_excerpt, .. } => Some(prompt_excerpt.clone()),
                _ => None,
            })
            .expect("LlmRequestStarted must be present");
        assert!(
            !prompt.contains("sk-very-secret-12345"),
            "raw secret leaked into prompt_excerpt: {prompt:?}",
        );
        assert!(
            prompt.contains("***"),
            "redactor must substitute `***` for the secret value: {prompt:?}",
        );
    }

    #[tokio::test]
    async fn prompt_excerpt_truncates_at_configured_cap() {
        // A multi-KB rendered prompt must not flood the event stream.
        // Same truncation policy as LlmFailure::ApiError.body_excerpt —
        // head bytes win (the operator instruction sits at the front),
        // ellipsis marker appended when truncation happened.
        let sink = Arc::new(InMemorySink::new());
        // Explicit 32-byte cap makes truncation observable without
        // needing a megabyte prompt.
        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(DummyTransport),
            sink: sink.clone(),
            redactor: Arc::new(crate::events::Redactor::default()),
            body_excerpt_max_bytes: 32,
            tools: Vec::new(),
        });
        let mut req = request();
        req.initial_prompt = "A".repeat(1000);

        agent
            .run("run_test", "n", req)
            .await
            .expect("dummy transport always succeeds");

        let events = sink.snapshot();
        let prompt = events
            .iter()
            .find_map(|e| match &e.event {
                Event::LlmRequestStarted { prompt_excerpt, .. } => Some(prompt_excerpt.clone()),
                _ => None,
            })
            .expect("LlmRequestStarted must be present");
        assert!(
            prompt.ends_with('…'),
            "truncation marker missing from long prompt: {prompt:?}",
        );
        assert!(
            prompt.len() <= 32 + '…'.len_utf8(),
            "excerpt exceeds cap+marker ({} bytes): {prompt:?}",
            prompt.len(),
        );
        assert!(
            prompt.starts_with("AAAA"),
            "excerpt must keep the head of the prompt: {prompt:?}",
        );
    }

    #[tokio::test]
    async fn single_iteration_with_text_response_succeeds() {
        // `DummyTransport` returns a plain-text response with no tool
        // calls, so the loop exits on the first iteration with the
        // model's answer — no iteration-limit breach even at
        // `max_iterations = 1`.
        let sink = Arc::new(InMemorySink::new());
        let agent = LoopAgent::with_defaults(Arc::new(DummyTransport), sink);
        let mut req = request();
        req.policy.max_iterations = 1;

        agent
            .run("run_test", "n", req)
            .await
            .expect("single iteration with text response should succeed");
    }

    /// Minimal `ToolHandler` that returns a canned output for any call.
    /// Used to exercise the tool-dispatch path without real I/O.
    struct EchoTool {
        effect: ToolEffect,
        output: &'static str,
        name: &'static str,
    }

    impl EchoTool {
        fn new(effect: ToolEffect, output: &'static str) -> Self {
            Self {
                effect,
                output,
                name: "EchoTool",
            }
        }
    }

    #[async_trait]
    impl ToolHandler for EchoTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "Returns a fixed string."
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn effect(&self) -> ToolEffect {
            self.effect
        }
        async fn invoke(
            &self,
            _inv: ToolInvocation<'_>,
            _args: serde_json::Value,
        ) -> Result<String, ToolError> {
            Ok(self.output.to_string())
        }
    }

    #[tokio::test]
    async fn iteration_limit_exceeded_when_model_keeps_calling_tools() {
        // ADR 0005 §1: bounded iteration. A transport that never stops
        // emitting tool-call turns must terminate the loop at
        // `max_iterations`, not spin forever.
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(EchoTool::new(ToolEffect::ReadOnly, "done"));

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response("c1", "EchoTool", serde_json::json!({})),
            ScriptedTransport::tool_call_response("c2", "EchoTool", serde_json::json!({})),
            ScriptedTransport::tool_call_response("c3", "EchoTool", serde_json::json!({})),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink,
            redactor: Arc::new(crate::events::Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 2;
        req.allowed_tools = vec!["EchoTool".into()];

        let err = agent
            .run("run_test", "n", req)
            .await
            .expect_err("must exceed iteration limit");
        match err {
            AgentError::IterationLimitExceeded { max } => assert_eq!(max, 2),
            other => panic!("expected IterationLimitExceeded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_call_budget_exceeded() {
        // ADR 0005 §4: bounded resources. The second tool call in a run
        // with `max_tool_calls = 1` must terminate with the typed
        // `BudgetKind::ToolCalls` variant so downstream alerting can
        // distinguish it from a token breach.
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(EchoTool::new(ToolEffect::ReadOnly, "ok"));

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response("c1", "EchoTool", serde_json::json!({})),
            ScriptedTransport::tool_call_response("c2", "EchoTool", serde_json::json!({})),
            ScriptedTransport::text_response("done"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink,
            redactor: Arc::new(crate::events::Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 5;
        req.policy.max_tool_calls = 1;
        req.allowed_tools = vec!["EchoTool".into()];

        let err = agent
            .run("run_test", "n", req)
            .await
            .expect_err("must exceed tool call budget");
        match err {
            AgentError::BudgetExceeded { kind } => assert!(matches!(kind, BudgetKind::ToolCalls)),
            other => panic!("expected BudgetExceeded(ToolCalls), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_calling_unknown_tool_terminates_with_unknown_tool_called() {
        // ADR 0005 §2: bounded tool surface. A tool-call turn naming a
        // handler the agent was never given must terminate with
        // `UnknownToolCalled` — not silently drop, not retry, not ask
        // the model to pick again.
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(EchoTool::new(ToolEffect::ReadOnly, "ok"));

        let transport = ScriptedTransport::new(vec![ScriptedTransport::tool_call_response(
            "c1",
            "HackerTool",
            serde_json::json!({}),
        )]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink,
            redactor: Arc::new(crate::events::Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.allowed_tools = vec!["EchoTool".into()];

        let err = agent
            .run("run_test", "n", req)
            .await
            .expect_err("calling unknown tool must terminate");
        match err {
            AgentError::UnknownToolCalled { name } => assert_eq!(name, "HackerTool"),
            other => panic!("expected UnknownToolCalled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn context_writes_denied_feeds_back_as_tool_result_and_loop_continues() {
        // ADR 0025 §3: `ContextSelf` tools are gated by
        // `allow_context_writes`. Like `Mutations` / `Network` denials,
        // a refusal is non-terminal — the denial string reaches the
        // model as a `ToolResult` and the loop continues. A SetState
        // call with the flag off must not mutate the node-state buffer
        // and must not terminate the loop.
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(EchoTool::new(ToolEffect::ContextSelf, "should not run"));

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response("c1", "EchoTool", serde_json::json!({})),
            ScriptedTransport::text_response("noted — state writes disabled"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(crate::events::Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.allow_context_writes = false;
        req.allowed_tools = vec!["EchoTool".into()];

        let out = agent
            .run("run_test", "n", req)
            .await
            .expect("context-writes denial must feed back, not terminate");

        assert!(
            out.content.contains("state writes disabled"),
            "final output should carry the model's acknowledgment: {:?}",
            out.content,
        );
        // The denial string must be routed as the tool's `ToolResult`
        // so a downstream pair of `ToolCallRecorded` + next
        // `LlmRequestStarted` shows the gate fired.
        let events = sink.snapshot();
        let recorded = events
            .iter()
            .find(|e| matches!(e.event, Event::ToolCallRecorded { .. }))
            .expect("ToolCallRecorded must still fire for a gated ContextSelf call");
        if let Event::ToolCallRecorded { output_excerpt, .. } = &recorded.event {
            assert!(
                output_excerpt.contains("allow_context_writes=false"),
                "denial excerpt must name the gate: {output_excerpt:?}",
            );
        }
        // ADR 0025 §2: because no SetState write landed, the buffer stays
        // empty and `AgentOutput.state` collapses to `None`.
        assert!(
            out.state.is_none(),
            "no SetState call landed, state must be None: {:?}",
            out.state,
        );
    }

    #[tokio::test]
    async fn mutations_denied_feeds_back_as_tool_result_and_loop_continues() {
        // ADR 0005 §3: bounded effects via the feed-back mechanism. A
        // denied mutation is *not* a terminal error — the denial string
        // reaches the next LLM turn as a `ToolResult` and the model
        // adapts. The loop only terminates on iteration limit, budget,
        // unknown tool, or a final text turn.
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(EchoTool::new(ToolEffect::Mutations, "mutation done"));

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response("c1", "EchoTool", serde_json::json!({})),
            ScriptedTransport::text_response("understood, I cannot mutate"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink,
            redactor: Arc::new(crate::events::Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.allow_mutations = false;
        req.allowed_tools = vec!["EchoTool".into()];

        let out = agent
            .run("run_test", "n", req)
            .await
            .expect("policy denial must not terminate the loop — it feeds back to the model");

        assert!(
            out.content.contains("cannot mutate"),
            "final output should carry the model's acknowledgment: {:?}",
            out.content,
        );
    }

    #[tokio::test]
    async fn tool_dispatch_success_feeds_result_to_next_llm_turn() {
        // Happy-path companion to the strictness tests: model calls a
        // tool, the result reaches the next LLM turn, the model emits a
        // text response, and the loop exits with `finish_reason: stop`.
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(EchoTool::new(ToolEffect::ReadOnly, "file contents here"));

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response("c1", "EchoTool", serde_json::json!({})),
            ScriptedTransport::text_response("I read the file successfully"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink,
            redactor: Arc::new(crate::events::Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.allowed_tools = vec!["EchoTool".into()];

        let out = agent
            .run("run_test", "n", req)
            .await
            .expect("tool dispatch succeeds");
        assert!(
            out.content.contains("successfully"),
            "model's final response should be the text turn: {:?}",
            out.content,
        );
        assert_eq!(
            out.finish_reason.as_deref(),
            Some("stop"),
            "finish_reason should be stop for a completed text turn",
        );
    }

    #[tokio::test]
    async fn set_state_call_surfaces_in_agent_output_state() {
        // ADR 0025 end-to-end check: a `ContextSelf` tool that writes
        // through the per-call `state_handle` must appear in the
        // returned `AgentOutput.state`. The flag is on, the handler is
        // the real `SetStateHandler`, and the transport scripts one
        // SetState call followed by a text turn so the loop exits on
        // iteration two with the buffer populated.
        use crate::tool::SetStateHandler;

        let sink = Arc::new(InMemorySink::new());
        let redactor = Arc::new(crate::events::Redactor::default());
        let state_tool = Arc::new(SetStateHandler::new(redactor.clone(), 2048));

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response(
                "c1",
                "SetState",
                serde_json::json!({ "key": "plan", "value": { "status": "ready" } }),
            ),
            ScriptedTransport::text_response("wrote plan"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink,
            redactor,
            body_excerpt_max_bytes: 2048,
            tools: vec![state_tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.allow_context_writes = true;
        req.allowed_tools = vec!["SetState".into()];

        let out = agent
            .run("run_test", "n", req)
            .await
            .expect("SetState dispatch succeeds");

        let state = out
            .state
            .expect("state must be present after a SetState call");
        assert_eq!(state["plan"]["status"], "ready");
    }

    /// Dotted-name handler used to exercise the wire-name translation
    /// path without a full `SubagentHandler` dispatch. Returns a fixed
    /// string — the assertion is that the LLM's wire-form tool call
    /// (`subagent_child`) routes back to this handler whose YAML name
    /// contains a dot.
    struct DottedEchoTool;

    #[async_trait]
    impl ToolHandler for DottedEchoTool {
        fn name(&self) -> &str {
            "subagent.child"
        }
        fn description(&self) -> &str {
            "Dotted-name echo for wire translation."
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn effect(&self) -> ToolEffect {
            ToolEffect::ReadOnly
        }
        async fn invoke(
            &self,
            _inv: ToolInvocation<'_>,
            _args: serde_json::Value,
        ) -> Result<String, ToolError> {
            Ok("dotted ok".to_string())
        }
    }

    #[tokio::test]
    async fn subagent_depth_gate_denies_when_child_depth_exceeds_max_and_emits_event() {
        // ADR 0006: at depth N with `max_subagent_depth = 0`, any
        // subagent call would run at depth 1 which is > 0, so the gate
        // must fire. The child is never invoked; the parent receives a
        // denial string and an observability event appears on the wire.
        let sink = Arc::new(InMemorySink::new());
        let dotted = Arc::new(DottedEchoTool);

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response("c1", "subagent_child", serde_json::json!({})),
            ScriptedTransport::text_response("acknowledging denial"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(crate::events::Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![dotted],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.max_subagent_depth = 0;
        req.allowed_tools = vec!["subagent.child".into()];

        let out = agent
            .run("run_test", "n", req)
            .await
            .expect("depth-gate denial is non-terminal");
        assert!(
            out.content.contains("acknowledging denial"),
            "parent should continue after denial: {:?}",
            out.content,
        );

        let events = sink.snapshot();
        let depth_exceeded = events
            .iter()
            .find(|e| matches!(e.event, Event::SubagentDepthExceeded { .. }))
            .expect("SubagentDepthExceeded must fire on depth overflow");
        if let Event::SubagentDepthExceeded {
            attempted_child_agent,
            depth_attempted,
            max_depth,
            ..
        } = &depth_exceeded.event
        {
            assert_eq!(attempted_child_agent, "child");
            assert_eq!(*depth_attempted, 1);
            assert_eq!(*max_depth, 0);
        }

        // Denial reaches the model as a tool-result string, so the
        // ToolCallRecorded envelope carries it verbatim — that's the
        // ADR 0005 §3 feed-back contract applied to the depth case.
        let recorded = events
            .iter()
            .find(|e| matches!(e.event, Event::ToolCallRecorded { .. }))
            .expect("ToolCallRecorded must still fire for a denied subagent call");
        if let Event::ToolCallRecorded { output_excerpt, .. } = &recorded.event {
            assert!(
                output_excerpt.contains("exceeding max_subagent_depth"),
                "denial excerpt should name the gate: {output_excerpt:?}",
            );
        }
    }

    #[tokio::test]
    async fn token_budget_breach_still_emits_llm_response_received() {
        // ADR 0023 pairing invariant: every `LlmRequestStarted` must be
        // paired with `LlmResponseReceived` (or `LlmRequestFailed`) on
        // the wire. Before the fix the token-budget check ran BEFORE
        // the response-received emission, so a breach at the end of an
        // iteration left a dangling `LlmRequestStarted` and the
        // operator saw only "agent exceeded budget" with no record of
        // the final model turn. This regression guards the ordering.
        let sink = Arc::new(InMemorySink::new());
        // `text_response` reports 15 total_tokens; cap of 10 trips on
        // the very first response.
        let transport = ScriptedTransport::new(vec![ScriptedTransport::text_response("over cap")]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(crate::events::Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: Vec::new(),
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.max_total_tokens = 10;

        let err = agent
            .run("run_test", "n", req)
            .await
            .expect_err("must trip the token budget");
        match err {
            AgentError::BudgetExceeded { kind } => assert!(matches!(kind, BudgetKind::Tokens)),
            other => panic!("expected BudgetExceeded(Tokens), got {other:?}"),
        }

        let events = sink.snapshot();
        let started_idx = events
            .iter()
            .position(|e| matches!(e.event, Event::LlmRequestStarted { .. }))
            .expect("LlmRequestStarted must fire");
        let received_idx = events
            .iter()
            .position(|e| matches!(e.event, Event::LlmResponseReceived { .. }))
            .expect(
                "LlmResponseReceived must fire even on budget breach — pairing invariant (ADR 0023)",
            );
        assert!(
            started_idx < received_idx,
            "LlmRequestStarted must precede LlmResponseReceived on the wire",
        );
    }

    #[tokio::test]
    async fn dotted_tool_name_translates_to_underscore_on_wire_and_back() {
        // The LLM sees `subagent_child` (underscore) because providers
        // reject dots in function names; when the model's tool call
        // comes back with that wire form, the loop must reverse the
        // translation and dispatch to the `subagent.child` handler.
        let sink = Arc::new(InMemorySink::new());
        let dotted = Arc::new(DottedEchoTool);

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response("c1", "subagent_child", serde_json::json!({})),
            ScriptedTransport::text_response("done"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(crate::events::Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![dotted],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        // Raise the subagent-depth budget so the gate does NOT fire —
        // this test is about name translation, not about the gate.
        req.policy.max_subagent_depth = 1;
        req.allowed_tools = vec!["subagent.child".into()];

        let out = agent
            .run("run_test", "n", req)
            .await
            .expect("dotted-name dispatch should succeed after wire translation");
        assert!(out.content.contains("done"));

        // The dotted handler's canned output ("dotted ok") must reach
        // the next LLM turn as the tool result — only possible if the
        // wire-form tool call resolved to the dotted handler.
        let events = sink.snapshot();
        let tool_call = events
            .iter()
            .find(|e| matches!(e.event, Event::ToolCallRecorded { .. }))
            .expect("ToolCallRecorded must fire");
        if let Event::ToolCallRecorded { output_excerpt, .. } = &tool_call.event {
            assert!(
                output_excerpt.contains("dotted ok"),
                "handler output must be the dotted echo's fixed string: {output_excerpt:?}",
            );
        }
    }
}
