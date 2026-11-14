//! [`Agent`] trait implementation for [`LoopAgent`].
//!
//! The main iteration loop sits here so the declarative `Agent`
//! contract is readable end-to-end without the policy and retry
//! helpers from `policy.rs` in the middle. Enforces the five
//! strictness dimensions and emits the full paired event stream.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Map, Value};
use tracing::{instrument, warn};

use crate::agent::{Agent, AgentOutput, AgentRequest, BudgetScope};
use crate::error::AgentError;
use crate::events::BudgetKind;
use crate::events::redactor::Keep;
use crate::events::{Event, LlmFailure};
use crate::llm::{LlmRequest, OrnoChatMessage, OrnoChatTool};
use crate::tool::net_guard::DomainRules;
use crate::tool::{StateHandle, ToolInvocation};

use super::history::History;
use super::{LoopAgent, SUBAGENT_PREFIX, wire_name_from_yaml};

/// Default cap on the per-call tool output bytes appended to the
/// conversation history. Mirrors the `AgentPolicy.max_tool_output_bytes`
/// field — when the operator omits the policy field, this constant
/// applies. Sized to comfortably hold a single Read of a moderate file
/// or a `WebFetch` of a typical HTML page; a runaway tool that returns a
/// gigabyte of output gets truncated to this cap before it reaches the
/// LLM transport.
const DEFAULT_MAX_TOOL_OUTPUT_BYTES: usize = 256 * 1024;

/// Snapshot the per-node state buffer for `AgentOutput.state`. Returns
/// `None` when no `SetState` call landed — keeps the wire shape of
/// `nodes.<id>` unchanged for pipelines that never opt into the feature.
/// A poisoned mutex also reports `None` since the offending panic has
/// already terminated the relevant tool call; a partial buffer would be
/// worse than no state.
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

        // Filesystem-tool root check: if any registered handler in
        // `allowed_tools` declares `requires_jail() == true`, `policy.roots`
        // must be non-empty. Without a root the path-jail is a no-op and an
        // LLM call could read or overwrite anywhere on the host. The check is
        // data-driven so a future filesystem handler automatically opts in by
        // overriding `requires_jail` (BS3).
        let uses_jail_tool = req
            .allowed_tools
            .iter()
            .any(|t| self.find_handler(t).is_some_and(|h| h.requires_jail()));
        if uses_jail_tool && req.policy.roots.is_empty() {
            return Err(AgentError::InvalidPolicy(
                "policy.roots must be non-empty when allowed_tools contains \
                 a handler that requires a root jail (Read, Write, Edit, or \
                 any handler overriding requires_jail())"
                    .to_string(),
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

        // Per-node state buffer for the `SetState` builtin. One object
        // per `run()` call, visible only to tool dispatches inside this
        // loop. Subagent recursion calls `run()` again and gets a fresh
        // buffer — child state never leaks into the parent.
        let state_buffer: Mutex<Value> = Mutex::new(Value::Object(Map::new()));
        let state_handle = StateHandle::new(&state_buffer);

        // Build the tool definitions the LLM sees on each request —
        // intersection of `allowed_tools` and the registered handler
        // set. Empty vector when the agent declared no tools. The
        // `Arc` wrapper means each iteration's `LlmRequest` clones the
        // pointer instead of the underlying schema vector.
        let declared_tools: Arc<Vec<OrnoChatTool>> = Arc::new(
            self.config
                .tools
                .iter()
                .filter(|h| req.allowed_tools.iter().any(|n| n == h.name()))
                .map(|h| OrnoChatTool {
                    name: wire_name_from_yaml(h.name()),
                    description: h.description().to_string(),
                    schema: h.schema(),
                })
                .collect::<Vec<_>>(),
        );

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
                (wire_name_from_yaml(&yaml), yaml)
            })
            .collect();

        let prompt_excerpt = self.excerpt_for_wire(&req.initial_prompt);
        let system_excerpt = req.system.as_deref().map(|s| self.excerpt_for_wire(s));

        // Pre-compute the per-loop immutable string copies of the
        // request's `Arc<str>` fields. Each iteration clones these
        // owned `String`s into the outgoing `LlmRequest`/event
        // envelopes; doing the conversion once per `run` avoids
        // re-paying the `Arc<str>::to_string` cost on every iteration.
        let provider_str = req.provider.to_string();
        let model_str = req.model.to_string();
        let prompt_str = req.initial_prompt.to_string();
        let system_str = req.system.as_deref().map(str::to_string);

        // Assistant tool-call turns and their paired `ToolResult`s; the
        // initial user turn rides in `LlmRequest.prompt`.
        let mut history = History::new();
        // Linked to the parent's scope so child and grandchild spend
        // count against every ancestor's token and tool-call limits.
        let budget = BudgetScope::new(
            req.policy.max_total_tokens,
            req.policy.max_tool_calls,
            req.parent_budget.clone(),
        );

        for iteration in 0..req.policy.max_iterations {
            self.config
                .sink
                .record(Event::AgentIterationStarted {
                    run_id: run_id.to_string(),
                    node_id: node_id.to_string(),
                    // Hard-coded because only `kind: agent` nodes
                    // drive a `LoopAgent` iteration; if a future shell
                    // executor learns to loop, this site lifts the
                    // value from the caller. See ADR 0009 for why
                    // there is no peer kind today.
                    node_kind: "agent".to_string(),
                    iteration,
                })
                .await;

            // Per-iteration parse-retry budget. `call_id`s are issued
            // per LLM turn and unique per iteration in practice, so a
            // fresh set each iteration narrows the contract: one parse
            // retry per `call_id` within the iteration that produced it.
            let mut retried_parse_errors: HashSet<String> = HashSet::new();

            // Ask for what is left on the tightest limit up the chain. A
            // spent budget fails here, before an uncapped, billable call.
            let max_tokens = match budget.tokens_remaining() {
                None => None,
                Some(0) => {
                    return Err(AgentError::BudgetExceeded {
                        kind: BudgetKind::Tokens,
                    });
                },
                Some(remaining) => Some(u32::try_from(remaining).unwrap_or(u32::MAX)),
            };

            let llm_req = LlmRequest {
                provider: provider_str.clone(),
                model: model_str.clone(),
                prompt: prompt_str.clone(),
                system: system_str.clone(),
                temperature: None,
                max_tokens,
                messages: history.snapshot(),
                tools: Arc::clone(&declared_tools),
            };

            self.config
                .sink
                .record(Event::LlmRequestStarted {
                    run_id: run_id.to_string(),
                    node_id: node_id.to_string(),
                    provider: provider_str.clone(),
                    model: model_str.clone(),
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
                            provider: provider_str.clone(),
                            model: model_str.clone(),
                            failure,
                        })
                        .await;
                    return Err(AgentError::from(err));
                },
            };

            // Pairing invariant: every `LlmRequestStarted` must be
            // paired with a terminal envelope. Emit `LlmResponseReceived`
            // BEFORE any post-response budget check so a token-budget
            // breach at the end of an iteration does not leave the
            // `LlmRequestStarted` dangling on the wire. The consumer's
            // pairing logic can then rely on the invariant unconditionally.
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
                budget
                    .charge_tokens(u64::from(usage.total_tokens))
                    .map_err(|kind| AgentError::BudgetExceeded { kind })?;
            } else if req.policy.max_total_tokens > 0 {
                // The provider returned no `usage` block. Token-budget
                // enforcement is intrinsically per-response — without
                // usage we cannot bump `total_tokens`, so the configured
                // cap is effectively degraded for this response. Warn so
                // the operator can spot transports/models that
                // systematically omit usage and add a wrapper that
                // estimates instead. The dimension contract still holds
                // for any iteration where usage IS reported.
                warn!(
                    policy.max_total_tokens = req.policy.max_total_tokens,
                    llm.provider = %req.provider,
                    llm.model = %req.model,
                    "provider returned no usage; token budget enforcement is degraded for this response",
                );
            }

            // No tool calls → the model produced a final text answer.
            if response.tool_calls.is_empty() {
                // The `state` field is `None` when the agent made no
                // `SetState` calls. An empty buffer maps to `None` so
                // pipelines that never use the feature see no shape
                // change on `nodes.<id>`.
                let state = final_state(&state_buffer);
                return Ok(AgentOutput {
                    content: response.content,
                    finish_reason: response.finish_reason,
                    usage: response.usage,
                    iterations: iteration,
                    total_tokens: budget.tokens_used(),
                    state,
                });
            }

            history.push(OrnoChatMessage::ToolCalls {
                calls: response.tool_calls.clone(),
            });

            for tool_call in &response.tool_calls {
                budget
                    .charge_tool_call()
                    .map_err(|kind| AgentError::BudgetExceeded { kind })?;

                let yaml_name = wire_to_yaml
                    .get(&tool_call.fn_name)
                    .cloned()
                    .unwrap_or_else(|| tool_call.fn_name.clone());

                // `wire_to_yaml` is built only from allowed tools, but the
                // `.unwrap_or_else` fallback lets an unallowed wire name
                // reach `find_handler`, which searches ALL registered handlers.
                // Enforce the allowlist boundary before any dispatch.
                if !req.allowed_tools.iter().any(|n| n == &yaml_name) {
                    return Err(AgentError::UnknownToolCalled {
                        name: tool_call.fn_name.clone(),
                    });
                }

                let make_inv = || ToolInvocation {
                    run_id,
                    node_id,
                    call_id: &tool_call.call_id,
                    depth: req.depth,
                    state_handle: Some(state_handle),
                    budget: Some(Arc::clone(&budget)),
                    render_scope: req.render_scope.as_ref(),
                    roots: &req.policy.roots,
                    domains: DomainRules {
                        allowed: &req.policy.allowed_domains,
                        blocked: &req.policy.blocked_domains,
                    },
                };

                // Subagent calls are routed through the recursion-depth
                // gate before dispatch. If the gate fires, the child
                // loop is never entered; the parent's next LLM turn
                // carries a denial string as the tool's result so the
                // model can adapt.
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
                             exceeding max_subagent_depth={}",
                            req.policy.max_subagent_depth,
                        )
                    } else {
                        let handler = self.find_handler(&yaml_name).ok_or_else(|| {
                            AgentError::UnknownToolCalled {
                                name: tool_call.fn_name.clone(),
                            }
                        })?;
                        let inv = make_inv();
                        self.invoke_with_parse_retry(
                            handler,
                            tool_call,
                            &req.policy,
                            inv,
                            &mut retried_parse_errors,
                            &wire_to_yaml,
                        )
                        .await?
                    }
                } else {
                    let handler = self.find_handler(&yaml_name).ok_or_else(|| {
                        AgentError::UnknownToolCalled {
                            name: tool_call.fn_name.clone(),
                        }
                    })?;
                    let inv = make_inv();
                    self.invoke_with_parse_retry(
                        handler,
                        tool_call,
                        &req.policy,
                        inv,
                        &mut retried_parse_errors,
                        &wire_to_yaml,
                    )
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

                // Truncate the per-call tool output that flows back into
                // the conversation history so a single runaway tool call
                // cannot drag the next LLM request past the provider's
                // prompt-size ceiling. The handler's wire-event
                // `output_excerpt` above is already capped separately;
                // this cap governs the conversation-history payload only.
                let cap = req
                    .policy
                    .max_tool_output_bytes
                    .unwrap_or(DEFAULT_MAX_TOOL_OUTPUT_BYTES);
                // Redact before bounding so a secret straddling the cap is
                // still matched; the history is replayed on the next request.
                let redacted =
                    self.config
                        .redactor
                        .redact_bounded(&result_content, cap, Keep::Head);
                history.push(OrnoChatMessage::ToolResult {
                    call_id: tool_call.call_id.clone(),
                    content: redacted,
                });
            }
            // Once per turn, after every result is in, so a multi-call turn
            // is never cut between a call and its result.
            if let Some(cap) = req.policy.max_message_history_bytes {
                history.enforce_cap(cap);
            }
        }

        Err(AgentError::IterationLimitExceeded {
            max: req.policy.max_iterations,
        })
    }
}
