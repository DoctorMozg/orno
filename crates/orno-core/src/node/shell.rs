//! Shell node executor. Spawns a child via `tokio::process::Command`
//! and captures `stdout`, `stderr`, and `exit_code` into the node
//! response payload. No shell is interposed; `command` is the
//! program name, `args` are argv entries.

use std::process::Stdio;

use async_trait::async_trait;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{instrument, warn};

use crate::error::NodeError;

use super::{NodeExecutor, NodeRequest, NodeResponse, ShellNodeRequest};

#[derive(Debug, Default, Clone)]
pub struct ShellExecutor;

#[async_trait]
impl NodeExecutor for ShellExecutor {
    #[instrument(
        skip(self, req),
        fields(node.id = %id, node.kind = "shell", pipeline.run_id = %_run_id),
    )]
    async fn execute(
        &self,
        _run_id: &str,
        id: &str,
        req: NodeRequest,
    ) -> Result<NodeResponse, NodeError> {
        let NodeRequest::Shell(ShellNodeRequest {
            command,
            args,
            stdin,
        }) = req
        else {
            return Err(NodeError::Execution {
                id: id.to_string(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "ShellExecutor received non-shell NodeRequest",
                )
                .into(),
            });
        };

        // kill_on_drop keeps the child tree from outliving orno if the
        // parent process is interrupted. stdin defaults to `Stdio::null()`
        // so a child reading stdin cannot hang the pipeline in CI; it is
        // upgraded to `Stdio::piped()` only when the node declares a
        // stdin payload.
        let mut cmd = Command::new(&command);
        cmd.args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if stdin.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }

        let mut child = cmd.spawn().map_err(|e| NodeError::Execution {
            id: id.to_string(),
            source: Box::new(e),
        })?;

        // When a stdin payload is present, drive it from a concurrent
        // task so the parent can drain stdout/stderr via
        // `wait_with_output` at the same time. Writing stdin inline
        // would deadlock against a child that produces enough output
        // to fill its pipe buffer before it finishes reading stdin.
        // A broken-pipe write is expected whenever the child exits
        // before consuming everything (e.g. `head`) — log and move on;
        // the child's own exit code tells the real story.
        let writer_handle = if let Some(payload) = stdin {
            let mut child_stdin = child.stdin.take().expect("stdin was piped");
            Some(tokio::spawn(async move {
                let res = child_stdin.write_all(payload.as_bytes()).await;
                let _ = child_stdin.shutdown().await;
                res
            }))
        } else {
            None
        };

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| NodeError::Execution {
                id: id.to_string(),
                source: Box::new(e),
            })?;

        if let Some(handle) = writer_handle {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(err)) if err.kind() == std::io::ErrorKind::BrokenPipe => {
                    warn!(node.id = %id, "child closed stdin before full payload written");
                }
                Ok(Err(err)) => {
                    warn!(node.id = %id, error = %err, "stdin writer failed");
                }
                Err(join_err) => {
                    warn!(node.id = %id, error = %join_err, "stdin writer task panicked");
                }
            }
        }

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let exit_code = output.status.code().unwrap_or(-1);

        Ok(NodeResponse {
            node_id: id.to_string(),
            output: json!({
                "stdout": stdout,
                "stderr": stderr,
                "exit_code": exit_code,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::pipeline::{AgentPolicy, OnParseError};

    fn test_policy() -> AgentPolicy {
        AgentPolicy {
            max_iterations: 1,
            max_total_tokens: 0,
            max_tool_calls: 0,
            max_subagent_depth: 0,
            allow_mutations: false,
            allow_network: false,
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
            on_parse_error: OnParseError::Fail,
        }
    }

    #[tokio::test]
    async fn echoes_stdout() {
        let exec = ShellExecutor;
        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "echo".to_string(),
            args: vec!["hi".to_string()],
            stdin: None,
        });

        let resp = exec.execute("run_test", "test_node", req).await.unwrap();

        assert_eq!(resp.node_id, "test_node");
        let stdout = resp.output["stdout"].as_str().unwrap();
        assert!(
            stdout.contains("hi"),
            "stdout should contain 'hi': {stdout:?}"
        );
        assert_eq!(resp.output["exit_code"], json!(0));
    }

    // `false` is a POSIX utility; Windows has no equivalent builtin that
    // is guaranteed on PATH, so we gate this to unix.
    #[cfg(unix)]
    #[tokio::test]
    async fn captures_non_zero_exit() {
        let exec = ShellExecutor;
        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "false".to_string(),
            args: Vec::new(),
            stdin: None,
        });

        let resp = exec
            .execute("run_test", "fail_node", req)
            .await
            .expect("execute should return Ok on non-zero exit");

        let code = resp.output["exit_code"].as_i64().expect("exit_code is i64");
        assert_ne!(
            code, 0,
            "exit_code for `false` should be non-zero: got {code}"
        );
    }

    #[tokio::test]
    async fn agent_request_rejected_as_execution_error() {
        // Defensive regression guard: if a scheduler bug ever routes an
        // agent payload into the shell executor, `execute` must return
        // NodeError::Execution (carrying the node id) rather than panic
        // or silently spawn something.
        use super::super::AgentNodeRequest;
        let exec = ShellExecutor;
        let req = NodeRequest::Agent(AgentNodeRequest {
            agent: "x".to_string(),
            initial_prompt: String::new(),
            system: None,
            provider: "openai".into(),
            model: "gpt-5".into(),
            policy: test_policy(),
            allowed_tools: Vec::new(),
        });

        let err = exec
            .execute("run_test", "wrong_kind", req)
            .await
            .expect_err("agent request must be rejected");

        match err {
            NodeError::Execution { id, source: _ } => {
                assert_eq!(id, "wrong_kind");
            }
            other => panic!("expected NodeError::Execution, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_program_is_execution_error() {
        let exec = ShellExecutor;
        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "definitely-not-a-real-program-xyz-12345".to_string(),
            args: Vec::new(),
            stdin: None,
        });

        let err = exec
            .execute("run_test", "unknown", req)
            .await
            .expect_err("spawn failure should return Err");

        match err {
            NodeError::Execution { id, source: _ } => {
                assert_eq!(id, "unknown");
            }
            other => panic!("expected NodeError::Execution, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdin_content_is_piped_to_child() {
        let exec = ShellExecutor;
        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "cat".to_string(),
            args: Vec::new(),
            stdin: Some("hello over stdin\n".to_string()),
        });

        let resp = exec.execute("run_test", "cat_node", req).await.unwrap();

        assert_eq!(resp.output["stdout"], json!("hello over stdin\n"));
        assert_eq!(resp.output["exit_code"], json!(0));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdin_none_matches_prior_null_behavior() {
        // Regression guard for the pre-stdin contract: `None` must keep
        // stdin closed. Reading stdin of a null pipe returns EOF
        // immediately, so cat exits cleanly with empty stdout.
        let exec = ShellExecutor;
        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "cat".to_string(),
            args: Vec::new(),
            stdin: None,
        });

        let resp = exec.execute("run_test", "cat_node", req).await.unwrap();

        assert_eq!(resp.output["stdout"], json!(""));
        assert_eq!(resp.output["exit_code"], json!(0));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdin_large_payload_does_not_deadlock() {
        // A payload larger than a typical pipe buffer (64 KiB on Linux,
        // 16 KiB on macOS) would deadlock if we wrote stdin inline and
        // the child produced stdout faster than we consumed it. The
        // concurrent writer task exists to prevent exactly this.
        let payload = "A".repeat(256 * 1024);
        let exec = ShellExecutor;
        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "cat".to_string(),
            args: Vec::new(),
            stdin: Some(payload.clone()),
        });

        let resp = exec.execute("run_test", "big", req).await.unwrap();

        assert_eq!(resp.output["exit_code"], json!(0));
        assert_eq!(
            resp.output["stdout"].as_str().unwrap().len(),
            payload.len(),
            "round-trip size must match"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdin_broken_pipe_is_tolerated() {
        // `head -c 4` consumes four bytes and exits; the rest of our
        // stdin write fails with EPIPE. That failure must not bubble
        // up as a NodeError — the child's own exit status is the
        // source of truth.
        let exec = ShellExecutor;
        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "head".to_string(),
            args: vec!["-c".to_string(), "4".to_string()],
            stdin: Some("A".repeat(256 * 1024)),
        });

        let resp = exec.execute("run_test", "head_node", req).await.unwrap();

        assert_eq!(resp.output["exit_code"], json!(0));
        assert_eq!(resp.output["stdout"], json!("AAAA"));
    }
}
