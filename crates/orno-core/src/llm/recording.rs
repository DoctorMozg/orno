//! `RecordingTransport` — decorator that writes one tape entry per call
//! to an NDJSON tape while passing the call through to an inner
//! transport. Pairs with [`super::ReplayTransport`] for the
//! bounded-non-determinism guarantee in the strictness contract.
//!
//! Tape format: one JSON object per line with `{ "key", "seq", "outcome":
//! { "kind": "ok"|"err", … } }`. Both successful responses and typed
//! failures are recorded so a replayed run reproduces both branches of
//! the original control flow. The request body is not stored: replay
//! only needs the key to find the response, and a full message history
//! per entry would make tapes grow quadratically.
//!
//! Portable DTOs only — no `genai::*` types touch the tape, so tapes
//! remain valid across library version bumps.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::instrument;

use crate::error::LlmError;
use crate::events::{LlmFailure, Redactor};
use crate::util::{SeqCounter, TapeFile, canonical_json};

use super::{LlmRequest, LlmResponse, LlmTransport, OrnoChatTool};

/// Cap on the body excerpt captured into `LlmFailure::ApiError` when a
/// failure is recorded onto the tape. Mirrors the engine-level
/// `max_output_bytes` window applied to shell stderr tails so a
/// provider that returns megabytes of debug HTML cannot flood a tape
/// file. Hard-coded here — the recording transport is not parameterized
/// over `EngineConfig` and the strictness contract requires a bounded
/// excerpt regardless of caller configuration.
const RECORDED_API_BODY_EXCERPT_BYTES: usize = 4096;

pub struct RecordingTransport {
    inner: Arc<dyn LlmTransport>,
    tape: TapeFile,
    seq: SeqCounter,
    recorded_tools: Mutex<HashSet<String>>,
    redactor: Arc<Redactor>,
}

/// Single tape entry. Kept public so `ReplayTransport` and external
/// tools (e.g. a future `orno tape inspect`) can deserialize without
/// re-implementing the shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TapeEntry {
    /// `provider:model:blake3(canonical_json(redacted request))`. The
    /// request is redacted before hashing because replay rebuilds it
    /// from redacted tapes and has no secrets to substitute back.
    pub key: String,
    /// Index of this call among the calls that share `key`, assigned
    /// when the request started so completion order cannot reorder
    /// identical requests on replay.
    pub seq: u32,
    /// Tool definitions that appeared in the request for the first time
    /// on this tape. `orno replay` rebuilds MCP stubs from them so the
    /// replayed request hashes to the same key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<OrnoChatTool>,
    /// Whether the call succeeded or failed, plus the recorded payload
    /// for that branch. `outcome.kind: "ok"` carries the response;
    /// `outcome.kind: "err"` carries a typed `LlmFailure`.
    pub outcome: TapeOutcome,
}

/// Recorded outcome of a single LLM call. Internally tagged on
/// `"kind"`, matching the convention used elsewhere in the wire
/// format. New variants land via `#[non_exhaustive]` so an older
/// reader treats unknown kinds as a parse error rather than silently
/// substituting one for another.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TapeOutcome {
    /// The inner transport returned a successful response.
    Ok {
        /// Recorded response, replayed verbatim by `ReplayTransport`.
        res: LlmResponse,
    },
    /// The inner transport returned an error. The recorded form is
    /// the same `LlmFailure` classification used by
    /// `Event::LlmRequestFailed`; replay reconstructs an `LlmError`
    /// via [`LlmFailure::to_llm_error`] using the originating
    /// request's provider/model fields.
    Err {
        /// Recorded failure classification.
        err: LlmFailure,
    },
}

/// Tape key for `req`. `redactor` is `None` on replay, where the
/// request already carries the redaction placeholder.
///
/// Canonical JSON sorts every nested object's keys so a producer whose
/// insertion order shifts (e.g. `schemars` tool schemas) does not
/// invalidate tapes. The full 64-char digest is kept; truncating it
/// would narrow the keyspace for no gain.
pub(crate) fn tape_key(req: &LlmRequest, redactor: Option<&Redactor>) -> String {
    let mut value = serde_json::to_value(req).expect("LlmRequest serializes");
    if let Some(redactor) = redactor {
        redactor.redact_json(&mut value);
    }
    let hash = blake3::hash(canonical_json(&value).as_bytes());
    format!("{}:{}:{}", req.provider, req.model, hash.to_hex())
}

impl RecordingTransport {
    /// Create the tape file at `path` and wrap `inner`. Parent
    /// directory must exist; the path itself must not exist —
    /// `O_EXCL` forces an explicit caller decision about reusing a
    /// stale tape.
    ///
    /// `redactor` is applied to every recorded outcome and to the
    /// request before it is keyed. Pass `Arc::new(Redactor::default())`
    /// when recording in a context that has no secrets (e.g. tests).
    pub fn create(
        inner: Arc<dyn LlmTransport>,
        path: impl Into<std::path::PathBuf>,
        redactor: Arc<Redactor>,
    ) -> Result<Self, std::io::Error> {
        Ok(Self {
            inner,
            tape: TapeFile::create(path)?,
            seq: SeqCounter::default(),
            recorded_tools: Mutex::new(HashSet::new()),
            redactor,
        })
    }

    /// Path the tape is being written to. Primarily useful for tests
    /// that need to round-trip through `ReplayTransport::load`.
    #[must_use]
    pub fn tape_path(&self) -> &Path {
        self.tape.path()
    }

    /// Flush buffered bytes to disk. Call before dropping when the
    /// tape must survive a crash mid-run.
    pub fn flush(&self) -> Result<(), std::io::Error> {
        self.tape.flush()
    }

    /// Tool definitions in `req` that no earlier entry carried.
    fn unseen_tools(&self, req: &LlmRequest) -> Vec<OrnoChatTool> {
        let mut seen = self
            .recorded_tools
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        req.tools
            .iter()
            .filter(|tool| seen.insert(tool.name.clone()))
            .cloned()
            .collect()
    }

    /// Redact the outcome and append the entry. A tape write error
    /// surfaces as `LlmError::Transport` rather than hiding behind the
    /// original call's success path — a tape write error that cannot be
    /// observed defeats the strictness guarantee.
    fn write_entry(&self, entry: &TapeEntry) -> Result<(), LlmError> {
        let mut line =
            serde_json::to_value(entry).map_err(|e| LlmError::ParseError(e.to_string()))?;
        if let Some(outcome) = line.get_mut("outcome") {
            self.redactor.redact_json(outcome);
        }
        self.tape
            .append(&line)
            .map_err(|e| LlmError::Transport(Box::new(e)))
    }
}

#[async_trait]
impl LlmTransport for RecordingTransport {
    #[instrument(
        skip(self, req),
        fields(llm.provider = %req.provider, llm.model = %req.model),
    )]
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        // The sequence number is taken before the call starts so it
        // reflects call order, not completion order.
        let key = tape_key(&req, Some(&self.redactor));
        let seq = self.seq.next(&key);
        let tools = self.unseen_tools(&req);

        // Capture the inner result instead of `?`-ing it so a failed
        // call still lands on the tape. The original failure is
        // re-raised to the caller after the entry has been written.
        match self.inner.complete(req).await {
            Ok(res) => {
                let entry = TapeEntry {
                    key,
                    seq,
                    tools,
                    outcome: TapeOutcome::Ok { res: res.clone() },
                };
                self.write_entry(&entry)?;
                Ok(res)
            },
            Err(err) => {
                let failure = LlmFailure::from_llm_error(
                    &err,
                    &self.redactor,
                    RECORDED_API_BODY_EXCERPT_BYTES,
                );
                let entry = TapeEntry {
                    key,
                    seq,
                    tools,
                    outcome: TapeOutcome::Err { err: failure },
                };
                // If the entry cannot be persisted (disk full, file
                // vanished), surface the tape error rather than the
                // original — the missing tape line is the more
                // dangerous failure for replay. Log the original first
                // so it is not lost.
                if let Err(tape_err) = self.write_entry(&entry) {
                    tracing::warn!(
                        error = ?err,
                        "original LLM error masked by tape-write failure",
                    );
                    return Err(tape_err);
                }
                Err(err)
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::DummyTransport;
    use std::io::BufRead;

    #[tokio::test]
    async fn writes_ndjson_entry_per_call() {
        // `RecordingTransport::create` opens with `O_EXCL`, so the tape
        // path must not exist before the call. Use a fresh subdirectory
        // and pick an unused filename inside it.
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let path = tmp_dir.path().join("tape.ndjson");
        let rec = RecordingTransport::create(
            Arc::new(DummyTransport),
            &path,
            Arc::new(Redactor::default()),
        )
        .unwrap();

        rec.complete(LlmRequest::from_prompt(
            "openai".into(),
            "gpt-5".into(),
            "hello".into(),
            None,
            None,
            None,
        ))
        .await
        .unwrap();
        rec.complete(LlmRequest::from_prompt(
            "openai".into(),
            "gpt-5".into(),
            "world".into(),
            None,
            None,
            None,
        ))
        .await
        .unwrap();

        rec.flush().unwrap();
        drop(rec);

        let f = std::fs::File::open(&path).unwrap();
        let lines: Vec<String> = std::io::BufReader::new(f)
            .lines()
            .map(Result::unwrap)
            .collect();
        assert_eq!(lines.len(), 2, "one line per call");
        for line in &lines {
            let entry: TapeEntry = serde_json::from_str(line).unwrap();
            assert!(entry.key.starts_with("openai:gpt-5:"), "{}", entry.key);
            match entry.outcome {
                TapeOutcome::Ok { res } => assert!(!res.content.is_empty()),
                TapeOutcome::Err { err } => panic!("dummy transport must not fail: {err:?}"),
            }
        }
    }

    #[tokio::test]
    async fn secret_not_written_to_tape() {
        use std::collections::BTreeMap;

        // `RecordingTransport::create` opens with `O_EXCL`, so the tape
        // path must not exist before the call. Use a fresh subdirectory
        // and pick an unused filename inside it.
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let path = tmp_dir.path().join("tape.ndjson");
        let mut secrets = BTreeMap::new();
        secrets.insert("api_key".to_string(), "sk-secret-abc123".to_string());
        let rec = RecordingTransport::create(
            Arc::new(DummyTransport),
            &path,
            Arc::new(Redactor::new(&secrets)),
        )
        .unwrap();

        rec.complete(LlmRequest::from_prompt(
            "openai".into(),
            "gpt-5".into(),
            "my key is sk-secret-abc123".into(),
            None,
            None,
            None,
        ))
        .await
        .unwrap();
        rec.flush().unwrap();
        drop(rec);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            !contents.contains("sk-secret-abc123"),
            "secret must not appear in tape",
        );
        assert!(contents.contains("***"), "redacted placeholder must appear");
    }

    /// In-memory transport that always returns the configured `LlmError`,
    /// used to exercise the failure-path tape recording.
    struct FailingTransport {
        kind: ErrorKind,
    }

    enum ErrorKind {
        AuthFailed,
        RateLimited,
        Api { status: u16, body: String },
    }

    #[async_trait]
    impl LlmTransport for FailingTransport {
        async fn complete(&self, _req: LlmRequest) -> Result<LlmResponse, LlmError> {
            Err(match &self.kind {
                ErrorKind::AuthFailed => LlmError::AuthFailed {
                    provider: "openai".into(),
                },
                ErrorKind::RateLimited => LlmError::RateLimited {
                    provider: "openai".into(),
                },
                ErrorKind::Api { status, body } => LlmError::ApiError {
                    provider: "openai".into(),
                    status: *status,
                    body: body.clone(),
                },
            })
        }
    }

    #[tokio::test]
    async fn failure_is_recorded_to_tape_and_re_raised() {
        // Strictness guarantee: a failed call lands on the tape AND
        // surfaces to the caller. Both paths must hold — hiding the
        // failure from the tape breaks replay determinism; hiding it
        // from the caller breaks the agent loop's error handling.
        // `RecordingTransport::create` opens with `O_EXCL`, so the tape
        // path must not exist before the call. Use a fresh subdirectory
        // and pick an unused filename inside it.
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let path = tmp_dir.path().join("tape.ndjson");
        let rec = RecordingTransport::create(
            Arc::new(FailingTransport {
                kind: ErrorKind::AuthFailed,
            }),
            &path,
            Arc::new(Redactor::default()),
        )
        .unwrap();

        let err = rec
            .complete(LlmRequest::from_prompt(
                "openai".into(),
                "gpt-5".into(),
                "hi".into(),
                None,
                None,
                None,
            ))
            .await
            .expect_err("transport returned Err — caller must see it");
        assert!(
            matches!(err, LlmError::AuthFailed { .. }),
            "original error class lost on re-raise: {err:?}",
        );

        rec.flush().unwrap();
        drop(rec);

        let contents = std::fs::read_to_string(&path).unwrap();
        let entry: TapeEntry = serde_json::from_str(contents.trim()).unwrap();
        match entry.outcome {
            TapeOutcome::Err {
                err: LlmFailure::AuthFailed,
            } => {},
            other => panic!("expected recorded auth_failed entry, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn api_error_body_is_truncated_when_recorded() {
        // A provider that returns megabytes of debug HTML must not
        // flood the tape file. The recorded excerpt is bounded by
        // `RECORDED_API_BODY_EXCERPT_BYTES`.
        // `RecordingTransport::create` opens with `O_EXCL`, so the tape
        // path must not exist before the call. Use a fresh subdirectory
        // and pick an unused filename inside it.
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let path = tmp_dir.path().join("tape.ndjson");
        let huge = "x".repeat(10 * RECORDED_API_BODY_EXCERPT_BYTES);
        let rec = RecordingTransport::create(
            Arc::new(FailingTransport {
                kind: ErrorKind::Api {
                    status: 502,
                    body: huge,
                },
            }),
            &path,
            Arc::new(Redactor::default()),
        )
        .unwrap();

        drop(
            rec.complete(LlmRequest::from_prompt(
                "openai".into(),
                "gpt-5".into(),
                "hi".into(),
                None,
                None,
                None,
            ))
            .await,
        );
        rec.flush().unwrap();
        drop(rec);

        let contents = std::fs::read_to_string(&path).unwrap();
        let entry: TapeEntry = serde_json::from_str(contents.trim()).unwrap();
        match entry.outcome {
            TapeOutcome::Err {
                err:
                    LlmFailure::ApiError {
                        status,
                        body_excerpt,
                    },
            } => {
                assert_eq!(status, 502);
                // Truncated body has the ellipsis marker appended
                // when the original exceeded the cap.
                assert!(
                    body_excerpt.ends_with('…'),
                    "missing truncation marker: {body_excerpt}",
                );
                assert!(
                    body_excerpt.len() <= RECORDED_API_BODY_EXCERPT_BYTES + '…'.len_utf8(),
                    "excerpt larger than cap+marker: {} bytes",
                    body_excerpt.len(),
                );
            },
            other => panic!("expected ApiError variant, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rate_limited_failure_round_trips_through_tape() {
        // Sanity check that the tape entry's serialized form is a
        // valid `TapeOutcome::Err` discriminator — a subtle wire-
        // format regression would surface here as a deserialization
        // error rather than a typed match failure.
        // `RecordingTransport::create` opens with `O_EXCL`, so the tape
        // path must not exist before the call. Use a fresh subdirectory
        // and pick an unused filename inside it.
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let path = tmp_dir.path().join("tape.ndjson");
        let rec = RecordingTransport::create(
            Arc::new(FailingTransport {
                kind: ErrorKind::RateLimited,
            }),
            &path,
            Arc::new(Redactor::default()),
        )
        .unwrap();

        drop(
            rec.complete(LlmRequest::from_prompt(
                "openai".into(),
                "gpt-5".into(),
                "hi".into(),
                None,
                None,
                None,
            ))
            .await,
        );
        rec.flush().unwrap();
        drop(rec);

        let contents = std::fs::read_to_string(&path).unwrap();
        // Wire-format check: the serialized form must carry the
        // `outcome.kind = "err"` discriminator and the inner
        // `LlmFailure::RateLimited` payload kind. Parsing structurally
        // (rather than substring-matching) avoids coupling the test
        // to canonical-json's alphabetical key ordering, which puts
        // `err` before `kind` inside `outcome`.
        let parsed: serde_json::Value =
            serde_json::from_str(contents.trim()).expect("recorded line must parse as JSON");
        assert_eq!(
            parsed["outcome"]["kind"], "err",
            "wire format drift — outcome.kind discriminator missing: {contents}",
        );
        assert_eq!(
            parsed["outcome"]["err"]["kind"], "rate_limited",
            "wire format drift — inner failure kind missing: {contents}",
        );
    }

    #[tokio::test]
    async fn entries_store_the_key_not_the_request() {
        // Entries used to carry the whole request, so a tape grew
        // quadratically with the message history.
        use crate::llm::OrnoChatMessage;

        let tmp_dir = tempfile::TempDir::new().unwrap();
        let path = tmp_dir.path().join("tape.ndjson");
        let rec = RecordingTransport::create(
            Arc::new(DummyTransport),
            &path,
            Arc::new(Redactor::default()),
        )
        .unwrap();
        let mut req = LlmRequest::from_prompt(
            "openai".into(),
            "gpt-5".into(),
            "hi".into(),
            None,
            None,
            None,
        );
        req.messages = Arc::new(vec![OrnoChatMessage::User {
            content: "distinctive-history-marker".into(),
        }]);
        rec.complete(req).await.unwrap();
        rec.flush().unwrap();
        drop(rec);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            !contents.contains("distinctive-history-marker"),
            "{contents}"
        );
        let entry: TapeEntry = serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(entry.seq, 0);
    }

    #[tokio::test]
    async fn tool_definitions_are_recorded_once() {
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let path = tmp_dir.path().join("tape.ndjson");
        let rec = RecordingTransport::create(
            Arc::new(DummyTransport),
            &path,
            Arc::new(Redactor::default()),
        )
        .unwrap();
        let tool = OrnoChatTool {
            name: "mcp_fs_read".into(),
            description: "Read".into(),
            schema: serde_json::json!({"type": "object"}),
        };
        for prompt in ["one", "two"] {
            let req = LlmRequest::from_prompt(
                "openai".into(),
                "gpt-5".into(),
                prompt.into(),
                None,
                None,
                None,
            )
            .with_tools(vec![tool.clone()]);
            rec.complete(req).await.unwrap();
        }
        rec.flush().unwrap();
        drop(rec);

        let entries: Vec<TapeEntry> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(entries[0].tools.len(), 1);
        assert!(entries[1].tools.is_empty(), "already recorded");
    }
}
