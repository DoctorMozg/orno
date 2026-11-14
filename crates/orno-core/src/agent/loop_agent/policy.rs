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
use crate::tool::net_guard::{DomainRules, DomainVerdict, is_blocked_ip, literal_ip};
use crate::tool::{ToolEffect, ToolHandler, ToolInvocation};

use super::{LoopAgent, SUBAGENT_PREFIX};

/// Argument names tools conventionally use for their destination. A
/// value under one of them is gated whatever its scheme, so `file://`
/// is denied rather than ignored.
const URL_KEYS: [&str; 5] = ["url", "endpoint", "uri", "target", "link"];

/// Every http(s) URL in a tool call's arguments, at any nesting depth,
/// so a tool that hides its destination under an unexpected key or
/// inside an array still goes through the gate. `Url::parse` lowercases
/// the scheme, which makes `HTTP://` count too.
fn extract_network_urls(args: &Value) -> Vec<reqwest::Url> {
    let mut urls = Vec::new();
    collect_urls(args, None, &mut urls);
    urls
}

fn collect_urls(value: &Value, key: Option<&str>, out: &mut Vec<reqwest::Url>) {
    match value {
        Value::String(s) => {
            if let Ok(url) = reqwest::Url::parse(s)
                && (matches!(url.scheme(), "http" | "https")
                    || key.is_some_and(|k| URL_KEYS.contains(&k)))
            {
                out.push(url);
            }
        },
        Value::Array(items) => items.iter().for_each(|v| collect_urls(v, key, out)),
        Value::Object(map) => map.iter().for_each(|(k, v)| collect_urls(v, Some(k), out)),
        _ => {},
    }
}

impl LoopAgent {
    /// Emit a `ToolDenied` event and return the denial string fed back
    /// to the model. `tool_name` is the wire-form name the LLM called
    /// (dotted YAML names are sanitized to underscores at the schema
    /// boundary — see `wire_name_from_yaml` in `mod.rs`); `wire_to_yaml`
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

    /// Apply the domain/SSRF gate for any network-reaching tool call.
    /// Called from both [`ToolEffect::Network`] and
    /// [`ToolEffect::MutationsAndNetwork`] arms so domain policy is
    /// consistently enforced for all network tools, including MCP tools.
    /// Returns `Some(denial)` when the request should be denied (the
    /// caller feeds the string back to the model as a `ToolResult`),
    /// or `None` to allow the call through.
    async fn gate_network_url(
        &self,
        tool_call: &OrnoChatToolCall,
        policy: &AgentPolicy,
        inv: &ToolInvocation<'_>,
        wire_to_yaml: &HashMap<String, String>,
    ) -> Option<String> {
        // A subagent's arguments are a prompt, not a destination; its own
        // loop gates whatever it calls.
        if wire_to_yaml
            .get(&tool_call.fn_name)
            .is_some_and(|n| n.starts_with(SUBAGENT_PREFIX))
        {
            return None;
        }
        // Egress with no allowlist means every domain not in
        // `blocked_domains` is reachable — surface this so an
        // operator who forgot to populate the allowlist sees it.
        if policy.allowed_domains.is_empty() {
            tracing::warn!(
                tool.name = %tool_call.fn_name,
                "allow_network=true but allowed_domains is empty; every non-blocked domain is reachable",
            );
        }
        // Missing/unparsable URLs fall through to the handler, which
        // produces its own error; it cannot reach the network without one.
        for url in extract_network_urls(&tool_call.fn_arguments) {
            if let Some(reason) = self.url_denial(&url, policy).await {
                return Some(
                    self.deny(inv, &tool_call.fn_name, reason, wire_to_yaml)
                        .await,
                );
            }
        }
        None
    }

    /// Why `url` must not be requested under `policy`, or `None` when it
    /// may proceed.
    async fn url_denial(&self, url: &reqwest::Url, policy: &AgentPolicy) -> Option<String> {
        let scheme = url.scheme();
        if scheme != "http" && scheme != "https" {
            return Some(format!(
                "scheme `{scheme}` not permitted; only http and https are allowed"
            ));
        }
        let host = url.host_str()?;
        // Literal IPs are rejected before the domain lists so a blocked
        // address never costs a DNS round-trip.
        let literal = literal_ip(url);
        if let Some(ip) = literal
            && is_blocked_ip(ip)
        {
            return Some(format!("IP `{ip}` is not a routable public address"));
        }
        let verdict = DomainRules {
            allowed: &policy.allowed_domains,
            blocked: &policy.blocked_domains,
        }
        .check(host);
        match verdict {
            DomainVerdict::Blocked => return Some(format!("blocked_domains contains `{host}`")),
            DomainVerdict::NotAllowed => {
                return Some(format!("allowed_domains does not contain `{host}`"));
            },
            _ => {},
        }
        if literal.is_some() {
            return None;
        }
        // Pre-resolve so the denial names the offending address. Open
        // egress fails closed on DNS errors; an allowlisted host is
        // trusted by the operator and its lookup failure only warns (the
        // connect-time guard still filters what is dialed).
        match self.resolver.resolve(host).await {
            Ok(addrs) => addrs
                .iter()
                .find(|ip| is_blocked_ip(**ip))
                .map(|ip| format!("host `{host}` resolved to blocked IP `{ip}`")),
            Err(e) if verdict == DomainVerdict::Allowlisted => {
                tracing::warn!(
                    net.host = %host,
                    error = %e,
                    "DNS resolution failed for allowlisted host; allowing request",
                );
                None
            },
            Err(e) => {
                tracing::warn!(net.host = %host, error = %e, "DNS resolution failed; denying request");
                Some(format!("DNS resolution failed for host `{host}`: {e}"))
            },
        }
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
                if let Some(denial) = self
                    .gate_network_url(tool_call, policy, &inv, wire_to_yaml)
                    .await
                {
                    return Ok(denial);
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
                if let Some(denial) = self
                    .gate_network_url(tool_call, policy, &inv, wire_to_yaml)
                    .await
                {
                    return Ok(denial);
                }
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

#[cfg(test)]
mod tests {
    use super::extract_network_urls;
    use rstest::rstest;
    use serde_json::{Value, json};

    fn found(args: &Value) -> Vec<String> {
        extract_network_urls(args)
            .iter()
            .map(|u| u.as_str().to_string())
            .collect()
    }

    #[rstest]
    #[case::conventional_key(json!({ "endpoint": "https://api.example.com/data" }), &["https://api.example.com/data"])]
    #[case::unconventional_key(json!({ "my_url": "https://api.example.com/data" }), &["https://api.example.com/data"])]
    #[case::uppercase_scheme(json!({ "x": "HTTP://Example.com/a" }), &["http://example.com/a"])]
    #[case::nested_object(json!({ "req": { "opts": { "dest": "http://169.254.169.254/" } } }), &["http://169.254.169.254/"])]
    #[case::inside_array(json!({ "hosts": ["http://a.test/", "plain", "https://b.test/"] }), &["http://a.test/", "https://b.test/"])]
    #[case::non_http_under_url_key(json!({ "url": "file:///etc/passwd" }), &["file:///etc/passwd"])]
    #[case::non_http_under_other_key(json!({ "note": "file:///etc/passwd" }), &[])]
    #[case::no_urls(json!({ "name": "foo", "count": 42, "flag": true }), &[])]
    #[case::primitive_args(json!("just a string"), &[])]
    fn urls_are_found_at_any_depth(#[case] args: Value, #[case] expected: &[&str]) {
        assert_eq!(found(&args), expected);
    }
}
