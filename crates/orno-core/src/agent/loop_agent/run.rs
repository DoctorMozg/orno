//! [`Agent`] trait implementation for [`LoopAgent`].
//!
//! The main iteration loop sits here so the declarative `Agent`
//! contract is readable end-to-end without the policy and retry
//! helpers from `policy.rs` in the middle. Enforces the five
//! strictness dimensions of ADR 0005 and emits the full paired
//! event stream described in ADR 0023 / 0024.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Map, Value};
use tracing::instrument;

use crate::agent::{Agent, AgentOutput, AgentRequest};
use crate::error::AgentError;
use crate::events::{BudgetKind, Event, LlmFailure};
use crate::llm::{LlmRequest, OrnoChatMessage, OrnoChatTool};
use crate::tool::{StateHandle, ToolInvocation};

use super::{LoopAgent, SUBAGENT_PREFIX};

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

            // Per-iteration parse-retry budget. `call_id`s are issued
            // per LLM turn and unique per iteration in practice, so a
            // fresh set each iteration narrows the contract: one parse
            // retry per `call_id` within the iteration that produced it.
            let mut retried_parse_errors: HashSet<String> = HashSet::new();

            // Each request asks for the budget remaining AFTER previous
            // iterations' usage. `total_tokens` is updated below from
            // the response's `Usage`; the saturating subtraction guards
            // against the model overshooting the prior cap.
            let max_tokens = if req.policy.max_total_tokens == 0 {
                None
            } else {
                let remaining = req.policy.max_total_tokens.saturating_sub(total_tokens);
                (remaining > 0).then(|| u32::try_from(remaining).unwrap_or(u32::MAX))
            };

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
                },
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
                    let inv = ToolInvocation {
                        run_id,
                        node_id,
                        call_id: &tool_call.call_id,
                        depth: req.depth,
                        state_handle: Some(state_handle),
                    };
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
