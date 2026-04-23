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

#[tokio::test]
async fn rmcp_client_http_returns_unsupported() {
    use orno_core::pipeline::schema::{McpAuthConfig, McpHttpConfig};

    let cfg = McpHttpConfig {
        url: "http://localhost:9999".to_string(),
        auth: Some(McpAuthConfig::None),
        headers: std::collections::BTreeMap::new(),
    };
    let mut client = RmcpClient::new_http("http-server".to_string(), &cfg);
    let err = client.initialize().await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("unsupported") || msg.to_lowercase().contains("http"),
        "error should indicate unsupported transport: {msg}"
    );
}
