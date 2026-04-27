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
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use serde_json::Value;

use crate::error::{AgentError, ToolError};
use crate::events::Event;
use crate::llm::OrnoChatToolCall;
use crate::pipeline::AgentPolicy;
use crate::tool::{ToolEffect, ToolHandler, ToolInvocation};

use super::LoopAgent;

/// Block every `IPv4` address an outbound HTTP request must never
/// reach. Covers loopback, RFC 1918 private, link-local (including
/// the `169.254.169.254` cloud metadata IP), CGNAT (RFC 6598),
/// documentation, benchmarking, and broadcast. The full
/// `127.0.0.0/8` loopback range is checked explicitly because
/// `Ipv4Addr::is_loopback` is documented as covering only
/// `127.0.0.1` on some platforms.
#[must_use]
pub(crate) fn is_blocked_ipv4(v4: Ipv4Addr) -> bool {
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        // CGNAT 100.64.0.0/10 — RFC 6598; not flagged by is_private.
        || (u32::from(v4) & 0xffc0_0000) == 0x6440_0000
        // Full 127.0.0.0/8 loopback range.
        || v4.octets()[0] == 127
        || v4.is_documentation()
        || v4 == Ipv4Addr::BROADCAST
}

/// Block every `IPv6` address an outbound HTTP request must never
/// reach. Covers loopback, link-local (`fe80::/10`), unique-local
/// (`fc00::/7`), and any IPv4-mapped or IPv4-compatible address
/// whose embedded v4 form is itself blocked.
#[must_use]
pub(crate) fn is_blocked_ipv6(v6: Ipv6Addr) -> bool {
    v6.is_loopback()
        // fe80::/10 link-local.
        || (v6.segments()[0] & 0xffc0) == 0xfe80
        // fc00::/7 unique local addresses (RFC 4193).
        || (v6.segments()[0] & 0xfe00) == 0xfc00
        // IPv4-mapped (::ffff:0:0/96) — re-check the embedded v4.
        || v6.to_ipv4_mapped().is_some_and(is_blocked_ipv4)
        // IPv4-compatible (deprecated, but may still appear).
        || v6.to_ipv4().is_some_and(is_blocked_ipv4)
}

/// Extract the network URL from a tool-call's argument bundle.
/// Tries the conventional argument names first, then falls back to
/// scanning every string value for an `http(s)://` prefix so a
/// tool that declares its URL under an unexpected key still routes
/// through the gate.
fn extract_network_url(args: &Value) -> Option<reqwest::Url> {
    for key in &["url", "endpoint", "uri", "target", "link"] {
        if let Some(url) = args
            .get(*key)
            .and_then(Value::as_str)
            .and_then(|u| reqwest::Url::parse(u).ok())
        {
            return Some(url);
        }
    }
    if let Some(obj) = args.as_object() {
        for val in obj.values() {
            if let Some(s) = val.as_str()
                && (s.starts_with("http://") || s.starts_with("https://"))
                && let Ok(url) = reqwest::Url::parse(s)
            {
                return Some(url);
            }
        }
    }
    None
}

/// Resolve `host` via blocking DNS in a worker thread and return the
/// list of resolved addresses. `lookup_host` is unavailable without
/// the `tokio/net` feature, so we offload `std::net::ToSocketAddrs`
/// to `spawn_blocking` to keep the runtime responsive.
async fn resolve_host(host: &str) -> std::io::Result<Vec<IpAddr>> {
    let target = format!("{host}:0");
    let join = tokio::task::spawn_blocking(move || {
        use std::net::ToSocketAddrs;
        target
            .to_socket_addrs()
            .map(|iter| iter.map(|sa| sa.ip()).collect::<Vec<_>>())
    })
    .await;
    match join {
        Ok(res) => res,
        Err(e) => Err(std::io::Error::other(e)),
    }
}

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
                // Egress with no allowlist means every domain not in
                // `blocked_domains` is reachable — surface this so an
                // operator who forgot to populate the allowlist sees it.
                if policy.allowed_domains.is_empty() {
                    tracing::warn!(
                        tool.name = %tool_call.fn_name,
                        "allow_network=true but allowed_domains is empty; every non-blocked domain is reachable",
                    );
                }
                // Domain gate: URL extracted from any common URL-shaped
                // arg. Missing/unparsable URLs fall through to the
                // handler which produces its own error — the handler
                // cannot reach the network without a URL anyway.
                if let Some(parsed) = extract_network_url(&tool_call.fn_arguments) {
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
                        // Literal-IP block list. Hostnames are also
                        // pre-resolved below — but only after the cheap
                        // domain-list checks pass, since a host already
                        // rejected by the operator's allowlist need not
                        // burn a DNS round-trip.
                        if let Ok(ip) = host.parse::<IpAddr>() {
                            let blocked = match ip {
                                IpAddr::V4(v4) => is_blocked_ipv4(v4),
                                IpAddr::V6(v6) => is_blocked_ipv6(v6),
                            };
                            if blocked {
                                return Ok(self
                                    .deny(
                                        &inv,
                                        &tool_call.fn_name,
                                        format!(
                                            "IP `{ip}` is not routable \
                                             (loopback / private / link-local / \
                                             CGNAT / documentation)"
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
                        let explicitly_allowed = !policy.allowed_domains.is_empty()
                            && policy.allowed_domains.iter().any(host_matches);
                        if !policy.allowed_domains.is_empty() && !explicitly_allowed {
                            return Ok(self
                                .deny(
                                    &inv,
                                    &tool_call.fn_name,
                                    format!("allowed_domains does not contain `{host}`"),
                                    wire_to_yaml,
                                )
                                .await);
                        }
                        // Hostname input that survived the domain rules:
                        // pre-resolve DNS and reject if any returned address
                        // is in a blocked range. For open-network requests
                        // (no allowlist) fail closed on DNS errors — a
                        // failing resolver must not bypass the gate. For
                        // explicitly-allowlisted hosts DNS failure is treated
                        // as a transient resolver issue and the request is
                        // allowed through with a warning; the operator
                        // accepted responsibility when they added the domain.
                        if host.parse::<IpAddr>().is_err() {
                            match resolve_host(&host).await {
                                Ok(addrs) => {
                                    if let Some(blocked_ip) = addrs.iter().find(|ip| match ip {
                                        IpAddr::V4(v4) => is_blocked_ipv4(*v4),
                                        IpAddr::V6(v6) => is_blocked_ipv6(*v6),
                                    }) {
                                        return Ok(self
                                            .deny(
                                                &inv,
                                                &tool_call.fn_name,
                                                format!(
                                                    "host `{host}` resolved to blocked IP \
                                                     `{blocked_ip}`"
                                                ),
                                                wire_to_yaml,
                                            )
                                            .await);
                                    }
                                },
                                Err(e) => {
                                    if explicitly_allowed {
                                        tracing::warn!(
                                            host = %host,
                                            error = ?e,
                                            "DNS resolution failed for allowlisted host; \
                                             allowing request (operator-trusted domain)",
                                        );
                                    } else {
                                        tracing::warn!(
                                            host = %host,
                                            error = ?e,
                                            "DNS resolution failed; denying request",
                                        );
                                        return Ok(self
                                            .deny(
                                                &inv,
                                                &tool_call.fn_name,
                                                format!(
                                                    "DNS resolution failed for host `{host}`: {e}"
                                                ),
                                                wire_to_yaml,
                                            )
                                            .await);
                                    }
                                },
                            }
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
                // Bash is not subject to the domain gate — it opens arbitrary
                // network connections that orno cannot intercept. Surface this
                // asymmetry so operators who set allowed_domains know it does
                // not restrict Bash.
                if policy.allow_network && !policy.allowed_domains.is_empty() {
                    tracing::warn!(
                        "Bash tool is allowed network access; allowed_domains \
                         enforcement does not apply to shell commands — \
                         consider allow_network=false to prevent egress from Bash",
                    );
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
