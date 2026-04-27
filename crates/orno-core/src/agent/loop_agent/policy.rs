//! Policy gate and parse-error retry for [`LoopAgent`].
//!
//! Split out of `run.rs` so the effect-based denial logic and the
//! parse-error retry wrapper stay readable. Effect-based denials are
//! *non-terminal* — the denial string is fed back to the model as a
//! `ToolResult`, and the enclosing loop in `run.rs` continues.
//!
//! Visibility: the single entry point `invoke_with_parse_retry` is
//! `pub(super)` because `run.rs` (a sibling) calls it; the helpers
//! `check_policy_and_invoke` and `deny` stay private to this file
//! because they are only reached through the retry wrapper.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;

use crate::error::{AgentError, ToolError};
use crate::events::Event;
use crate::llm::OrnoChatToolCall;
use crate::pipeline::AgentPolicy;
use crate::tool::{ToolEffect, ToolHandler, ToolInvocation};

use super::LoopAgent;

impl LoopAgent {
    /// Emit a `ToolDenied` event and return the denial string fed back
    /// to the model. `tool_name` is the wire-form name the LLM called
    /// (dotted YAML names are sanitized to underscores at the schema
    /// boundary — see `to_wire_name` in `mod.rs`); `wire_to_yaml`
    /// reverse-translates so both the emitted `tool_name` and the
    /// "denied: tool `<name>` blocked by …" text use the YAML form
    /// operators wrote in their pipeline. Builtins map to themselves
    /// because their YAML names are already wire-safe.
    async fn deny(
        &self,
        inv: &ToolInvocation<'_>,
        tool_name: &str,
        reason: String,
        wire_to_yaml: &HashMap<String, String>,
    ) -> String {
        let yaml_name = wire_to_yaml
            .get(tool_name)
            .map_or(tool_name, String::as_str);
        self.config
            .sink
            .record(Event::ToolDenied {
                run_id: inv.run_id.to_string(),
                node_id: inv.node_id.to_string(),
                tool_name: yaml_name.to_string(),
                reason: reason.clone(),
            })
            .await;
        format!("denied: tool `{yaml_name}` blocked by {reason}")
    }

    /// Apply the effect-based policy gate and invoke the handler. A
    /// policy denial is *not* a terminal error — it is fed back to the
    /// model as a `ToolResult` denial string so the model can adapt. A
    /// handler error still terminates the loop via [`AgentError::Tool`].
    /// `wire_to_yaml` is the per-`run()` reverse map used by `deny` to
    /// surface the YAML-form name on every denial event and feed-back
    /// string.
    #[expect(
        clippy::too_many_lines,
        reason = "policy gate enumerates every ToolEffect variant inline"
    )]
    #[expect(
        clippy::too_many_arguments,
        reason = "wire_to_yaml is per-run; threading through deny call sites — see plan WU-W2C"
    )]
    async fn check_policy_and_invoke(
        &self,
        handler: &Arc<dyn ToolHandler>,
        tool_call: &OrnoChatToolCall,
        policy: &AgentPolicy,
        inv: ToolInvocation<'_>,
        wire_to_yaml: &HashMap<String, String>,
    ) -> Result<String, AgentError> {
        match handler.effect() {
            ToolEffect::Mutations => {
                if !policy.allow_mutations {
                    return Ok(self
                        .deny(
                            &inv,
                            &tool_call.fn_name,
                            "allow_mutations=false".into(),
                            wire_to_yaml,
                        )
                        .await);
                }
            },
            ToolEffect::Network => {
                if !policy.allow_network {
                    return Ok(self
                        .deny(
                            &inv,
                            &tool_call.fn_name,
                            "allow_network=false".into(),
                            wire_to_yaml,
                        )
                        .await);
                }
                // Domain gate: URL extracted from `url` arg. Missing/unparsable
                // URLs fall through to the handler which produces its own error —
                // the handler cannot reach the network without a URL anyway.
                if let Some(parsed) = tool_call
                    .fn_arguments
                    .get("url")
                    .and_then(Value::as_str)
                    .and_then(|u| reqwest::Url::parse(u).ok())
                {
                    // Deny non-HTTP(S) schemes to block file://, ftp://, data://.
                    let scheme = parsed.scheme();
                    if scheme != "http" && scheme != "https" {
                        return Ok(self
                            .deny(
                                &inv,
                                &tool_call.fn_name,
                                format!(
                                    "scheme `{scheme}` not permitted; \
                                     only http and https are allowed"
                                ),
                                wire_to_yaml,
                            )
                            .await);
                    }
                    if let Some(host) = parsed.host_str().map(str::to_string) {
                        // Deny bare IPs in loopback/private/link-local ranges.
                        // Hostnames that resolve to these addresses are a network-
                        // boundary concern, not a policy concern.
                        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
                            let blocked = match ip {
                                std::net::IpAddr::V4(v4) => {
                                    v4.is_loopback() || v4.is_private() || v4.is_link_local()
                                },
                                std::net::IpAddr::V6(v6) => {
                                    v6.is_loopback()
                                        // fe80::/10 link-local (no stable is_link_local API yet)
                                        || (v6.segments()[0] & 0xffc0) == 0xfe80
                                },
                            };
                            if blocked {
                                return Ok(self
                                    .deny(
                                        &inv,
                                        &tool_call.fn_name,
                                        format!(
                                            "IP `{ip}` is not routable \
                                             (loopback / private / link-local)"
                                        ),
                                        wire_to_yaml,
                                    )
                                    .await);
                            }
                        }
                        // Suffix match: `d` matches host `h` if `h == d` or
                        // `h` ends with `.d`. This closes the footgun where
                        // `blocked_domains: ["evil.com"]` lets `sub.evil.com`
                        // through under naive equality, and applies the same
                        // rule to the allowlist.
                        let host_matches =
                            |d: &String| -> bool { host == *d || host.ends_with(&format!(".{d}")) };
                        if policy.blocked_domains.iter().any(host_matches) {
                            return Ok(self
                                .deny(
                                    &inv,
                                    &tool_call.fn_name,
                                    format!("blocked_domains contains `{host}`"),
                                    wire_to_yaml,
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
                                    wire_to_yaml,
                                )
                                .await);
                        }
                    }
                }
            },
            ToolEffect::MutationsAndNetwork => {
                if !policy.allow_mutations {
                    return Ok(self
                        .deny(
                            &inv,
                            &tool_call.fn_name,
                            "allow_mutations=false".into(),
                            wire_to_yaml,
                        )
                        .await);
                }
                if !policy.allow_network {
                    return Ok(self
                        .deny(
                            &inv,
                            &tool_call.fn_name,
                            "allow_network=false".into(),
                            wire_to_yaml,
                        )
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
                            wire_to_yaml,
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
    /// terminates with [`AgentError::ParseFailed`]. `wire_to_yaml` is
    /// forwarded into the policy gate so denial events surface the
    /// YAML-form tool name operators wrote.
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
        wire_to_yaml: &HashMap<String, String>,
    ) -> Result<String, AgentError> {
        match self
            .check_policy_and_invoke(handler, tool_call, policy, inv, wire_to_yaml)
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
