//! Smoke-level CLI integration tests.

use std::io::Write;

use assert_cmd::Command;
use predicates::str::contains;

fn orno() -> Command {
    Command::cargo_bin("orno").expect("orno binary should build")
}

fn write_pipeline(yaml: &str) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().expect("tempfile");
    file.write_all(yaml.as_bytes()).expect("write");
    file.flush().expect("flush");
    file
}

#[test]
fn prints_version() {
    orno().arg("--version").assert().success();
}

#[test]
fn validates_example_pipeline() {
    orno()
        .args(["validate", "../../examples/hello.yaml"])
        .assert()
        .success()
        .stdout(contains("ok: version=1 nodes=1"));
}

#[test]
fn run_emits_lifecycle_events() {
    let assert = orno()
        .args(["run", "../../examples/hello.yaml"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    // Lifecycle events are NDJSON; we look for the literal type discriminants.
    assert!(
        stdout.contains(r#""type":"run_started""#),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains(r#""type":"node_started""#),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains(r#""type":"run_finished""#),
        "stdout: {stdout}"
    );
}

#[test]
fn schema_prints_valid_json() {
    let assert = orno().arg("schema").assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let _: serde_json::Value = serde_json::from_str(&stdout).expect("schema must be valid JSON");
}

#[test]
fn run_shell_node_executes_command() {
    let yaml = r"
version: 1
nodes:
  - id: greet
    kind: shell
    command: echo
    args: [hi]
";
    let file = write_pipeline(yaml);
    let assert = orno()
        .args(["run", file.path().to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains(r#""type":"node_finished""#) && stdout.contains(r#""node_id":"greet""#),
        "expected node_finished for greet: {stdout}",
    );
    // Two ok:true events (node_finished + run_finished) prove the child
    // spawned and exited cleanly and the aggregate run agrees. A single
    // occurrence would be ambiguous (schema_version:1 also contains ":1",
    // future schema fields may ship stray "ok":true literals).
    let successes = stdout.matches(r#""ok":true"#).count();
    assert!(
        successes >= 2,
        "expected at least 2 ok:true occurrences (node + run), got {successes}: {stdout}",
    );
}

#[cfg(unix)]
#[test]
fn run_shell_node_nonzero_exit_reports_failure() {
    let yaml = r#"
version: 1
nodes:
  - id: fail_node
    kind: shell
    command: "false"
"#;
    let file = write_pipeline(yaml);
    // The CLI process itself exits 0 even when the pipeline reports
    // failure — pipeline `ok: false` is a stream-level signal, not a
    // process-level one.
    let assert = orno()
        .args(["run", file.path().to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains(r#""ok":false"#),
        "expected ok:false: {stdout}",
    );
    // Both the failing node_finished and the aggregate run_finished
    // should carry ok:false.
    let failures = stdout.matches(r#""ok":false"#).count();
    assert!(
        failures >= 2,
        "expected at least 2 ok:false occurrences (node + run), got {failures}: {stdout}",
    );
}

#[cfg(unix)]
#[test]
fn run_two_node_pipeline_failure_propagates_skip() {
    let yaml = r#"
version: 1
nodes:
  - id: fail_node
    kind: shell
    command: "false"
  - id: downstream
    kind: shell
    command: echo
    args: [should-not-run]
    needs: [fail_node]
"#;
    let file = write_pipeline(yaml);
    let assert = orno()
        .args(["run", file.path().to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(
        stdout.contains(r#""node_id":"fail_node""#),
        "fail_node must appear in stream: {stdout}",
    );
    assert!(
        stdout.contains(r#""type":"node_skipped""#),
        "expected node_skipped event: {stdout}",
    );
    assert!(
        stdout.contains(r#""node_id":"downstream""#),
        "downstream must appear: {stdout}",
    );
    assert!(
        stdout.contains(r#""kind":"dependency_failed""#)
            && stdout.contains(r#""upstream":"fail_node""#),
        "expected SkipReason::DependencyFailed{{upstream:\"fail_node\"}}: {stdout}",
    );

    // Negative assertions: downstream was skipped, not executed.
    let lines: Vec<&str> = stdout.lines().collect();
    let downstream_started = lines
        .iter()
        .any(|l| l.contains(r#""type":"node_started""#) && l.contains(r#""node_id":"downstream""#));
    let downstream_finished = lines.iter().any(|l| {
        l.contains(r#""type":"node_finished""#) && l.contains(r#""node_id":"downstream""#)
    });
    assert!(
        !downstream_started,
        "downstream must not have node_started: {stdout}",
    );
    assert!(
        !downstream_finished,
        "downstream must not have node_finished: {stdout}",
    );
    // echo never ran, so its argv entry cannot have reached stdout.
    assert!(
        !stdout.contains("should-not-run"),
        "echo should not have run: {stdout}",
    );
}

#[test]
fn run_template_syntax_error_fails_node_and_run() {
    // A malformed template expression must not panic or silently render
    // — `dispatch_node` surfaces `PipelineError::Template` as a node
    // failure. Verifies the render-error branch of the engine's
    // dispatch logic end-to-end.
    let yaml = r#"
version: 1
nodes:
  - id: bad_template
    kind: shell
    command: "{{ invalid syntax ]}"
"#;
    let file = write_pipeline(yaml);
    let assert = orno()
        .args(["run", file.path().to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains(r#""type":"node_finished""#)
            && stdout.contains(r#""node_id":"bad_template""#),
        "expected node_finished for bad_template: {stdout}",
    );
    // Template render failure collapses to ok:false on the node and the
    // aggregate run.
    let failures = stdout.matches(r#""ok":false"#).count();
    assert!(
        failures >= 2,
        "expected at least 2 ok:false occurrences (node + run), got {failures}: {stdout}",
    );
}

#[test]
fn run_template_renders_vars_into_shell_args() {
    // End-to-end templating: both `command` and an `args` entry are
    // `{{ vars.* }}` expressions. If templating failed silently (e.g.
    // vars never reached the template context), `command` would render
    // as the empty string, `Command::spawn` would return ENOENT, and the
    // node would report `ok: false`. Successful `ok: true` therefore
    // proves the rendered `echo` and `world` reached the child process.
    //
    // The shell-captured stdout ("hello world") is not re-emitted into
    // the lifecycle event stream today, so we cannot assert on it as a
    // substring of the NDJSON output.
    let yaml = r#"
version: 1
vars:
  cmd: echo
  arg: world
nodes:
  - id: greet
    kind: shell
    command: "{{ vars.cmd }}"
    args: [hello, "{{ vars.arg }}"]
"#;
    let file = write_pipeline(yaml);
    let assert = orno()
        .args(["run", file.path().to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains(r#""type":"node_finished""#) && stdout.contains(r#""node_id":"greet""#),
        "expected node_finished for greet: {stdout}",
    );
    // Two ok:true events (node_finished + run_finished) prove both the
    // render and the spawn succeeded.
    let successes = stdout.matches(r#""ok":true"#).count();
    assert!(
        successes >= 2,
        "expected at least 2 ok:true occurrences (node + run), got {successes}: {stdout}",
    );
}
