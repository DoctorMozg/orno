//! `Bash` tool — run a shell command (ADR 0008). Requires both
//! `allow_mutations` and `allow_network`.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;
use tracing::{debug, instrument};

use super::{ToolEffect, ToolHandler, ToolInvocation};
use crate::error::ToolError;

/// Default `timeout_secs` when the caller omits the field.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Default, Clone)]
pub struct BashHandler;

#[async_trait]
impl ToolHandler for BashHandler {
    fn name(&self) -> &str {
        "Bash"
    }
    fn description(&self) -> &str {
        "Run a shell command via /bin/sh -c and return stdout and stderr."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute." },
                "timeout_secs": { "type": "integer", "description": "Max seconds to wait. Defaults to 60." },
                "cwd": { "type": "string", "description": "Working directory override." }
            },
            "required": ["command"]
        })
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::MutationsAndNetwork
    }

    #[instrument(skip(self, args), fields(tool.name = "Bash", tool.call_id = %inv.call_id))]
    async fn invoke(&self, inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "Bash".to_string(),
                message: "missing or non-string `command`".to_string(),
            })?
            .to_string();

        let timeout_secs = args
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        let cwd = args.get("cwd").and_then(Value::as_str).map(str::to_string);

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(&command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(dir) = &cwd {
            cmd.current_dir(dir);
        }

        debug!(
            command = %command,
            timeout_secs,
            cwd = cwd.as_deref().unwrap_or(""),
            call_id = %inv.call_id,
            "invoking shell command",
        );

        let output = tokio::time::timeout(Duration::from_secs(timeout_secs), cmd.output())
            .await
            .map_err(|_| ToolError::Invocation {
                name: "Bash".to_string(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("command timed out after {timeout_secs}s"),
                )),
            })?
            .map_err(|e| ToolError::Invocation {
                name: "Bash".to_string(),
                source: Box::new(e),
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let exit_code = output.status.code().unwrap_or(-1);

        debug!(exit_code, "shell command finished");

        Ok(format!(
            "exit_code: {exit_code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    #[tokio::test]
    async fn runs_simple_command_and_captures_stdout() {
        let handler = BashHandler;
        let out = handler
            .invoke(
                ToolInvocation::for_test("call-1"),
                json!({ "command": "echo hello" }),
            )
            .await
            .expect("echo should succeed");

        assert!(out.contains("exit_code: 0"), "unexpected output: {out}");
        assert!(
            out.contains("hello"),
            "stdout should contain 'hello': {out}"
        );
    }

    #[tokio::test]
    async fn captures_stderr_output() {
        let handler = BashHandler;
        let out = handler
            .invoke(
                ToolInvocation::for_test("call-1"),
                json!({ "command": "echo err 1>&2" }),
            )
            .await
            .expect("stderr redirect should succeed");

        assert!(out.contains("exit_code: 0"), "unexpected output: {out}");
        assert!(out.contains("err"), "stderr should contain 'err': {out}");
    }

    #[tokio::test]
    async fn returns_nonzero_exit_code_in_output_not_as_error() {
        let handler = BashHandler;
        let out = handler
            .invoke(
                ToolInvocation::for_test("call-1"),
                json!({ "command": "exit 42" }),
            )
            .await
            .expect("non-zero exit must not be an error");

        assert!(
            out.contains("exit_code: 42"),
            "output should contain exit_code 42: {out}"
        );
    }

    #[tokio::test]
    async fn missing_command_arg_returns_invalid_args() {
        let handler = BashHandler;
        let err = handler
            .invoke(ToolInvocation::for_test("call-1"), json!({}))
            .await
            .expect_err("missing command must fail");

        match err {
            ToolError::InvalidArgs { name, message } => {
                assert_eq!(name, "Bash");
                assert!(message.contains("command"), "unexpected message: {message}");
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cwd_is_respected() {
        let tmp = TempDir::new().expect("create tempdir");
        // Resolve symlinks so the comparison matches `pwd -P`'s output on
        // platforms (macOS) where $TMPDIR contains symlinked components.
        let canonical = tmp.path().canonicalize().expect("canonicalize tempdir");
        let handler = BashHandler;

        let out = handler
            .invoke(
                ToolInvocation::for_test("call-1"),
                json!({ "command": "pwd -P", "cwd": tmp.path().to_str().unwrap() }),
            )
            .await
            .expect("pwd in tempdir should succeed");

        let expected = canonical.to_str().expect("utf8 tempdir path");
        assert!(
            out.contains(expected),
            "output should contain tempdir path {expected}: {out}"
        );
    }
}
