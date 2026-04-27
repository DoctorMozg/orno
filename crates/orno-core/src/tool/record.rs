//! `RecordingToolHandler` — decorator that writes every `invoke` result
//! to an NDJSON tape while passing the call through to an inner handler.
//! Pairs with [`super::replay::ReplayToolHandler`] for the
//! bounded-non-determinism guarantee in the strictness contract.
//!
//! Tape format: one JSON object per line —
//! `{ "key": "<hex>", "content": "...", "error": null }` on success,
//! `{ "key": "<hex>", "content": null, "error": "..." }` on failure.
//!
//! Key: `blake3( tool_name + ":" + call_id + ":" + canonical_json(args) )`.
//! Using the LLM-issued `call_id` couples the tool tape to the LLM tape
//! so a full replay is consistent when both tapes are loaded together.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ToolEffect, ToolHandler, ToolInvocation};
use crate::error::ToolError;
use crate::events::Redactor;
use crate::util::canonical_json;

/// One entry persisted to or loaded from the tool tape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolTapeEntry {
    /// `blake3( tool_name + ":" + call_id + ":" + args_json )` hex.
    pub key: String,
    /// Tool return value on success. `None` when `error` is set.
    pub content: Option<String>,
    /// `Display` form of the `ToolError` on failure. `None` on success.
    pub error: Option<String>,
}

/// Wraps any `ToolHandler`: passes every call through, then appends the
/// result to a shared NDJSON tape. `flush()` must be called after the run
/// because `BufWriter` may hold unflushed bytes.
pub struct RecordingToolHandler {
    inner: Arc<dyn ToolHandler>,
    tape: Arc<Mutex<BufWriter<File>>>,
    path: PathBuf,
    redactor: Arc<Redactor>,
}

impl std::fmt::Debug for RecordingToolHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingToolHandler")
            .field("tool", &self.inner.name())
            .field("path", &self.path)
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
        let path = path.into();
        let file = {
            let mut opts = OpenOptions::new();
            // `create_new` (O_EXCL) prevents a local attacker from
            // pre-creating a symlink at the tape path that would
            // redirect writes elsewhere; if a stale tape sits at the
            // path the caller must delete it before re-recording.
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                // Tape files capture full tool args and outputs —
                // they must not be world-readable on shared hosts.
                opts.mode(0o600);
            }
            opts.open(&path)?
        };
        Ok(Self {
            inner,
            tape: Arc::new(Mutex::new(BufWriter::new(file))),
            path,
            redactor,
        })
    }

    /// Wrap `inner` and share an already-opened tape. Use this when
    /// multiple handlers must all write to the same NDJSON file — the
    /// caller is responsible for flushing the tape after the run.
    pub fn with_shared_tape(
        inner: Arc<dyn ToolHandler>,
        tape: Arc<Mutex<BufWriter<File>>>,
        path: PathBuf,
        redactor: Arc<Redactor>,
    ) -> Self {
        Self {
            inner,
            tape,
            path,
            redactor,
        }
    }

    #[must_use]
    pub fn tape_path(&self) -> &Path {
        &self.path
    }

    /// Flush buffered bytes to disk. Must be called before dropping to
    /// avoid silently losing the last entries.
    pub fn flush(&self) -> Result<(), std::io::Error> {
        let mut guard = self
            .tape
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.flush()?;
        // fsync: a `BufWriter::flush` only pushes bytes to the kernel;
        // without `sync_all` the OS page cache can lose the tape across
        // a power cut, breaking the bounded-non-determinism guarantee
        // when replay runs against an apparently-flushed file.
        guard.get_mut().sync_all()
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

    async fn invoke(&self, inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        let key = tool_tape_key(self.inner.name(), inv.call_id, &args);
        let result = self.inner.invoke(inv, args).await;

        let entry = ToolTapeEntry {
            key,
            content: result
                .as_ref()
                .ok()
                .map(|s| self.redactor.redact(s).into_owned()),
            error: result.as_ref().err().map(ToString::to_string),
        };
        let line = serde_json::to_string(&entry)
            .unwrap_or_else(|e| format!("{{\"serialize_err\":\"{e}\"}}"));
        {
            let mut guard = self.tape.lock().expect("tape mutex poisoned");
            // Record/replay is a strictness dimension, so a tape-write
            // failure aborts the call rather than silently dropping the
            // entry — replay would otherwise diverge.
            if let Err(e) = writeln!(*guard, "{line}") {
                tracing::warn!(
                    error = %e,
                    tool = self.inner.name(),
                    path = %self.path.display(),
                    "tool tape write failed — replay bundle is incomplete",
                );
                return Err(ToolError::Invocation {
                    name: self.inner.name().to_string(),
                    source: Box::new(e),
                });
            }
        }
        result
    }
}

/// Tool-tape key = `blake3( tool_name + ":" + call_id + ":" + canonical_json(args) )`.
///
/// `canonical_json` sorts every nested JSON object's keys alphabetically,
/// pinning the hash input to the args' content rather than to the
/// insertion order of whatever produced them. This shields tapes from
/// silent invalidation when a `schemars` (or other serde-Value) minor
/// bump shifts key order in tool arguments.
///
/// `call_id` is provider-issued (Anthropic and `OpenAI` emit different
/// IDs for the same logical call) so cross-provider tool-tape replay is
/// intentionally unsupported. Document this constraint to operators in
/// `docs/tutorials/record-replay.md`.
pub(crate) fn tool_tape_key(tool_name: &str, call_id: &str, args: &Value) -> String {
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
        // `/dev/full` accepts opens but every `write` returns ENOSPC. A
        // zero-capacity `BufWriter` forwards each byte straight to the
        // underlying file, so the failure cannot hide in the buffer.
        let path = PathBuf::from("/dev/full");
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        let tape = Arc::new(Mutex::new(BufWriter::with_capacity(0, file)));
        let rec = RecordingToolHandler::with_shared_tape(
            Arc::new(EchoHandler),
            tape,
            path,
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
