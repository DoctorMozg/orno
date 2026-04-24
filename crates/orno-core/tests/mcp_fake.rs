//! Integration tests for `RmcpClient` using a minimal in-process fake MCP
//! server written as an inline Python3 script. The script speaks the MCP
//! stdio protocol (newline-delimited JSON-RPC 2.0) and exposes one fake
//! tool called `echo`.
//!
//! Tests requiring a live server (real npm packages, etc.) live in
//! `mcp_real.rs` and are gated behind `#[ignore]`.
#![allow(clippy::print_stderr)] // skip-tracing in integration tests

use orno_core::mcp::{McpClient, RmcpClient};
use orno_core::pipeline::schema::McpStdioConfig;

/// A minimal Python3 MCP server. Handles `initialize`, the
/// `notifications/initialized` notification, `tools/list`, and `tools/call`.
const FAKE_MCP_SCRIPT: &str = r#"
import sys, json

def send(msg):
    line = json.dumps(msg)
    sys.stdout.write(line + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    msg = json.loads(raw)
    method = msg.get("method", "")
    mid = msg.get("id")

    if method == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{
            "protocolVersion":"2024-11-05",
            "capabilities":{},
            "serverInfo":{"name":"fake","version":"0.1"}
        }})
    elif method == "notifications/initialized":
        pass  # notification, no response
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":[{
            "name":"echo",
            "description":"Echo the message back",
            "inputSchema":{"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}
        }]}})
    elif method == "tools/call":
        args = msg.get("params",{}).get("arguments",{})
        text = args.get("message","(no message)")
        send({"jsonrpc":"2.0","id":mid,"result":{
            "content":[{"type":"text","text":text}],
            "isError":False
        }})
    elif method == "notifications/cancelled" or method == "notifications/progress":
        pass
"#;

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn fake_server_config() -> McpStdioConfig {
    McpStdioConfig {
        command: vec![
            "python3".to_string(),
            "-c".to_string(),
            FAKE_MCP_SCRIPT.to_string(),
        ],
        env: std::collections::BTreeMap::new(),
    }
}

#[tokio::test]
async fn rmcp_client_initialize_returns_tools() {
    if !python3_available() {
        eprintln!("skipping mcp_fake: python3 not available");
        return;
    }

    let cfg = fake_server_config();
    let mut client = RmcpClient::new_stdio("fake".to_string(), &cfg);
    let tools = client
        .initialize()
        .await
        .expect("initialize should succeed");

    assert_eq!(tools.len(), 1, "fake server advertises exactly one tool");
    assert_eq!(tools[0].name, "echo");
    assert!(!tools[0].description.is_empty());

    client.shutdown().await.expect("shutdown should succeed");
}

#[tokio::test]
async fn rmcp_client_call_tool_returns_content() {
    if !python3_available() {
        eprintln!("skipping mcp_fake: python3 not available");
        return;
    }

    let cfg = fake_server_config();
    let mut client = RmcpClient::new_stdio("fake".to_string(), &cfg);
    client
        .initialize()
        .await
        .expect("initialize should succeed");

    let args = serde_json::json!({"message": "hello from test"});
    let result = client
        .call_tool("echo", args)
        .await
        .expect("call_tool should succeed");

    assert!(result.ok, "echo tool should succeed: {result:?}");
    assert_eq!(result.content, "hello from test");

    client.shutdown().await.expect("shutdown should succeed");
}

#[tokio::test]
async fn rmcp_client_call_tool_before_initialize_returns_error() {
    let cfg = fake_server_config();
    let client = RmcpClient::new_stdio("fake".to_string(), &cfg);

    let err = client
        .call_tool("echo", serde_json::json!({"message": "x"}))
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(msg.contains("fake"), "error should name the server: {msg}");
}

#[tokio::test]
async fn rmcp_client_empty_command_returns_spawn_error() {
    let cfg = McpStdioConfig {
        command: vec![],
        env: std::collections::BTreeMap::new(),
    };
    let mut client = RmcpClient::new_stdio("empty".to_string(), &cfg);
    let err = client.initialize().await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("empty") || msg.to_lowercase().contains("spawn") || msg.contains("command"),
        "error should mention the server or spawn failure: {msg}"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// HTTP transport — wire-level tests
//
// The full streamable-HTTP MCP handshake involves session negotiation, JSON
// or SSE responses, and `Mcp-Session-Id` headers. That is far more than
// `wiremock` can fake without re-implementing the protocol. These tests
// instead pin the wire-level contract: the right URL is hit, the right
// headers go out, and config-time errors surface as `HandshakeFailed`.
// Wiremock returns 500 on every request so the handshake fails immediately
// after the first POST, which is enough to verify request shape via
// `.expect(1..)`. End-to-end protocol coverage lives in `mcp_real.rs`
// against actual MCP servers.
// ──────────────────────────────────────────────────────────────────────────────

mod http_transport {
    use orno_core::mcp::{McpClient, RmcpClient};
    use orno_core::pipeline::schema::{McpAuthConfig, McpHttpConfig};
    use std::collections::BTreeMap;
    use std::error::Error as _;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A 500 response is the simplest way to short-circuit the rmcp
    /// streamable-HTTP handshake: the transport bails with `OtherStatus`
    /// before content-type negotiation, which propagates up to
    /// `HandshakeFailed`. That's exactly the failure mode we want — we
    /// only care that the request was sent, not that the handshake
    /// completed.
    fn fail_with_500() -> ResponseTemplate {
        ResponseTemplate::new(500)
    }

    #[tokio::test]
    async fn http_initialize_posts_to_configured_url() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(fail_with_500())
            .expect(1u64..)
            .mount(&server)
            .await;

        let cfg = McpHttpConfig {
            url: format!("{}/mcp", server.uri()),
            auth: None,
            headers: BTreeMap::new(),
        };

        let mut client = RmcpClient::new_http("http-server".to_string(), &cfg);
        let err = client
            .initialize()
            .await
            .expect_err("500 from server must surface as a handshake error");

        let msg = err.to_string();
        assert!(
            msg.contains("handshake failed") && msg.contains("http-server"),
            "expected HandshakeFailed naming the server, got: {msg}"
        );
    }

    #[tokio::test]
    async fn http_initialize_sends_bearer_auth_header() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(header("authorization", "Bearer test-token-abc"))
            .respond_with(fail_with_500())
            .expect(1u64..)
            .mount(&server)
            .await;

        let cfg = McpHttpConfig {
            url: format!("{}/mcp", server.uri()),
            auth: Some(McpAuthConfig::Bearer {
                token: "test-token-abc".to_string(),
            }),
            headers: BTreeMap::new(),
        };

        let mut client = RmcpClient::new_http("http-bearer".to_string(), &cfg);
        // We expect the handshake to fail (server returned 500). The real
        // assertion is on `MockServer::drop` — the `.expect(1u64..)` matcher
        // panics if no request with the right Authorization header arrived.
        assert!(
            client.initialize().await.is_err(),
            "500 from server must surface as an error"
        );
    }

    #[tokio::test]
    async fn http_initialize_forwards_custom_headers() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(header("x-orno-test", "demo-123"))
            .respond_with(fail_with_500())
            .expect(1u64..)
            .mount(&server)
            .await;

        let mut headers = BTreeMap::new();
        headers.insert("X-Orno-Test".to_string(), "demo-123".to_string());

        let cfg = McpHttpConfig {
            url: format!("{}/mcp", server.uri()),
            auth: None,
            headers,
        };

        let mut client = RmcpClient::new_http("http-headers".to_string(), &cfg);
        assert!(
            client.initialize().await.is_err(),
            "500 from server must surface as an error"
        );
    }

    #[tokio::test]
    async fn http_basic_auth_returns_unsupported_with_guidance() {
        let cfg = McpHttpConfig {
            // Port 1 will refuse on every platform; we never get there
            // because Basic auth is rejected before any network call.
            url: "http://127.0.0.1:1/mcp".to_string(),
            auth: Some(McpAuthConfig::Basic {
                user: "u".to_string(),
                password: "p".to_string(),
            }),
            headers: BTreeMap::new(),
        };

        let mut client = RmcpClient::new_http("http-basic".to_string(), &cfg);
        let err = client
            .initialize()
            .await
            .expect_err("Basic auth must surface as UnsupportedTransport");

        let msg = err.to_string();
        assert!(
            msg.contains("not supported"),
            "expected UnsupportedTransport, got: {msg}"
        );
        assert!(
            msg.contains("bearer"),
            "operator-facing message must point at the bearer-auth alternative: {msg}"
        );
    }

    #[tokio::test]
    async fn http_invalid_header_name_returns_handshake_failed() {
        // A space inside the header name is invalid per RFC 7230 and
        // `HeaderName::try_from` will reject it before we ever spawn a
        // request.
        let mut headers = BTreeMap::new();
        headers.insert("Bad Header".to_string(), "value".to_string());

        let cfg = McpHttpConfig {
            url: "http://127.0.0.1:1/mcp".to_string(),
            auth: None,
            headers,
        };

        let mut client = RmcpClient::new_http("http-bad-header".to_string(), &cfg);
        let err = client
            .initialize()
            .await
            .expect_err("invalid header name must surface as a HandshakeFailed");

        let top = err.to_string();
        assert!(
            top.contains("handshake failed") && top.contains("http-bad-header"),
            "top-level error should be HandshakeFailed naming the server: {top}"
        );

        let source = err
            .source()
            .expect("HandshakeFailed should carry the validation error as its source");
        let inner = source.to_string();
        assert!(
            inner.contains("invalid http header name") && inner.contains("Bad Header"),
            "source error should name the offending header: {inner}"
        );
    }
}
