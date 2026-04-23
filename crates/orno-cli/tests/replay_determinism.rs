//! End-to-end tests for the record/replay tape and for `examples/pr-review.yaml`
//! structural validation.
//!
//! These tests are designed to run without a live LLM API key. Agent runs use
//! `ORNO_TEST_LLM_TRANSPORT=dummy` to get deterministic responses; replay runs
//! use `--replay-tape` which replaces the transport entirely.

use std::io::Write;

use assert_cmd::Command;
use predicates::str::contains;

fn orno() -> Command {
    Command::cargo_bin("orno").expect("orno binary should build")
}

fn orno_with_dummy_transport() -> Command {
    let mut cmd = orno();
    cmd.env("ORNO_TEST_LLM_TRANSPORT", "dummy");
    cmd
}

/// Extract the `type` field from every NDJSON line in `stdout` and return
/// them in order. Empty lines and lines without a `type` key are skipped.
fn event_types(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            v.get("event")?.get("type")?.as_str().map(str::to_string)
        })
        .collect()
}

/// Extract `"ok": <bool>` values from `node_finished` and `run_finished`
/// events, in order. Used to verify replay preserves pass/fail semantics.
fn outcome_sequence(stdout: &str) -> Vec<bool> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            let ev = v.get("event")?;
            let ty = ev.get("type")?.as_str()?;
            if ty == "node_finished" || ty == "run_finished" {
                ev.get("ok")?.as_bool()
            } else {
                None
            }
        })
        .collect()
}

// ─── pr-review.yaml validation ──────────────────────────────────────────────

#[test]
fn validate_pr_review_yaml_succeeds() {
    // Exercises: MCP server allowlist cross-check (WU-7) — each `mcp.*`
    // tool referenced in an agent must name a server declared in
    // `mcp_servers`. Subagent compose-down enforcement: a child agent
    // may not have broader `allow_mutations`/`allow_network` than its
    // parent. Both checks are in `pipeline::load::validate()`.
    orno()
        .args(["validate", "../../examples/pr-review.yaml"])
        .assert()
        .success()
        .stdout(contains("ok: version=1 nodes=2"));
}

// ─── Record / replay determinism ────────────────────────────────────────────

#[test]
fn record_then_replay_produces_identical_event_type_sequence() {
    let tape = tempfile::NamedTempFile::new().expect("tempfile for tape");
    let tape_path = tape.path().to_str().expect("utf8 tape path");

    // Phase 1: record — run hello.yaml with DummyTransport and capture the
    // LLM tape. `--record-tape` wraps DummyTransport in RecordingTransport
    // which writes one NDJSON entry per LLM call.
    let record_assert = orno_with_dummy_transport()
        .args([
            "run",
            "--record-tape",
            tape_path,
            "../../examples/hello.yaml",
        ])
        .assert()
        .success();
    let record_stdout =
        String::from_utf8(record_assert.get_output().stdout.clone()).expect("utf8 stdout");

    // Sanity: tape must be non-empty (i.e. at least one LLM call was made).
    let tape_bytes = std::fs::metadata(tape.path()).expect("tape metadata").len();
    assert!(
        tape_bytes > 0,
        "record tape should be non-empty after a run with an agent node",
    );

    // Phase 2: replay — replay the tape without DummyTransport. The
    // ReplayTransport replaces the live transport entirely.
    let replay_assert = orno()
        .args([
            "run",
            "--replay-tape",
            tape_path,
            "../../examples/hello.yaml",
        ])
        .assert()
        .success();
    let replay_stdout =
        String::from_utf8(replay_assert.get_output().stdout.clone()).expect("utf8 stdout");

    // The sequence of event `type` values must be identical across both
    // runs. `run_id`, `seq`, `timestamp` are legitimately different and
    // are not compared here.
    let recorded_types = event_types(&record_stdout);
    let replayed_types = event_types(&replay_stdout);

    assert_eq!(
        recorded_types, replayed_types,
        "replay must emit the same event type sequence as the original run\n\
         recorded: {recorded_types:?}\n\
         replayed: {replayed_types:?}",
    );

    // Both runs must agree on pass/fail for every node and the aggregate
    // run. This catches a replay that changes which nodes succeed.
    let recorded_outcomes = outcome_sequence(&record_stdout);
    let replayed_outcomes = outcome_sequence(&replay_stdout);

    assert_eq!(
        recorded_outcomes, replayed_outcomes,
        "replay must produce identical ok/fail outcomes\n\
         recorded: {recorded_outcomes:?}\n\
         replayed: {replayed_outcomes:?}",
    );

    // Both runs must complete successfully (hello.yaml has no failure path).
    assert!(
        recorded_outcomes.iter().all(|&ok| ok),
        "record run had unexpected failures: {recorded_outcomes:?}",
    );
    assert!(
        replayed_outcomes.iter().all(|&ok| ok),
        "replay run had unexpected failures: {replayed_outcomes:?}",
    );
}

#[test]
fn replay_tape_miss_is_reported_as_node_failure() {
    // An empty tape causes every LLM call to miss. ReplayTransport returns
    // LlmError::ReplayMiss, which the LoopAgent surfaces as a node failure.
    // The CLI process still exits 0 — node failures are pipeline-level
    // signals, not process-level ones.
    let empty_tape = tempfile::NamedTempFile::new().expect("tempfile for empty tape");
    let tape_path = empty_tape.path().to_str().expect("utf8 tape path");

    let assert = orno()
        .args([
            "run",
            "--replay-tape",
            tape_path,
            "../../examples/hello.yaml",
        ])
        .assert()
        .success(); // process still exits 0

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 stdout");

    // The agent node and the aggregate run must report failure.
    let failures = stdout.matches(r#""ok":false"#).count();
    assert!(
        failures >= 2,
        "expected at least 2 ok:false occurrences (node_finished + run_finished), \
         got {failures}: {stdout}",
    );

    // The tape-miss failure must be reflected via llm_request_failed or a
    // matching error string. Both surfacing forms are acceptable.
    let has_failure_signal = stdout.contains(r#""type":"llm_request_failed""#)
        || stdout.contains("replay")
        || stdout.contains("ReplayMiss");
    assert!(
        has_failure_signal,
        "expected a replay-miss signal in the event stream: {stdout}",
    );
}

#[test]
fn record_replay_shell_only_pipeline_produces_identical_events() {
    // Shell-only pipelines produce no LLM calls, so the tape is empty.
    // Validate that record + replay of a shell-only pipeline both succeed
    // and emit identical event type sequences without touching the LLM.
    let yaml = r"
version: 1
nodes:
  - id: greet
    kind: shell
    command: echo
    args: [hello]
";
    let mut file = tempfile::NamedTempFile::new().expect("pipeline tempfile");
    file.write_all(yaml.as_bytes()).expect("write pipeline");
    file.flush().expect("flush");
    let pipeline_path = file.path().to_str().expect("utf8 pipeline path");

    let tape = tempfile::NamedTempFile::new().expect("tape tempfile");
    let tape_path = tape.path().to_str().expect("utf8 tape path");

    let record_assert = orno()
        .args(["run", "--record-tape", tape_path, pipeline_path])
        .assert()
        .success();
    let record_stdout = String::from_utf8(record_assert.get_output().stdout.clone()).expect("utf8");

    let replay_assert = orno()
        .args(["run", "--replay-tape", tape_path, pipeline_path])
        .assert()
        .success();
    let replay_stdout = String::from_utf8(replay_assert.get_output().stdout.clone()).expect("utf8");

    assert_eq!(
        event_types(&record_stdout),
        event_types(&replay_stdout),
        "shell-only pipeline must produce identical event type sequences under record and replay",
    );
}
