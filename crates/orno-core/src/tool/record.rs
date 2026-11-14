//! `RecordingToolHandler` — decorator that writes every `invoke` result
//! to an NDJSON tape while passing the call through to an inner handler.
//! Pairs with [`super::replay::ReplayToolHandler`] for the
//! bounded-non-determinism guarantee in the strictness contract.
//!
//! Tape format: one JSON object per line —
//! `{ "key": "<hex>", "seq": 0, "outcome": { "kind": "ok", "content": "..." } }`
//! on success, with `"kind"` naming the `ToolError` class on failure
//! (see [`ToolTapeOutcome`]).
//!
//! Key: `blake3( tool_name + ":" + call_id + ":" + canonical_json(redacted args) )`.
//! Using the LLM-issued `call_id` couples the tool tape to the LLM tape
//! so a full replay is consistent when both tapes are loaded together.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::instrument;

use super::{ToolEffect, ToolHandler, ToolInvocation};
use crate::error::{RecoverableKind, ToolError};
use crate::events::Redactor;
use crate::util::{SeqCounter, TapeFile, canonical_json};

/// One entry persisted to or loaded from the tool tape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolTapeEntry {
    /// `blake3( tool_name + ":" + call_id + ":" + args_json )` hex.
    pub key: String,
    /// Index of this call among the calls that share `key`, assigned
    /// before the tool ran.
    pub seq: u32,
    /// Recorded result of the call.
    pub outcome: ToolTapeOutcome,
}

/// Recorded result of one tool call, keeping the `ToolError` class so
/// replay hands the agent loop the same recoverable-or-terminal
/// decision it saw live. Failure messages are redacted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolTapeOutcome {
    /// The tool returned its output.
    Ok {
        /// Tool return value.
        content: String,
    },
    /// `ToolError::Recoverable`.
    Recoverable {
        /// Why the model can retry.
        recoverable: RecoverableKind,
        /// Text shown to the model.
        message: String,
    },
    /// `ToolError::Invocation`; the source chain is flattened.
    Invocation {
        /// Rendered source error.
        message: String,
    },
    /// `ToolError::InvalidArgs`.
    InvalidArgs {
        /// What was wrong with the arguments.
        message: String,
    },
    /// `ToolError::StateTooLarge`.
    StateTooLarge {
        /// Serialized state size that was rejected.
        bytes: usize,
        /// Cap it exceeded.
        cap: usize,
    },
    /// `ToolError::NotImplemented`.
    NotImplemented {
        /// The unwired feature.
        feature: String,
    },
    /// `ToolError::Denied`.
    Denied {
        /// Why access was refused.
        reason: String,
    },
}

impl ToolTapeOutcome {
    /// Capture `result`, redacting every free-text field.
    pub(crate) fn from_result(result: &Result<String, ToolError>, redactor: &Redactor) -> Self {
        let clean = |s: &str| redactor.redact(s).into_owned();
        match result {
            Ok(content) => Self::Ok {
                content: clean(content),
            },
            Err(ToolError::Recoverable { kind, message, .. }) => Self::Recoverable {
                recoverable: *kind,
                message: clean(message),
            },
            Err(ToolError::Invocation { source, .. }) => Self::Invocation {
                message: clean(&source.to_string()),
            },
            Err(ToolError::InvalidArgs { message, .. }) => Self::InvalidArgs {
                message: clean(message),
            },
            Err(ToolError::StateTooLarge { bytes, cap, .. }) => Self::StateTooLarge {
                bytes: *bytes,
                cap: *cap,
            },
            Err(ToolError::NotImplemented { feature, .. }) => Self::NotImplemented {
                feature: clean(feature),
            },
            Err(ToolError::Denied { reason }) => Self::Denied {
                reason: clean(reason),
            },
        }
    }

    /// Rebuild the result for the tool called `name`.
    pub(crate) fn into_result(self, name: &str) -> Result<String, ToolError> {
        let name = name.to_string();
        match self {
            Self::Ok { content } => Ok(content),
            Self::Recoverable {
                recoverable,
                message,
            } => Err(ToolError::Recoverable {
                name,
                kind: recoverable,
                message,
            }),
            Self::Invocation { message } => Err(ToolError::Invocation {
                name,
                source: Box::new(std::io::Error::other(message)),
            }),
            Self::InvalidArgs { message } => Err(ToolError::InvalidArgs { name, message }),
            Self::StateTooLarge { bytes, cap } => {
                Err(ToolError::StateTooLarge { name, bytes, cap })
            },
            Self::NotImplemented { feature } => Err(ToolError::NotImplemented { name, feature }),
            Self::Denied { reason } => Err(ToolError::Denied { reason }),
        }
    }
}

/// Wraps any `ToolHandler`: passes every call through, then appends the
/// result to a shared NDJSON tape. `flush()` must be called after the run
/// because the tape buffers its writes.
pub struct RecordingToolHandler {
    inner: Arc<dyn ToolHandler>,
    tape: Arc<TapeFile>,
    seq: SeqCounter,
    redactor: Arc<Redactor>,
}

impl std::fmt::Debug for RecordingToolHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingToolHandler")
            .field("tool", &self.inner.name())
            .field("path", &self.tape.path())
            .finish_non_exhaustive()
    }
}

impl RecordingToolHandler {
    /// Wrap `inner` and create the tape at `path`. The path must not
    /// already exist — `O_EXCL` forces an explicit caller decision
    /// about reusing a stale tape.
    pub fn create(
        inner: Arc<dyn ToolHandler>,
        path: impl Into<PathBuf>,
        redactor: Arc<Redactor>,
    ) -> Result<Self, std::io::Error> {
        let tape = Arc::new(TapeFile::create(path)?);
        Ok(Self::with_shared_tape(inner, tape, redactor))
    }

    /// Wrap `inner` and share an already-opened tape. Use this when
    /// multiple handlers must all write to the same NDJSON file — the
    /// caller is responsible for flushing the tape after the run.
    pub fn with_shared_tape(
        inner: Arc<dyn ToolHandler>,
        tape: Arc<TapeFile>,
        redactor: Arc<Redactor>,
    ) -> Self {
        Self {
            inner,
            tape,
            seq: SeqCounter::default(),
            redactor,
        }
    }

    #[must_use]
    pub fn tape_path(&self) -> &Path {
        self.tape.path()
    }

    /// Flush buffered bytes to disk. Must be called before dropping to
    /// avoid silently losing the last entries.
    pub fn flush(&self) -> Result<(), std::io::Error> {
        self.tape.flush()
    }
}

#[async_trait]
impl ToolHandler for RecordingToolHandler {
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

    #[instrument(skip(self, inv, args), fields(tool.name = %self.inner.name(), tool.call_id = %inv.call_id))]
    async fn invoke(&self, inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        let key = tool_tape_key(self.inner.name(), inv.call_id, &args, Some(&self.redactor));
        let seq = self.seq.next(&key);
        let result = self.inner.invoke(inv, args).await;

        let entry = ToolTapeEntry {
            key,
            seq,
            outcome: ToolTapeOutcome::from_result(&result, &self.redactor),
        };
        // Record/replay is a strictness dimension, so a tape-write
        // failure aborts the call rather than silently dropping the
        // entry — replay would otherwise diverge.
        if let Err(e) = self.tape.append(&entry) {
            tracing::warn!(
                error = %e,
                tool.name = self.inner.name(),
                file.path = %self.tape.path().display(),
                "tool tape write failed — replay bundle is incomplete",
            );
            return Err(ToolError::Invocation {
                name: self.inner.name().to_string(),
                source: Box::new(e),
            });
        }
        result
    }
}

/// Tool-tape key = `blake3( tool_name + ":" + call_id + ":" + canonical_json(args) )`.
///
/// Record passes its `redactor` so the args are keyed in redacted form:
/// replay receives them from the redacted LLM tape and has no secrets to
/// restore, so only the redacted form is reproducible. `canonical_json`
/// sorts every nested object's keys so the hash does not depend on the
/// insertion order of whatever produced the args.
///
/// `call_id` is provider-issued (Anthropic and `OpenAI` emit different
/// IDs for the same logical call) so cross-provider tool-tape replay is
/// intentionally unsupported.
pub(crate) fn tool_tape_key(
    tool_name: &str,
    call_id: &str,
    args: &Value,
    redactor: Option<&Redactor>,
) -> String {
    let mut redacted;
    let args = match redactor {
        Some(redactor) if !redactor.is_noop() => {
            redacted = args.clone();
            redactor.redact_json(&mut redacted);
            &redacted
        },
        _ => args,
    };
    let raw = format!("{tool_name}:{call_id}:{}", canonical_json(args));
    blake3::hash(raw.as_bytes()).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::events::Redactor;
    use crate::tool::ToolInvocation;

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

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn tape_write_failure_returns_invocation_error() {
        // `/dev/full` accepts opens but every `write` returns ENOSPC; an
        // unbuffered tape keeps the failure from hiding in the buffer.
        let path = PathBuf::from("/dev/full");
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let tape = Arc::new(TapeFile::from_unbuffered(file, path));
        let rec = RecordingToolHandler::with_shared_tape(
            Arc::new(EchoHandler),
            tape,
            Arc::new(Redactor::default()),
        );

        let err = rec
            .invoke(ToolInvocation::for_test("c1"), json!({"x": 1}))
            .await
            .unwrap_err();

        assert!(
            matches!(err, ToolError::Invocation { ref name, .. } if name == "Echo"),
            "expected Invocation error from tape-write failure, got {err:?}",
        );
    }

    #[tokio::test]
    async fn secret_not_written_to_tape() {
        use std::collections::BTreeMap;

        // `RecordingToolHandler::create` opens with `O_EXCL`, so the
        // tape path must not exist before the call. Use a fresh
        // subdirectory and pick an unused filename inside it.
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let path = tmp_dir.path().join("tape.ndjson");
        let mut secrets = BTreeMap::new();
        secrets.insert("token".to_string(), "bearer-secret-xyz".to_string());
        let rec = RecordingToolHandler::create(
            Arc::new(EchoHandler),
            &path,
            Arc::new(Redactor::new(&secrets)),
        )
        .unwrap();

        // EchoHandler returns args.to_string() — so the secret in args flows to output.
        rec.invoke(
            ToolInvocation::for_test("c1"),
            json!({"data": "bearer-secret-xyz"}),
        )
        .await
        .unwrap();
        rec.flush().unwrap();
        drop(rec);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            !contents.contains("bearer-secret-xyz"),
            "secret must not appear in tape",
        );
        assert!(contents.contains("***"), "redacted placeholder must appear");
    }
}
