//! Smoke-level CLI integration tests.

use assert_cmd::Command;
use predicates::str::contains;

fn orno() -> Command {
    Command::cargo_bin("orno").expect("orno binary should build")
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
