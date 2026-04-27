//! `Bash` tool — run a shell command. Requires both
//! `allow_mutations` and `allow_network`.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;
use tracing::{debug, instrument};

use super::{ToolEffect, ToolHandler, ToolInvocation};
use crate::error::ToolError;

/// Default `timeout_secs` when the caller omits the field.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    #[schemars(description = "Shell command to execute.")]
    command: String,
    #[schemars(description = "Max seconds to wait. Defaults to 60.")]
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[schemars(description = "Working directory override.")]
    #[serde(default)]
    cwd: Option<String>,
}

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
        serde_json::to_value(schemars::schema_for!(BashArgs)).expect("static schema")
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::MutationsAndNetwork
    }

    #[instrument(skip(self, args), fields(tool.name = "Bash", tool.call_id = %inv.call_id))]
    async fn invoke(&self, inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        #[cfg(not(unix))]
        {
            let _ = (inv, args);
            return Err(ToolError::NotImplemented {
                name: "Bash".to_string(),
                feature: "requires Unix /bin/sh — not available on Windows".to_string(),
            });
        }
        let BashArgs {
            command,
            timeout_secs,
            cwd,
        } = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
            name: "Bash".to_string(),
            message: e.to_string(),
        })?;

        let timeout_secs = timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);

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
        let (exit_code_label, signal_suffix) = format_status(output.status);

        debug!(
            exit_code = exit_code_label.as_str(),
            signal_suffix = signal_suffix.as_str(),
            "shell command finished",
        );

        Ok(format!(
            "exit_code: {exit_code_label}{signal_suffix}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        ))
    }
}

/// Render a child process exit status into the two header pieces used
/// by the model-facing tool output. `exit_code_label` is the numeric
/// exit code on a normal exit, or the literal `"signal"` when the
/// child was killed (in which case `signal_suffix` carries
/// ` (signal: N)`). Keeping signal information visible to the model
/// matches the wire-format `NodePayloadFailure { signal }` addition
/// in `schema_version: 2` so the agent reasons over the same data.
fn format_status(status: std::process::ExitStatus) -> (String, String) {
    if let Some(code) = status.code() {
        (code.to_string(), String::new())
    } else if let Some(sig) = signal_from_status(status) {
        ("signal".to_string(), format!(" (signal: {sig})"))
    } else {
        ("unknown".to_string(), String::new())
    }
}

#[cfg(unix)]
fn signal_from_status(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal()
}

#[cfg(not(unix))]
fn signal_from_status(_status: std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;
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
            },
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[test]
    fn schema_contains_expected_fields() {
        let schema = BashHandler.schema();

        assert_eq!(
            schema["type"].as_str(),
            Some("object"),
            "schema root must be an object: {schema}"
        );

        let properties = schema["properties"]
            .as_object()
            .expect("schema must expose a properties object");
        for field in ["command", "timeout_secs", "cwd"] {
            assert!(
                properties.contains_key(field),
                "properties missing {field}: {schema}"
            );
        }

        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("schema must expose a required array")
            .iter()
            .map(|v| v.as_str().expect("required entries are strings"))
            .collect();
        assert!(
            required.contains(&"command"),
            "`command` must be required: {required:?}"
        );
        assert!(
            !required.contains(&"timeout_secs"),
            "`timeout_secs` must remain optional: {required:?}"
        );
        assert!(
            !required.contains(&"cwd"),
            "`cwd` must remain optional: {required:?}"
        );
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
