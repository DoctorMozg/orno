//! Shell node executor. Spawns a child via `tokio::process::Command`
//! and captures `stdout`, `stderr`, and `exit_code` into the node
//! response payload. No shell is interposed; `command` is the
//! program name, `args` are argv entries.
//!
//! Output capture is **bounded**: each of `stdout` and `stderr` is
//! streamed into an in-memory buffer capped at
//! `EngineConfig.max_node_output_bytes` (default 8 MiB). A child that
//! produces more than the cap on either stream has the captured prefix
//! retained and an [`Event::NodeOutputTruncated`] envelope emitted; the
//! child is allowed to keep running so it does not block on a full PIPE
//! buffer (the overflow is drained to `/dev/null`). This is the only
//! built-in defense against a node that prints unbounded output.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::process::Command;
use tracing::{instrument, warn};

use crate::error::NodeError;
use crate::events::{Event, EventSink};
use crate::util::child_guard::{Capture, ChildGuard, Collected, Finished};

use super::{NodeExecutor, NodeRequest, NodeResponse, ShellNodeRequest};

/// Default per-stream capture cap. Mirrors
/// `EngineConfig::default().max_node_output_bytes` so a `ShellExecutor`
/// constructed without an explicit config matches the value an embedder
/// gets from the engine. Kept in sync with the engine default by the
/// `engine_default_node_output_cap_matches_shell_executor` test in
/// `execution::scheduler::tests`.
pub const DEFAULT_MAX_NODE_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// Allowlist of environment variable names propagated from the orno
/// process into a shell node child. Everything else is stripped by
/// `env_clear()` to prevent API keys and MCP bearer tokens from leaking.
const SAFE_ENV_VARS: &[&str] = &["PATH", "HOME", "USER", "TMPDIR", "LANG", "LC_ALL", "TERM"];

#[derive(Clone)]
pub struct ShellExecutor {
    /// Per-stream capture cap. Shared with the engine's
    /// `max_node_output_bytes` so the wire envelope's `cap_bytes` and
    /// the executor's actual cap agree.
    cap: usize,
    /// Where [`Event::NodeOutputTruncated`] envelopes go. The
    /// `Default` impl uses an `InMemorySink` so unit tests that do not
    /// care about events still construct a `ShellExecutor` cheaply;
    /// the production wiring in `orno-cli` passes the run's shared
    /// sink so truncation envelopes interleave with the rest of the
    /// run's stream.
    sink: Arc<dyn EventSink>,
}

impl std::fmt::Debug for ShellExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Sink is a trait object with no useful Debug — name the field
        // and skip the value so the formatter does not panic and logs
        // stay readable.
        f.debug_struct("ShellExecutor")
            .field("cap", &self.cap)
            .finish_non_exhaustive()
    }
}

impl Default for ShellExecutor {
    fn default() -> Self {
        Self {
            cap: DEFAULT_MAX_NODE_OUTPUT_BYTES,
            sink: Arc::new(crate::events::InMemorySink::new()),
        }
    }
}

impl ShellExecutor {
    /// Construct an executor wired to a real sink and cap. The CLI
    /// uses this so the run-wide sink receives `NodeOutputTruncated`
    /// envelopes alongside the rest of the lifecycle stream.
    #[must_use]
    pub fn with_config(sink: Arc<dyn EventSink>, cap: usize) -> Self {
        Self { cap, sink }
    }

    /// Warn and emit one `NodeOutputTruncated` for an overflowed pipe.
    async fn emit_truncation(&self, run_id: &str, id: &str, stream: &str, total: usize) {
        let cap = self.cap;
        warn!(
            node.id = %id,
            stream = stream,
            total_bytes = total,
            cap_bytes = cap,
            "shell node output truncated to cap",
        );
        self.sink
            .record(Event::NodeOutputTruncated {
                run_id: run_id.to_string(),
                node_id: id.to_string(),
                node_kind: "shell".to_string(),
                stream: stream.to_string(),
                total_bytes: total as u64,
                cap_bytes: cap as u64,
            })
            .await;
    }
}

#[async_trait]
impl NodeExecutor for ShellExecutor {
    #[instrument(
        skip(self, req),
        fields(node.id = %id, node.kind = "shell", pipeline.run_id = %run_id),
    )]
    async fn execute(
        &self,
        run_id: &str,
        id: &str,
        req: NodeRequest,
    ) -> Result<NodeResponse, NodeError> {
        let NodeRequest::Shell(ShellNodeRequest {
            command,
            args,
            stdin,
            env,
            timeout,
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

        // stdin is null unless the node declares a payload, so a child
        // reading stdin cannot hang the pipeline in CI. `ChildGuard` owns
        // process-group cleanup and the output readers.
        let mut cmd = Command::new(&command);
        cmd.args(&args);

        // Defense against secret leakage: a child inheriting the orno
        // process's full environment can see API keys, MCP bearer
        // tokens, and any other credential the runner pulled in. Clear
        // it, restore a small allowlist of variables a generic shell
        // node legitimately needs (PATH for binaries, HOME / USER for
        // tools that read them, locale and TERM for output), then
        // apply per-node overrides on top so a node can shadow any of
        // those values without being exposed to the rest.
        cmd.env_clear();
        for name in SAFE_ENV_VARS {
            if let Ok(val) = std::env::var(name) {
                cmd.env(name, val);
            }
        }
        for (k, v) in &env {
            cmd.env(k, v);
        }

        let to_node_err = |e: std::io::Error| NodeError::Execution {
            id: id.to_string(),
            source: Box::new(e),
        };
        let cap = self.cap;
        let finished = ChildGuard::spawn(&mut cmd, stdin)
            .map_err(to_node_err)?
            .run_capped_within(cap, timeout)
            .await
            .map_err(to_node_err)?;
        let (status, stdout, stderr) = match finished {
            Finished::Exited(Collected {
                status,
                stdout,
                stderr,
                stdin_error,
            }) => {
                // A broken pipe is expected whenever the child exits before
                // consuming everything (e.g. `head`); the exit code tells the
                // real story.
                match stdin_error {
                    Some(err) if err.kind() == std::io::ErrorKind::BrokenPipe => {
                        warn!(node.id = %id, "child closed stdin before full payload written");
                    },
                    Some(err) => warn!(node.id = %id, error = %err, "stdin writer failed"),
                    None => {},
                }
                (Some(status), stdout, stderr)
            },
            Finished::TimedOut { stdout, stderr } => (None, stdout, stderr),
        };
        let Capture {
            bytes: stdout_buf,
            total: stdout_total,
        } = stdout;
        let Capture {
            bytes: stderr_buf,
            total: stderr_total,
        } = stderr;

        // Emit one envelope per truncated stream. The cap is reported
        // alongside the captured total so a downstream tool can
        // surface "captured / cap" without a follow-up query.
        if stdout_total > cap {
            self.emit_truncation(run_id, id, "stdout", stdout_total)
                .await;
        }
        if stderr_total > cap {
            self.emit_truncation(run_id, id, "stderr", stderr_total)
                .await;
        }

        let stdout = String::from_utf8_lossy(&stdout_buf).into_owned();
        let stderr = String::from_utf8_lossy(&stderr_buf).into_owned();
        // exit_code is `None` exactly when the child was killed by a
        // signal on Unix; the matching `signal` value is populated
        // on the same path. The two fields are mutually exclusive in
        // practice — at most one is `Some` per terminated process —
        // but the JSON emits both keys uniformly so downstream
        // templates can pattern-match without a presence check.
        let Some(status) = status else {
            // The scheduler turns `timed_out` into `NodeTimedOut`; the
            // captured prefix still lands in the context.
            warn!(node.id = %id, "shell node timed out; returning captured output");
            return Ok(NodeResponse {
                node_id: id.to_string(),
                output: json!({
                    "stdout": stdout,
                    "stderr": stderr,
                    "exit_code": null,
                    "signal": null,
                    "timed_out": true,
                }),
            });
        };
        let exit_code: Option<i64> = status.code().map(i64::from);
        let signal: Option<i32> = signal_from_status(status);

        Ok(NodeResponse {
            node_id: id.to_string(),
            output: json!({
                "stdout": stdout,
                "stderr": stderr,
                "exit_code": exit_code,
                "signal": signal,
            }),
        })
    }
}

/// Extract the terminating signal number from a child process exit
/// status. Returns `None` on non-Unix targets and on Unix when the
/// child exited normally (with a code, not a signal). Mirrors the
/// `signal: Option<i32>` field on
/// [`crate::events::NodeFailure::NodePayloadFailure`] so the JSON
/// `"signal"` key and the wire envelope agree.
#[cfg(unix)]
fn signal_from_status(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal()
}

#[cfg(not(unix))]
fn signal_from_status(_status: std::process::ExitStatus) -> Option<i32> {
    None
}

/// A node executor that refuses every `kind: shell` node during `orno
/// replay`. Shell side-effects — subprocess spawning, filesystem writes,
/// network I/O — are not captured in a record bundle, so replaying them
/// live would break the bounded-non-determinism guarantee. Registering
/// this executor as "shell" in `commands::replay` ensures a clear
/// `NodeError::ReplayRefused` rather than silently running live
/// subprocesses against the host.
#[derive(Debug, Clone, Default)]
pub struct DenyShellExecutor;

#[async_trait]
impl NodeExecutor for DenyShellExecutor {
    #[instrument(
        skip(self, _req),
        fields(node.id = %id, node.kind = "shell", pipeline.run_id = %_run_id),
    )]
    async fn execute(
        &self,
        _run_id: &str,
        id: &str,
        _req: NodeRequest,
    ) -> Result<NodeResponse, NodeError> {
        Err(NodeError::ReplayRefused { id: id.to_string() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use crate::events::InMemorySink;
    use crate::pipeline::{AgentPolicy, OnParseError};

    fn test_policy() -> AgentPolicy {
        AgentPolicy {
            max_iterations: 1,
            max_total_tokens: 0,
            max_tool_calls: 0,
            max_subagent_depth: 0,
            max_tool_output_bytes: None,
            allow_mutations: false,
            allow_network: false,
            allow_context_writes: false,
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
            on_parse_error: OnParseError::Fail,
            roots: Vec::new(),
            max_message_history_bytes: None,
            unconfined_shell: false,
        }
    }

    #[tokio::test]
    async fn echoes_stdout() {
        let exec = ShellExecutor::default();
        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "echo".to_string(),
            args: vec!["hi".to_string()],
            stdin: None,
            env: BTreeMap::new(),
            timeout: None,
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
        let exec = ShellExecutor::default();
        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "false".to_string(),
            args: Vec::new(),
            stdin: None,
            env: BTreeMap::new(),
            timeout: None,
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
        let exec = ShellExecutor::default();
        let req = NodeRequest::Agent(AgentNodeRequest {
            agent: "x".to_string(),
            initial_prompt: String::new(),
            system: None,
            provider: "openai".into(),
            model: "gpt-5".into(),
            policy: test_policy(),
            allowed_tools: Vec::new(),
            render_scope: None,
        });

        let err = exec
            .execute("run_test", "wrong_kind", req)
            .await
            .expect_err("agent request must be rejected");

        match err {
            NodeError::Execution { id, source: _ } => {
                assert_eq!(id, "wrong_kind");
            },
            other => panic!("expected NodeError::Execution, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_program_is_execution_error() {
        let exec = ShellExecutor::default();
        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "definitely-not-a-real-program-xyz-12345".to_string(),
            args: Vec::new(),
            stdin: None,
            env: BTreeMap::new(),
            timeout: None,
        });

        let err = exec
            .execute("run_test", "unknown", req)
            .await
            .expect_err("spawn failure should return Err");

        match err {
            NodeError::Execution { id, source: _ } => {
                assert_eq!(id, "unknown");
            },
            other => panic!("expected NodeError::Execution, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdin_content_is_piped_to_child() {
        let exec = ShellExecutor::default();
        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "cat".to_string(),
            args: Vec::new(),
            stdin: Some("hello over stdin\n".to_string()),
            env: BTreeMap::new(),
            timeout: None,
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
        let exec = ShellExecutor::default();
        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "cat".to_string(),
            args: Vec::new(),
            stdin: None,
            env: BTreeMap::new(),
            timeout: None,
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
        let exec = ShellExecutor::default();
        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "cat".to_string(),
            args: Vec::new(),
            stdin: Some(payload.clone()),
            env: BTreeMap::new(),
            timeout: None,
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
        let exec = ShellExecutor::default();
        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "head".to_string(),
            args: vec!["-c".to_string(), "4".to_string()],
            stdin: Some("A".repeat(256 * 1024)),
            env: BTreeMap::new(),
            timeout: None,
        });

        let resp = exec.execute("run_test", "head_node", req).await.unwrap();

        assert_eq!(resp.output["exit_code"], json!(0));
        assert_eq!(resp.output["stdout"], json!("AAAA"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdout_capped_at_max_node_output_bytes_emits_truncation_event() {
        // Pinned cap (16 KiB) chosen to keep the test fast — the child
        // produces ~64 KiB of stdout, comfortably above the cap and
        // well above the read-chunk size so the truncation branch
        // executes inside the loop, not at EOF.
        let cap = 16 * 1024;
        let sink = Arc::new(InMemorySink::new());
        let exec = ShellExecutor::with_config(sink.clone(), cap);

        // `yes A | head -c 65536` produces deterministic, easily
        // verifiable bytes without depending on /dev/urandom or shell
        // tricks. Wrapped in `sh -c` so the pipeline runs in a single
        // child process tree.
        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "yes A | head -c 65536".to_string()],
            stdin: None,
            env: BTreeMap::new(),
            timeout: None,
        });

        let resp = exec.execute("run_test", "noisy", req).await.unwrap();

        // Captured stdout is exactly the cap (UTF-8 boundary check is
        // a no-op since 'A' is single-byte). The exit code is 0
        // because `head` ends the pipeline cleanly.
        assert_eq!(
            resp.output["stdout"].as_str().unwrap().len(),
            cap,
            "captured stdout must be truncated to cap",
        );
        assert_eq!(resp.output["exit_code"], json!(0));

        // One NodeOutputTruncated envelope, stream=stdout,
        // total_bytes=64 KiB, cap=16 KiB.
        let envs = sink.snapshot();
        let trunc = envs
            .iter()
            .filter_map(|e| match &e.event {
                Event::NodeOutputTruncated {
                    node_id,
                    stream,
                    total_bytes,
                    cap_bytes,
                    ..
                } => Some((node_id.clone(), stream.clone(), *total_bytes, *cap_bytes)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(trunc.len(), 1, "exactly one truncation event: {envs:?}");
        let (node_id, stream, captured, cap_reported) = &trunc[0];
        assert_eq!(node_id, "noisy");
        assert_eq!(stream, "stdout");
        assert_eq!(*captured, 65536, "total_bytes must echo total");
        assert_eq!(*cap_reported, cap as u64);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stderr_capped_independently_from_stdout() {
        // Each stream has its own cap budget — a noisy stderr must not
        // prevent stdout from being captured up to the same cap.
        let cap = 8 * 1024;
        let sink = Arc::new(InMemorySink::new());
        let exec = ShellExecutor::with_config(sink.clone(), cap);

        // 32 KiB of stdout + 32 KiB of stderr, both above the 8 KiB cap.
        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "yes O | head -c 32768; yes E | head -c 32768 1>&2".to_string(),
            ],
            stdin: None,
            env: BTreeMap::new(),
            timeout: None,
        });

        let resp = exec.execute("run_test", "both", req).await.unwrap();

        assert_eq!(resp.output["stdout"].as_str().unwrap().len(), cap);
        assert_eq!(resp.output["stderr"].as_str().unwrap().len(), cap);

        let envs = sink.snapshot();
        let streams: Vec<String> = envs
            .iter()
            .filter_map(|e| match &e.event {
                Event::NodeOutputTruncated { stream, .. } => Some(stream.clone()),
                _ => None,
            })
            .collect();
        assert!(streams.contains(&"stdout".to_string()));
        assert!(streams.contains(&"stderr".to_string()));
        assert_eq!(streams.len(), 2, "one envelope per truncated stream");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn under_cap_output_is_not_truncated_no_event_emitted() {
        // Negative regression: a child whose total output stays under
        // the cap must not produce a NodeOutputTruncated envelope.
        let cap = 1024;
        let sink = Arc::new(InMemorySink::new());
        let exec = ShellExecutor::with_config(sink.clone(), cap);

        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "echo".to_string(),
            args: vec!["small payload".to_string()],
            stdin: None,
            env: BTreeMap::new(),
            timeout: None,
        });

        let resp = exec.execute("run_test", "small", req).await.unwrap();

        assert!(resp.output["stdout"].as_str().unwrap().contains("small"));
        let envs = sink.snapshot();
        let any_trunc = envs
            .iter()
            .any(|e| matches!(e.event, Event::NodeOutputTruncated { .. }));
        assert!(!any_trunc, "no truncation event for under-cap output");
    }

    #[tokio::test]
    async fn deny_shell_executor_returns_replay_refused() {
        // Replay-time contract: every `kind: shell` node must surface
        // `NodeError::ReplayRefused` carrying the original node id, not
        // silently spawn a real subprocess against the host. The id round
        // trips so a downstream `Event::NodeFinished` carrying the failure
        // can pinpoint which node was refused.
        let exec = DenyShellExecutor;
        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "echo".to_string(),
            args: vec!["does not matter".to_string()],
            stdin: None,
            env: BTreeMap::new(),
            timeout: None,
        });

        let err = exec
            .execute("run_test", "shell_node", req)
            .await
            .expect_err("DenyShellExecutor must refuse shell nodes during replay");

        match err {
            NodeError::ReplayRefused { id } => {
                assert_eq!(id, "shell_node", "ReplayRefused must echo the node id");
            },
            other => panic!("expected NodeError::ReplayRefused, got {other:?}"),
        }
    }

    #[cfg(unix)]
    fn sh_node(script: String) -> NodeRequest {
        NodeRequest::Shell(ShellNodeRequest {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), script],
            stdin: None,
            env: BTreeMap::new(),
            timeout: None,
        })
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_grandchild_does_not_hang_the_node() {
        let exec = ShellExecutor::default();
        let run = exec.execute("run_test", "bg", sh_node("sleep 100 & echo hi".into()));
        let resp = tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("node must not wait for the grandchild")
            .unwrap();

        assert_eq!(resp.output["stdout"], json!("hi\n"));
        assert_eq!(resp.output["exit_code"], json!(0));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timed_out_node_leaves_no_grandchild() {
        use crate::util::child_guard::test_support::{read_pid_file, wait_until_gone};
        let tmp = tempfile::TempDir::new().unwrap();
        let pid_file = tmp.path().join("pid");
        let script = format!("sleep 100 & echo $! > {}; wait", pid_file.display());

        let exec = ShellExecutor::default();
        let run = exec.execute("run_test", "slow", sh_node(script));
        // Mirrors the scheduler's backstop, which drops the future.
        let timed_out = tokio::time::timeout(std::time::Duration::from_secs(1), run).await;
        assert!(timed_out.is_err(), "node should still be running");

        let pid = read_pid_file(&pid_file).await;
        assert!(
            wait_until_gone(pid).await,
            "grandchild {pid} survived the timeout"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn request_timeout_returns_captured_output_and_kills_the_group() {
        use crate::util::child_guard::test_support::{read_pid_file, wait_until_gone};
        let tmp = tempfile::TempDir::new().unwrap();
        let pid_file = tmp.path().join("pid");
        let script = format!(
            "echo partial-out; echo partial-err 1>&2; sleep 100 & echo $! > {}; wait",
            pid_file.display()
        );
        let NodeRequest::Shell(mut req) = sh_node(script) else {
            unreachable!()
        };
        req.timeout = Some(std::time::Duration::from_secs(1));

        let exec = ShellExecutor::default();
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            exec.execute("run_test", "slow", NodeRequest::Shell(req)),
        )
        .await
        .expect("executor must enforce its own deadline")
        .unwrap();

        assert_eq!(resp.output["timed_out"], json!(true));
        assert_eq!(resp.output["stdout"], json!("partial-out\n"));
        assert_eq!(resp.output["stderr"], json!("partial-err\n"));
        assert_eq!(resp.output["exit_code"], json!(null));
        let pid = read_pid_file(&pid_file).await;
        assert!(
            wait_until_gone(pid).await,
            "grandchild {pid} survived the timeout"
        );
    }
}
