//! `Bash` tool — run a shell command. Requires both
//! `allow_mutations` and `allow_network`.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{debug, instrument};

use super::path_guard::jail_path;
use super::{ToolEffect, ToolHandler, ToolInvocation};
use crate::error::ToolError;

/// Default `timeout_secs` when the caller omits the field.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Lower bound on `timeout_secs`. Anything below would round to zero in
/// practice and let a runaway command never get cancelled.
const MIN_TIMEOUT_SECS: u64 = 1;

/// Upper bound on `timeout_secs`. Ten minutes is the longest the loop
/// will wait for a single shell invocation; longer running work belongs
/// in a dedicated `kind: shell` node where the universal node-level
/// `timeout:` attribute applies.
const MAX_TIMEOUT_SECS: u64 = 600;

/// Per-stream cap on captured Bash output. Mirrors the agent loop's
/// default `max_tool_output_bytes` so a runaway shell command cannot
/// drag the next LLM request past the provider's prompt-size ceiling.
/// The agent loop applies its own (potentially smaller) cap when the
/// pipeline configures `max_tool_output_bytes`; this constant only
/// bounds the bytes the handler holds in memory.
const MAX_OUTPUT_BYTES_PER_STREAM: usize = 256 * 1024;

/// Pipe drain buffer size. 8 KiB matches the task spec; a typical pipe
/// buffer is 64 KiB on Linux so the child rarely blocks waiting for
/// the reader between chunks.
const READ_CHUNK_BYTES: usize = 8 * 1024;

/// Allowlist of environment variables the spawned shell inherits.
/// Everything outside this list is dropped via `Command::env_clear`
/// so the agent cannot leak provider API keys or other host secrets
/// into a child process. Each entry is a name the host process may
/// or may not have set; missing entries are silently skipped.
const SAFE_ENV_VARS: &[&str] = &["PATH", "HOME", "USER", "TMPDIR", "LANG", "LC_ALL", "TERM"];

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    #[schemars(description = "Shell command to execute.")]
    command: String,
    #[schemars(description = "Max seconds to wait. Must be between 1 and 600. Defaults to 60.")]
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

        if let Some(secs) = timeout_secs
            && !(MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&secs)
        {
            return Err(ToolError::Invocation {
                name: "Bash".to_string(),
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "timeout_secs must be between {MIN_TIMEOUT_SECS} and {MAX_TIMEOUT_SECS}",
                    ),
                )),
            });
        }
        let timeout_secs = timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(&command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // Make the child its own process group leader so any descendants
        // (e.g. `sh -c 'foo | bar'`) share that group. `kill_on_drop`
        // signals only the direct child; without a fresh group, grandchildren
        // would survive an aborted invocation. Safe API — no `unsafe` needed.
        #[cfg(unix)]
        cmd.process_group(0);

        // Drop every host environment variable, then re-export only the
        // entries on the safe allowlist. Without this the spawned shell
        // would inherit provider API keys (`OPENAI_API_KEY`, etc.) and
        // any other secrets the orno process was launched with — a
        // prompt-injected command could exfiltrate them with a single
        // `env` or `curl`.
        cmd.env_clear();
        for name in SAFE_ENV_VARS {
            if let Ok(val) = std::env::var(name) {
                cmd.env(name, val);
            }
        }

        // Jail the requested `cwd` against the agent's first declared
        // root so the LLM cannot `cd` outside the policy-allowed tree
        // and run commands from there. Mirrors the path-guard contract
        // used by Read / Write / Edit. When `roots` is empty (handler
        // dispatched outside a `LoopAgent`, or agent declared no root),
        // fall through to the raw path so tests and root-less pipelines
        // keep working — policy enforcement is the caller's job there.
        if let Some(dir) = &cwd {
            let cwd_path: PathBuf = if let Some(root) = inv.roots.first() {
                jail_path(root, dir).map_err(|e| ToolError::Invocation {
                    name: "Bash".to_string(),
                    source: Box::new(std::io::Error::other(e.to_string())),
                })?
            } else {
                PathBuf::from(dir)
            };
            cmd.current_dir(&cwd_path);
        }

        debug!(
            command = %command,
            timeout_secs,
            cwd = cwd.as_deref().unwrap_or(""),
            call_id = %inv.call_id,
            "invoking shell command",
        );

        let RunOutcome {
            status,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        } = tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            run_with_capped_pipes(cmd),
        )
        .await
        .map_err(|_| ToolError::Invocation {
            name: "Bash".to_string(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("command timed out after {timeout_secs}s"),
            )),
        })??;

        let (exit_code_label, signal_suffix) = format_status(status);

        debug!(
            exit_code = exit_code_label.as_str(),
            signal_suffix = signal_suffix.as_str(),
            stdout_truncated,
            stderr_truncated,
            "shell command finished",
        );

        let mut out = String::new();
        if stdout_truncated {
            let _ = writeln!(
                out,
                "[stdout truncated at {MAX_OUTPUT_BYTES_PER_STREAM} bytes]",
            );
        }
        if stderr_truncated {
            let _ = writeln!(
                out,
                "[stderr truncated at {MAX_OUTPUT_BYTES_PER_STREAM} bytes]",
            );
        }
        let _ = write!(
            out,
            "exit_code: {exit_code_label}{signal_suffix}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        );
        Ok(out)
    }
}

/// Outcome of a single capped Bash invocation. Bundles the child's exit
/// status with the captured output and per-stream truncation flags so
/// the caller can prefix the result string with the right marker.
struct RunOutcome {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

/// Spawn the child, drain stdout and stderr concurrently into in-memory
/// buffers each capped at `MAX_OUTPUT_BYTES_PER_STREAM`, then await
/// the child's exit. Past the cap each reader keeps draining its pipe
/// to a scratch buffer so the child does not block on a full PIPE
/// buffer; the overflow bytes are counted but discarded. The outer
/// `tokio::time::timeout` enforces wall-clock; this helper assumes it
/// will be polled from inside that timeout (drop-cancellation kills
/// the spawned child via `kill_on_drop`).
async fn run_with_capped_pipes(mut cmd: Command) -> Result<RunOutcome, ToolError> {
    let mut child = cmd.spawn().map_err(|e| ToolError::Invocation {
        name: "Bash".to_string(),
        source: Box::new(e),
    })?;

    let stdout_pipe = child.stdout.take().expect("stdout was piped");
    let stderr_pipe = child.stderr.take().expect("stderr was piped");

    let stdout_fut = read_capped(stdout_pipe, MAX_OUTPUT_BYTES_PER_STREAM);
    let stderr_fut = read_capped(stderr_pipe, MAX_OUTPUT_BYTES_PER_STREAM);
    // drain both concurrently to EOF — `join!` ensures a failure on one
    // pipe does not cancel the other mid-drain, which would leave the
    // child blocked on a full PIPE buffer.
    let (stdout_result, stderr_result) = tokio::join!(stdout_fut, stderr_fut);

    let status = child.wait().await.map_err(|e| ToolError::Invocation {
        name: "Bash".to_string(),
        source: Box::new(e),
    })?;
    let (stdout_buf, stdout_total) = stdout_result.map_err(|e| ToolError::Invocation {
        name: "Bash".to_string(),
        source: Box::new(e),
    })?;
    let (stderr_buf, stderr_total) = stderr_result.map_err(|e| ToolError::Invocation {
        name: "Bash".to_string(),
        source: Box::new(e),
    })?;

    let stdout_truncated = stdout_total > MAX_OUTPUT_BYTES_PER_STREAM;
    let stderr_truncated = stderr_total > MAX_OUTPUT_BYTES_PER_STREAM;
    let stdout = String::from_utf8_lossy(&stdout_buf).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_buf).into_owned();

    Ok(RunOutcome {
        status,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    })
}

/// Stream a child pipe into a `Vec<u8>` capped at `cap` bytes. Returns
/// `(captured_buffer, total_bytes_written_by_child)`. Past the cap the
/// reader keeps draining the pipe so the child does not block on a
/// full PIPE buffer; the overflow bytes are counted but discarded.
/// Truncates the captured buffer back to the nearest UTF-8 lead byte
/// when the cap split a multi-byte sequence so downstream consumers do
/// not see a phantom replacement char.
async fn read_capped<R>(mut pipe: R, cap: usize) -> std::io::Result<(Vec<u8>, usize)>
where
    R: AsyncReadExt + Unpin + Send,
{
    let mut captured: Vec<u8> = Vec::with_capacity(cap.min(READ_CHUNK_BYTES));
    let mut total: usize = 0;
    let mut scratch = vec![0u8; READ_CHUNK_BYTES];

    loop {
        let n = pipe.read(&mut scratch).await?;
        if n == 0 {
            break;
        }
        total += n;
        if captured.len() < cap {
            let remaining = cap - captured.len();
            let take = remaining.min(n);
            captured.extend_from_slice(&scratch[..take]);
        }
    }

    if captured.len() == cap {
        let mut end = captured.len();
        while end > 0 {
            // A continuation byte has the high bits 10xxxxxx.
            if (captured[end - 1] & 0b1100_0000) == 0b1000_0000 {
                end -= 1;
            } else {
                break;
            }
        }
        // If we walked back past a leading byte that takes more bytes
        // than we have, drop that leading byte too.
        if end > 0 && (captured[end - 1] & 0b1000_0000) != 0 {
            let lead = captured[end - 1];
            let expected = utf8_lead_byte_len(lead);
            let have = captured.len() - end + 1;
            if expected > have {
                end -= 1;
            }
        }
        captured.truncate(end);
    }

    Ok((captured, total))
}

/// Decode the expected length of a UTF-8 sequence given its leading
/// byte. Returns 1 for ASCII / invalid leads (which then fall through
/// to a single-byte truncation), so the caller never reads past
/// `captured.len()`.
fn utf8_lead_byte_len(b: u8) -> usize {
    if b & 0b1000_0000 == 0 {
        1
    } else if b & 0b1110_0000 == 0b1100_0000 {
        2
    } else if b & 0b1111_0000 == 0b1110_0000 {
        3
    } else if b & 0b1111_1000 == 0b1111_0000 {
        4
    } else {
        1
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

    #[cfg(unix)]
    #[tokio::test]
    async fn cwd_outside_root_is_denied_when_root_is_set() {
        // F3 regression: BashHandler must jail the `cwd` field when policy.roots
        // is non-empty. An LLM trying to cd outside the root must get Denied.
        let root = TempDir::new().expect("create root");
        let outside = TempDir::new().expect("create outside");
        let roots = vec![root.path().to_path_buf()];

        let handler = BashHandler;
        let err = handler
            .invoke(
                ToolInvocation::for_test_with_roots("call-1", &roots),
                json!({
                    "command": "pwd",
                    "cwd": outside.path().to_str().unwrap()
                }),
            )
            .await
            .expect_err("cwd outside root must be denied");

        assert!(
            matches!(err, ToolError::Invocation { .. }),
            "expected Invocation (wrapping Denied), got {err:?}",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cwd_inside_root_is_allowed_when_root_is_set() {
        // Companion: cwd inside the declared root must work normally.
        let root = TempDir::new().expect("create root");
        let canonical = root.path().canonicalize().expect("canonicalize");
        let roots = vec![root.path().to_path_buf()];

        let handler = BashHandler;
        let out = handler
            .invoke(
                ToolInvocation::for_test_with_roots("call-1", &roots),
                json!({
                    "command": "pwd -P",
                    "cwd": root.path().to_str().unwrap()
                }),
            )
            .await
            .expect("cwd inside root must succeed");

        let expected = canonical.to_str().expect("utf8");
        assert!(
            out.contains(expected),
            "output should contain root path {expected}: {out}",
        );
    }

    /// F16 regression: switching from `try_join!` to `join!` must still
    /// drain both pipes to EOF when the child writes to stdout *and*
    /// stderr in the same invocation. Both payloads must reach the
    /// captured output verbatim.
    #[tokio::test]
    async fn bash_join_drains_both_pipes_on_eof() {
        let handler = BashHandler;
        let out = handler
            .invoke(
                ToolInvocation::for_test("call-1"),
                json!({
                    "command": "echo out_marker; echo err_marker 1>&2",
                }),
            )
            .await
            .expect("dual-stream write should succeed");

        assert!(out.contains("exit_code: 0"), "unexpected output: {out}");
        assert!(
            out.contains("out_marker"),
            "stdout payload missing from captured output: {out}",
        );
        assert!(
            out.contains("err_marker"),
            "stderr payload missing from captured output: {out}",
        );
    }

    /// F6 regression: on Unix the child must run in its own process
    /// group. The handler spawns `/bin/sh -c <command>`, so `$$` inside
    /// the command is the *direct* child's PID. With `process_group(0)`
    /// set, that child became its own group leader, so its PGID equals
    /// its PID. Without the fix, the child would inherit the test
    /// runner's process group and the two would differ.
    #[cfg(unix)]
    #[tokio::test]
    async fn bash_process_group_is_set() {
        let handler = BashHandler;
        let out = handler
            .invoke(
                ToolInvocation::for_test("call-1"),
                json!({
                    "command": "echo $$; ps -o pgid= -p $$",
                }),
            )
            .await
            .expect("pgid query should succeed");

        assert!(out.contains("exit_code: 0"), "unexpected output: {out}");

        // Pull the two whitespace-separated numbers out of stdout: the
        // first is the child shell's PID (`$$`), the second is the PGID
        // reported by `ps`. The handler's output format places stdout
        // after the `stdout:\n` line and before `stderr:`.
        let stdout_section = out
            .split("stdout:\n")
            .nth(1)
            .and_then(|s| s.split("\nstderr:").next())
            .expect("stdout section present");
        let nums: Vec<u32> = stdout_section
            .split_whitespace()
            .filter_map(|s| s.parse::<u32>().ok())
            .collect();
        assert_eq!(
            nums.len(),
            2,
            "expected two numeric tokens (PID, PGID) in {stdout_section:?}",
        );
        assert_eq!(
            nums[0], nums[1],
            "child must be its own process group leader: PID={} PGID={}",
            nums[0], nums[1],
        );
    }
}
