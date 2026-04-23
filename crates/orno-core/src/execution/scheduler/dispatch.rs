//! Per-node dispatch — the inner loop of `Engine::run`.
//!
//! Kept in a sibling file because dispatch concerns (executor lookup,
//! template render, timeout wrapping, payload classification) are
//! orthogonal to `Engine::run`'s orchestration of the walker and the
//! run-level aggregates. The inherent `impl Engine` block here adds
//! `dispatch_node` + `on_payload_failure`; the free helpers
//! (`render_error_chain`, `truncate_tail`, `node_response_ok`) stay
//! module-private and get their unit coverage in the test submodule
//! below. `DispatchOutcome` is `pub(super)` so `Engine::run` in
//! `mod.rs` can destructure it.
//!
//! The split is size-driven (CLAUDE.md's 300-LOC soft cap on
//! non-test LOC); there is no behavioral change versus the original
//! monolithic `scheduler.rs`.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use crate::events::{Event, NodeFailure};
use crate::execution::context::Context;
use crate::node::{NodeResponse, kind_str, render_request};
use crate::pipeline::schema::NodeKind;

use super::Engine;

/// Result of `Engine::dispatch_node`. A struct rather than a tuple
/// because the success/failure invariant is now load-bearing
/// (`failure.is_some() ⇔ !ok`) and the field count grew from two
/// to three when `Phase 2` started recording payload-on-failure.
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

    /// A failure with no executor output to record — used for the
    /// no-executor, template-render, and executor-Err paths.
    fn failed(failure: NodeFailure) -> Self {
        Self {
            ok: false,
            failure: Some(failure),
            output: None,
        }
    }
}

impl Engine {
    /// Dispatch a single ready node. Every failure mode (missing
    /// executor, template render error, executor error, non-zero shell
    /// exit) collapses to `ok = false` with a populated `failure` and
    /// a structured WARN on stderr. The executor's payload is returned
    /// even on failure so the engine can fold it into `Context` —
    /// downstream templates that read `node.<id>.stderr` for recovery
    /// branches must see the data even when the producer node failed.
    pub(super) async fn dispatch_node(
        &self,
        run_id: &str,
        node: &crate::pipeline::Node,
        context: &Context,
        agents: &BTreeMap<String, crate::pipeline::AgentConfig>,
        redactor: &crate::events::Redactor,
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

        let req = match render_request(&node.kind, &self.templates, context, agents) {
            Ok(req) => req,
            Err(err) => {
                // Redact before logging: a template error's Display chain
                // can quote the offending expression, which may include a
                // rendered `secrets.*` value (ADR 0020).
                let error = redactor.redact(&format!("{err:#}")).into_owned();
                tracing::warn!(
                    node.id = %node.id,
                    node.kind = kind,
                    error = %error,
                    "render_request failed",
                );
                return DispatchOutcome::failed(NodeFailure::TemplateRenderFailed { error });
            }
        };

        let started = std::time::Instant::now();
        let result = if let Some(limit) = node.timeout {
            match tokio::time::timeout(
                Duration::from_secs(limit),
                exec.execute(run_id, &node.id, req),
            )
            .await
            {
                Ok(inner) => inner,
                Err(_elapsed) => {
                    let elapsed_ms =
                        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    self.sink
                        .record(Event::NodeTimedOut {
                            run_id: run_id.to_string(),
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
                    return DispatchOutcome::failed(NodeFailure::TimedOut { limit_secs: limit });
                }
            }
        } else {
            exec.execute(run_id, &node.id, req).await
        };

        match result {
            Ok(resp) => {
                if node_response_ok(&node.kind, &resp) {
                    return DispatchOutcome::ok(resp.output);
                }
                self.on_payload_failure(node, kind, resp, redactor)
            }
            Err(err) => {
                // Walk the full `source()` chain — thiserror's Display
                // only renders the top-level variant, so a `NodeError::
                // Execution { source: AgentError::BudgetExceeded(...) }`
                // would otherwise surface as the opaque
                // ``node `digest` failed`` without the underlying cause
                // that the operator actually needs to see. Redact and
                // cap because an LLM transport error can surface a
                // multi-KB HTML body and a rendered prompt can carry
                // a secret value.
                let error = truncate_tail(
                    &redactor.redact(&render_error_chain(&err)),
                    self.config.max_output_bytes,
                );
                tracing::warn!(
                    node.id = %node.id,
                    node.kind = kind,
                    error = %error,
                    "node execution failed",
                );
                DispatchOutcome::failed(NodeFailure::ExecutorError { error })
            }
        }
    }

    /// Format the shell-payload-failure branch of `dispatch_node`. A
    /// shell node that ran and exited non-zero used to silently drop
    /// its stderr/stdout; this branch emits a typed `NodeFailure` on
    /// the wire, logs a structured WARN, and still hands `resp.output`
    /// back to Context so a downstream `gate` / recovery node can read
    /// `node.<id>.stderr` and decide what to do. Extracted from
    /// `dispatch_node` because the wire-formatting concern is distinct
    /// from dispatch orchestration.
    fn on_payload_failure(
        &self,
        node: &crate::pipeline::Node,
        kind: &str,
        resp: NodeResponse,
        redactor: &crate::events::Redactor,
    ) -> DispatchOutcome {
        let exit_code = resp.output.get("exit_code").and_then(Value::as_i64);
        let stderr_tail = resp.output.get("stderr").and_then(Value::as_str).map(|s| {
            redactor
                .redact(&truncate_tail(s, self.config.max_output_bytes))
                .into_owned()
        });
        let stdout_tail = if self.config.verbose {
            resp.output
                .get("stdout")
                .and_then(Value::as_str)
                .map(|s| {
                    redactor
                        .redact(&truncate_tail(s, self.config.max_output_bytes))
                        .into_owned()
                })
                .unwrap_or_default()
        } else {
            String::new()
        };
        tracing::warn!(
            node.id = %node.id,
            node.kind = kind,
            exit_code = exit_code.unwrap_or(-1),
            stderr_tail = %stderr_tail.as_deref().unwrap_or(""),
            stdout_tail = %stdout_tail,
            "node returned failure in payload",
        );
        DispatchOutcome {
            ok: false,
            failure: Some(NodeFailure::NodePayloadFailure {
                exit_code,
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

/// Keep the **last** `max_bytes` of a string, on a UTF-8 boundary.
/// Last bytes win because the trailing fragment of a tool's stderr
/// is almost always where the actionable error sits — earlier output
/// is setup noise. Returns the input unchanged when it already fits.
fn truncate_tail(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let cut = s.len() - max_bytes;
    let mut start = cut;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &s[start..])
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
            }
        },
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::schema::{AgentNode, ShellNode};
    use serde_json::json;

    fn shell_kind() -> NodeKind {
        NodeKind::Shell(ShellNode {
            command: "echo".to_string(),
            args: Vec::new(),
            stdin: None,
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
