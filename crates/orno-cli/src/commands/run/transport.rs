//! LLM transport selection: live `GenAiTransport`, optional
//! `RecordingTransport` wrapper, `ReplayTransport` override, and the
//! test-only escape hatch that swaps in `DummyTransport` /
//! `ScriptedTransport` when the `test-transport` feature is active.

use std::collections::BTreeMap;
use std::sync::Arc;

#[cfg(any(test, feature = "test-transport"))]
use anyhow::anyhow;
use anyhow::{Context, Result};

use orno_core::events::Redactor;
#[cfg(any(test, feature = "test-transport"))]
use orno_core::llm::{DummyTransport, ScriptedTransport};
use orno_core::llm::{GenAiTransport, LlmTransport, RecordingTransport, ReplayTransport};
use orno_core::pipeline::Pipeline;

use super::{RunFlags, TransportPair};

/// Test-only escape hatch: when set to `dummy`, `orno run` swaps
/// `GenAiTransport` for `DummyTransport` so integration tests can
/// snapshot the event stream without a live API key. When set to
/// `scripted`, tests can drive multi-turn agent + subagent loops
/// by pre-seeding the response queue via `ORNO_TEST_SCRIPTED_TAPE`
/// — required for record/replay coverage of subagent dispatch
/// (Phase 6). Only honored when the `test-transport` feature is
/// enabled (or `cfg(test)` is live for in-crate unit tests); a
/// stock `cargo install` build silently ignores the variable.
#[cfg(any(test, feature = "test-transport"))]
const TEST_TRANSPORT_ENV: &str = "ORNO_TEST_LLM_TRANSPORT";

/// Resolve the live `LlmTransport` plus optional recording wrapper from
/// the resolved flags and pipeline. Honors the `ORNO_TEST_LLM_TRANSPORT`
/// escape hatch (`dummy` / `scripted`) only when the `test-transport`
/// feature is enabled (or in `cfg(test)` builds); production binaries
/// fall straight through to `GenAiTransport`. `--replay-tape` overrides
/// everything; `--record-tape` wraps the live transport so the post-run
/// flush can call `flush()` directly. `--replay-tape` and `--record-tape`
/// are mutually exclusive at the CLI layer, so the precedence here
/// cannot conflict in practice.
pub(super) fn build_transport(
    flags: &RunFlags,
    pipeline: &Pipeline,
    secrets: &BTreeMap<String, String>,
    redactor: &Arc<Redactor>,
) -> Result<TransportPair> {
    let base_transport: Arc<dyn LlmTransport> = match try_test_transport()? {
        Some(t) => t,
        None => Arc::new(
            GenAiTransport::from_agents(&pipeline.agents, secrets)
                .context("constructing LLM transport from pipeline agents")?,
        ),
    };

    if let Some(path) = &flags.replay_tape {
        let replay = ReplayTransport::load(path)
            .with_context(|| format!("loading replay tape `{}`", path.display()))?;
        Ok((Arc::new(replay), None))
    } else if let Some(path) = &flags.record_tape {
        let rec = Arc::new(
            RecordingTransport::create(base_transport, path, redactor.clone())
                .with_context(|| format!("creating record tape `{}`", path.display()))?,
        );
        Ok((rec.clone() as Arc<dyn LlmTransport>, Some(rec)))
    } else {
        Ok((base_transport, None))
    }
}

/// Resolve the test-only `ORNO_TEST_LLM_TRANSPORT` escape hatch when
/// the `test-transport` feature (or `cfg(test)`) gates it in. Returns
/// `Some(transport)` for `dummy` / `scripted`, `None` otherwise — the
/// caller falls back to the live `GenAiTransport`. Misconfiguration of
/// the `scripted` mode (missing `ORNO_TEST_SCRIPTED_TAPE`, malformed
/// tape) surfaces as a structured anyhow error rather than a panic so
/// the integration harness can assert on a clean message.
#[cfg(any(test, feature = "test-transport"))]
fn try_test_transport() -> Result<Option<Arc<dyn LlmTransport>>> {
    let Ok(val) = std::env::var(TEST_TRANSPORT_ENV) else {
        return Ok(None);
    };
    match val.as_str() {
        "dummy" => Ok(Some(Arc::new(DummyTransport))),
        "scripted" => {
            let tape_path = std::env::var("ORNO_TEST_SCRIPTED_TAPE").map_err(|_| {
                anyhow!("ORNO_TEST_SCRIPTED_TAPE must be set when ORNO_TEST_LLM_TRANSPORT=scripted")
            })?;
            let json = std::fs::read_to_string(&tape_path)
                .with_context(|| format!("reading scripted tape `{tape_path}`"))?;
            let responses: Vec<orno_core::llm::LlmResponse> = serde_json::from_str(&json)
                .with_context(|| {
                    format!("parsing scripted tape `{tape_path}` as Vec<LlmResponse>")
                })?;
            Ok(Some(Arc::new(ScriptedTransport::new(responses))))
        },
        _ => Ok(None),
    }
}

/// Production-build counterpart to the cfg-gated `try_test_transport`.
/// Always returns `None` so the live `GenAiTransport` is the only
/// transport path a stock `cargo install` build can reach.
#[cfg(not(any(test, feature = "test-transport")))]
fn try_test_transport() -> Result<Option<Arc<dyn LlmTransport>>> {
    Ok(None)
}
