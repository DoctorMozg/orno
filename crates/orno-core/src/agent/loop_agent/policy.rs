//! Policy gate and parse-error retry for [`LoopAgent`].
//!
//! Split out of `run.rs` so the effect-based denial logic and the
//! parse-error retry wrapper stay readable. Per ADR 0005 §3 these
//! denials are *non-terminal* — the denial string is fed back to the
//! model as a `ToolResult`, and the enclosing loop in `run.rs`
//! continues.
//!
//! Visibility: the single entry point `invoke_with_parse_retry` is
//! `pub(super)` because `run.rs` (a sibling) calls it; the helpers
//! `check_policy_and_invoke` and `deny` stay private to this file
//! because they are only reached through the retry wrapper.

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::Value;

use crate::error::{AgentError, ToolError};
use crate::events::Event;
use crate::llm::OrnoChatToolCall;
use crate::pipeline::AgentPolicy;
use crate::tool::{ToolEffect, ToolHandler, ToolInvocation};

use super::LoopAgent;

impl LoopAgent {
    async fn deny(&self, inv: &ToolInvocation<'_>, tool_name: &str, reason: String) -> String {
        self.config
            .sink
            .record(Event::ToolDenied {
                run_id: inv.run_id.to_string(),
                node_id: inv.node_id.to_string(),
                tool_name: tool_name.to_string(),
                reason: reason.clone(),
            })
            .await;
        format!("denied: tool `{tool_name}` blocked by {reason}")
    }

    /// Apply the effect-based policy gate and invoke the handler. Per
    /// ADR 0005 §3 a policy denial is *not* a terminal error — it is
    /// fed back to the model as a `ToolResult` denial string so the
    /// model can adapt. A handler error still terminates the loop via
    /// [`AgentError::Tool`].
    #[expect(
        clippy::too_many_lines,
        reason = "policy gate enumerates every ToolEffect variant inline per ADR 0005 §3"
    )]
    async fn check_policy_and_invoke(
        &self,
        handler: &Arc<dyn ToolHandler>,
        tool_call: &OrnoChatToolCall,
        policy: &AgentPolicy,
        inv: ToolInvocation<'_>,
    ) -> Result<String, AgentError> {
        match handler.effect() {
            ToolEffect::Mutations => {
                if !policy.allow_mutations {
                    return Ok(self
                        .deny(&inv, &tool_call.fn_name, "allow_mutations=false".into())
                        .await);
                }
            },
            ToolEffect::Network => {
                if !policy.allow_network {
                    return Ok(self
                        .deny(&inv, &tool_call.fn_name, "allow_network=false".into())
                        .await);
                }
                // Gap 1 — domain gate. URL extracted from `url` arg;
                // anything else (missing arg, non-string, unparseable)
                // falls through to the handler which will produce its
                // own error. Skipping the check on a missing URL is safe
                // because the handler itself cannot reach the network
                // without one.
                if let Some(host) = tool_call
                    .fn_arguments
                    .get("url")
                    .and_then(Value::as_str)
                    .and_then(|u| reqwest::Url::parse(u).ok())
                    .and_then(|parsed| parsed.host_str().map(str::to_string))
                {
                    // Suffix match: `d` matches host `h` if `h == d` or
                    // `h` ends with `.d`. This closes the footgun where
                    // `blocked_domains: ["evil.com"]` lets `sub.evil.com`
                    // through under naive equality, and applies the same
                    // rule to the allowlist so operators can whitelist a
                    // parent domain without enumerating every subdomain.
                    let host_matches = |d: &String| -> bool {
                        host == *d || host.ends_with(&format!(".{d}"))
                    };
                    if policy.blocked_domains.iter().any(host_matches) {
                        return Ok(self
                            .deny(
                                &inv,
                                &tool_call.fn_name,
                                format!("blocked_domains contains `{host}`"),
                            )
                            .await);
                    }
                    if !policy.allowed_domains.is_empty()
                        && !policy.allowed_domains.iter().any(host_matches)
                    {
                        return Ok(self
                            .deny(
                                &inv,
                                &tool_call.fn_name,
                                format!("allowed_domains does not contain `{host}`"),
                            )
                            .await);
                    }
                }
            },
            ToolEffect::MutationsAndNetwork => {
                if !policy.allow_mutations {
                    return Ok(self
                        .deny(&inv, &tool_call.fn_name, "allow_mutations=false".into())
                        .await);
                }
                if !policy.allow_network {
                    return Ok(self
                        .deny(&inv, &tool_call.fn_name, "allow_network=false".into())
                        .await);
                }
                // Intentionally NOT subject to the domain gate —
                // MutationsAndNetwork is Bash, which does not surface
                // a URL in its arguments.
            },
            ToolEffect::ContextSelf => {
                if !policy.allow_context_writes {
                    return Ok(self
                        .deny(
                            &inv,
                            &tool_call.fn_name,
                            "allow_context_writes=false".into(),
                        )
                        .await);
                }
            },
            ToolEffect::ReadOnly => {},
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

    /// Run `check_policy_and_invoke` with a single parse-error retry
    /// budget per `call_id`, honoring `AgentPolicy.on_parse_error`.
    /// `RetryOnce` feeds the `InvalidArgs` message back to the model as
    /// a tool result; the second breach on the same `call_id`
    /// terminates with [`AgentError::ParseFailed`].
    #[expect(
        clippy::too_many_arguments,
        reason = "parse-retry wraps check_policy_and_invoke; collapsing into a struct would add indirection without simplifying calls"
    )]
    pub(super) async fn invoke_with_parse_retry(
        &self,
        handler: &Arc<dyn ToolHandler>,
        tool_call: &OrnoChatToolCall,
        policy: &AgentPolicy,
        inv: ToolInvocation<'_>,
        retried: &mut HashSet<String>,
    ) -> Result<String, AgentError> {
        match self
            .check_policy_and_invoke(handler, tool_call, policy, inv)
            .await
        {
            Ok(s) => Ok(s),
            Err(AgentError::Tool {
                name,
                source: ToolError::InvalidArgs { message, .. },
            }) => match policy.on_parse_error {
                crate::pipeline::OnParseError::RetryOnce
                    if !retried.contains(&tool_call.call_id) =>
                {
                    retried.insert(tool_call.call_id.clone());
                    Ok(format!("tool args invalid: {message}"))
                },
                _ => Err(AgentError::ParseFailed {
                    tool: name,
                    error: message,
                }),
            },
            Err(e) => Err(e),
        }
    }
}
