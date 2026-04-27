//! `RecordingTransport` — decorator that writes `(request, outcome)`
//! pairs to an NDJSON tape while passing the call through to an inner
//! transport. Pairs with [`super::ReplayTransport`] for the
//! bounded-non-determinism guarantee in the strictness contract.
//!
//! Tape format: one JSON object per line with `{ "req": …, "outcome":
//! { "kind": "ok"|"err", … } }`. Both successful responses and typed
//! failures are recorded so a replayed run reproduces both branches of
//! the original control flow — a transient API failure that drove a
//! downstream branching decision in the live run replays as the same
//! failure class without ever touching the network.
//!
//! Portable DTOs only — no `genai::*` types touch the tape, so tapes
//! remain valid across library version bumps.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::LlmError;
use crate::events::{LlmFailure, Redactor};

use super::{LlmRequest, LlmResponse, LlmTransport};

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
    tape: Arc<Mutex<BufWriter<File>>>,
    path: PathBuf,
    redactor: Arc<Redactor>,
}

/// Single tape entry. Kept public so `ReplayTransport` and external
/// tools (e.g. a future `orno tape inspect`) can deserialize without
/// re-implementing the shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TapeEntry {
    /// The request that produced this entry. Identifies the entry on
    /// replay via the `(provider, model, blake3(canonical_json(req)))`
    /// key.
    pub req: LlmRequest,
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

impl RecordingTransport {
    /// Create or truncate the tape file at `path` and wrap `inner`.
    /// Parent directory must exist — the tape file itself is created
    /// or truncated by this call.
    ///
    /// `redactor` is applied to every tape entry before it is written
    /// to disk. Pass `Arc::new(Redactor::default())` when recording
    /// in a context that has no secrets (e.g. tests).
    pub fn create(
        inner: Arc<dyn LlmTransport>,
        path: impl Into<PathBuf>,
        redactor: Arc<Redactor>,
    ) -> Result<Self, std::io::Error> {
        let path = path.into();
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        Ok(Self {
            inner,
            tape: Arc::new(Mutex::new(BufWriter::new(file))),
            path,
            redactor,
        })
    }

    /// Path the tape is being written to. Primarily useful for tests
    /// that need to round-trip through `ReplayTransport::load`.
    #[must_use]
    pub fn tape_path(&self) -> &Path {
        &self.path
    }

    /// Flush buffered bytes to disk. Call before dropping when the
    /// tape must survive a crash mid-run. `Drop` flushes too, but
    /// silently — errors get lost.
    pub fn flush(&self) -> Result<(), std::io::Error> {
        let mut guard = self.tape.lock().expect("tape mutex poisoned");
        guard.flush()
    }

    /// Serialize one tape entry, redact, and write as an NDJSON line.
    /// Failures here surface as `LlmError::Transport` rather than
    /// hiding behind the original call's success path — a tape write
    /// error that cannot be observed defeats the strictness guarantee.
    fn write_entry(&self, entry: &TapeEntry) -> Result<(), LlmError> {
        let raw_entry =
            serde_json::to_value(entry).map_err(|e| LlmError::ParseError(e.to_string()))?;
        let line = serde_json::to_string(&self.redactor.redact_json(&raw_entry))
            .map_err(|e| LlmError::ParseError(e.to_string()))?;
        let mut guard = self.tape.lock().expect("tape mutex poisoned");
        writeln!(*guard, "{line}").map_err(|e| LlmError::Transport(Box::new(e)))?;
        Ok(())
    }
}

#[async_trait]
impl LlmTransport for RecordingTransport {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        // Capture the inner result instead of `?`-ing it so a failed
        // call still lands on the tape. The original failure is
        // re-raised to the caller after the entry has been written.
        match self.inner.complete(req.clone()).await {
            Ok(res) => {
                let entry = TapeEntry {
                    req,
                    outcome: TapeOutcome::Ok { res: res.clone() },
                };
                self.write_entry(&entry)?;
                Ok(res)
            },
            Err(err) => {
                let failure = LlmFailure::from_llm_error(&err, RECORDED_API_BODY_EXCERPT_BYTES);
                let entry = TapeEntry {
                    req,
                    outcome: TapeOutcome::Err { err: failure },
                };
                // Best-effort tape write: if the entry cannot be
                // persisted (disk full, file vanished), surface the
                // tape error rather than the original — the missing
                // tape line is the more dangerous failure for replay.
                self.write_entry(&entry)?;
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
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
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

        let f = File::open(&path).unwrap();
        let lines: Vec<String> = std::io::BufReader::new(f)
            .lines()
            .map(Result::unwrap)
            .collect();
        assert_eq!(lines.len(), 2, "one line per call");
        for line in &lines {
            let entry: TapeEntry = serde_json::from_str(line).unwrap();
            assert_eq!(entry.req.provider, "openai");
            match entry.outcome {
                TapeOutcome::Ok { res } => assert!(!res.content.is_empty()),
                TapeOutcome::Err { err } => panic!("dummy transport must not fail: {err:?}"),
            }
        }
    }

    #[tokio::test]
    async fn secret_not_written_to_tape() {
        use std::collections::BTreeMap;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
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
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
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
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
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
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
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
}
