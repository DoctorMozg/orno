//! Real MCP server integration test using `@modelcontextprotocol/server-filesystem`.
//!
//! These tests are gated behind `#[ignore]` — they require Node.js 24+ (the
//! pinned server's `engines` floor) and an npm package install that is not
//! guaranteed in CI. Run them explicitly with:
//!
//!   cargo test -p orno-core --test `mcp_real` -- --ignored
//!
//! They exercise `RmcpClient` against the actual MCP protocol
//! rather than the inline Python fake in `mcp_fake.rs`.
#![allow(clippy::print_stderr)] // skip-tracing in integration tests

use orno_core::mcp::{McpClient, RmcpClient};
use orno_core::pipeline::schema::McpStdioConfig;

/// Pinned so a new upstream release cannot change the server under test.
const FILESYSTEM_SERVER_PACKAGE: &str = "@modelcontextprotocol/server-filesystem@2.3.0";

/// Show debug tracing in the test output, so a failed handshake carries the
/// server's stderr lines that `RmcpClient` forwards at debug level.
fn init_test_tracing() {
    drop(
        tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::DEBUG)
            .try_init(),
    );
}

fn npx_available() -> bool {
    std::process::Command::new("npx")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn filesystem_server_config(allowed_dir: &str) -> McpStdioConfig {
    McpStdioConfig {
        command: vec![
            "npx".to_string(),
            "-y".to_string(),
            FILESYSTEM_SERVER_PACKAGE.to_string(),
            allowed_dir.to_string(),
        ],
        env: std::collections::BTreeMap::new(),
        call_timeout_secs: None,
    }
}

#[tokio::test]
#[ignore = "requires Node.js + @modelcontextprotocol/server-filesystem; run with --ignored"]
async fn real_filesystem_server_initialize_returns_tools() {
    init_test_tracing();
    if !npx_available() {
        eprintln!("skipping: npx not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = filesystem_server_config(tmp.path().to_str().expect("utf8 path"));

    let mut client = RmcpClient::new_stdio("filesystem".to_string(), &cfg);
    let tools = client
        .initialize()
        .await
        .expect("initialize should succeed");

    assert!(
        !tools.is_empty(),
        "filesystem server should advertise tools"
    );
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names
            .iter()
            .any(|n| n.contains("read") || n.contains("list")),
        "expected at least one read/list tool, got: {names:?}"
    );

    client.shutdown().await.expect("shutdown should succeed");
}

#[tokio::test]
#[ignore = "requires Node.js + @modelcontextprotocol/server-filesystem; run with --ignored"]
async fn real_filesystem_server_list_directory() {
    init_test_tracing();
    if !npx_available() {
        eprintln!("skipping: npx not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    // Write a test file so list_directory has something to return.
    std::fs::write(tmp.path().join("hello.txt"), "world").expect("write test file");

    let cfg = filesystem_server_config(tmp.path().to_str().expect("utf8 path"));
    let mut client = RmcpClient::new_stdio("filesystem".to_string(), &cfg);
    let tools = client
        .initialize()
        .await
        .expect("initialize should succeed");

    // Find a list_directory or list tool.
    let list_tool = tools
        .iter()
        .find(|t| t.name.contains("list"))
        .expect("filesystem server should have a list tool");

    let result = client
        .call_tool(
            &list_tool.name,
            serde_json::json!({ "path": tmp.path().to_str().expect("utf8") }),
        )
        .await
        .expect("call_tool should succeed");

    assert!(result.ok, "list tool should succeed: {:?}", result.content);
    assert!(
        result.content.contains("hello.txt"),
        "listing should contain hello.txt: {}",
        result.content
    );

    client.shutdown().await.expect("shutdown should succeed");
}
