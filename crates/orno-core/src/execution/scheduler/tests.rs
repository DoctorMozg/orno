use super::*;
use crate::events::{InMemorySink, NodeFailure};
use crate::pipeline::schema::{Node, NodeKind, Pipeline, ShellNode};

/// Build a `Pipeline` carrying the given nodes and no vars/env/secrets.
fn pipeline_of(nodes: Vec<Node>) -> Pipeline {
    Pipeline {
        version: 1,
        vars: BTreeMap::new(),
        pass_env: Vec::new(),
        secrets: Vec::new(),
        agents: BTreeMap::new(),
        mcp_servers: BTreeMap::new(),
        nodes,
    }
}

fn shell_node(id: &str, command: &str, needs: &[&str]) -> Node {
    Node {
        id: id.to_string(),
        kind: NodeKind::Shell(ShellNode {
            command: command.to_string(),
            args: Vec::new(),
            stdin: None,
            env: BTreeMap::new(),
        }),
        needs: needs.iter().map(|s| (*s).to_string()).collect(),
        timeout: None,
    }
}

// One global subscriber captures WARN+ into a shared buffer and a mutex
// serializes the tests reading it. A per-test thread-local default came
// back empty under CI load when sibling tests scheduled their own
// subscribers across tokio workers.
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use tracing_subscriber::fmt::MakeWriter;

static TRACE_CAPTURE_LOCK: Mutex<()> = Mutex::new(());
static TRACE_BUFFER: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
static TRACE_INIT: OnceLock<()> = OnceLock::new();

#[derive(Clone)]
struct BufferWriter(Arc<Mutex<Vec<u8>>>);

impl<'a> MakeWriter<'a> for BufferWriter {
    type Writer = BufferGuard;
    fn make_writer(&'a self) -> Self::Writer {
        BufferGuard(self.0.clone())
    }
}

struct BufferGuard(Arc<Mutex<Vec<u8>>>);

impl Write for BufferGuard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("buffer mutex").write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Install the global JSON-capturing tracing subscriber on first
/// call, return the shared buffer on every call. Caller MUST hold
/// `TRACE_CAPTURE_LOCK` for the duration of its capture window —
/// otherwise concurrent capture tests will see each other's events.
/// The buffer is cleared on every entry so each test starts clean.
fn install_global_capture() -> Arc<Mutex<Vec<u8>>> {
    let buf = TRACE_BUFFER
        .get_or_init(|| Arc::new(Mutex::new(Vec::new())))
        .clone();
    TRACE_INIT.get_or_init(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(BufferWriter(buf.clone()))
            .with_max_level(tracing::Level::WARN)
            .json()
            .finish();
        // `set_global_default` is the only sound way to share one
        // subscriber across every test thread without per-thread
        // races. It can only be called once per process, hence the
        // `OnceLock`.
        tracing::subscriber::set_global_default(subscriber)
            .expect("global tracing subscriber installed exactly once");
    });
    buf.lock().expect("buffer mutex").clear();
    buf
}

fn capture_drain(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8(buf.lock().expect("buffer mutex").clone())
        .expect("captured tracing output is valid UTF-8")
}

#[tokio::test]
async fn missing_executor_surfaces_failure() {
    // Engine must treat a kind with no registered executor as a node
    // failure — never panic, never silently succeed. An empty
    // registry over a shell node is the shortest path to that branch.
    // Also asserts the wire `failure` carries
    // `NoExecutorRegistered { node_kind }` so consumers know *why*.
    let pipeline = pipeline_of(vec![shell_node("lonely", "echo", &[])]);
    let sink = Arc::new(InMemorySink::new());
    let registry = Arc::new(NodeRegistry::new());
    let templates = Arc::new(TemplateEngine::new());
    let engine = Engine::new(sink.clone(), registry, templates, EngineConfig::default());

    engine
        .run("run_test", &pipeline, RunInputs::default())
        .await
        .expect("engine::run itself returns Ok even on node failure");

    let envelopes = sink.snapshot();
    let node_finished = envelopes.iter().find_map(|e| match &e.event {
        Event::NodeFinished {
            node_id,
            ok,
            failure,
            ..
        } if node_id == "lonely" => Some((*ok, failure.clone())),
        _ => None,
    });
    let (node_ok, failure) = node_finished.expect("NodeFinished for `lonely` must exist");
    assert!(!node_ok, "missing executor must emit NodeFinished ok:false");
    match failure {
        Some(NodeFailure::NoExecutorRegistered { node_kind }) => {
            assert_eq!(node_kind, "shell", "node_kind must echo the YAML kind");
        },
        other => panic!("expected NoExecutorRegistered, got {other:?}"),
    }
    let run_ok = envelopes.iter().find_map(|e| match &e.event {
        Event::RunFinished { ok, .. } => Some(*ok),
        _ => None,
    });
    assert_eq!(
        run_ok,
        Some(false),
        "aggregate run must surface ok:false when any node fails",
    );
}

#[tokio::test]
async fn stderr_tail_redacts_secret_cut_by_truncation_window() {
    use crate::node::shell::ShellExecutor;

    let secret = "supersecretvalue";
    let mut node = shell_node("leaky", "sh", &[]);
    if let NodeKind::Shell(shell) = &mut node.kind {
        shell.args = vec![
            "-c".to_string(),
            format!("printf '%050d%s' 0 {secret} >&2; exit 1"),
        ];
    }
    let sink = Arc::new(InMemorySink::new());
    let mut reg = NodeRegistry::new();
    reg.register("shell", Arc::new(ShellExecutor::default()));
    let config = EngineConfig {
        // The 10-byte tail window starts inside the secret.
        max_output_bytes: 10,
        ..EngineConfig::default()
    };
    let engine = Engine::new(
        sink.clone(),
        Arc::new(reg),
        Arc::new(TemplateEngine::new()),
        config,
    );
    let inputs = RunInputs {
        secrets: BTreeMap::from([("TOKEN".to_string(), secret.to_string())]),
        ..RunInputs::default()
    };

    engine
        .run("run_test", &pipeline_of(vec![node]), inputs)
        .await
        .expect("run completes with a failed node");

    let tail = sink
        .snapshot()
        .iter()
        .find_map(|e| match &e.event {
            Event::NodeFinished {
                failure: Some(NodeFailure::NodePayloadFailure { stderr_tail, .. }),
                ..
            } => stderr_tail.clone(),
            _ => None,
        })
        .expect("payload failure with stderr tail");
    assert!(!tail.contains("ecretvalue"), "secret tail leaked: {tail}");
}

#[tokio::test]
async fn independent_success_does_not_rescue_run_ok() {
    // Two independent nodes, only one of which fails. The succeeding
    // sibling must still run (sibling_failure_does_not_skip_independent
    // covers the walker half of this invariant); the aggregate run
    // must report ok:false because `run_ok` is latched by any failure.
    use crate::node::shell::ShellExecutor;

    let pipeline = pipeline_of(vec![
        shell_node(
            "fail",
            "definitely-not-a-real-program-for-tests-xyz-12345",
            &[],
        ),
        shell_node("ok", "true", &[]),
    ]);
    let sink = Arc::new(InMemorySink::new());
    let mut reg = NodeRegistry::new();
    reg.register("shell", Arc::new(ShellExecutor::default()));
    let registry = Arc::new(reg);
    let templates = Arc::new(TemplateEngine::new());
    let engine = Engine::new(sink.clone(), registry, templates, EngineConfig::default());

    engine
        .run("run_test", &pipeline, RunInputs::default())
        .await
        .expect("engine::run returns Ok even on partial failure");

    let envelopes = sink.snapshot();
    let mut fail_ok = None;
    let mut ok_ok = None;
    let mut run_ok = None;
    for e in &envelopes {
        match &e.event {
            Event::NodeFinished { node_id, ok, .. } if node_id == "fail" => fail_ok = Some(*ok),
            Event::NodeFinished { node_id, ok, .. } if node_id == "ok" => ok_ok = Some(*ok),
            Event::RunFinished { ok, .. } => run_ok = Some(*ok),
            _ => {},
        }
    }
    assert_eq!(fail_ok, Some(false), "failing node must report ok:false");
    assert_eq!(
        ok_ok,
        Some(true),
        "independent `ok` node must still execute and report ok:true",
    );
    assert_eq!(
        run_ok,
        Some(false),
        "run must report ok:false when any node fails, even with a succeeding sibling",
    );
}

#[tokio::test]
async fn run_finished_aggregates_failed_and_skipped_in_causal_order() {
    // `RunFinished` carries causal-order
    // vectors so a downstream tool that reads only the last
    // envelope sees the failure footprint without folding the
    // stream. "Causal order" means stream-emission order — the
    // failed node first, then each transitively-skipped descendant
    // in the order the walker surfaced them.
    use crate::node::shell::ShellExecutor;

    let pipeline = pipeline_of(vec![
        shell_node(
            "fail",
            "definitely-not-a-real-program-for-tests-xyz-12345",
            &[],
        ),
        shell_node("mid", "true", &["fail"]),
        shell_node("leaf", "true", &["mid"]),
        // Independent sibling: must NOT appear in either vector.
        shell_node("indep", "true", &[]),
    ]);
    let sink = Arc::new(InMemorySink::new());
    let mut reg = NodeRegistry::new();
    reg.register("shell", Arc::new(ShellExecutor::default()));
    let registry = Arc::new(reg);
    let templates = Arc::new(TemplateEngine::new());
    let engine = Engine::new(sink.clone(), registry, templates, EngineConfig::default());

    engine
        .run("run_test", &pipeline, RunInputs::default())
        .await
        .expect("engine::run returns Ok even with a failing node");

    let envelopes = sink.snapshot();
    let (ok, failed_nodes, skipped_nodes) = envelopes
        .iter()
        .find_map(|e| match &e.event {
            Event::RunFinished {
                ok,
                failed_nodes,
                skipped_nodes,
                ..
            } => Some((*ok, failed_nodes.clone(), skipped_nodes.clone())),
            _ => None,
        })
        .expect("RunFinished must be in the stream");

    assert!(!ok, "any failure latches run.ok to false");
    assert_eq!(failed_nodes, vec!["fail".to_string()]);
    // Skip cascade order is whatever the walker surfaces; both
    // descendants must be present and `indep` must not be.
    assert!(
        skipped_nodes.contains(&"mid".to_string()) && skipped_nodes.contains(&"leaf".to_string()),
        "transitive descendants must be in skipped_nodes: {skipped_nodes:?}",
    );
    assert!(
        !skipped_nodes.contains(&"indep".to_string()),
        "independent sibling must not be reported as skipped: {skipped_nodes:?}",
    );
    assert_eq!(skipped_nodes.len(), 2, "exactly two descendants skipped");
}

#[tokio::test]
async fn run_finished_aggregates_are_empty_on_clean_run() {
    // A fully-green run must report empty vectors so a downstream
    // tool can branch on `failed_nodes.is_empty() && skipped_nodes.is_empty()`
    // without special-casing the absence of the keys.
    use crate::node::shell::ShellExecutor;

    let pipeline = pipeline_of(vec![
        shell_node("a", "true", &[]),
        shell_node("b", "true", &["a"]),
    ]);
    let sink = Arc::new(InMemorySink::new());
    let mut reg = NodeRegistry::new();
    reg.register("shell", Arc::new(ShellExecutor::default()));
    let registry = Arc::new(reg);
    let templates = Arc::new(TemplateEngine::new());
    let engine = Engine::new(sink.clone(), registry, templates, EngineConfig::default());

    engine
        .run("run_test", &pipeline, RunInputs::default())
        .await
        .expect("clean run returns Ok");

    let envelopes = sink.snapshot();
    let (ok, failed, skipped) = envelopes
        .iter()
        .find_map(|e| match &e.event {
            Event::RunFinished {
                ok,
                failed_nodes,
                skipped_nodes,
                ..
            } => Some((*ok, failed_nodes.clone(), skipped_nodes.clone())),
            _ => None,
        })
        .expect("RunFinished must be present");
    assert!(ok);
    assert!(failed.is_empty(), "no failures expected: {failed:?}");
    assert!(skipped.is_empty(), "no skips expected: {skipped:?}");
}

#[tokio::test]
async fn shell_nonzero_exit_emits_node_payload_failure_on_wire() {
    // The wire `failure` field on NodeFinished must
    // carry exit_code and a stderr_tail when the shell exits non-zero.
    // The tail must surface the actual stderr substring so a UI can
    // display it without parsing tracing JSON.
    use crate::node::shell::ShellExecutor;

    let pipeline = pipeline_of(vec![Node {
        id: "fail".to_string(),
        kind: NodeKind::Shell(ShellNode {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "echo 'visible-cause' 1>&2; exit 7".to_string(),
            ],
            stdin: None,
            env: BTreeMap::new(),
        }),
        needs: Vec::new(),
        timeout: None,
    }]);
    let sink = Arc::new(InMemorySink::new());
    let mut reg = NodeRegistry::new();
    reg.register("shell", Arc::new(ShellExecutor::default()));
    let registry = Arc::new(reg);
    let templates = Arc::new(TemplateEngine::new());
    let engine = Engine::new(sink.clone(), registry, templates, EngineConfig::default());

    engine
        .run("run_test", &pipeline, RunInputs::default())
        .await
        .expect("engine::run returns Ok even on node failure");

    let envelopes = sink.snapshot();
    let failure = envelopes
        .iter()
        .find_map(|e| match &e.event {
            Event::NodeFinished {
                node_id, failure, ..
            } if node_id == "fail" => failure.clone(),
            _ => None,
        })
        .expect("failed NodeFinished must carry a failure");

    match failure {
        NodeFailure::NodePayloadFailure {
            exit_code,
            signal,
            stderr_tail,
        } => {
            assert_eq!(exit_code, Some(7), "exit_code must round-trip on the wire");
            assert_eq!(signal, None, "normal non-zero exit must report signal=None");
            let tail = stderr_tail.expect("stderr_tail must be present when stderr ran");
            assert!(
                tail.contains("visible-cause"),
                "stderr_tail must surface the actual cause: {tail}",
            );
        },
        other => panic!("expected NodePayloadFailure, got {other:?}"),
    }
}

#[tokio::test]
async fn shell_failure_payload_still_lands_in_context_for_downstream() {
    // Even when a shell node fails, its `stderr` and `exit_code` flow
    // into Context under `node.<id>.*`, so a recovery template
    // (`{{ node.fail.exit_code }}`) can read them.
    use crate::node::shell::ShellExecutor;

    let pipeline = pipeline_of(vec![
        Node {
            id: "fail".to_string(),
            kind: NodeKind::Shell(ShellNode {
                command: "sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    "echo 'recoverable' 1>&2; exit 9".to_string(),
                ],
                stdin: None,
                env: BTreeMap::new(),
            }),
            needs: Vec::new(),
            timeout: None,
        },
        // `recover` does NOT depend on `fail` (depending would
        // skip-cascade it). The point is to prove the *Context*
        // entry exists; we read it via the snapshot below.
        shell_node("recover", "true", &[]),
    ]);
    let sink = Arc::new(InMemorySink::new());
    let mut reg = NodeRegistry::new();
    reg.register("shell", Arc::new(ShellExecutor::default()));
    let registry = Arc::new(reg);
    let templates = Arc::new(TemplateEngine::new());
    let engine = Engine::new(sink.clone(), registry, templates, EngineConfig::default());

    engine
        .run("run_test", &pipeline, RunInputs::default())
        .await
        .expect("engine::run must succeed even with a failing node");

    // The wire-format check is the easiest indirect way to prove the
    // payload was preserved past dispatch_node: if the executor's
    // resp.output had been dropped, NodePayloadFailure.exit_code
    // would be None.
    let exit_code = sink.snapshot().iter().find_map(|e| match &e.event {
        Event::NodeFinished {
            node_id,
            failure: Some(NodeFailure::NodePayloadFailure { exit_code, .. }),
            ..
        } if node_id == "fail" => *exit_code,
        _ => None,
    });
    assert_eq!(
        exit_code,
        Some(9),
        "failing shell payload must reach the wire, proving it was not discarded",
    );
}

#[expect(
    clippy::await_holding_lock,
    reason = "TRACE_CAPTURE_LOCK guards a `()`; it serializes capture tests so the shared global tracing buffer is read by exactly one test at a time. Holding across `.await` is safe on the `current_thread` runtime — there is no other task that could contend for the same lock."
)]
#[tokio::test(flavor = "current_thread")]
async fn shell_nonzero_exit_emits_warn_with_exit_code_and_stderr_tail() {
    use crate::node::shell::ShellExecutor;
    // A shell that runs and exits non-zero must surface its
    // stderr/exit_code in a structured WARN for log pipelines.
    let _capture_lock = TRACE_CAPTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let pipeline = pipeline_of(vec![Node {
        id: "loud_fail".to_string(),
        kind: NodeKind::Shell(ShellNode {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "echo 'this is the failure reason' 1>&2; exit 2".to_string(),
            ],
            stdin: None,
            env: BTreeMap::new(),
        }),
        needs: Vec::new(),
        timeout: None,
    }]);
    let sink = Arc::new(InMemorySink::new());
    let mut reg = NodeRegistry::new();
    reg.register("shell", Arc::new(ShellExecutor::default()));
    let registry = Arc::new(reg);
    let templates = Arc::new(TemplateEngine::new());
    let engine = Engine::new(sink, registry, templates, EngineConfig::default());

    let buf = install_global_capture();

    engine
        .run("run_test", &pipeline, RunInputs::default())
        .await
        .expect("engine::run returns Ok even when a node fails");

    let logs = capture_drain(&buf);

    assert!(
        logs.contains("node returned failure in payload"),
        "warn message missing: {logs}",
    );
    assert!(
        logs.contains(r#""exit_code":2"#),
        "exit_code field missing or wrong: {logs}",
    );
    assert!(
        logs.contains("this is the failure reason"),
        "stderr_tail must surface child stderr: {logs}",
    );
}

#[expect(
    clippy::await_holding_lock,
    reason = "TRACE_CAPTURE_LOCK guards a `()`; it serializes capture tests so the shared global tracing buffer is read by exactly one test at a time. Holding across `.await` is safe on the `current_thread` runtime — there is no other task that could contend for the same lock."
)]
#[tokio::test(flavor = "current_thread")]
async fn walker_construction_failure_emits_warn_before_propagating() {
    // A cyclic graph never gets a node started — so the per-node
    // failure path can't surface it. Engine::run must emit a WARN
    // before propagating the InvalidGraph error so log pipelines
    // see the cause without parsing the CLI's stderr error chain.
    let _capture_lock = TRACE_CAPTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let pipeline = pipeline_of(vec![
        shell_node("a", "true", &["b"]),
        shell_node("b", "true", &["a"]),
    ]);
    let sink = Arc::new(InMemorySink::new());
    let registry = Arc::new(NodeRegistry::new());
    let templates = Arc::new(TemplateEngine::new());
    let engine = Engine::new(sink, registry, templates, EngineConfig::default());

    let buf = install_global_capture();

    let err = engine
        .run("run_test", &pipeline, RunInputs::default())
        .await
        .expect_err("cycle must surface as Err");

    let logs = capture_drain(&buf);

    assert!(
        logs.contains("walker construction failed"),
        "expected walker-failure WARN: {logs}",
    );
    assert!(
        logs.contains("cycle detected"),
        "WARN must include the underlying reason: {logs}",
    );
    // Sanity-check: the error itself still propagates with the
    // same diagnostic so the CLI can print the chain.
    assert!(
        format!("{err}").contains("graph") || format!("{err:#}").contains("cycle"),
        "propagated error should mention graph/cycle: {err:#}",
    );
}

#[expect(
    clippy::too_many_lines,
    reason = "integration test scenario; long by necessity"
)]
#[tokio::test(flavor = "current_thread")]
async fn shell_node_timeout_fires_and_emits_node_timed_out_and_failure() {
    use crate::node::shell::ShellExecutor;

    let pipeline = pipeline_of(vec![Node {
        id: "slow".to_string(),
        kind: NodeKind::Shell(ShellNode {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "sleep 10".to_string()],
            stdin: None,
            env: BTreeMap::new(),
        }),
        needs: Vec::new(),
        timeout: Some(1),
    }]);
    let sink = Arc::new(InMemorySink::new());
    let mut reg = NodeRegistry::new();
    reg.register("shell", Arc::new(ShellExecutor::default()));
    let registry = Arc::new(reg);
    let templates = Arc::new(TemplateEngine::new());
    let engine = Engine::new(sink.clone(), registry, templates, EngineConfig::default());

    engine
        .run("run_test", &pipeline, RunInputs::default())
        .await
        .expect("engine::run returns Ok even when a node times out");

    let envelopes = sink.snapshot();
    let timed_out_idx = envelopes
        .iter()
        .position(|e| matches!(e.event, Event::NodeTimedOut { .. }))
        .expect("Event::NodeTimedOut must fire on timeout");
    let finished_idx = envelopes
        .iter()
        .position(|e| matches!(e.event, Event::NodeFinished { ok: false, .. }))
        .expect("NodeFinished(ok: false) must fire");
    assert!(
        timed_out_idx < finished_idx,
        "NodeTimedOut must precede NodeFinished in stream order",
    );

    if let Event::NodeTimedOut {
        limit_secs,
        elapsed_ms,
        node_id,
        ..
    } = &envelopes[timed_out_idx].event
    {
        assert_eq!(node_id, "slow");
        assert_eq!(*limit_secs, 1);
        assert!(
            *elapsed_ms >= 1000,
            "elapsed_ms should be >= 1000: {elapsed_ms}"
        );
    }

    let failure = envelopes
        .iter()
        .find_map(|e| match &e.event {
            Event::NodeFinished { failure, .. } => failure.clone(),
            _ => None,
        })
        .expect("NodeFinished must carry a failure");
    assert!(
        matches!(failure, NodeFailure::TimedOut { limit_secs: 1, .. }),
        "failure must classify as TimedOut(1): {failure:?}",
    );

    let failed_nodes = envelopes
        .iter()
        .find_map(|e| match &e.event {
            Event::RunFinished { failed_nodes, .. } => Some(failed_nodes.clone()),
            _ => None,
        })
        .expect("RunFinished must be present");
    assert_eq!(failed_nodes, vec!["slow".to_string()]);
}

#[tokio::test(flavor = "current_thread")]
async fn shell_node_timeout_keeps_partial_output() {
    use crate::node::shell::ShellExecutor;

    let pipeline = pipeline_of(vec![Node {
        id: "slow".to_string(),
        kind: NodeKind::Shell(ShellNode {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "echo stuck-on-step-3 1>&2; sleep 10".to_string(),
            ],
            stdin: None,
            env: BTreeMap::new(),
        }),
        needs: Vec::new(),
        timeout: Some(1),
    }]);
    let sink = Arc::new(InMemorySink::new());
    let mut reg = NodeRegistry::new();
    reg.register("shell", Arc::new(ShellExecutor::default()));
    let engine = Engine::new(
        sink.clone(),
        Arc::new(reg),
        Arc::new(TemplateEngine::new()),
        EngineConfig::default(),
    );

    let started = std::time::Instant::now();
    engine
        .run("run_test", &pipeline, RunInputs::default())
        .await
        .expect("engine::run returns Ok even when a node times out");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "timeout must not wait for the sleep"
    );

    let envelopes = sink.snapshot();
    assert_eq!(
        envelopes
            .iter()
            .filter(|e| matches!(e.event, Event::NodeTimedOut { .. }))
            .count(),
        1,
        "exactly one NodeTimedOut"
    );
    let failure = envelopes
        .iter()
        .find_map(|e| match &e.event {
            Event::NodeFinished { failure, .. } => failure.clone(),
            _ => None,
        })
        .expect("NodeFinished must carry a failure");
    match failure {
        NodeFailure::TimedOut {
            limit_secs: 1,
            stderr_tail: Some(tail),
        } => assert_eq!(tail, "stuck-on-step-3\n"),
        other => panic!("expected TimedOut with stderr_tail: {other:?}"),
    }
}

#[tokio::test]
async fn timeout_none_lets_node_run_to_completion() {
    use crate::node::shell::ShellExecutor;

    let pipeline = pipeline_of(vec![Node {
        id: "quick".to_string(),
        kind: NodeKind::Shell(ShellNode {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "sleep 0.2 && echo done".to_string()],
            stdin: None,
            env: BTreeMap::new(),
        }),
        needs: Vec::new(),
        timeout: None,
    }]);
    let sink = Arc::new(InMemorySink::new());
    let mut reg = NodeRegistry::new();
    reg.register("shell", Arc::new(ShellExecutor::default()));
    let registry = Arc::new(reg);
    let templates = Arc::new(TemplateEngine::new());
    let engine = Engine::new(sink.clone(), registry, templates, EngineConfig::default());

    engine
        .run("run_test", &pipeline, RunInputs::default())
        .await
        .expect("engine::run succeeds");

    let envelopes = sink.snapshot();
    assert!(
        !envelopes
            .iter()
            .any(|e| matches!(e.event, Event::NodeTimedOut { .. })),
        "NodeTimedOut must not fire when timeout is None",
    );
    let ok = envelopes
        .iter()
        .find_map(|e| match &e.event {
            Event::NodeFinished { ok, .. } => Some(*ok),
            _ => None,
        })
        .expect("NodeFinished present");
    assert!(ok, "node with timeout=None must succeed");
}

#[tokio::test]
async fn pipeline_vars_are_rendered_against_env_before_node_dispatch() {
    // End-to-end guard for the var-pre-render pass that the
    // release-notes / pr-review / flaky-test-triage examples depend
    // on. Without it, a shell node referencing `{{ vars.X }}` —
    // where `vars.X` was authored as `"{{ env.X }}"` — would receive
    // the literal six-token template source instead of the env
    // value. The test runs a real `ShellExecutor` with `printf` so
    // the captured stdout proves the value made the round trip.
    use crate::node::shell::ShellExecutor;
    use crate::pipeline::schema::ShellNode;

    let mut vars = BTreeMap::new();
    vars.insert(
        "tag".to_string(),
        serde_json::json!("{{ env.RELEASE_TAG }}"),
    );
    let mut pipeline = pipeline_of(vec![Node {
        id: "echo_tag".to_string(),
        kind: NodeKind::Shell(ShellNode {
            command: "printf".to_string(),
            args: vec!["%s".to_string(), "{{ vars.tag }}".to_string()],
            stdin: None,
            env: BTreeMap::new(),
        }),
        needs: Vec::new(),
        timeout: None,
    }]);
    pipeline.vars = vars;

    let sink = Arc::new(InMemorySink::new());
    let mut reg = NodeRegistry::new();
    reg.register("shell", Arc::new(ShellExecutor::default()));
    let registry = Arc::new(reg);
    let templates = Arc::new(TemplateEngine::new());
    let engine = Engine::new(sink.clone(), registry, templates, EngineConfig::default());

    let mut env = BTreeMap::new();
    env.insert("RELEASE_TAG".to_string(), "v0.1.1".to_string());
    let inputs = RunInputs {
        env,
        secrets: BTreeMap::new(),
        rendered_vars: None,
    };

    let outcome = engine
        .run("run_test", &pipeline, inputs)
        .await
        .expect("engine::run succeeds");
    assert!(outcome.ok, "node must succeed: {outcome:?}");

    // The shell executor returns its captured stdout as the node
    // output payload. Read it back through the InMemorySink and
    // confirm the env value made the round trip.
    let envelopes = sink.snapshot();
    let finished = envelopes
        .iter()
        .find(|e| matches!(&e.event, Event::NodeFinished { node_id, .. } if node_id == "echo_tag"))
        .expect("NodeFinished present");
    match &finished.event {
        Event::NodeFinished { ok, failure, .. } => {
            assert!(ok, "node must finish ok, got failure={failure:?}");
        },
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn missing_env_in_pipeline_var_surfaces_template_error() {
    // The render pass is strict-undefined: a `vars.X: "{{ env.MISSING }}"`
    // with no `env.MISSING` declared must fail the run before the
    // first node fires, with a `PipelineError::Template` carrying
    // the offending var name. The early failure is preferable to a
    // mid-DAG render error because the user gets a single, scoped
    // diagnostic instead of a cascade.
    use crate::node::shell::ShellExecutor;
    use crate::pipeline::schema::ShellNode;

    let mut vars = BTreeMap::new();
    vars.insert(
        "tag".to_string(),
        serde_json::json!("{{ env.NOT_DECLARED }}"),
    );
    let mut pipeline = pipeline_of(vec![Node {
        id: "noop".to_string(),
        kind: NodeKind::Shell(ShellNode {
            command: "true".to_string(),
            args: Vec::new(),
            stdin: None,
            env: BTreeMap::new(),
        }),
        needs: Vec::new(),
        timeout: None,
    }]);
    pipeline.vars = vars;

    let sink = Arc::new(InMemorySink::new());
    let mut reg = NodeRegistry::new();
    reg.register("shell", Arc::new(ShellExecutor::default()));
    let registry = Arc::new(reg);
    let templates = Arc::new(TemplateEngine::new());
    let engine = Engine::new(sink.clone(), registry, templates, EngineConfig::default());

    let err = engine
        .run("run_test", &pipeline, RunInputs::default())
        .await
        .expect_err("run must fail when a var references a missing env");

    match err {
        CoreError::Pipeline(crate::error::PipelineError::Template { name, .. }) => {
            assert_eq!(name, "vars.tag", "error must name the offending var");
        },
        other => panic!("expected PipelineError::Template, got {other:?}"),
    }
}
