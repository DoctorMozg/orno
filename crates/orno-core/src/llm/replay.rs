//! `ReplayTransport` — load a tape written by [`super::RecordingTransport`]
//! and return responses deterministically without touching the network.
//!
//! Keyed by `(provider, model, blake3(serialized request))`. A tape
//! miss returns [`LlmError::ReplayMiss`]; `ReplayTransport` never falls
//! through to a live call. That's what makes it usable as the
//! determinism guarantee in ADR 0005 dimension 5.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use async_trait::async_trait;

use crate::error::LlmError;

use super::recording::TapeEntry;
use super::{LlmRequest, LlmResponse, LlmTransport};

#[derive(Debug)]
pub struct ReplayTransport {
    entries: HashMap<String, LlmResponse>,
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
            entries.insert(tape_key(&entry.req), entry.res);
        }
        Ok(Self { entries })
    }

    /// In-memory constructor for tests — skips file I/O.
    #[must_use]
    pub fn from_entries(entries: Vec<TapeEntry>) -> Self {
        let map = entries
            .into_iter()
            .map(|e| (tape_key(&e.req), e.res))
            .collect();
        Self { entries: map }
    }
}

#[async_trait]
impl LlmTransport for ReplayTransport {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        let key = tape_key(&req);
        self.entries
            .get(&key)
            .cloned()
            .ok_or(LlmError::ReplayMiss { key })
    }
}

/// Tape key = `provider:model:blake3(json(request))`. The JSON
/// encoding is stable because `LlmRequest` has named fields and a
/// fixed order; blake3 collapses the prompt + system + sampling
/// knobs into a 64-char hex digest. Truncating the hash would
/// narrow the keyspace for no gain in readability, so the full
/// digest is kept.
fn tape_key(req: &LlmRequest) -> String {
    let bytes = serde_json::to_vec(req).expect("LlmRequest serializes");
    let hash = blake3::hash(&bytes);
    format!("{}:{}:{}", req.provider, req.model, hash.to_hex())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[tokio::test]
    async fn round_trip_dummy_to_replay_is_bit_identical() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let rec = RecordingTransport::create(Arc::new(DummyTransport), &path).unwrap();

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
        let entry = TapeEntry {
            req: req("known"),
            res: LlmResponse {
                content: "answer".into(),
                finish_reason: Some("stop".into()),
                usage: Some(Usage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                }),
                tool_calls: Vec::new(),
            },
        };
        let replay = ReplayTransport::from_entries(vec![entry]);
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
}
