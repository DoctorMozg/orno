//! End-to-end tests for the record/replay tape and for `examples/pr-review.yaml`
//! structural validation.
//!
//! These tests are designed to run without a live LLM API key. Agent runs use
//! `ORNO_TEST_LLM_TRANSPORT=dummy` to get deterministic responses; replay runs
//! use `--replay-tape` which replaces the transport entirely.

use std::io::Write;

use assert_cmd::Command;
use predicates::str::contains;
use serde_json::json;

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

/// Build a scripted `LlmResponse` whose sole content is a single tool call
/// — i.e. a turn where the model is dispatching to `subagent_name`.
fn subagent_call_response(call_id: &str, subagent_name: &str) -> serde_json::Value {
    json!({
        "content": "",
        "finish_reason": "tool_calls",
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
        "tool_calls": [{"call_id": call_id, "fn_name": subagent_name, "fn_arguments": {"prompt": "review"}}]
    })
}

/// Build a scripted `LlmResponse` whose content is a terminal text turn
/// (no tool calls) — used for both subagent results and the parent's
/// final synthesis.
fn text_response(content: &str) -> serde_json::Value {
    json!({
        "content": content,
        "finish_reason": "stop",
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
        "tool_calls": []
    })
}

/// Count NDJSON envelopes on `stdout` whose `event.type` equals `event_type`.
fn count_event_type(stdout: &str, event_type: &str) -> usize {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| {
                    v.get("event")?
                        .get("type")?
                        .as_str()
                        .map(|t| t == event_type)
                })
                .unwrap_or(false)
        })
        .count()
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

// ─── Three-subagent record / replay ─────────────────────────────────────────

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "a single record+replay scenario; splitting would share mutable tape state across tests"
)]
fn three_subagent_run_record_then_replay_preserves_event_sequence() {
    // The parent reviewer calls three subagent lenses in sequence; each
    // subagent makes one LLM call and returns a text result. The full
    // run requires 7 LLM responses total:
    //   1. parent → call subagent.security_lens
    //   2. security_lens → text result
    //   3. parent → call subagent.perf_lens
    //   4. perf_lens → text result
    //   5. parent → call subagent.docs_lens
    //   6. docs_lens → text result
    //   7. parent → final synthesis
    let scripted: Vec<serde_json::Value> = vec![
        subagent_call_response("call_1", "subagent.security_lens"),
        text_response("{\"findings\":[],\"verdict\":\"ok\"}"),
        subagent_call_response("call_2", "subagent.perf_lens"),
        text_response("{\"findings\":[],\"verdict\":\"ok\"}"),
        subagent_call_response("call_3", "subagent.docs_lens"),
        text_response("{\"findings\":[],\"verdict\":\"ok\"}"),
        text_response("{\"verdict\":\"approve\",\"findings\":[]}"),
    ];
    let scripted_json = serde_json::to_string(&scripted).expect("serialize scripted responses");

    let scripted_file = tempfile::NamedTempFile::new().expect("scripted tempfile");
    std::fs::write(scripted_file.path(), &scripted_json).expect("write scripted tape");

    let tape = tempfile::NamedTempFile::new().expect("tape tempfile");
    let tape_path = tape.path().to_str().expect("utf8 tape path");

    // Phase 1: record — run three-lens.yaml with ScriptedTransport and
    // capture the LLM tape.
    let record_assert = orno()
        .env("ORNO_TEST_LLM_TRANSPORT", "scripted")
        .env(
            "ORNO_TEST_SCRIPTED_TAPE",
            scripted_file.path().to_str().expect("utf8 scripted path"),
        )
        .args([
            "run",
            "--record-tape",
            tape_path,
            "tests/fixtures/three-lens.yaml",
        ])
        .assert()
        .success();
    let record_stdout =
        String::from_utf8(record_assert.get_output().stdout.clone()).expect("utf8 stdout");

    // All three subagent lenses must have fired.
    assert_eq!(
        count_event_type(&record_stdout, "subagent_started"),
        3,
        "expected 3 subagent_started events in record run:\n{record_stdout}",
    );
    assert_eq!(
        count_event_type(&record_stdout, "subagent_completed"),
        3,
        "expected 3 subagent_completed events in record run:\n{record_stdout}",
    );

    // Tape must be non-empty.
    let tape_bytes = std::fs::metadata(tape.path()).expect("tape metadata").len();
    assert!(
        tape_bytes > 0,
        "LLM tape must be non-empty after subagent run"
    );

    // Phase 2: replay — no ScriptedTransport; ReplayTransport takes over.
    let replay_assert = orno()
        .args([
            "run",
            "--replay-tape",
            tape_path,
            "tests/fixtures/three-lens.yaml",
        ])
        .assert()
        .success();
    let replay_stdout =
        String::from_utf8(replay_assert.get_output().stdout.clone()).expect("utf8 stdout");

    // Event sequence must be identical.
    let recorded_types = event_types(&record_stdout);
    let replayed_types = event_types(&replay_stdout);
    assert_eq!(
        recorded_types, replayed_types,
        "replay must emit the same event type sequence as the original run\n\
         recorded: {recorded_types:?}\n\
         replayed: {replayed_types:?}",
    );

    // All subagent events must appear in the replay too.
    assert_eq!(
        count_event_type(&replay_stdout, "subagent_started"),
        3,
        "replay must also emit 3 subagent_started events",
    );
    assert_eq!(
        count_event_type(&replay_stdout, "subagent_completed"),
        3,
        "replay must also emit 3 subagent_completed events",
    );
}
