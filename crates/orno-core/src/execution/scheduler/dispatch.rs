//! Per-node dispatch: executor lookup, template render, timeout and
//! payload classification for one ready node.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::error::NodeError;
use crate::events::redactor::Keep;
use crate::events::{Event, NodeFailure};
use crate::node::{NodeExecutor, NodeRequest, NodeResponse, kind_str, render_request};
use crate::pipeline::schema::NodeKind;

use super::{Engine, RunState};

/// Result of `Engine::dispatch_node`. Invariant: `failure.is_some()` iff
/// `!ok`.
pub(super) struct DispatchOutcome {
    pub(super) ok: bool,
    pub(super) failure: Option<NodeFailure>,
    pub(super) output: Option<Value>,
}

impl DispatchOutcome {
    fn ok(output: Value) -> Self {
        Self {
            ok: true,
            failure: None,
            output: Some(output),
        }
    }

    /// A failure with no executor output to record.
    fn failed(failure: NodeFailure) -> Self {
        Self {
            ok: false,
            failure: Some(failure),
            output: None,
        }
    }
}

impl Engine {
    /// Dispatch a single ready node. Every failure mode collapses to
    /// `ok = false` with a populated `failure` and a structured WARN. The
    /// executor payload is returned even on failure so downstream
    /// templates can read `nodes.<id>.stderr` of a failed producer.
    pub(super) async fn dispatch_node(
        &self,
        run: &RunState<'_>,
        node: &crate::pipeline::Node,
    ) -> DispatchOutcome {
        let kind = kind_str(&node.kind);
        let Some(exec) = self.registry.get(kind) else {
            tracing::warn!(
                node.id = %node.id,
                node.kind = kind,
                "no executor registered for kind",
            );
            return DispatchOutcome::failed(NodeFailure::NoExecutorRegistered {
                node_kind: kind.to_string(),
            });
        };

        let req = match render_request(
            &node.kind,
            &self.templates,
            &run.context,
            &run.pipeline.agents,
        ) {
            Ok(req) => req,
            Err(err) => {
                // A template error can quote the offending expression,
                // which may include a rendered `secrets.*` value.
                let error = run.redactor.redact(&format!("{err:#}")).into_owned();
                tracing::warn!(
                    node.id = %node.id,
                    node.kind = kind,
                    error = %error,
                    "render_request failed",
                );
                return DispatchOutcome::failed(NodeFailure::TemplateRenderFailed { error });
            },
        };

        match self.execute(run, node, &exec, req).await {
            Ok(resp) if node_response_ok(&node.kind, &resp) => DispatchOutcome::ok(resp.output),
            Ok(resp) => self.on_payload_failure(node, kind, resp, &run.redactor),
            Err(outcome) => outcome,
        }
    }

    /// Run the executor under the node's wall-clock limit. Timeouts and
    /// executor errors come back as the failed outcome to report.
    async fn execute(
        &self,
        run: &RunState<'_>,
        node: &crate::pipeline::Node,
        exec: &Arc<dyn NodeExecutor>,
        req: NodeRequest,
    ) -> Result<NodeResponse, DispatchOutcome> {
        let kind = kind_str(&node.kind);
        let started = std::time::Instant::now();
        let result = match node.timeout {
            Some(limit) => {
                let call = exec.execute(run.run_id, &node.id, req);
                match tokio::time::timeout(Duration::from_secs(limit), call).await {
                    Ok(inner) => inner,
                    Err(_elapsed) => {
                        let elapsed_ms =
                            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                        self.sink
                            .record(Event::NodeTimedOut {
                                run_id: run.run_id.to_string(),
                                node_id: node.id.clone(),
                                limit_secs: limit,
                                elapsed_ms,
                            })
                            .await;
                        tracing::warn!(
                            node.id = %node.id,
                            node.kind = kind,
                            limit_secs = limit,
                            elapsed_ms,
                            "node exceeded wall-clock budget",
                        );
                        return Err(DispatchOutcome::failed(NodeFailure::TimedOut {
                            limit_secs: limit,
                        }));
                    },
                }
            },
            None => exec.execute(run.run_id, &node.id, req).await,
        };

        result.map_err(|err| self.executor_failure(node, kind, &err, &run.redactor))
    }

    fn executor_failure(
        &self,
        node: &crate::pipeline::Node,
        kind: &str,
        err: &NodeError,
        redactor: &crate::events::Redactor,
    ) -> DispatchOutcome {
        // `Display` shows only the top-level variant, so walk the chain to
        // surface the underlying cause. Redacted and capped because a
        // transport error can carry a multi-KB body and a rendered prompt.
        let error = redactor.redact_bounded(
            &render_error_chain(err),
            self.config.max_output_bytes,
            Keep::Tail,
        );
        tracing::warn!(
            node.id = %node.id,
            node.kind = kind,
            error = %error,
            "node execution failed",
        );
        DispatchOutcome::failed(NodeFailure::ExecutorError { error })
    }

    /// A shell node that ran and exited non-zero: emit a typed
    /// `NodeFailure`, log a WARN, and hand `resp.output` back so
    /// recovery nodes can read `nodes.<id>.stderr`.
    fn on_payload_failure(
        &self,
        node: &crate::pipeline::Node,
        kind: &str,
        resp: NodeResponse,
        redactor: &crate::events::Redactor,
    ) -> DispatchOutcome {
        let exit_code = resp.output.get("exit_code").and_then(Value::as_i64);
        // `signal` is `Some(n)` exactly when the child was killed by
        // a signal on Unix (see `node::shell` and `tool::bash`). The
        // schema_version: 2 wire-format addition. `as_i64` is the
        // safe path through `serde_json::Value::Number`; downcast to
        // i32 once the value is known to fit.
        let signal = resp
            .output
            .get("signal")
            .and_then(Value::as_i64)
            .and_then(|s| i32::try_from(s).ok());
        let max = self.config.max_output_bytes;
        let stderr_tail = resp
            .output
            .get("stderr")
            .and_then(Value::as_str)
            .map(|s| redactor.redact_bounded(s, max, Keep::Tail));
        let stdout_tail = if self.config.verbose {
            resp.output
                .get("stdout")
                .and_then(Value::as_str)
                .map(|s| redactor.redact_bounded(s, max, Keep::Tail))
                .unwrap_or_default()
        } else {
            String::new()
        };
        tracing::warn!(
            node.id = %node.id,
            node.kind = kind,
            exit_code = exit_code.unwrap_or(-1),
            signal = signal.unwrap_or(0),
            stderr_tail = %stderr_tail.as_deref().unwrap_or(""),
            stdout_tail = %stdout_tail,
            "node returned failure in payload",
        );
        DispatchOutcome {
            ok: false,
            failure: Some(NodeFailure::NodePayloadFailure {
                exit_code,
                signal,
                stderr_tail,
            }),
            output: Some(resp.output),
        }
    }
}

/// Render an error with its full `source()` chain. `thiserror`'s
/// `Display` only emits the top-level variant — for a terminal like
/// `NodeError::Execution { source: AgentError::BudgetExceeded {..} }`
/// the raw `Display` reads as `node `digest` failed`, hiding the
/// budget-breach that the operator actually needs to see. Each link
/// is joined with `: ` so the output reads as a single diagnostic
/// line. The `'static` bound is the same one `std::error::Error`'s
/// inherent `source()` walker requires — `NodeError` satisfies it.
fn render_error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut current = err.source();
    while let Some(src) = current {
        out.push_str(": ");
        out.push_str(&src.to_string());
        current = src.source();
    }
    out
}

/// Classify a successful `NodeResponse` against the originating node kind.
/// Only shell nodes treat their payload's `exit_code` as a failure signal;
/// agent and any future kinds default to success when `execute` returns `Ok`
/// so an agent payload that happens to carry an `exit_code` key is never
/// misread as a shell exit.
///
/// Distinguishes "key absent" (forward-compat success — a future shell
/// executor variant may omit it) from "key present but not a valid
/// integer" (a contract violation by the executor; logged and treated as
/// failure rather than silently passed).
fn node_response_ok(kind: &NodeKind, resp: &NodeResponse) -> bool {
    match kind {
        NodeKind::Shell(_) => match resp.output.get("exit_code") {
            None => true,
            Some(v) => {
                if let Some(code) = v.as_i64() {
                    code == 0
                } else {
                    tracing::warn!(
                        exit_code = ?v,
                        "shell exit_code is not an integer; treating as failure",
                    );
                    false
                }
            },
        },
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::truncate_tail;
    use crate::pipeline::schema::{AgentNode, ShellNode};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn shell_kind() -> NodeKind {
        NodeKind::Shell(ShellNode {
            command: "echo".to_string(),
            args: Vec::new(),
            stdin: None,
            env: BTreeMap::new(),
        })
    }

    fn agent_kind() -> NodeKind {
        NodeKind::Agent(AgentNode {
            agent: "dummy".to_string(),
            initial_prompt: String::new(),
        })
    }

    fn resp(output: Value) -> NodeResponse {
        NodeResponse {
            node_id: "n".to_string(),
            output,
        }
    }

    #[test]
    fn shell_zero_exit_is_ok() {
        assert!(node_response_ok(
            &shell_kind(),
            &resp(json!({"exit_code": 0, "stdout": "", "stderr": ""})),
        ));
    }

    #[test]
    fn shell_nonzero_exit_is_fail() {
        assert!(!node_response_ok(
            &shell_kind(),
            &resp(json!({"exit_code": 2, "stdout": "", "stderr": ""})),
        ));
    }

    #[test]
    fn shell_missing_exit_code_is_ok() {
        // Defensive: if a future ShellExecutor variant omits exit_code,
        // fall back to success rather than flagging the node failed.
        assert!(node_response_ok(&shell_kind(), &resp(json!({}))));
    }

    #[test]
    fn shell_null_exit_code_is_fail() {
        // An `exit_code: null` payload is a contract violation (the key
        // is present but unusable) — must surface as failure rather than
        // being silently treated as success the way an absent key is.
        assert!(!node_response_ok(
            &shell_kind(),
            &resp(json!({"exit_code": null, "stdout": "", "stderr": ""})),
        ));
    }

    #[test]
    fn agent_payload_with_exit_code_is_still_ok() {
        // Regression guard: an agent response whose JSON payload happens
        // to contain an `exit_code` field must not be misread as failed.
        assert!(node_response_ok(
            &agent_kind(),
            &resp(json!({"exit_code": 1, "assistant": "done"})),
        ));
    }

    #[test]
    fn render_error_chain_walks_source_links() {
        // A `thiserror` Display on `NodeError::Execution` only renders
        // the top-level variant. The operator needs the actual cause
        // (e.g. `agent exceeded budget: Tokens` under `node `digest`
        // failed`) to act on the failure. This guards the chain walker
        // that restores that information.
        use crate::error::{AgentError, NodeError};
        use crate::events::BudgetKind;
        let err = NodeError::Execution {
            id: "digest".to_string(),
            source: Box::new(AgentError::BudgetExceeded {
                kind: BudgetKind::Tokens,
            }),
        };
        let rendered = render_error_chain(&err);
        assert!(
            rendered.contains("node `digest` failed"),
            "rendered chain missing top-level message: {rendered}",
        );
        assert!(
            rendered.contains("agent exceeded budget"),
            "rendered chain missing nested source: {rendered}",
        );
        assert!(
            rendered.contains("Tokens"),
            "rendered chain missing BudgetKind classifier: {rendered}",
        );
    }

    #[test]
    fn render_error_chain_handles_terminal_error() {
        // An error with no source must still render its own Display
        // without trailing separators or panics — the walker stops
        // cleanly at the first `source() -> None`.
        use crate::error::NodeError;
        let err = NodeError::NotImplemented {
            id: "stub".to_string(),
        };
        let rendered = render_error_chain(&err);
        assert!(rendered.contains("not implemented"));
        assert!(
            !rendered.ends_with(": "),
            "trailing separator leaked: {rendered}",
        );
    }

    #[test]
    fn truncate_tail_keeps_trailing_window() {
        // Last bytes win because the tail of stderr is where the cause
        // typically sits (stack trace, fatal: line). The leading "…"
        // marker is the only signal that truncation happened.
        let s: String = (0u8..100).map(|i| char::from(b'a' + (i % 26))).collect();
        let cut = truncate_tail(&s, 10);
        assert!(cut.starts_with('…'), "marker missing: {cut}");
        assert_eq!(
            cut.chars().count(),
            11,
            "expected 1 marker + 10 chars: {cut}"
        );
        assert!(s.ends_with(&cut[cut.char_indices().nth(1).unwrap().0..]));
    }

    #[test]
    fn truncate_tail_passthrough_when_within_cap() {
        assert_eq!(truncate_tail("short", 100), "short");
    }

    #[test]
    fn truncate_tail_respects_utf8_boundaries() {
        // Multi-byte chars at the boundary must not be split. Each "❤"
        // is 3 bytes; cap of 4 must round forward to a boundary, not
        // emit broken UTF-8.
        let s = "abc❤def❤ghi";
        let cut = truncate_tail(s, 4);
        // The function returns a String; if the slice was mid-codepoint
        // the format! would have panicked, so a successful return is
        // already proof of boundary safety.
        assert!(cut.starts_with('…'));
        assert!(s.ends_with(cut.trim_start_matches('…')));
    }
}
