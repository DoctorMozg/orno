//! `ReplayToolHandler` — load a tape written by
//! [`super::record::RecordingToolHandler`] and return tool results
//! deterministically without invoking the inner handler. A tape miss
//! is a hard error: replay never falls through to a live call, which
//! is what makes it usable as the bounded-non-determinism guarantee
//! in the strictness contract.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tracing::instrument;

use super::record::{ToolTapeEntry, ToolTapeOutcome, tool_tape_key};
use super::{ToolEffect, ToolHandler, ToolInvocation};
use crate::error::ToolError;
use crate::util::{TapeIndex, read_tape};

/// Wraps any `ToolHandler` for metadata but intercepts every `invoke`
/// call, returning the result stored in the loaded tape instead of
/// calling the real handler. A tape miss is a hard error.
pub struct ReplayToolHandler {
    inner: Arc<dyn ToolHandler>,
    // Served per `(key, seq)`: an agent that calls the same tool twice
    // with byte-identical args gets the two recorded results back in
    // call order.
    index: TapeIndex<ToolTapeOutcome>,
}

impl std::fmt::Debug for ReplayToolHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayToolHandler")
            .field("tool", &self.inner.name())
            .field("entries", &self.index.remaining())
            .finish()
    }
}

impl ReplayToolHandler {
    /// Load entries from a tape file. A corrupt line aborts with an
    /// `io::Error` naming the file and line so a half-read tape cannot
    /// silently replay stale data.
    pub fn load(
        inner: Arc<dyn ToolHandler>,
        path: impl AsRef<Path>,
    ) -> Result<Self, std::io::Error> {
        Self::from_entries(inner, read_tape(path)?)
    }

    /// Build a handler from entries already in memory, such as a replay
    /// bundle. Two entries claiming one `(key, seq)` slot are an error.
    pub fn from_entries(
        inner: Arc<dyn ToolHandler>,
        entries: Vec<ToolTapeEntry>,
    ) -> Result<Self, std::io::Error> {
        let index = TapeIndex::new(entries.into_iter().map(|e| (e.key, e.seq, e.outcome)))
            .map_err(std::io::Error::other)?;
        Ok(Self { inner, index })
    }
}

#[async_trait]
impl ToolHandler for ReplayToolHandler {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn schema(&self) -> Value {
        self.inner.schema()
    }
    fn effect(&self) -> ToolEffect {
        self.inner.effect()
    }

    #[instrument(skip(self, inv, args), fields(tool.name = %self.inner.name(), call_id = %inv.call_id))]
    async fn invoke(&self, inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        let key = tool_tape_key(self.inner.name(), inv.call_id, &args, None);
        let outcome = self.index.take(&key).ok_or_else(|| ToolError::Invocation {
            name: self.inner.name().to_string(),
            source: Box::new(std::io::Error::other(format!(
                "tool tape miss for key `{key}` — was this call recorded?"
            ))),
        })?;
        outcome.into_result(self.inner.name())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::error::RecoverableKind;
    use crate::events::Redactor;
    use crate::tool::record::RecordingToolHandler;
    use crate::tool::{ToolInvocation, record::ToolTapeEntry};

    struct EchoHandler;

    impl std::fmt::Debug for EchoHandler {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "EchoHandler")
        }
    }

    #[async_trait]
    impl ToolHandler for EchoHandler {
        fn name(&self) -> &str {
            "Echo"
        }
        fn description(&self) -> &str {
            "echoes args"
        }
        fn schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn effect(&self) -> ToolEffect {
            ToolEffect::ReadOnly
        }
        async fn invoke(&self, _inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
            Ok(args.to_string())
        }
    }

    struct AlwaysErrHandler;

    impl std::fmt::Debug for AlwaysErrHandler {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "AlwaysErrHandler")
        }
    }

    #[async_trait]
    impl ToolHandler for AlwaysErrHandler {
        fn name(&self) -> &str {
            "Fail"
        }
        fn description(&self) -> &str {
            "always fails"
        }
        fn schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn effect(&self) -> ToolEffect {
            ToolEffect::ReadOnly
        }
        async fn invoke(
            &self,
            _inv: ToolInvocation<'_>,
            _args: Value,
        ) -> Result<String, ToolError> {
            Err(ToolError::Invocation {
                name: "Fail".to_string(),
                source: Box::new(std::io::Error::other("simulated failure")),
            })
        }
    }

    #[tokio::test]
    async fn round_trip_success() {
        // `RecordingToolHandler::create` opens with `O_EXCL`, so the
        // tape path must not exist before the call. Use a fresh
        // subdirectory and pick an unused filename inside it.
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let path = tmp_dir.path().join("tape.ndjson");

        let rec = RecordingToolHandler::create(
            Arc::new(EchoHandler),
            &path,
            Arc::new(Redactor::default()),
        )
        .unwrap();
        let args = json!({"x": 1});
        let result = rec
            .invoke(ToolInvocation::for_test("c1"), args.clone())
            .await
            .unwrap();
        rec.flush().unwrap();

        let replay = ReplayToolHandler::load(Arc::new(EchoHandler), &path).unwrap();
        let replayed = replay
            .invoke(ToolInvocation::for_test("c1"), args)
            .await
            .unwrap();

        assert_eq!(result, replayed);
    }

    #[tokio::test]
    async fn round_trip_error() {
        // `RecordingToolHandler::create` opens with `O_EXCL`, so the
        // tape path must not exist before the call. Use a fresh
        // subdirectory and pick an unused filename inside it.
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let path = tmp_dir.path().join("tape.ndjson");

        let rec = RecordingToolHandler::create(
            Arc::new(AlwaysErrHandler),
            &path,
            Arc::new(Redactor::default()),
        )
        .unwrap();
        rec.invoke(ToolInvocation::for_test("c2"), json!({}))
            .await
            .unwrap_err();
        rec.flush().unwrap();

        let replay = ReplayToolHandler::load(Arc::new(AlwaysErrHandler), &path).unwrap();
        let err = replay
            .invoke(ToolInvocation::for_test("c2"), json!({}))
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::Invocation { .. }));
    }

    #[tokio::test]
    async fn tape_miss_returns_error() {
        // `RecordingToolHandler::create` opens with `O_EXCL`, so the
        // tape path must not exist before the call. Use a fresh
        // subdirectory and pick an unused filename inside it.
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let path = tmp_dir.path().join("tape.ndjson");

        // Write a tape with call_id "c1"
        let rec = RecordingToolHandler::create(
            Arc::new(EchoHandler),
            &path,
            Arc::new(Redactor::default()),
        )
        .unwrap();
        rec.invoke(ToolInvocation::for_test("c1"), json!({"x": 1}))
            .await
            .unwrap();
        rec.flush().unwrap();

        // Replay with a different call_id → miss
        let replay = ReplayToolHandler::load(Arc::new(EchoHandler), &path).unwrap();
        let err = replay
            .invoke(ToolInvocation::for_test("c99"), json!({"x": 1}))
            .await
            .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("Echo"), "error should name the tool: {msg}");
    }

    #[tokio::test]
    async fn corrupt_tape_line_error_includes_path_and_line() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let good = serde_json::to_string(&ToolTapeEntry {
            key: "k".to_string(),
            seq: 0,
            outcome: ToolTapeOutcome::Ok {
                content: "ok".to_string(),
            },
        })
        .unwrap();
        std::fs::write(&path, format!("{good}\nnot-json\n{good}\n")).unwrap();

        let err = ReplayToolHandler::load(Arc::new(EchoHandler), &path).unwrap_err();

        let msg = err.to_string();
        let path_disp = path.display().to_string();
        assert!(
            msg.contains(&path_disp),
            "error should mention tape path `{path_disp}`: {msg}",
        );
        assert!(
            msg.contains(":2:"),
            "error should mention 1-indexed line number `:2:`: {msg}",
        );
    }

    /// Fails with a configurable `ToolError`, to check the class survives replay.
    struct FailWith(fn() -> ToolError);

    impl std::fmt::Debug for FailWith {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "FailWith")
        }
    }

    #[async_trait]
    impl ToolHandler for FailWith {
        fn name(&self) -> &str {
            "Read"
        }
        fn description(&self) -> &str {
            "fails"
        }
        fn schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn effect(&self) -> ToolEffect {
            ToolEffect::ReadOnly
        }
        async fn invoke(
            &self,
            _inv: ToolInvocation<'_>,
            _args: Value,
        ) -> Result<String, ToolError> {
            Err((self.0)())
        }
    }

    async fn replay_failure(make: fn() -> ToolError) -> ToolError {
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let path = tmp_dir.path().join("tape.ndjson");
        let handler: Arc<dyn ToolHandler> = Arc::new(FailWith(make));
        let rec = RecordingToolHandler::create(
            Arc::clone(&handler),
            &path,
            Arc::new(Redactor::default()),
        )
        .unwrap();
        rec.invoke(ToolInvocation::for_test("c1"), json!({}))
            .await
            .unwrap_err();
        rec.flush().unwrap();

        ReplayToolHandler::load(handler, &path)
            .unwrap()
            .invoke(ToolInvocation::for_test("c1"), json!({}))
            .await
            .unwrap_err()
    }

    #[tokio::test]
    async fn recoverable_error_stays_recoverable_with_its_kind() {
        let err = replay_failure(|| ToolError::Recoverable {
            name: "Read".into(),
            kind: RecoverableKind::NotFound,
            message: "no such file".into(),
        })
        .await;

        match err {
            ToolError::Recoverable {
                kind: RecoverableKind::NotFound,
                message,
                ..
            } => assert_eq!(message, "no such file"),
            other => panic!("expected Recoverable(NotFound), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn terminal_error_classes_survive_replay() {
        assert!(matches!(
            replay_failure(|| ToolError::Denied { reason: "jail".into() }).await,
            ToolError::Denied { reason } if reason == "jail"
        ));
        assert!(matches!(
            replay_failure(|| ToolError::StateTooLarge {
                name: "Read".into(),
                bytes: 9,
                cap: 4
            })
            .await,
            ToolError::StateTooLarge {
                bytes: 9,
                cap: 4,
                ..
            }
        ));
        assert!(matches!(
            replay_failure(|| ToolError::InvalidArgs { name: "Read".into(), message: "bad".into() })
                .await,
            ToolError::InvalidArgs { message, .. } if message == "bad"
        ));
    }

    #[tokio::test]
    async fn secret_in_args_replays_from_the_redacted_args() {
        // Replay receives the args from the redacted LLM tape, so the
        // key must have been computed over the redacted form.
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let path = tmp_dir.path().join("tape.ndjson");
        let secrets =
            std::collections::BTreeMap::from([("t".to_string(), "tok-abcdef123".to_string())]);
        let rec = RecordingToolHandler::create(
            Arc::new(EchoHandler),
            &path,
            Arc::new(Redactor::new(&secrets)),
        )
        .unwrap();
        rec.invoke(
            ToolInvocation::for_test("c1"),
            json!({"auth": "Bearer tok-abcdef123"}),
        )
        .await
        .unwrap();
        rec.flush().unwrap();

        let replayed = ReplayToolHandler::load(Arc::new(EchoHandler), &path)
            .unwrap()
            .invoke(
                ToolInvocation::for_test("c1"),
                json!({"auth": "Bearer ***"}),
            )
            .await
            .expect("redacted args must hit the recorded entry");
        assert_eq!(replayed, json!({"auth": "Bearer ***"}).to_string());
    }

    #[tokio::test]
    async fn identical_calls_replay_in_seq_order() {
        let key = tool_tape_key("Echo", "c1", &json!({}), None);
        let entry = |seq, content: &str| ToolTapeEntry {
            key: key.clone(),
            seq,
            outcome: ToolTapeOutcome::Ok {
                content: content.into(),
            },
        };
        let replay = ReplayToolHandler::from_entries(
            Arc::new(EchoHandler),
            vec![entry(1, "second"), entry(0, "first")],
        )
        .unwrap();

        for expected in ["first", "second"] {
            let got = replay
                .invoke(ToolInvocation::for_test("c1"), json!({}))
                .await
                .unwrap();
            assert_eq!(got, expected);
        }
    }
}
