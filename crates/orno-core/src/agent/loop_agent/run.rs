//! [`Agent`] implementation for [`LoopAgent`]: the iteration loop,
//! tool dispatch and final-answer assembly.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Map, Value};
use tracing::{instrument, warn};

use crate::agent::{Agent, AgentOutput, AgentRequest, BudgetScope};
use crate::error::AgentError;
use crate::events::redactor::Keep;
use crate::events::{BudgetKind, Event, LlmFailure};
use crate::llm::{LlmRequest, LlmResponse, OrnoChatMessage, OrnoChatTool, OrnoChatToolCall};
use crate::tool::net_guard::DomainRules;
use crate::tool::{StateHandle, ToolInvocation};

use super::history::History;
use super::policy::ToolCall;
use super::{LoopAgent, SUBAGENT_PREFIX, wire_name_from_yaml};

/// Per-call cap on tool output appended to the history when the policy
/// sets none.
const DEFAULT_MAX_TOOL_OUTPUT_BYTES: usize = 256 * 1024;

/// `None` for an empty buffer so pipelines that never call `SetState`
/// keep the same `nodes.<id>` shape; a poisoned lock also reads as
/// `None` because a partial buffer would be worse than none.
fn final_state(buf: &Mutex<Value>) -> Option<Value> {
    let guard = buf.lock().ok()?;
    match &*guard {
        Value::Object(m) if m.is_empty() => None,
        other => Some(other.clone()),
    }
}

/// What one iteration decided.
enum Turn {
    Continue,
    Done(AgentOutput),
}

/// Inputs fixed for the whole `run` plus the history that grows with it.
pub(super) struct RunState<'a> {
    pub(super) run_id: &'a str,
    pub(super) node_id: &'a str,
    pub(super) req: &'a AgentRequest,
    state_buffer: &'a Mutex<Value>,
    declared_tools: Arc<Vec<OrnoChatTool>>,
    wire_to_yaml: HashMap<String, String>,
    prompt_excerpt: String,
    system_excerpt: Option<String>,
    pub(super) history: History,
    budget: Arc<BudgetScope>,
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
        self.validate_request(&req)?;

        // One buffer per `run`: a subagent's `run` gets its own, so child
        // state never leaks into the parent.
        let state_buffer = Mutex::new(Value::Object(Map::new()));
        let mut st = self.new_run_state(run_id, node_id, &req, &state_buffer);

        for iteration in 0..req.policy.max_iterations {
            if let Turn::Done(output) = self.iterate(&mut st, iteration).await? {
                return Ok(output);
            }
        }
        Err(AgentError::IterationLimitExceeded {
            max: req.policy.max_iterations,
        })
    }
}

impl LoopAgent {
    fn validate_request(&self, req: &AgentRequest) -> Result<(), AgentError> {
        if req.policy.max_iterations == 0 {
            return Err(AgentError::InvalidPolicy(
                "max_iterations must be >= 1".to_string(),
            ));
        }

        // Without a root the path-jail is a no-op, so any handler that
        // declares `requires_jail` forces `policy.roots` to be set.
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

        // Fail up-front so the model never sees a schema it cannot call.
        for name in &req.allowed_tools {
            if self.find_handler(name).is_none() {
                return Err(AgentError::UnknownToolCalled { name: name.clone() });
            }
        }
        Ok(())
    }

    fn new_run_state<'a>(
        &self,
        run_id: &'a str,
        node_id: &'a str,
        req: &'a AgentRequest,
        state_buffer: &'a Mutex<Value>,
    ) -> RunState<'a> {
        let allowed = || {
            self.config
                .tools
                .iter()
                .filter(|h| req.allowed_tools.iter().any(|n| n == h.name()))
        };
        let declared_tools = Arc::new(
            allowed()
                .map(|h| OrnoChatTool {
                    name: wire_name_from_yaml(h.name()),
                    description: h.description().to_string(),
                    schema: h.schema(),
                })
                .collect(),
        );
        // The model answers with wire names; handlers are registered under
        // their YAML names.
        let wire_to_yaml = allowed()
            .map(|h| (wire_name_from_yaml(h.name()), h.name().to_string()))
            .collect();

        RunState {
            run_id,
            node_id,
            req,
            state_buffer,
            declared_tools,
            wire_to_yaml,
            prompt_excerpt: self.excerpt_for_wire(&req.initial_prompt),
            system_excerpt: req.system.as_deref().map(|s| self.excerpt_for_wire(s)),
            history: History::new(),
            // Linked to the parent scope so child spend counts against
            // every ancestor's limits.
            budget: BudgetScope::new(
                req.policy.max_total_tokens,
                req.policy.max_tool_calls,
                req.parent_budget.clone(),
            ),
        }
    }

    async fn iterate(&self, st: &mut RunState<'_>, iteration: u32) -> Result<Turn, AgentError> {
        self.config
            .sink
            .record(Event::AgentIterationStarted {
                run_id: st.run_id.to_string(),
                node_id: st.node_id.to_string(),
                // Only `kind: agent` nodes drive a `LoopAgent`.
                node_kind: "agent".to_string(),
                iteration,
            })
            .await;

        let response = self.request_llm(st).await?;
        Self::charge_usage(st, &response)?;

        if response.tool_calls.is_empty() {
            return Ok(Turn::Done(Self::finish(st, response, iteration)));
        }

        st.history.push(OrnoChatMessage::ToolCalls {
            calls: response.tool_calls.clone(),
        });
        // One parse retry per `call_id`, scoped to the turn that issued it.
        let mut retried_parse_errors = HashSet::new();
        for tool_call in &response.tool_calls {
            self.dispatch_tool(st, tool_call, &mut retried_parse_errors)
                .await?;
        }
        // After every result is in, so a multi-call turn is never cut
        // between a call and its result.
        self.compact_history(st).await?;
        Ok(Turn::Continue)
    }

    /// Completion cap for the next call: what is left on the tightest limit
    /// up the chain, lowered to `ceiling` when one is given. A spent budget
    /// fails here, before an uncapped, billable call.
    pub(super) fn completion_cap(
        st: &RunState<'_>,
        ceiling: Option<u32>,
    ) -> Result<Option<u32>, AgentError> {
        let remaining = match st.budget.tokens_remaining() {
            None => None,
            Some(0) => {
                return Err(AgentError::BudgetExceeded {
                    kind: BudgetKind::Tokens,
                });
            },
            Some(remaining) => Some(u32::try_from(remaining).unwrap_or(u32::MAX)),
        };
        Ok(match (remaining, ceiling) {
            (Some(r), Some(c)) => Some(r.min(c)),
            (r, c) => r.or(c),
        })
    }

    fn build_llm_request(st: &RunState<'_>) -> Result<LlmRequest, AgentError> {
        let max_tokens = Self::completion_cap(st, None)?;
        Ok(LlmRequest {
            provider: Arc::clone(&st.req.provider),
            model: Arc::clone(&st.req.model),
            prompt: Arc::clone(&st.req.initial_prompt),
            system: st.req.system.clone(),
            temperature: None,
            max_tokens,
            messages: st.history.snapshot(),
            tools: Arc::clone(&st.declared_tools),
        })
    }

    async fn request_llm(&self, st: &RunState<'_>) -> Result<LlmResponse, AgentError> {
        let llm_req = Self::build_llm_request(st)?;
        self.send_llm(
            st,
            llm_req,
            st.prompt_excerpt.clone(),
            st.system_excerpt.clone(),
        )
        .await
    }

    /// Emits the started/received (or failed) event pair around the call.
    pub(super) async fn send_llm(
        &self,
        st: &RunState<'_>,
        llm_req: LlmRequest,
        prompt_excerpt: String,
        system_excerpt: Option<String>,
    ) -> Result<LlmResponse, AgentError> {
        let req = st.req;
        self.config
            .sink
            .record(Event::LlmRequestStarted {
                run_id: st.run_id.to_string(),
                node_id: st.node_id.to_string(),
                provider: req.provider.to_string(),
                model: req.model.to_string(),
                prompt_excerpt,
                system_excerpt,
            })
            .await;

        let response = match self.config.transport.complete(llm_req).await {
            Ok(resp) => resp,
            Err(err) => {
                let failure = LlmFailure::from_llm_error(
                    &err,
                    &self.config.redactor,
                    self.config.body_excerpt_max_bytes,
                );
                self.config
                    .sink
                    .record(Event::LlmRequestFailed {
                        run_id: st.run_id.to_string(),
                        node_id: st.node_id.to_string(),
                        provider: req.provider.to_string(),
                        model: req.model.to_string(),
                        failure,
                    })
                    .await;
                return Err(AgentError::from(err));
            },
        };

        // Emitted before the budget check so a token breach never leaves
        // `LlmRequestStarted` unpaired on the wire.
        self.config
            .sink
            .record(Event::LlmResponseReceived {
                run_id: st.run_id.to_string(),
                node_id: st.node_id.to_string(),
                finish_reason: response.finish_reason.clone(),
                usage: response.usage.clone(),
                content_excerpt: self.excerpt_for_wire(&response.content),
            })
            .await;
        Ok(response)
    }

    pub(super) fn charge_usage(
        st: &RunState<'_>,
        response: &LlmResponse,
    ) -> Result<(), AgentError> {
        if let Some(usage) = &response.usage {
            st.budget
                .charge_tokens(u64::from(usage.total_tokens))
                .map_err(|kind| AgentError::BudgetExceeded { kind })?;
        } else if st.req.policy.max_total_tokens > 0 {
            // Without a usage block the cap cannot be charged for this
            // response; warn so operators spot transports that omit it.
            warn!(
                policy.max_total_tokens = st.req.policy.max_total_tokens,
                llm.provider = %st.req.provider,
                llm.model = %st.req.model,
                "provider returned no usage; token budget enforcement is degraded for this response",
            );
        }
        Ok(())
    }

    fn finish(st: &RunState<'_>, response: LlmResponse, iteration: u32) -> AgentOutput {
        AgentOutput {
            content: response.content,
            finish_reason: response.finish_reason,
            usage: response.usage,
            iterations: iteration,
            total_tokens: st.budget.tokens_used(),
            state: final_state(st.state_buffer),
        }
    }

    /// Runs one tool call and appends its redacted result to the history.
    async fn dispatch_tool(
        &self,
        st: &mut RunState<'_>,
        tool_call: &OrnoChatToolCall,
        retried: &mut HashSet<String>,
    ) -> Result<(), AgentError> {
        st.budget
            .charge_tool_call()
            .map_err(|kind| AgentError::BudgetExceeded { kind })?;

        let yaml_name = st
            .wire_to_yaml
            .get(&tool_call.fn_name)
            .map_or(tool_call.fn_name.as_str(), String::as_str);
        // The fallback above lets an unallowed wire name through, and
        // `find_handler` searches every registered handler, so the
        // allowlist is enforced here.
        if !st.req.allowed_tools.iter().any(|n| n == yaml_name) {
            return Err(AgentError::UnknownToolCalled {
                name: tool_call.fn_name.clone(),
            });
        }

        let result_content = match self.subagent_depth_denial(st, yaml_name).await {
            Some(denial) => denial,
            None => self.invoke_tool(st, yaml_name, tool_call, retried).await?,
        };

        self.config
            .sink
            .record(Event::ToolCallRecorded {
                run_id: st.run_id.to_string(),
                node_id: st.node_id.to_string(),
                tool_name: tool_call.fn_name.clone(),
                call_id: tool_call.call_id.clone(),
                input_excerpt: self.excerpt_for_wire(
                    &serde_json::to_string(&tool_call.fn_arguments).unwrap_or_default(),
                ),
                output_excerpt: self.excerpt_for_wire(&result_content),
            })
            .await;

        let cap = st
            .req
            .policy
            .max_tool_output_bytes
            .unwrap_or(DEFAULT_MAX_TOOL_OUTPUT_BYTES);
        // Redact before bounding so a secret straddling the cap is still
        // matched; the history is replayed on the next request.
        let content = self
            .config
            .redactor
            .redact_bounded(&result_content, cap, Keep::Head);
        st.history.push(OrnoChatMessage::ToolResult {
            call_id: tool_call.call_id.clone(),
            content,
        });
        Ok(())
    }

    /// The denial string fed back to the model when a subagent call would
    /// exceed `max_subagent_depth`; the child loop is never entered then.
    async fn subagent_depth_denial(&self, st: &RunState<'_>, yaml_name: &str) -> Option<String> {
        let child_agent = yaml_name.strip_prefix(SUBAGENT_PREFIX)?;
        let child_depth = st.req.depth.saturating_add(1);
        let max_depth = st.req.policy.max_subagent_depth;
        if child_depth <= max_depth {
            return None;
        }
        self.config
            .sink
            .record(Event::SubagentDepthExceeded {
                run_id: st.run_id.to_string(),
                parent_node_id: st.node_id.to_string(),
                attempted_child_agent: child_agent.to_string(),
                depth_attempted: child_depth,
                max_depth,
            })
            .await;
        Some(format!(
            "denied: subagent `{child_agent}` would run at depth {child_depth}, \
             exceeding max_subagent_depth={max_depth}",
        ))
    }

    async fn invoke_tool(
        &self,
        st: &RunState<'_>,
        yaml_name: &str,
        tool_call: &OrnoChatToolCall,
        retried: &mut HashSet<String>,
    ) -> Result<String, AgentError> {
        let handler =
            self.find_handler(yaml_name)
                .ok_or_else(|| AgentError::UnknownToolCalled {
                    name: tool_call.fn_name.clone(),
                })?;
        let inv = ToolInvocation {
            run_id: st.run_id,
            node_id: st.node_id,
            call_id: &tool_call.call_id,
            depth: st.req.depth,
            state_handle: Some(StateHandle::new(st.state_buffer)),
            budget: Some(Arc::clone(&st.budget)),
            render_scope: st.req.render_scope.as_ref(),
            roots: &st.req.policy.roots,
            domains: DomainRules {
                allowed: &st.req.policy.allowed_domains,
                blocked: &st.req.policy.blocked_domains,
            },
        };
        let call = ToolCall {
            handler,
            tool_call,
            policy: &st.req.policy,
            wire_to_yaml: &st.wire_to_yaml,
        };
        self.invoke_with_parse_retry(&call, inv, retried).await
    }
}
