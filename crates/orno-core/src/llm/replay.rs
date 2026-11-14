//! `ReplayTransport` — load a tape written by [`super::RecordingTransport`]
//! and return responses deterministically without touching the network.
//!
//! Keyed by `(key, seq)`: the n-th request that hashes to a key gets the
//! entry recorded with seq n. A tape miss returns
//! [`LlmError::ReplayMiss`]; `ReplayTransport` never falls through to a
//! live call. That's what makes it usable as the bounded-non-determinism
//! guarantee in the strictness contract.
//!
//! Both `Ok` and `Err` outcomes are replayed: a tape entry recorded
//! with `TapeOutcome::Err` is reconstructed back into the same
//! `LlmError` class via [`crate::events::LlmFailure::to_llm_error`]
//! using the originating request's provider/model fields, so a
//! transient API failure that drove a branching decision in the live
//! run replays as the same failure without touching the network.

use std::path::Path;

use async_trait::async_trait;
use tracing::instrument;

use crate::error::LlmError;
use crate::util::{TapeIndex, read_tape};

use super::recording::{TapeEntry, TapeOutcome, tape_key};
use super::{LlmRequest, LlmResponse, LlmTransport};

#[derive(Debug)]
pub struct ReplayTransport {
    index: TapeIndex<TapeOutcome>,
}

impl ReplayTransport {
    /// Load a tape file into memory. Corrupt entries are rejected
    /// with a `ConfigError` pointing at the offending line — half-
    /// readable tapes replay stale responses silently, which defeats
    /// the whole point of replay.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, LlmError> {
        let entries = read_tape::<TapeEntry>(path.as_ref())
            .map_err(|e| LlmError::ConfigError(format!("tape parse error: {e}")))?;
        Self::from_entries(entries)
    }

    /// Build a transport from entries already in memory (a bundle, or a
    /// test). Two entries claiming the same `(key, seq)` slot are a
    /// `ConfigError`.
    pub fn from_entries(entries: Vec<TapeEntry>) -> Result<Self, LlmError> {
        let index = TapeIndex::new(entries.into_iter().map(|e| (e.key, e.seq, e.outcome)))
            .map_err(|e| LlmError::ConfigError(e.to_string()))?;
        Ok(Self { index })
    }
}

#[async_trait]
impl LlmTransport for ReplayTransport {
    #[instrument(
        skip(self, req),
        fields(llm.provider = %req.provider, llm.model = %req.model),
    )]
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        let key = tape_key(&req, None);
        match self.index.take(&key) {
            Some(TapeOutcome::Ok { res }) => Ok(res),
            Some(TapeOutcome::Err { err }) => Err(err.to_llm_error(&req.provider, &req.model)),
            None => Err(LlmError::ReplayMiss { key }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{LlmFailure, Redactor};
    use crate::llm::{DummyTransport, OrnoChatMessage, RecordingTransport, Usage};
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    fn req(prompt: &str) -> LlmRequest {
        LlmRequest::from_prompt("openai", "gpt-5", prompt)
    }

    fn response(content: &str) -> LlmResponse {
        LlmResponse {
            content: content.into(),
            finish_reason: Some("stop".into()),
            usage: Some(Usage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
            }),
            tool_calls: Vec::new(),
        }
    }

    fn entry(prompt: &str, seq: u32, outcome: TapeOutcome) -> TapeEntry {
        TapeEntry {
            key: tape_key(&req(prompt), None),
            seq,
            tools: Vec::new(),
            outcome,
        }
    }

    fn ok_entry(prompt: &str, seq: u32, content: &str) -> TapeEntry {
        entry(
            prompt,
            seq,
            TapeOutcome::Ok {
                res: response(content),
            },
        )
    }

    fn fresh_tape() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("tape.ndjson");
        (dir, path)
    }

    #[tokio::test]
    async fn round_trip_dummy_to_replay_is_bit_identical() {
        let (_dir, path) = fresh_tape();
        let rec = RecordingTransport::create(
            Arc::new(DummyTransport),
            &path,
            Arc::new(Redactor::default()),
        )
        .unwrap();

        let live = [
            rec.complete(req("one")).await.unwrap(),
            rec.complete(req("two")).await.unwrap(),
            rec.complete(req("three")).await.unwrap(),
        ];
        rec.flush().unwrap();
        drop(rec);

        let replay = ReplayTransport::load(&path).unwrap();
        for (prompt, live) in ["one", "two", "three"].into_iter().zip(&live) {
            let replayed = replay.complete(req(prompt)).await.unwrap();
            assert_eq!(replayed.content, live.content);
            assert_eq!(replayed.finish_reason, live.finish_reason);
        }
    }

    /// Answers with a fixed string that does not echo the prompt, so the
    /// recorded and the replayed response can be compared exactly.
    struct FixedAnswer;

    #[async_trait]
    impl LlmTransport for FixedAnswer {
        async fn complete(&self, _req: LlmRequest) -> Result<LlmResponse, LlmError> {
            Ok(response("done"))
        }
    }

    #[tokio::test]
    async fn secret_in_prompt_replays_from_the_redacted_request() {
        // Replay never sees the secret: the prompt it rebuilds carries
        // the placeholder, and must still find the entry.
        let (_dir, path) = fresh_tape();
        let secrets = BTreeMap::from([("api".to_string(), "sk-secret-abc123".to_string())]);
        let rec = RecordingTransport::create(
            Arc::new(FixedAnswer),
            &path,
            Arc::new(Redactor::new(&secrets)),
        )
        .unwrap();
        let live = rec
            .complete(req("authenticate with sk-secret-abc123"))
            .await
            .unwrap();
        rec.flush().unwrap();
        drop(rec);

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("sk-secret-abc123"), "{on_disk}");

        let replay = ReplayTransport::load(&path).unwrap();
        let replayed = replay
            .complete(req("authenticate with ***"))
            .await
            .expect("redacted request must hit the recorded entry");
        assert_eq!(replayed.content, live.content);
        assert_eq!(
            replayed.usage.map(|u| u.total_tokens),
            live.usage.map(|u| u.total_tokens)
        );
    }

    #[tokio::test]
    async fn identical_requests_replay_in_call_order_despite_completion_order() {
        use std::time::Duration;

        /// First call is slow, second is fast, so the second completes
        /// (and is written to the tape) first.
        struct SlowFirst {
            calls: Mutex<u32>,
        }
        #[async_trait]
        impl LlmTransport for SlowFirst {
            async fn complete(&self, _req: LlmRequest) -> Result<LlmResponse, LlmError> {
                let n = {
                    let mut calls = self.calls.lock().unwrap();
                    *calls += 1;
                    *calls
                };
                if n == 1 {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    Ok(response("first"))
                } else {
                    Ok(response("second"))
                }
            }
        }

        let (_dir, path) = fresh_tape();
        let rec = RecordingTransport::create(
            Arc::new(SlowFirst {
                calls: Mutex::new(0),
            }),
            &path,
            Arc::new(Redactor::default()),
        )
        .unwrap();
        let (a, b) = tokio::join!(rec.complete(req("same")), rec.complete(req("same")));
        assert_eq!(a.unwrap().content, "first");
        assert_eq!(b.unwrap().content, "second");
        rec.flush().unwrap();
        drop(rec);

        let tape = std::fs::read_to_string(&path).unwrap();
        assert!(
            tape.find("\"second\"").unwrap() < tape.find("\"first\"").unwrap(),
            "precondition: tape is in completion order: {tape}",
        );

        let replay = ReplayTransport::load(&path).unwrap();
        assert_eq!(replay.complete(req("same")).await.unwrap().content, "first");
        assert_eq!(
            replay.complete(req("same")).await.unwrap().content,
            "second"
        );
    }

    #[tokio::test]
    async fn out_of_order_entries_are_served_by_seq() {
        let replay = ReplayTransport::from_entries(vec![
            ok_entry("p", 1, "second"),
            ok_entry("p", 0, "first"),
        ])
        .unwrap();

        assert_eq!(replay.complete(req("p")).await.unwrap().content, "first");
        assert_eq!(replay.complete(req("p")).await.unwrap().content, "second");
        assert!(matches!(
            replay.complete(req("p")).await,
            Err(LlmError::ReplayMiss { .. })
        ));
    }

    #[test]
    fn duplicate_key_and_seq_is_rejected() {
        let err = ReplayTransport::from_entries(vec![ok_entry("p", 0, "a"), ok_entry("p", 0, "b")])
            .expect_err("two entries in one slot");
        assert!(matches!(err, LlmError::ConfigError(msg) if msg.contains("duplicate")));
    }

    #[tokio::test]
    async fn miss_returns_replay_miss() {
        let replay = ReplayTransport::from_entries(vec![ok_entry("known", 0, "answer")]).unwrap();
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
            LlmError::ConfigError(msg) => assert!(msg.contains(":1:"), "{msg}"),
            other => panic!("expected ConfigError, got {other:?}"),
        }
    }

    #[test]
    fn different_message_history_produces_different_keys() {
        let base_req = req("hello");
        let mut with_history = base_req.clone();
        with_history.messages = Arc::new(vec![OrnoChatMessage::User {
            content: "previous turn".into(),
        }]);

        assert_ne!(
            tape_key(&base_req, None),
            tape_key(&with_history, None),
            "different message history must produce different tape keys",
        );
    }

    #[tokio::test]
    async fn err_outcome_replays_as_matching_llm_error() {
        // The `provider`/`model` fields come from the request being
        // replayed because the on-the-wire `LlmFailure` omits them.
        let replay = ReplayTransport::from_entries(vec![entry(
            "auth-fail",
            0,
            TapeOutcome::Err {
                err: LlmFailure::AuthFailed,
            },
        )])
        .unwrap();

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
        let replay = ReplayTransport::from_entries(vec![
            ok_entry("good", 0, "fine"),
            entry(
                "bad",
                0,
                TapeOutcome::Err {
                    err: LlmFailure::RateLimited,
                },
            ),
        ])
        .unwrap();

        assert_eq!(replay.complete(req("good")).await.unwrap().content, "fine");
        let err = replay
            .complete(req("bad"))
            .await
            .expect_err("rate-limited entry must surface as Err");
        assert!(matches!(err, LlmError::RateLimited { .. }));
    }

    #[tokio::test]
    async fn full_round_trip_records_and_replays_failed_call() {
        // A failing inner transport runs once through `RecordingTransport`;
        // replay surfaces the same failure class without invoking it again.
        struct ApiFailure {
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
        let (_dir, path) = fresh_tape();
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

        let calls_before_replay = *calls.lock().unwrap();
        let replay = ReplayTransport::load(&path).unwrap();
        match replay.complete(req("explode")).await.unwrap_err() {
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
