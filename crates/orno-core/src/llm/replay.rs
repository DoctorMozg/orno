//! `ReplayTransport` — load a tape written by [`super::RecordingTransport`]
//! and return responses deterministically without touching the network.
//!
//! Keyed by `(provider, model, blake3(serialized request))`. A tape
//! miss returns [`LlmError::ReplayMiss`]; `ReplayTransport` never falls
//! through to a live call. That's what makes it usable as the
//! bounded-non-determinism guarantee in the strictness contract.
//!
//! Both `Ok` and `Err` outcomes are replayed: a tape entry recorded
//! with `TapeOutcome::Err` is reconstructed back into the same
//! `LlmError` class via [`crate::events::LlmFailure::to_llm_error`]
//! using the originating request's provider/model fields. This keeps
//! replay indistinguishable from the live call from the agent loop's
//! perspective — a transient API failure that drove a downstream
//! branching decision in the live run replays as the same failure
//! without ever touching the network.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use async_trait::async_trait;

use crate::error::LlmError;
use crate::util::canonical_json;

use super::recording::{TapeEntry, TapeOutcome};
use super::{LlmRequest, LlmResponse, LlmTransport};

#[derive(Debug)]
pub struct ReplayTransport {
    entries: HashMap<String, TapeOutcome>,
}

impl ReplayTransport {
    /// Load a tape file into memory. Corrupt entries are rejected
    /// with a `ConfigError` pointing at the offending line — half-
    /// readable tapes replay stale responses silently, which defeats
    /// the whole point of replay.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, LlmError> {
        let file = File::open(path.as_ref()).map_err(|e| {
            LlmError::ConfigError(format!(
                "could not open tape `{}`: {}",
                path.as_ref().display(),
                e,
            ))
        })?;
        let mut entries = HashMap::new();
        for (lineno, line) in BufReader::new(file).lines().enumerate() {
            let line = line.map_err(|e| {
                LlmError::ConfigError(format!("tape read error on line {}: {}", lineno + 1, e))
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: TapeEntry = serde_json::from_str(&line).map_err(|e| {
                LlmError::ConfigError(format!("tape parse error on line {}: {}", lineno + 1, e))
            })?;
            entries.insert(tape_key(&entry.req), entry.outcome);
        }
        Ok(Self { entries })
    }

    /// In-memory constructor for tests — skips file I/O.
    #[must_use]
    pub fn from_entries(entries: Vec<TapeEntry>) -> Self {
        let map = entries
            .into_iter()
            .map(|e| (tape_key(&e.req), e.outcome))
            .collect();
        Self { entries: map }
    }
}

#[async_trait]
impl LlmTransport for ReplayTransport {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        let key = tape_key(&req);
        match self.entries.get(&key) {
            Some(TapeOutcome::Ok { res }) => Ok(res.clone()),
            Some(TapeOutcome::Err { err }) => Err(err.to_llm_error(&req.provider, &req.model)),
            None => Err(LlmError::ReplayMiss { key }),
        }
    }
}

/// Tape key = `provider:model:blake3(canonical_json(request))`. The
/// canonical form recursively sorts every nested JSON object's keys so
/// a `serde_json::Value` field whose underlying producer (e.g. a tool
/// argument schema from `schemars`) shifts insertion order across
/// minor versions does not silently invalidate the tape. blake3
/// collapses the prompt + system + sampling knobs into a 64-char hex
/// digest. Truncating the hash would narrow the keyspace for no gain
/// in readability, so the full digest is kept.
fn tape_key(req: &LlmRequest) -> String {
    let value = serde_json::to_value(req).expect("LlmRequest serializes");
    let canonical = canonical_json(&value);
    let hash = blake3::hash(canonical.as_bytes());
    format!("{}:{}:{}", req.provider, req.model, hash.to_hex())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{LlmFailure, Redactor};
    use crate::llm::{DummyTransport, RecordingTransport, Usage};
    use std::sync::Arc;

    fn req(prompt: &str) -> LlmRequest {
        LlmRequest::from_prompt(
            "openai".into(),
            "gpt-5".into(),
            prompt.into(),
            None,
            None,
            None,
        )
    }

    fn ok_entry(prompt: &str, content: &str) -> TapeEntry {
        TapeEntry {
            req: req(prompt),
            outcome: TapeOutcome::Ok {
                res: LlmResponse {
                    content: content.into(),
                    finish_reason: Some("stop".into()),
                    usage: Some(Usage {
                        prompt_tokens: 1,
                        completion_tokens: 1,
                        total_tokens: 2,
                    }),
                    tool_calls: Vec::new(),
                },
            },
        }
    }

    #[tokio::test]
    async fn round_trip_dummy_to_replay_is_bit_identical() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let rec = RecordingTransport::create(
            Arc::new(DummyTransport),
            &path,
            Arc::new(Redactor::default()),
        )
        .unwrap();

        let r1 = rec.complete(req("one")).await.unwrap();
        let r2 = rec.complete(req("two")).await.unwrap();
        let r3 = rec.complete(req("three")).await.unwrap();
        rec.flush().unwrap();
        drop(rec);

        let replay = ReplayTransport::load(&path).unwrap();
        let p1 = replay.complete(req("one")).await.unwrap();
        let p2 = replay.complete(req("two")).await.unwrap();
        let p3 = replay.complete(req("three")).await.unwrap();

        assert_eq!(r1.content, p1.content);
        assert_eq!(r2.content, p2.content);
        assert_eq!(r3.content, p3.content);
        assert_eq!(r1.finish_reason, p1.finish_reason);
    }

    #[tokio::test]
    async fn miss_returns_replay_miss() {
        let replay = ReplayTransport::from_entries(vec![ok_entry("known", "answer")]);
        match replay.complete(req("unknown")).await {
            Err(LlmError::ReplayMiss { key }) => assert!(key.contains("openai:gpt-5")),
            other => panic!("expected ReplayMiss, got {other:?}"),
        }
    }

    #[test]
    fn corrupt_tape_line_is_rejected() {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{{\"not\": \"a tape entry\"}}").unwrap();
        f.flush().unwrap();
        let err = ReplayTransport::load(f.path()).expect_err("must reject corrupt entry");
        match err {
            LlmError::ConfigError(msg) => assert!(msg.contains("parse error")),
            other => panic!("expected ConfigError, got {other:?}"),
        }
    }

    #[test]
    fn different_message_history_produces_different_keys() {
        use crate::llm::OrnoChatMessage;

        let base_req = LlmRequest::from_prompt(
            "openai".into(),
            "gpt-5".into(),
            "hello".into(),
            None,
            None,
            None,
        );

        let mut with_history = base_req.clone();
        with_history.messages = vec![OrnoChatMessage::User {
            content: "previous turn".into(),
        }];

        assert_ne!(
            tape_key(&base_req),
            tape_key(&with_history),
            "different message history must produce different tape keys",
        );
    }

    #[tokio::test]
    async fn err_outcome_replays_as_matching_llm_error() {
        // A failure recorded via `TapeOutcome::Err` must surface to the
        // agent loop as the same `LlmError` class the live run produced.
        // The `provider`/`model` fields come from the request being
        // replayed because the on-the-wire `LlmFailure` form does not
        // carry them.
        let entry = TapeEntry {
            req: req("auth-fail"),
            outcome: TapeOutcome::Err {
                err: LlmFailure::AuthFailed,
            },
        };
        let replay = ReplayTransport::from_entries(vec![entry]);

        let err = replay
            .complete(req("auth-fail"))
            .await
            .expect_err("Err outcome must replay as Err");
        match err {
            LlmError::AuthFailed { provider } => assert_eq!(provider, "openai"),
            other => panic!("expected AuthFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mixed_ok_and_err_entries_route_to_correct_branch() {
        // Determinism check: two requests with identical provider/model
        // but different prompts hit different cache slots; an Ok entry
        // for one must not fulfill an Err entry for the other.
        let replay = ReplayTransport::from_entries(vec![
            ok_entry("good", "fine"),
            TapeEntry {
                req: req("bad"),
                outcome: TapeOutcome::Err {
                    err: LlmFailure::RateLimited,
                },
            },
        ]);

        let ok = replay.complete(req("good")).await.unwrap();
        assert_eq!(ok.content, "fine");

        let err = replay
            .complete(req("bad"))
            .await
            .expect_err("rate-limited entry must surface as Err");
        assert!(matches!(err, LlmError::RateLimited { .. }));
    }

    #[tokio::test]
    async fn full_round_trip_records_and_replays_failed_call() {
        // End-to-end: a failing inner transport runs once through
        // `RecordingTransport`, the tape is loaded by `ReplayTransport`,
        // and the second call surfaces the same failure class without
        // ever invoking the inner transport. Catches regressions in
        // both the recording and replay paths simultaneously.
        use async_trait::async_trait;
        use std::sync::Mutex;

        struct ApiFailure {
            // Counter to verify the inner transport runs exactly once.
            calls: Arc<Mutex<usize>>,
        }
        #[async_trait]
        impl LlmTransport for ApiFailure {
            async fn complete(&self, _req: LlmRequest) -> Result<LlmResponse, LlmError> {
                *self.calls.lock().unwrap() += 1;
                Err(LlmError::ApiError {
                    provider: "openai".into(),
                    status: 503,
                    body: "service unavailable".into(),
                })
            }
        }

        let calls = Arc::new(Mutex::new(0_usize));
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let rec = RecordingTransport::create(
            Arc::new(ApiFailure {
                calls: Arc::clone(&calls),
            }),
            &path,
            Arc::new(Redactor::default()),
        )
        .unwrap();

        let live = rec
            .complete(req("explode"))
            .await
            .expect_err("inner transport returns Err");
        assert!(matches!(live, LlmError::ApiError { status: 503, .. }));
        rec.flush().unwrap();
        drop(rec);

        // Replay must reproduce the same failure class without
        // calling the inner transport again.
        let calls_before_replay = *calls.lock().unwrap();
        let replay = ReplayTransport::load(&path).unwrap();
        let replayed = replay
            .complete(req("explode"))
            .await
            .expect_err("replay must surface recorded failure");
        match replayed {
            LlmError::ApiError {
                provider,
                status,
                body,
            } => {
                assert_eq!(provider, "openai");
                assert_eq!(status, 503);
                assert_eq!(body, "service unavailable");
            },
            other => panic!("expected ApiError on replay, got {other:?}"),
        }
        assert_eq!(
            *calls.lock().unwrap(),
            calls_before_replay,
            "replay must not invoke the inner transport",
        );
    }
}
