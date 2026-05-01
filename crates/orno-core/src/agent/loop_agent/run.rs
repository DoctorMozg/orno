//! [`Agent`] trait implementation for [`LoopAgent`].
//!
//! The main iteration loop sits here so the declarative `Agent`
//! contract is readable end-to-end without the policy and retry
//! helpers from `policy.rs` in the middle. Enforces the five
//! strictness dimensions and emits the full paired event stream.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde_json::{Map, Value};
use tracing::{instrument, warn};

use crate::agent::{Agent, AgentOutput, AgentRequest};
use crate::error::AgentError;
use crate::events::{BudgetKind, Event, LlmFailure};
use crate::llm::{LlmRequest, OrnoChatMessage, OrnoChatTool};
use crate::tool::{StateHandle, ToolInvocation};

use super::{LoopAgent, SUBAGENT_PREFIX};

/// Default cap on the per-call tool output bytes appended to the
/// conversation history. Mirrors the `AgentPolicy.max_tool_output_bytes`
/// field — when the operator omits the policy field, this constant
/// applies. Sized to comfortably hold a single Read of a moderate file
/// or a `WebFetch` of a typical HTML page; a runaway tool that returns a
/// gigabyte of output gets truncated to this cap before it reaches the
/// LLM transport.
const DEFAULT_MAX_TOOL_OUTPUT_BYTES: usize = 256 * 1024;

/// Truncate a tool-result string to at most `cap` bytes (head-keep), and
/// append a one-character ellipsis marker if truncation happened. Used
/// before the result is pushed onto the conversation history so a single
/// runaway tool call cannot drag the next LLM request past the
/// provider's prompt-size ceiling. Splits on a UTF-8 char boundary so
/// the marker concatenation is always safe.
fn truncate_tool_output(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    // Walk back from `cap` to the previous UTF-8 boundary. `is_char_boundary(0)`
    // is always true so the loop terminates.
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + '…'.len_utf8());
    out.push_str(&s[..end]);
    out.push('…');
    out
}

/// Approximate byte cost of a single conversation message. Used by the
/// history-cap check so the loop does not need to serialize the full
/// history on every push. Content strings dominate; the small fixed
/// overhead per variant is intentionally ignored to keep the estimate
/// cheap and consistent across variants.
fn estimated_message_bytes(msg: &OrnoChatMessage) -> usize {
    #[expect(
        unreachable_patterns,
        reason = "OrnoChatMessage is #[non_exhaustive]; the catch-all handles future variants"
    )]
    match msg {
        OrnoChatMessage::User { content } | OrnoChatMessage::Assistant { content } => content.len(),
        OrnoChatMessage::ToolCalls { calls } => calls
            .iter()
            .map(|c| c.fn_name.len() + c.fn_arguments.to_string().len())
            .sum(),
        OrnoChatMessage::ToolResult { call_id, content } => call_id.len() + content.len(),
        // Non-exhaustive guard: new variants default to 0 so the cap
        // degrades gracefully rather than failing to compile.
        _ => 0,
    }
}

/// Enforce the `policy.max_message_history_bytes` soft cap on the
/// conversation history vector. When the total estimated byte size
/// exceeds the cap, evicts the oldest messages until the total is
/// below the cap, always keeping at least the two most recent messages
/// so the model sees at least one request-response pair.
fn enforce_message_history_cap(messages: &mut Vec<OrnoChatMessage>, cap: usize) {
    let mut total: usize = messages.iter().map(estimated_message_bytes).sum();
    if total <= cap {
        return;
    }
    // Single-pass O(n) eviction: maintain a running total by subtracting
    // each evicted message's estimated bytes instead of re-summing the
    // whole vector on every iteration. Defer the front shift to a single
    // `drain(..evict)` call so the underlying memmove also runs once.
    // Stop at len-2 so the model always sees at least one tool-call/result
    // pair even when the cap is smaller than any single message.
    let mut evict = 0usize;
    while messages.len() - evict > 2 {
        let msg_bytes = estimated_message_bytes(&messages[evict]);
        total = total.saturating_sub(msg_bytes);
        evict += 1;
        if total <= cap {
            break;
        }
    }
    if evict > 0 {
        messages.drain(..evict);
    }
    // Emit the warning once per truncation event, after eviction, so
    // the log shows the post-truncation byte total.
    let current_bytes: usize = messages.iter().map(estimated_message_bytes).sum();
    warn!(
        cap_bytes = cap,
        current_bytes, "message history truncated to stay within cap",
    );
}

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
        let mut messages: Arc<Vec<OrnoChatMessage>> = Arc::new(Vec::new());
        let mut tool_call_count: u32 = 0;
        // Per-call token counter. Replaces the previous `u64` so subagent
        // tool calls can be handed an `Arc` pointer to bump on every
        // child-loop LLM response — that keeps the parent's
        // `policy.max_total_tokens` budget transitively covering child
        // spend without each loop maintaining a private total. The
        // `Arc<AtomicU64>` is shared via `ToolInvocation.token_budget_share`
        // and via `AgentRequest.parent_token_counter` for nested children.
        let token_counter: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));

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

            // Each request asks for the budget remaining AFTER previous
            // iterations' usage. `token_counter` is updated below from
            // the response's `Usage`; the saturating subtraction guards
            // against the model overshooting the prior cap. `Relaxed` is
            // sufficient because the loop is single-tasked and the
            // counter is only observed across `.await` points within the
            // same task — there is no cross-thread race to order against.
            let max_tokens = if req.policy.max_total_tokens == 0 {
                None
            } else {
                let used = token_counter.load(Ordering::Relaxed);
                let remaining = req.policy.max_total_tokens.saturating_sub(used);
                // Pre-check: token budget fully consumed. Returning
                // `BudgetExceeded` before issuing the next LLM call
                // prevents an uncapped (`max_tokens: None`) request from
                // being billed and only being detected after the
                // response — the breach happens at iteration boundaries
                // too, not just inside a response that pushes us over.
                if remaining == 0 {
                    return Err(AgentError::BudgetExceeded {
                        kind: BudgetKind::Tokens,
                    });
                }
                Some(u32::try_from(remaining).unwrap_or(u32::MAX))
            };

            let llm_req = LlmRequest {
                provider: req.provider.to_string(),
                model: req.model.to_string(),
                prompt: req.initial_prompt.to_string(),
                system: req.system.as_deref().map(str::to_string),
                temperature: None,
                max_tokens,
                messages: Arc::clone(&messages),
                tools: declared_tools.clone(),
            };

            self.config
                .sink
                .record(Event::LlmRequestStarted {
                    run_id: run_id.to_string(),
                    node_id: node_id.to_string(),
                    provider: req.provider.to_string(),
                    model: req.model.to_string(),
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
                            provider: req.provider.to_string(),
                            model: req.model.to_string(),
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
                let delta = u64::from(usage.total_tokens);
                // Saturating add prevents `u64` wrap-around from
                // bypassing the token-budget check below: a near-`u64::MAX`
                // counter plus a large `delta` would otherwise wrap to
                // near 0 and silently re-enable an exhausted budget.
                // `fetch_update` retries on CAS failure but the closure
                // always returns `Some`, so the update succeeds on the
                // first try in this single-tasked loop.
                token_counter
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                        Some(old.saturating_add(delta))
                    })
                    .ok();
                // Bump the parent's counter in lockstep so a parent
                // loop's `policy.max_total_tokens` budget transitively
                // covers child subagent token spend. The chain is
                // recursive: a grandchild loop's responses reach the
                // grandparent through the parent's `parent_token_counter`
                // hop on the next-up loop's iteration check.
                if let Some(parent) = &req.parent_token_counter {
                    parent
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                            Some(old.saturating_add(delta))
                        })
                        .ok();
                }
                let total_tokens = token_counter.load(Ordering::Relaxed);
                if req.policy.max_total_tokens > 0 && total_tokens > req.policy.max_total_tokens {
                    return Err(AgentError::BudgetExceeded {
                        kind: BudgetKind::Tokens,
                    });
                }
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
                    total_tokens: token_counter.load(Ordering::Relaxed),
                    state,
                });
            }

            // Record the assistant's tool-call turn so the next LLM
            // request carries the full causal chain.
            Arc::make_mut(&mut messages).push(OrnoChatMessage::ToolCalls {
                calls: response.tool_calls.clone(),
            });
            if let Some(cap) = req.policy.max_message_history_bytes {
                enforce_message_history_cap(Arc::make_mut(&mut messages), cap);
            }

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
                        let inv = ToolInvocation {
                            run_id,
                            node_id,
                            call_id: &tool_call.call_id,
                            depth: req.depth,
                            state_handle: Some(state_handle),
                            token_budget_share: Some(Arc::clone(&token_counter)),
                            roots: &req.policy.roots,
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
                        token_budget_share: Some(Arc::clone(&token_counter)),
                        roots: &req.policy.roots,
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
                let truncated = truncate_tool_output(&result_content, cap);
                // Redact `secrets.*` leaves before the result enters the
                // conversation history. The handler may have read a
                // secret-bearing file or echoed an env var, and the
                // history is replayed verbatim on the next LLM request —
                // without this hop, a secret would leave the host on the
                // outbound prompt even though every other wire-format
                // field already runs through `redactor`.
                let redacted = self.config.redactor.redact(&truncated);
                Arc::make_mut(&mut messages).push(OrnoChatMessage::ToolResult {
                    call_id: tool_call.call_id.clone(),
                    content: redacted.into_owned(),
                });
                if let Some(cap) = req.policy.max_message_history_bytes {
                    enforce_message_history_cap(Arc::make_mut(&mut messages), cap);
                }
            }
        }

        Err(AgentError::IterationLimitExceeded {
            max: req.policy.max_iterations,
        })
    }
}
