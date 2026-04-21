//! Shell node executor. Spawns a child via `tokio::process::Command`
//! and captures `stdout`, `stderr`, and `exit_code` into the node
//! response payload. No shell is interposed; `command` is the
//! program name, `args` are argv entries.

use std::process::Stdio;

use async_trait::async_trait;
use serde_json::json;
use tokio::process::Command;
use tracing::instrument;

use crate::error::NodeError;

use super::{NodeExecutor, NodeRequest, NodeResponse, ShellNodeRequest};

#[derive(Debug, Default, Clone)]
pub struct ShellExecutor;

#[async_trait]
impl NodeExecutor for ShellExecutor {
    #[instrument(
        skip(self, req),
        fields(node.id = %id, node.kind = "shell"),
    )]
    async fn execute(&self, id: &str, req: NodeRequest) -> Result<NodeResponse, NodeError> {
        let NodeRequest::Shell(ShellNodeRequest { command, args }) = req else {
            return Err(NodeError::Execution {
                id: id.to_string(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "ShellExecutor received non-shell NodeRequest",
                )
                .into(),
            });
        };

        // stdin is nulled so a child reading stdin cannot hang the pipeline
        // in CI; kill_on_drop keeps the child tree from outliving orno if
        // the parent process is interrupted.
        let output = Command::new(&command)
            .args(&args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| NodeError::Execution {
                id: id.to_string(),
                source: Box::new(e),
            })?;

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

    #[tokio::test]
    async fn echoes_stdout() {
        let exec = ShellExecutor;
        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "echo".to_string(),
            args: vec!["hi".to_string()],
        });

        let resp = exec.execute("test_node", req).await.unwrap();

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
        });

        let resp = exec
            .execute("fail_node", req)
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
        });

        let err = exec
            .execute("wrong_kind", req)
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
        });

        let err = exec
            .execute("unknown", req)
            .await
            .expect_err("spawn failure should return Err");

        match err {
            NodeError::Execution { id, source: _ } => {
                assert_eq!(id, "unknown");
            }
            other => panic!("expected NodeError::Execution, got {other:?}"),
        }
    }
}
