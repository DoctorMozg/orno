//! `WebFetch` tool — HTTP GET a URL. Requires `allow_network`.
//! Domain policy is enforced by `LoopAgent` before dispatch.

use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{ToolEffect, ToolHandler, ToolInvocation};
use crate::agent::loop_agent::policy::{is_blocked_ipv4, is_blocked_ipv6};
use crate::error::ToolError;

/// Cap on the number of redirects this client will follow. Lower than
/// reqwest's default of 10 so a redirect-amplification chain does not
/// burn the per-request timeout before the gate's IP block-list has a
/// chance to interrupt it.
const MAX_REDIRECTS: usize = 5;

// Response bodies above this cap are truncated before being returned to
// the agent. Keeps a runaway page from blowing out the LLM context
// window on the next turn.
const MAX_BODY_BYTES: usize = 1_048_576;

const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WebFetchArgs {
    #[schemars(description = "URL to fetch.")]
    url: String,
    #[schemars(description = "Per-request timeout in seconds. Defaults to 30.")]
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// List the top-level field names of a JSON-object argument bundle
/// without echoing their values, for inclusion in `InvalidArgs`
/// messages.
fn arg_field_names(args: &Value) -> String {
    args.as_object()
        .map(|o| o.keys().map(String::as_str).collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

/// HTTP GET handler with a shared `reqwest::Client` reused across calls.
///
/// The client is built once at construction time so every invocation
/// reuses the same connection pool and TLS session cache. A per-call
/// `timeout_secs` argument overrides the client's default timeout via
/// `RequestBuilder::timeout` without rebuilding the client.
///
/// `allowed_domains` and `blocked_domains` are baked into the redirect
/// policy at construction time. The agent's policy gate already enforces
/// these on the *initial* URL; the redirect closure re-applies them on
/// every hop so a permitted public host cannot redirect *to* a host the
/// operator denied. Suffix matching mirrors `LoopAgent`'s `host_matches`
/// so `blocked_domains: ["evil.com"]` also denies `sub.evil.com`. Domain
/// lists are moved into the synchronous `'static` redirect closure at
/// construction, so they are not stored separately on the struct.
#[derive(Debug, Clone)]
pub struct WebFetchHandler {
    client: reqwest::Client,
    default_timeout: Duration,
}

/// Outcome of evaluating a redirect hop's host against the handler's
/// domain policy. Extracted from the redirect closure so the matching
/// rules are testable without constructing a `reqwest::redirect::Attempt`.
#[derive(Debug, PartialEq, Eq)]
enum RedirectDomainDecision {
    /// Host passes both lists (or neither list applies); follow.
    Allow,
    /// Host matches `blocked_domains`; deny with the captured host name.
    DenyBlocked(String),
    /// `allowed_domains` is non-empty and host matches none of it; deny.
    DenyOutsideAllowed(String),
}

/// Suffix-match a host against a single domain entry: `host == d` or
/// `host` ends with `.d`. Mirrors `LoopAgent::policy::host_matches` so
/// the redirect closure and the initial-URL gate apply the same rule.
fn host_matches_domain(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// Apply the domain policy to a redirect hop's host. Pure function so
/// the redirect closure stays a thin shim over testable logic.
fn evaluate_redirect_host(
    host: &str,
    allowed_domains: &[String],
    blocked_domains: &[String],
) -> RedirectDomainDecision {
    if blocked_domains.iter().any(|d| host_matches_domain(host, d)) {
        return RedirectDomainDecision::DenyBlocked(host.to_string());
    }
    if !allowed_domains.is_empty() && !allowed_domains.iter().any(|d| host_matches_domain(host, d))
    {
        return RedirectDomainDecision::DenyOutsideAllowed(host.to_string());
    }
    RedirectDomainDecision::Allow
}

impl WebFetchHandler {
    /// Construct a handler with a single shared `reqwest::Client` and
    /// empty domain lists. Domain enforcement on redirect hops is
    /// effectively a no-op for instances built this way; use
    /// [`WebFetchHandler::with_domains`] to bake in the operator's
    /// `allowed_domains` / `blocked_domains` for redirect-time defense.
    ///
    /// `default_timeout` is the timeout applied to a request that does
    /// not supply its own `timeout_secs` argument; passing `None` falls
    /// back to the built-in 30-second default.
    #[must_use]
    pub fn new(default_timeout: Option<Duration>) -> Self {
        Self::with_domains(default_timeout, Vec::new(), Vec::new())
    }

    /// Construct a handler whose redirect closure rejects hops to hosts
    /// outside the configured domain policy. The lists are moved into
    /// the synchronous redirect closure at construction time, so they
    /// must be supplied up front; callers that mutate per-agent policy
    /// at runtime build a separate handler per agent.
    ///
    /// Both lists are empty by default; `blocked_domains` always wins
    /// over `allowed_domains` for hosts that match both.
    #[must_use]
    pub fn with_domains(
        default_timeout: Option<Duration>,
        allowed_domains: Vec<String>,
        blocked_domains: Vec<String>,
    ) -> Self {
        let default_timeout =
            default_timeout.unwrap_or_else(|| Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        // Move the lists into the closure: reqwest's redirect callback is
        // synchronous and `'static`, so it keeps its own copies.
        let allowed_for_closure = allowed_domains;
        let blocked_for_closure = blocked_domains;
        // Redirect policy: re-run the literal-IP block list and the
        // domain policy on each hop, and cap the chain at MAX_REDIRECTS.
        // The agent's policy gate only sees the original URL, so without
        // this hook a permitted public host could redirect to
        // `127.0.0.1`, a metadata IP, or a blocked domain and reqwest
        // would happily follow.
        let redirect_policy = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                return attempt.error(format!("too many redirects (>{MAX_REDIRECTS})"));
            }
            if let Some(host) = attempt.url().host_str() {
                if let Ok(ip) = host.parse::<IpAddr>() {
                    let blocked = match ip {
                        IpAddr::V4(v4) => is_blocked_ipv4(v4),
                        IpAddr::V6(v6) => is_blocked_ipv6(v6),
                    };
                    if blocked {
                        return attempt.error(format!("redirect to blocked IP `{ip}`"));
                    }
                }
                match evaluate_redirect_host(host, &allowed_for_closure, &blocked_for_closure) {
                    RedirectDomainDecision::Allow => {},
                    RedirectDomainDecision::DenyBlocked(h) => {
                        return attempt.error(format!("redirect to blocked domain `{h}`"));
                    },
                    RedirectDomainDecision::DenyOutsideAllowed(h) => {
                        return attempt.error(format!("redirect to `{h}` outside allowed_domains"));
                    },
                }
            }
            attempt.follow()
        });
        // `reqwest::Client::builder().build()` only fails when TLS
        // backend init fails, which is a startup-fatal misconfiguration;
        // panic here is the same behavior as `Client::new()`.
        let client = reqwest::Client::builder()
            .timeout(default_timeout)
            .redirect(redirect_policy)
            .build()
            .expect("default reqwest client must build");
        Self {
            client,
            default_timeout,
        }
    }
}

impl Default for WebFetchHandler {
    fn default() -> Self {
        Self::new(None)
    }
}

#[async_trait]
impl ToolHandler for WebFetchHandler {
    fn name(&self) -> &str {
        "WebFetch"
    }
    fn description(&self) -> &str {
        "HTTP GET a URL and return the response body and content-type. Up to 1 MiB."
    }
    fn schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(WebFetchArgs)).expect("static schema")
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Network
    }
    async fn invoke(&self, _inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        // Retain the args' field names (not values) so the error message
        // pins the offending field even when serde only reports a
        // type-level mismatch (e.g. `invalid type: integer …, expected a
        // string`). Values are omitted to avoid echoing caller-supplied
        // payloads into the log.
        let fields = arg_field_names(&args);
        let WebFetchArgs { url, timeout_secs } =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
                name: "WebFetch".to_string(),
                message: format!("{e} (fields: {fields})"),
            })?;

        let request_timeout = timeout_secs.map_or(self.default_timeout, Duration::from_secs);
        let mut response = self
            .client
            .get(&url)
            .timeout(request_timeout)
            .send()
            .await
            .map_err(|err| ToolError::Invocation {
                name: "WebFetch".to_string(),
                source: Box::new(err),
            })?;

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string();

        // Stream body in chunks up to MAX_BODY_BYTES. Loading the full body
        // before capping would let a large response allocate hundreds of MiB
        // before truncation fires; stopping at the cap closes the connection
        // early and bounds RSS.
        let mut buf = Vec::with_capacity(MAX_BODY_BYTES.min(65_536));
        let mut truncated = false;
        loop {
            match response
                .chunk()
                .await
                .map_err(|err| ToolError::Invocation {
                    name: "WebFetch".to_string(),
                    source: Box::new(err),
                })? {
                None => break,
                Some(chunk) => {
                    let remaining = MAX_BODY_BYTES.saturating_sub(buf.len());
                    if remaining == 0 {
                        truncated = true;
                        break;
                    }
                    let take = chunk.len().min(remaining);
                    buf.extend_from_slice(&chunk[..take]);
                    if take < chunk.len() {
                        truncated = true;
                        break;
                    }
                },
            }
        }
        let body = String::from_utf8_lossy(&buf).into_owned();

        let mut out = format!("status: {status}\ncontent-type: {content_type}\n\n{body}");
        if truncated {
            out.push_str("\n[truncated at 1 MiB]");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn missing_url_arg_returns_invalid_args() {
        let handler = WebFetchHandler::default();
        let err = handler
            .invoke(ToolInvocation::for_test("call-1"), json!({}))
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidArgs { name, message } => {
                assert_eq!(name, "WebFetch");
                assert!(message.contains("url"), "unexpected message: {message}");
            },
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_string_url_returns_invalid_args() {
        let handler = WebFetchHandler::default();
        let err = handler
            .invoke(ToolInvocation::for_test("call-1"), json!({ "url": 42 }))
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidArgs { name, message } => {
                assert_eq!(name, "WebFetch");
                assert!(message.contains("url"), "unexpected message: {message}");
            },
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[test]
    fn schema_contains_expected_fields() {
        let schema = WebFetchHandler::default().schema();

        assert_eq!(
            schema["type"].as_str(),
            Some("object"),
            "schema root must be an object: {schema}"
        );

        let properties = schema["properties"]
            .as_object()
            .expect("schema must expose a properties object");
        assert!(
            properties.contains_key("url"),
            "properties missing url: {schema}"
        );
        assert!(
            properties.contains_key("timeout_secs"),
            "properties missing timeout_secs: {schema}"
        );

        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("schema must expose a required array")
            .iter()
            .map(|v| v.as_str().expect("required entries are strings"))
            .collect();
        assert!(
            required.contains(&"url"),
            "`url` must be required: {required:?}"
        );
        assert!(
            !required.contains(&"timeout_secs"),
            "timeout_secs must be optional: {required:?}"
        );
    }

    #[test]
    fn default_timeout_used_when_unset() {
        let args: WebFetchArgs = serde_json::from_value(json!({ "url": "https://example.com" }))
            .expect("WebFetchArgs must accept a body without timeout_secs");
        assert_eq!(args.url, "https://example.com");
        assert!(
            args.timeout_secs.is_none(),
            "timeout_secs must default to None when omitted"
        );
    }

    #[test]
    fn new_with_custom_default_timeout_is_retained() {
        let handler = WebFetchHandler::new(Some(Duration::from_secs(7)));
        assert_eq!(handler.default_timeout, Duration::from_secs(7));
    }

    #[test]
    fn new_with_none_falls_back_to_30_seconds() {
        let handler = WebFetchHandler::new(None);
        assert_eq!(
            handler.default_timeout,
            Duration::from_secs(DEFAULT_TIMEOUT_SECS)
        );
    }

    #[tokio::test]
    async fn invalid_url_returns_invocation_error() {
        let handler = WebFetchHandler::default();
        let err = handler
            .invoke(
                ToolInvocation::for_test("call-1"),
                json!({ "url": "not-a-url" }),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Invocation { name, .. } => assert_eq!(name, "WebFetch"),
            other => panic!("expected Invocation, got {other:?}"),
        }
    }

    #[tokio::test]
    #[ignore = "requires network — run with: cargo test -- --ignored"]
    async fn fetches_real_url() {
        let handler = WebFetchHandler::default();
        let args = json!({ "url": "https://example.com" });
        let out = handler
            .invoke(ToolInvocation::for_test("call-1"), args)
            .await
            .unwrap();
        assert!(out.starts_with("status: 200"));
        assert!(out.contains("content-type:"));
    }

    #[test]
    fn host_matches_domain_exact_and_suffix() {
        // Exact match.
        assert!(host_matches_domain("evil.test", "evil.test"));
        // Suffix match — `.evil.test` boundary must hold.
        assert!(host_matches_domain("sub.evil.test", "evil.test"));
        assert!(host_matches_domain("a.b.evil.test", "evil.test"));
        // Substring without dot boundary must NOT match — closes the
        // `notevil.test` / `evil.test` footgun.
        assert!(!host_matches_domain("notevil.test", "evil.test"));
        // Disjoint hosts.
        assert!(!host_matches_domain("good.test", "evil.test"));
    }

    #[test]
    fn webfetch_redirect_to_blocked_domain_denied() {
        // F5: redirect closure must reject hops whose host suffix-matches
        // `blocked_domains`. Tested via the pure helper because reqwest's
        // `redirect::Attempt` cannot be constructed from outside the crate.
        let blocked = vec!["evil.test".to_string()];
        let allowed: Vec<String> = Vec::new();

        let decision = evaluate_redirect_host("evil.test", &allowed, &blocked);
        assert_eq!(
            decision,
            RedirectDomainDecision::DenyBlocked("evil.test".to_string())
        );

        // Subdomain must be denied too — naive equality would let it through.
        let decision = evaluate_redirect_host("sub.evil.test", &allowed, &blocked);
        assert_eq!(
            decision,
            RedirectDomainDecision::DenyBlocked("sub.evil.test".to_string())
        );

        // A host that is not a suffix match passes.
        assert_eq!(
            evaluate_redirect_host("good.test", &allowed, &blocked),
            RedirectDomainDecision::Allow,
        );
    }

    #[test]
    fn webfetch_redirect_outside_allowlist_denied() {
        // F5: when `allowed_domains` is non-empty, any host outside it
        // must be rejected. Empty allowlist means "no restriction" —
        // every non-blocked host is allowed.
        let allowed = vec!["good.test".to_string()];
        let blocked: Vec<String> = Vec::new();

        // Outside the allowlist → denied.
        assert_eq!(
            evaluate_redirect_host("bad.test", &allowed, &blocked),
            RedirectDomainDecision::DenyOutsideAllowed("bad.test".to_string()),
        );

        // Exact allowlist match → allowed.
        assert_eq!(
            evaluate_redirect_host("good.test", &allowed, &blocked),
            RedirectDomainDecision::Allow,
        );

        // Subdomain of allowlist entry → allowed (suffix match).
        assert_eq!(
            evaluate_redirect_host("api.good.test", &allowed, &blocked),
            RedirectDomainDecision::Allow,
        );

        // Empty allowlist → all hosts pass when not on the blocklist.
        let empty_allow: Vec<String> = Vec::new();
        assert_eq!(
            evaluate_redirect_host("anything.test", &empty_allow, &blocked),
            RedirectDomainDecision::Allow,
        );
    }

    #[test]
    fn webfetch_redirect_blocklist_takes_precedence_over_allowlist() {
        // Hosts that match both lists must be denied — `blocked_domains`
        // wins. Without this, an operator who mistakenly added a domain
        // to both lists would silently allow it.
        let allowed = vec!["evil.test".to_string()];
        let blocked = vec!["evil.test".to_string()];
        assert_eq!(
            evaluate_redirect_host("evil.test", &allowed, &blocked),
            RedirectDomainDecision::DenyBlocked("evil.test".to_string()),
        );
    }

    #[test]
    fn with_domains_constructor_wires_timeout() {
        // Smoke test: the new constructor wires the timeout through and the
        // `reqwest::Client` builds successfully with a non-empty domain policy.
        // Domain lists are moved into the redirect closure and not stored on the
        // struct, so only the timeout is inspectable here; redirect behavior is
        // verified in the `blocked_redirect_*` and `allowed_redirect_*` tests.
        let handler = WebFetchHandler::with_domains(
            Some(Duration::from_secs(11)),
            vec!["good.test".into()],
            vec!["evil.test".into()],
        );
        assert_eq!(handler.default_timeout, Duration::from_secs(11));
    }

    #[test]
    fn default_handler_builds_without_panic() {
        // Backward-compat: the no-arg `Default` and `new(None)` paths must
        // succeed (no domain restrictions baked into the closure, only the IP
        // block list enforced on hops, matching pre-F5 behavior).
        let _handler = WebFetchHandler::default();
    }
}
