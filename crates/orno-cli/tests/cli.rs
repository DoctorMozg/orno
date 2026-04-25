//! Smoke-level CLI integration tests.

use std::io::Write;

use assert_cmd::Command;
use predicates::str::contains;

fn orno() -> Command {
    Command::cargo_bin("orno").expect("orno binary should build")
}

/// Build an `orno` command with the test-only `DummyTransport` env var
/// set. Needed for any test that drives an `agent` node without a real
/// LLM API key — otherwise the run hits the live provider. The var
/// name mirrors `run.rs::TEST_TRANSPORT_ENV`.
fn orno_with_dummy_transport() -> Command {
    let mut cmd = orno();
    cmd.env("ORNO_TEST_LLM_TRANSPORT", "dummy");
    cmd
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
fn validates_scoped_state_example() {
    // ADR 0025: the scoped-state example exercises `SetState` +
    // `allow_context_writes: true` + a downstream shell node that
    // templates `nodes.<id>.state.<key>`. Validation is enough here —
    // the happy-path runtime behavior is covered by unit tests in
    // `orno-core`, and running the example end-to-end would require a
    // live LLM key (DummyTransport can't script tool calls).
    orno()
        .args(["validate", "../../examples/scoped-state.yaml"])
        .assert()
        .success()
        .stdout(contains("ok: version=1 nodes=2"));
}

#[test]
fn run_emits_lifecycle_events() {
    let assert = orno_with_dummy_transport()
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
    // Agent nodes must bracket their transport call with LLM events.
    assert!(
        stdout.contains(r#""type":"llm_request_started""#),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains(r#""type":"llm_response_received""#),
        "stdout: {stdout}"
    );
}

#[test]
fn schema_prints_valid_json() {
    let assert = orno().arg("schema").assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let _unused: serde_json::Value =
        serde_json::from_str(&stdout).expect("schema must be valid JSON");
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

#[test]
fn plan_hello_yaml_emits_plan_outputs() {
    // `orno plan` enumerates the DAG without executing — every node
    // surfaces a `plan_node` event and the run closes with a single
    // `plan_summary`. The example pipeline declares one agent node `greet`
    // (see `examples/hello.yaml`).
    let assert = orno()
        .args(["plan", "../../examples/hello.yaml"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains(r#""type":"plan_node""#),
        "expected at least one plan_node: {stdout}",
    );
    assert!(
        stdout.contains(r#""type":"plan_summary""#),
        "expected plan_summary: {stdout}",
    );
    assert!(
        stdout.contains(r#""greet""#),
        "expected agent node id `greet` from hello.yaml: {stdout}",
    );
}

#[test]
fn plan_cyclic_pipeline_exits_nonzero() {
    // Two-node cycle a→b→a must fail DAG validation. Kahn's algorithm
    // surfaces the unvisited remainder; the CLI should propagate that as
    // a non-zero exit.
    let yaml = r#"
version: 1
nodes:
  - id: a
    kind: shell
    command: "true"
    needs: [b]
  - id: b
    kind: shell
    command: "true"
    needs: [a]
"#;
    let file = write_pipeline(yaml);
    let assert = orno()
        .args(["plan", file.path().to_str().unwrap()])
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stdout.to_lowercase().contains("cycle") || stderr.to_lowercase().contains("cycle"),
        "expected cycle in stdout or stderr.\nstdout: {stdout}\nstderr: {stderr}",
    );
}

#[test]
fn plan_shell_only_pipeline_produces_summary() {
    // No agent nodes means no LLM transport is needed. The summary must
    // count both shell nodes.
    let yaml = r#"
version: 1
nodes:
  - id: first
    kind: shell
    command: "echo hello"
  - id: second
    kind: shell
    command: "echo world"
    needs: [first]
"#;
    let file = write_pipeline(yaml);
    let assert = orno()
        .args(["plan", file.path().to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains(r#""type":"plan_summary""#),
        "expected plan_summary: {stdout}",
    );
    assert!(
        stdout.contains(r#""total_nodes":2"#),
        "expected total_nodes:2 in plan_summary: {stdout}",
    );
}

#[test]
fn record_bundle_then_replay_exits_success() {
    // Round-trip: record a bundle from a live `run`, then feed that
    // bundle back through `replay` and assert the replayed run reaches
    // `run_finished`. The DummyTransport keeps the recorded LLM
    // interactions deterministic so replay matches the recording.
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let bundle_path = tmp.path().to_str().expect("utf8 bundle path");

    orno_with_dummy_transport()
        .args([
            "run",
            "../../examples/hello.yaml",
            "--record-bundle",
            bundle_path,
        ])
        .assert()
        .success();

    let assert = orno_with_dummy_transport()
        .args(["replay", bundle_path])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains(r#""type":"run_finished""#),
        "expected run_finished after replay: {stdout}",
    );
}

#[test]
fn replay_missing_bundle_exits_nonzero() {
    // A non-existent bundle path is a hard error — the CLI must exit
    // non-zero and surface a readable diagnostic mentioning either the
    // operation ("reading bundle") or the OS-level cause ("No such file").
    let assert = orno()
        .args(["replay", "/tmp/orno_test_nonexistent_bundle_xyz.ndjson"])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let combined = format!("{stderr}{stdout}");
    assert!(
        combined.contains("reading bundle") || combined.contains("No such file"),
        "expected `reading bundle` or `No such file`.\nstdout: {stdout}\nstderr: {stderr}",
    );
}

#[test]
fn validate_unknown_tool_exits_nonzero() {
    // Tool allowlist names a builtin that doesn't exist. `validate`
    // should fail and the diagnostic must name the offending tool so the
    // user can find the typo.
    let yaml = r#"
version: 1
agents:
  worker:
    model: gpt-4o
    provider: openrouter
    allowed_tools: ["UnknownMagicTool"]
    policy:
      max_iterations: 3
      max_total_tokens: 10000
      max_tool_calls: 5
      max_subagent_depth: 0
      allow_mutations: false
      allow_network: false
      on_parse_error: fail
nodes:
  - id: run
    kind: agent
    agent: worker
    initial_prompt: "hello"
"#;
    let file = write_pipeline(yaml);
    let assert = orno()
        .args(["validate", file.path().to_str().unwrap()])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let combined = format!("{stderr}{stdout}");
    assert!(
        combined.contains("UnknownMagicTool"),
        "expected diagnostic to name `UnknownMagicTool`.\nstdout: {stdout}\nstderr: {stderr}",
    );
}

#[test]
fn validate_undeclared_agent_reference_exits_nonzero() {
    // Node references an agent name that isn't in the top-level `agents`
    // map. Validation must reject the pipeline and name the missing key.
    let yaml = r#"
version: 1
nodes:
  - id: run
    kind: agent
    agent: ghost_agent
    initial_prompt: "hello"
"#;
    let file = write_pipeline(yaml);
    let assert = orno()
        .args(["validate", file.path().to_str().unwrap()])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let combined = format!("{stderr}{stdout}");
    assert!(
        combined.contains("ghost_agent"),
        "expected diagnostic to name `ghost_agent`.\nstdout: {stdout}\nstderr: {stderr}",
    );
}

#[test]
fn validate_subagent_reference_to_unknown_agent_exits_nonzero() {
    // `subagent.<name>` in `allowed_tools` must point at a real agent
    // declared in the `agents` map. A dangling reference fails validation
    // and the diagnostic must name the missing agent.
    let yaml = r#"
version: 1
agents:
  main:
    model: gpt-4o
    provider: openrouter
    allowed_tools: ["subagent.nonexistent"]
    policy:
      max_iterations: 3
      max_total_tokens: 10000
      max_tool_calls: 5
      max_subagent_depth: 1
      allow_mutations: false
      allow_network: false
      on_parse_error: fail
nodes:
  - id: run
    kind: agent
    agent: main
    initial_prompt: "hello"
"#;
    let file = write_pipeline(yaml);
    let assert = orno()
        .args(["validate", file.path().to_str().unwrap()])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let combined = format!("{stderr}{stdout}");
    assert!(
        combined.contains("nonexistent"),
        "expected diagnostic to name `nonexistent`.\nstdout: {stdout}\nstderr: {stderr}",
    );
}

#[test]
fn plan_shows_allow_context_writes() {
    // The `triager` agent in `examples/scoped-state.yaml` opts into
    // scoped-state writes (ADR 0025). `orno plan` must surface that
    // gate as a serialized field on the `plan_node` so a reviewer sees
    // the elevated effect surface without running the pipeline.
    let assert = orno()
        .args(["plan", "../../examples/scoped-state.yaml"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains(r#""allow_context_writes":true"#),
        "expected allow_context_writes:true for triager agent: {stdout}",
    );
}

#[test]
fn validate_effect_gate_mismatch_exits_nonzero() {
    // `SetState` carries `ToolEffect::ContextSelf` (ADR 0025) — listing
    // it without `allow_context_writes:true` is a silent runtime denial.
    // Validation promotes that into an explicit failure so the agent
    // never ships with a dead allowlist entry.
    let yaml = r#"
version: 1
agents:
  worker:
    model: gpt-4o
    provider: openrouter
    allowed_tools: ["SetState"]
    policy:
      max_iterations: 3
      max_total_tokens: 10000
      max_tool_calls: 5
      max_subagent_depth: 0
      allow_mutations: false
      allow_network: false
      allow_context_writes: false
      on_parse_error: fail
nodes:
  - id: run
    kind: agent
    agent: worker
    initial_prompt: "hello"
"#;
    let file = write_pipeline(yaml);
    let assert = orno()
        .args(["validate", file.path().to_str().unwrap()])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("SetState") && stderr.contains("allow_context_writes"),
        "expected diagnostic naming SetState + allow_context_writes: {stderr}",
    );
}

#[test]
fn validate_network_gate_mismatch_exits_nonzero() {
    // Symmetric to the SetState gate above: `WebFetch` requires
    // `allow_network:true`. A mismatch is an error, not a warning.
    let yaml = r#"
version: 1
agents:
  worker:
    model: gpt-4o
    provider: openrouter
    allowed_tools: ["WebFetch"]
    policy:
      max_iterations: 3
      max_total_tokens: 10000
      max_tool_calls: 5
      max_subagent_depth: 0
      allow_mutations: false
      allow_network: false
      on_parse_error: fail
nodes:
  - id: run
    kind: agent
    agent: worker
    initial_prompt: "hello"
"#;
    let file = write_pipeline(yaml);
    let assert = orno()
        .args(["validate", file.path().to_str().unwrap()])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("WebFetch") && stderr.contains("allow_network"),
        "expected diagnostic naming WebFetch + allow_network: {stderr}",
    );
}

#[test]
fn validate_domain_list_with_no_network_warns() {
    // `allowed_domains` with `allow_network:false` is dormant config —
    // never consulted, but not an error either. Validation must surface
    // it as a `warning:` line and still exit success.
    let yaml = r#"
version: 1
agents:
  worker:
    model: gpt-4o
    provider: openrouter
    allowed_tools: []
    policy:
      max_iterations: 3
      max_total_tokens: 10000
      max_tool_calls: 5
      max_subagent_depth: 0
      allow_mutations: false
      allow_network: false
      allowed_domains: ["example.com"]
      on_parse_error: fail
nodes:
  - id: run
    kind: agent
    agent: worker
    initial_prompt: "hello"
"#;
    let file = write_pipeline(yaml);
    let assert = orno()
        .args(["validate", file.path().to_str().unwrap()])
        .assert()
        .success();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("warning:") && stderr.contains("allow_network"),
        "expected `warning:` line referencing allow_network: {stderr}",
    );
}

#[test]
fn mcp_init_crash_emits_run_started_and_run_finished() {
    // When the MCP init step fails, the CLI owns the lifecycle pair —
    // `Engine::run` never reaches the DAG. Verifies the crash path
    // emits both `run_started` and `run_finished{ok:false}` around the
    // `mcp_server_crashed` envelope so a stream consumer sees a
    // well-bracketed run even when nothing executed (ADR 0007 / H2).
    let yaml = r#"
version: 1
mcp_servers:
  broken:
    transport: stdio
    command: ["/nonexistent-mcp-server-xyz-12345"]
nodes:
  - id: noop
    kind: shell
    command: "true"
"#;
    let file = write_pipeline(yaml);
    let assert = orno_with_dummy_transport()
        .args(["run", file.path().to_str().unwrap()])
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains(r#""type":"run_started""#),
        "expected run_started before MCP crash: {stdout}",
    );
    assert!(
        stdout.contains(r#""type":"mcp_server_crashed""#),
        "expected mcp_server_crashed: {stdout}",
    );
    assert!(
        stdout.contains(r#""type":"run_finished""#) && stdout.contains(r#""ok":false"#),
        "expected run_finished ok:false: {stdout}",
    );
}
