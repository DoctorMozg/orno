//! Snapshot tests for `orno replay` against checked-in golden bundles.
//!
//! The committed bundle in `tests/bundles/` is an immutable input; the
//! `insta` snapshot captures the event stream the engine emits when
//! feeding that bundle back through `replay`. A diff signals one of
//! three drifts:
//!
//! 1. Bundle reader/writer changed how it (de)serializes recorded
//!    entries — a release-blocker, since old bundles must replay on
//!    new binaries.
//! 2. `EventEnvelope` shape changed — `RunFinished`, `NodeFinished`,
//!    or any other `Event` variant gained or lost a field. The NDJSON
//!    stream is a wire contract, so this is also user-visible.
//! 3. Engine ordering drift — the per-iteration event sequence the
//!    agent loop emits no longer matches the recording.
//!
//! All three deserve a review before the snapshot is regenerated. To
//! intentionally accept new output, run `cargo insta accept` (or
//! `cargo insta review` for an interactive diff).
//!
//! Bundles in `tests/bundles/` were recorded once, locally, with
//! `ORNO_TEST_LLM_TRANSPORT=dummy` so no live API key was burned.
//! The replay path itself does NOT need the `test-transport` feature:
//! the bundle carries the recorded LLM responses, so a default-feature
//! `cargo install` build replays the same bundles bit-for-bit.

use assert_cmd::Command;
use serde_json::Value;

fn orno() -> Command {
    Command::cargo_bin("orno").expect("orno binary should build")
}

/// Walk a parsed NDJSON envelope and overwrite the two
/// non-deterministic envelope fields in place:
/// - `run_id`: ULID minted per process (ADR 0019 form).
/// - `timestamp`: RFC 3339 UTC wall-clock (ADR 0018).
///
/// Values are replaced with stable string sentinels so the snapshot
/// remains structurally faithful (the keys still exist, the type is
/// still `String`) without forcing a regeneration on every run.
/// Doing the redaction in code (rather than via insta filters) keeps
/// the test self-contained — no extra insta cargo features required.
fn redact_envelope(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                match k.as_str() {
                    "run_id" => *child = Value::String("[run_id]".into()),
                    "timestamp" => *child = Value::String("[timestamp]".into()),
                    _ => redact_envelope(child),
                }
            }
        },
        Value::Array(items) => {
            for item in items {
                redact_envelope(item);
            }
        },
        _ => {},
    }
}

#[test]
fn hello_bundle_replays_to_stable_event_stream() {
    let assert = orno()
        .args(["replay", "tests/bundles/hello.ndjson"])
        .assert()
        .success();
    let stdout =
        String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 stdout from replay");

    // Parse each NDJSON line into a structured value so the snapshot
    // diff lives at the JSON-key level instead of the raw-string level
    // (a stray whitespace tweak in the writer would otherwise blow up
    // the snapshot for no real reason).
    let events: Vec<Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut v: Value =
                serde_json::from_str(line).expect("each NDJSON line must be valid JSON");
            redact_envelope(&mut v);
            v
        })
        .collect();

    insta::assert_yaml_snapshot!(events);
}
