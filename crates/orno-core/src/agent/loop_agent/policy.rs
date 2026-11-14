//! Policy gate and parse-error retry for [`LoopAgent`].
//!
//! Effect-based denials are non-terminal: the denial string goes back
//! to the model as a `ToolResult` and the loop continues.

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

/// One model-issued tool call with everything the policy gate needs.
pub(super) struct ToolCall<'a> {
    pub(super) handler: &'a Arc<dyn ToolHandler>,
    pub(super) tool_call: &'a OrnoChatToolCall,
    pub(super) policy: &'a AgentPolicy,
    pub(super) wire_to_yaml: &'a HashMap<String, String>,
}

/// Why the policy flags forbid `effect`, or `None` when it may run.
fn effect_denial(effect: ToolEffect, policy: &AgentPolicy) -> Option<&'static str> {
    match effect {
        ToolEffect::Mutations | ToolEffect::MutationsAndNetwork if !policy.allow_mutations => {
            Some("allow_mutations=false")
        },
        ToolEffect::Network | ToolEffect::MutationsAndNetwork if !policy.allow_network => {
            Some("allow_network=false")
        },
        ToolEffect::ContextSelf if !policy.allow_context_writes => {
            Some("allow_context_writes=false")
        },
        _ => None,
    }
}

/// Tool-result text for a recoverable failure. The caller's redaction
/// pass runs on it like on any other tool output.
fn recoverable_feedback(tool: &str, err: &ToolError) -> String {
    let kind = match err {
        ToolError::Recoverable { kind, .. } => kind.to_string(),
        _ => "not_implemented".to_string(),
    };
    // The message can echo user data, so only the kind is logged.
    tracing::warn!(tool.name = %tool, error.kind = %kind, "tool failed; feeding the error back to the model");
    match err {
        ToolError::Recoverable { message, .. } => format!("error: {message}"),
        ToolError::NotImplemented { name, feature } => {
            format!("error: tool `{name}` not yet implemented: {feature}")
        },
        other => format!("error: {other}"),
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
        call: &ToolCall<'_>,
        inv: &ToolInvocation<'_>,
    ) -> Option<String> {
        let ToolCall {
            tool_call,
            policy,
            wire_to_yaml,
            ..
        } = *call;
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

    /// Apply the effect-based policy gate and invoke the handler. Policy
    /// denials and recoverable handler errors ([`ToolError::is_recoverable`])
    /// are fed back to the model as the tool result; any other handler
    /// error terminates the loop via [`AgentError::Tool`].
    async fn check_policy_and_invoke(
        &self,
        call: &ToolCall<'_>,
        inv: ToolInvocation<'_>,
    ) -> Result<String, AgentError> {
        let tool_call = call.tool_call;
        let effect = call.handler.effect();
        if let Some(reason) = effect_denial(effect, call.policy) {
            return Ok(self
                .deny(&inv, &tool_call.fn_name, reason.into(), call.wire_to_yaml)
                .await);
        }
        if matches!(
            effect,
            ToolEffect::Network | ToolEffect::MutationsAndNetwork
        ) && let Some(denial) = self.gate_network_url(call, &inv).await
        {
            return Ok(denial);
        }

        match call
            .handler
            .invoke(inv, tool_call.fn_arguments.clone())
            .await
        {
            Ok(output) => Ok(output),
            Err(err) if err.is_recoverable() => Ok(recoverable_feedback(&tool_call.fn_name, &err)),
            Err(source) => Err(AgentError::Tool {
                name: tool_call.fn_name.clone(),
                source,
            }),
        }
    }

    /// Run `check_policy_and_invoke` with a single parse-error retry per
    /// `call_id`, honoring `AgentPolicy.on_parse_error`. `RetryOnce` feeds
    /// the `InvalidArgs` message back as a tool result; a second breach on
    /// the same `call_id` terminates with [`AgentError::ParseFailed`].
    pub(super) async fn invoke_with_parse_retry(
        &self,
        call: &ToolCall<'_>,
        inv: ToolInvocation<'_>,
        retried: &mut HashSet<String>,
    ) -> Result<String, AgentError> {
        let tool_call = call.tool_call;
        match self.check_policy_and_invoke(call, inv).await {
            Ok(s) => Ok(s),
            Err(AgentError::Tool {
                name,
                source: ToolError::InvalidArgs { message, .. },
            }) => match call.policy.on_parse_error {
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
