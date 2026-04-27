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

use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::task::JoinHandle;
use tracing::{instrument, warn};

use crate::error::NodeError;
use crate::events::{Event, EventSink};

use super::{NodeExecutor, NodeRequest, NodeResponse, ShellNodeRequest};

/// Default per-stream capture cap. Mirrors
/// `EngineConfig::default().max_node_output_bytes` so a `ShellExecutor`
/// constructed without an explicit config matches the value an embedder
/// gets from the engine. Kept in sync with the engine default by the
/// `engine_default_node_output_cap_matches_shell_executor` test in
/// `execution::scheduler::tests`.
pub const DEFAULT_MAX_NODE_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// Pipe drain buffer size. A 16 KB read fits in a typical pipe buffer
/// (64 KiB on Linux, 16 KiB on macOS) so the child rarely blocks
/// waiting for the reader.
const READ_CHUNK_BYTES: usize = 16 * 1024;

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
        // task so the parent can drain stdout/stderr at the same time.
        // Writing stdin inline would deadlock against a child that
        // produces enough output to fill its pipe buffer before it
        // finishes reading stdin. A broken-pipe write is expected
        // whenever the child exits before consuming everything (e.g.
        // `head`) — log and move on; the child's own exit code tells
        // the real story.
        let writer_handle = if let Some(payload) = stdin {
            let mut child_stdin = child.stdin.take().expect("stdin was piped");
            Some(tokio::spawn(async move {
                let res = child_stdin.write_all(payload.as_bytes()).await;
                drop(child_stdin.shutdown().await);
                res
            }))
        } else {
            None
        };

        // Take the pipes before awaiting — `child.wait()` consumes the
        // handle and would also drop the pipe ends, defeating the
        // concurrent-drain pattern.
        let stdout_pipe = child.stdout.take().expect("stdout was piped");
        let stderr_pipe = child.stderr.take().expect("stderr was piped");

        let cap = self.cap;
        let stdout_handle = tokio::spawn(read_capped(stdout_pipe, cap));
        let stderr_handle = tokio::spawn(read_capped(stderr_pipe, cap));

        let status = child.wait().await.map_err(|e| NodeError::Execution {
            id: id.to_string(),
            source: Box::new(e),
        })?;

        let (stdout_buf, stdout_total) = join_pipe_handle(stdout_handle, id, "stdout").await?;
        let (stderr_buf, stderr_total) = join_pipe_handle(stderr_handle, id, "stderr").await?;

        if let Some(handle) = writer_handle {
            match handle.await {
                Ok(Ok(())) => {},
                Ok(Err(err)) if err.kind() == std::io::ErrorKind::BrokenPipe => {
                    warn!(node.id = %id, "child closed stdin before full payload written");
                },
                Ok(Err(err)) => {
                    warn!(node.id = %id, error = %err, "stdin writer failed");
                },
                Err(join_err) => {
                    warn!(node.id = %id, error = %join_err, "stdin writer task panicked");
                },
            }
        }

        // Emit one envelope per truncated stream. The cap is reported
        // alongside the captured total so a downstream tool can
        // surface "captured / cap" without a follow-up query.
        if stdout_total > cap {
            emit_truncation(&self.sink, run_id, id, "stdout", stdout_total, cap).await;
        }
        if stderr_total > cap {
            emit_truncation(&self.sink, run_id, id, "stderr", stderr_total, cap).await;
        }

        let stdout = String::from_utf8_lossy(&stdout_buf).into_owned();
        let stderr = String::from_utf8_lossy(&stderr_buf).into_owned();
        // exit_code is `None` exactly when the child was killed by a
        // signal on Unix; the matching `signal` value is populated
        // on the same path. The two fields are mutually exclusive in
        // practice — at most one is `Some` per terminated process —
        // but the JSON emits both keys uniformly so downstream
        // templates can pattern-match without a presence check.
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

/// Resolve a pipe-reader join handle into the captured bytes and the
/// child's total write count, mapping both the join failure and the
/// inner I/O failure into a `NodeError::Execution` keyed on the node
/// id. `stream` names the pipe in the panic message so the failure
/// surface keeps stdout / stderr distinguishable.
async fn join_pipe_handle(
    handle: JoinHandle<std::io::Result<(Vec<u8>, usize)>>,
    id: &str,
    stream: &str,
) -> Result<(Vec<u8>, usize), NodeError> {
    handle
        .await
        .map_err(|e| NodeError::Execution {
            id: id.to_string(),
            source: Box::new(std::io::Error::other(format!(
                "{stream} reader task panicked: {e}",
            ))),
        })?
        .map_err(|e| NodeError::Execution {
            id: id.to_string(),
            source: Box::new(e),
        })
}

/// Log and emit a single `NodeOutputTruncated` envelope for one of the
/// child's pipes. The captured total is reported alongside the cap so
/// downstream tools can render "captured / cap" without a follow-up
/// lookup.
#[expect(
    clippy::too_many_arguments,
    reason = "private helper extracted from execute(); each arg is a distinct \
              primitive identifier (run_id, node id, stream name, totals, cap) \
              and bundling them into a struct would only push the same six \
              fields one indirection away"
)]
async fn emit_truncation(
    sink: &Arc<dyn EventSink>,
    run_id: &str,
    id: &str,
    stream: &str,
    total: usize,
    cap: usize,
) {
    warn!(
        node.id = %id,
        stream = stream,
        captured_bytes = total,
        cap_bytes = cap,
        "shell node output truncated to cap",
    );
    sink.record(Event::NodeOutputTruncated {
        run_id: run_id.to_string(),
        node_id: id.to_string(),
        node_kind: "shell".to_string(),
        stream: stream.to_string(),
        captured_bytes: total as u64,
        cap_bytes: cap as u64,
    })
    .await;
}

/// Stream a child pipe into a `Vec<u8>` capped at `cap` bytes. Returns
/// `(captured_buffer, total_bytes_written_by_child)`. Past the cap, the
/// reader keeps draining the pipe to a scratch buffer so the child does
/// not block on a full PIPE — the overflow bytes are counted but
/// discarded. Truncates the captured buffer on a UTF-8 boundary to
/// avoid splitting a multi-byte sequence; the truncation point stays at
/// `<= cap`.
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
        // After the cap, captured stays put and `total` keeps climbing.
    }

    // Walk back to the nearest UTF-8 boundary. `from_utf8_lossy` can
    // tolerate a mid-sequence split (it produces U+FFFD), but the
    // event reports the captured slice's exact byte count, so we
    // prefer to land on a boundary so downstream consumers do not see
    // a phantom replacement char in the captured stdout/stderr.
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
        // than we have, drop that leading byte too. `start_byte_len`
        // returns the expected sequence length 1..=4.
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

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    #[tokio::test]
    async fn echoes_stdout() {
        let exec = ShellExecutor::default();
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
        let exec = ShellExecutor::default();
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
        let exec = ShellExecutor::default();
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
        // captured_bytes=64 KiB, cap=16 KiB.
        let envs = sink.snapshot();
        let trunc = envs
            .iter()
            .filter_map(|e| match &e.event {
                Event::NodeOutputTruncated {
                    node_id,
                    stream,
                    captured_bytes,
                    cap_bytes,
                    ..
                } => Some((node_id.clone(), stream.clone(), *captured_bytes, *cap_bytes)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(trunc.len(), 1, "exactly one truncation event: {envs:?}");
        let (node_id, stream, captured, cap_reported) = &trunc[0];
        assert_eq!(node_id, "noisy");
        assert_eq!(stream, "stdout");
        assert_eq!(*captured, 65536, "captured_bytes must echo total");
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
    async fn read_capped_keeps_utf8_boundary_on_truncation() {
        // A buffer containing a multi-byte UTF-8 char straddling the
        // cap boundary must be truncated *before* the leading byte so
        // the captured slice is valid UTF-8 by itself. Synthetic — no
        // process spawned — to pin the boundary logic in isolation.
        // "h" + "é" (2 bytes 0xC3 0xA9) + "llo" = 6 bytes total.
        let cap = 4;
        let stream = std::io::Cursor::new(b"h\xC3\xA9llo".to_vec());
        let (buf, total) = read_capped(stream, cap).await.expect("read succeeds");
        assert_eq!(total, 6, "total counts every byte the source wrote");
        // After UTF-8 alignment, the captured prefix must round-trip
        // through `from_utf8` cleanly. The contract is "valid UTF-8
        // and length <= cap", not "exactly cap bytes".
        assert!(
            std::str::from_utf8(&buf).is_ok(),
            "captured bytes must be valid UTF-8: {buf:?}",
        );
        assert!(buf.len() <= cap, "captured length never exceeds cap");
    }
}
