//! `WebFetch` tool — HTTP GET a URL. Requires `allow_network`.
//! `LoopAgent` gates the first URL; this handler gates every redirect hop.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tracing::instrument;

use super::net_guard::{
    DomainRules, DomainVerdict, GuardedDns, SystemResolver, is_blocked_ip, literal_ip,
};
use super::{ToolEffect, ToolHandler, ToolInvocation};
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
/// The client follows no redirects itself: `invoke` walks each hop and
/// applies the calling agent's domain lists (`ToolInvocation::domains`)
/// plus the literal-IP block list before the next GET. Hostnames are
/// vetted at connect time by [`GuardedDns`], so the address that passes
/// the block list is the one dialed. Proxies are disabled because a
/// proxy resolves the destination itself and would sidestep both checks.
#[derive(Debug, Clone)]
pub struct WebFetchHandler {
    client: reqwest::Client,
    default_timeout: Duration,
}

impl WebFetchHandler {
    /// Construct a handler with a single shared `reqwest::Client`.
    ///
    /// `default_timeout` is the timeout applied to a request that does
    /// not supply its own `timeout_secs` argument; passing `None` falls
    /// back to the built-in 30-second default.
    #[must_use]
    pub fn new(default_timeout: Option<Duration>) -> Self {
        let dns = GuardedDns::new(Arc::new(SystemResolver));
        Self::build(default_timeout, dns)
    }

    fn build(default_timeout: Option<Duration>, dns: GuardedDns) -> Self {
        let default_timeout =
            default_timeout.unwrap_or_else(|| Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        // `build()` only fails when TLS backend init fails, which is a
        // startup-fatal misconfiguration, same as `Client::new()`.
        let client = reqwest::Client::builder()
            .timeout(default_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(dns)
            .no_proxy()
            .build()
            .expect("default reqwest client must build");
        Self {
            client,
            default_timeout,
        }
    }

    /// Handler whose DNS is replaced and unfiltered, so a test can point
    /// fake hostnames at a loopback server. Literal IPs stay blocked.
    #[cfg(test)]
    fn for_test(resolver: Arc<dyn super::net_guard::HostResolver>) -> Self {
        fn allow_all(_: std::net::IpAddr) -> bool {
            false
        }
        let dns = GuardedDns::with_filter(resolver, allow_all);
        Self::build(None, dns)
    }

    /// Reject a URL the handler must not request, whether it is the
    /// initial one or a redirect target.
    fn check_url(url: &reqwest::Url, domains: DomainRules<'_>) -> Result<(), ToolError> {
        let deny = |reason: String| ToolError::Invocation {
            name: "WebFetch".to_string(),
            source: Box::new(std::io::Error::other(reason)),
        };
        let scheme = url.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(deny(format!("scheme `{scheme}` not permitted")));
        }
        if let Some(ip) = literal_ip(url)
            && is_blocked_ip(ip)
        {
            return Err(deny(format!("`{ip}` is a blocked address")));
        }
        let host = url.host_str().unwrap_or_default();
        match domains.check(host) {
            DomainVerdict::Blocked => Err(deny(format!("`{host}` matches blocked_domains"))),
            DomainVerdict::NotAllowed => Err(deny(format!("`{host}` is outside allowed_domains"))),
            _ => Ok(()),
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
    #[instrument(skip(self, inv, args), fields(tool.name = "WebFetch", tool.call_id = %inv.call_id))]
    async fn invoke(&self, inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        // Retain the args' field names (not values) so the error message
        // pins the offending field even when serde only reports a
        // type-level mismatch. Values are omitted to avoid echoing
        // caller-supplied payloads into the log.
        let fields = arg_field_names(&args);
        let WebFetchArgs { url, timeout_secs } =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
                name: "WebFetch".to_string(),
                message: format!("{e} (fields: {fields})"),
            })?;

        let request_timeout = timeout_secs.map_or(self.default_timeout, Duration::from_secs);

        let mut current_url = reqwest::Url::parse(&url).map_err(|e| ToolError::Invocation {
            name: "WebFetch".to_string(),
            source: Box::new(e),
        })?;
        Self::check_url(&current_url, inv.domains)?;

        let mut hops: usize = 0;
        let mut response = loop {
            let response = self
                .client
                .get(current_url.as_str())
                .timeout(request_timeout)
                .send()
                .await
                .map_err(|err| ToolError::Invocation {
                    name: "WebFetch".to_string(),
                    source: Box::new(err),
                })?;

            if !response.status().is_redirection() {
                break response;
            }

            if hops >= MAX_REDIRECTS {
                return Err(ToolError::Invocation {
                    name: "WebFetch".to_string(),
                    source: Box::new(std::io::Error::other(format!(
                        "too many redirects (>{MAX_REDIRECTS})"
                    ))),
                });
            }

            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| ToolError::Invocation {
                    name: "WebFetch".to_string(),
                    source: Box::new(std::io::Error::other(
                        "redirect response missing Location header",
                    )),
                })?
                .to_string();

            let next_url = current_url
                .join(&location)
                .map_err(|e| ToolError::Invocation {
                    name: "WebFetch".to_string(),
                    source: Box::new(std::io::Error::other(format!(
                        "invalid redirect URL `{location}`: {e}"
                    ))),
                })?;

            Self::check_url(&next_url, inv.domains).inspect_err(|err| {
                tracing::warn!(
                    redirect.from = %current_url,
                    redirect.to = %next_url,
                    error = %err,
                    "WebFetch redirect denied",
                );
            })?;

            tracing::debug!(
                redirect.from = %current_url,
                redirect.to = %next_url,
                redirect.hop = hops + 1,
                "WebFetch following redirect",
            );

            current_url = next_url;
            hops += 1;
        };

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
    use crate::tool::net_guard::HostResolver;
    use rstest::rstest;
    use serde_json::json;
    use std::net::IpAddr;

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

    /// Maps every hostname to one fixed address.
    #[derive(Debug)]
    struct FixedResolver(IpAddr);

    #[async_trait]
    impl HostResolver for FixedResolver {
        async fn resolve(&self, _host: &str) -> std::io::Result<Vec<IpAddr>> {
            Ok(vec![self.0])
        }
    }

    fn loopback_handler() -> WebFetchHandler {
        WebFetchHandler::for_test(Arc::new(FixedResolver(IpAddr::from([127, 0, 0, 1]))))
    }

    /// Server that 302s every GET to `location`, with `{port}` replaced
    /// by the server's own port.
    async fn server_redirecting_to(location: &str) -> wiremock::MockServer {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let location = location.replace("{port}", &server.address().port().to_string());
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", location))
            .mount(&server)
            .await;
        server
    }

    fn failure_message(err: ToolError) -> String {
        match err {
            ToolError::Invocation { name, source } => {
                assert_eq!(name, "WebFetch");
                source.to_string()
            },
            other => panic!("expected Invocation, got {other:?}"),
        }
    }

    #[rstest]
    #[case::outside_allowlist("http://other.test/x", &["good.test"], &[], "outside allowed_domains")]
    #[case::blocked_domain("http://evil.test/x", &["good.test", "evil.test"], &["evil.test"], "blocked_domains")]
    #[case::blocked_domain_trailing_dot("http://evil.test./x", &[], &["evil.test"], "blocked_domains")]
    #[case::metadata_ip("http://169.254.169.254/x", &[], &[], "blocked address")]
    #[case::ipv6_loopback("http://[::1]/x", &[], &[], "blocked address")]
    #[tokio::test]
    async fn redirect_from_allowlisted_host_is_denied(
        #[case] target: &str,
        #[case] allowed: &[&str],
        #[case] blocked: &[&str],
        #[case] expected: &str,
    ) {
        let server = server_redirecting_to(target).await;
        let allowed: Vec<String> = allowed.iter().map(ToString::to_string).collect();
        let blocked: Vec<String> = blocked.iter().map(ToString::to_string).collect();
        let mut inv = ToolInvocation::for_test("call-redirect");
        inv.domains = DomainRules {
            allowed: &allowed,
            blocked: &blocked,
        };
        // `good.test` is allowlisted; the fake resolver sends it to wiremock.
        let url = format!("http://good.test:{}/start", server.address().port());

        let err = loopback_handler()
            .invoke(inv, json!({ "url": url }))
            .await
            .expect_err("redirect off the policy must be denied");

        let msg = failure_message(err);
        assert!(msg.contains(expected), "got: {msg}");
        let requests = server.received_requests().await.expect("recording is on");
        assert_eq!(requests.len(), 1, "the redirect target must not be fetched");
    }

    #[tokio::test]
    async fn allowlisted_redirect_within_the_allowlist_is_followed() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let port = server.address().port();
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("http://api.good.test:{port}/end")),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/end"))
            .respond_with(ResponseTemplate::new(200).set_body_string("arrived"))
            .mount(&server)
            .await;

        let allowed = vec!["good.test".to_string()];
        let mut inv = ToolInvocation::for_test("call-ok");
        inv.domains = DomainRules {
            allowed: &allowed,
            blocked: &[],
        };
        let out = loopback_handler()
            .invoke(
                inv,
                json!({ "url": format!("http://good.test:{port}/start") }),
            )
            .await
            .expect("in-policy redirect must be followed");
        assert!(
            out.starts_with("status: 200") && out.contains("arrived"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn redirect_chain_stops_at_the_hop_limit() {
        // Every request 302s to itself, so only MAX_REDIRECTS ends the walk.
        let server = server_redirecting_to("http://loop.test:{port}/next").await;
        let url = format!("http://loop.test:{}/start", server.address().port());

        let err = loopback_handler()
            .invoke(ToolInvocation::for_test("call-loop"), json!({ "url": url }))
            .await
            .expect_err("a redirect loop must not return Ok");

        let msg = failure_message(err);
        assert!(msg.contains("too many redirects"), "got: {msg}");
        let requests = server.received_requests().await.expect("recording is on");
        assert_eq!(requests.len(), MAX_REDIRECTS + 1);
    }

    #[tokio::test]
    async fn default_resolver_refuses_loopback_hostnames() {
        // `localhost` passes check_url as a name; only the connect-time
        // DNS guard can stop it.
        let err = WebFetchHandler::default()
            .invoke(
                ToolInvocation::for_test("call-localhost"),
                json!({ "url": "http://localhost:9/" }),
            )
            .await
            .expect_err("loopback hostname must not connect");
        let ToolError::Invocation { source, .. } = err else {
            panic!("expected Invocation");
        };
        let mut chain = source.to_string();
        let mut cause = source.source();
        while let Some(c) = cause {
            chain = format!("{chain} | {c}");
            cause = c.source();
        }
        assert!(
            chain.contains("resolves only to blocked addresses"),
            "got: {chain}"
        );
    }
}
