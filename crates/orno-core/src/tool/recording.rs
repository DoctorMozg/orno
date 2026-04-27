//! Tool-result tape: record every `invoke` response to NDJSON, replay
//! bit-for-bit on a second run. Implements the bounded-non-determinism
//! dimension of the strictness contract for the tool layer.
//!
//! Tape format: one JSON object per line —
//! `{ "key": "<hex>", "content": "...", "error": null }` on success,
//! `{ "key": "<hex>", "content": null, "error": "..." }` on failure.
//!
//! Key: `blake3( tool_name + ":" + call_id + ":" + args_json )`. Using
//! the LLM-issued `call_id` couples the tool tape to the LLM tape so a
//! full replay is consistent when both tapes are loaded together.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
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

// ─── RecordingToolHandler ────────────────────────────────────────────────────

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
    /// Wrap `inner` and create or truncate the tape at `path`.
    pub fn create(
        inner: Arc<dyn ToolHandler>,
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
        self.tape.lock().expect("tape mutex poisoned").flush()
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

// ─── ReplayToolHandler ───────────────────────────────────────────────────────

/// Wraps any `ToolHandler` for metadata but intercepts every `invoke`
/// call, returning the result stored in the loaded tape instead of
/// calling the real handler. A tape miss is a hard error.
pub struct ReplayToolHandler {
    inner: Arc<dyn ToolHandler>,
    entries: HashMap<String, ToolTapeEntry>,
}

impl std::fmt::Debug for ReplayToolHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayToolHandler")
            .field("tool", &self.inner.name())
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl ReplayToolHandler {
    /// Load entries from a tape file. Corrupt lines abort with a
    /// `ToolError::InvalidArgs` so a half-read tape cannot silently
    /// replay stale data.
    pub fn load(
        inner: Arc<dyn ToolHandler>,
        path: impl AsRef<Path>,
    ) -> Result<Self, std::io::Error> {
        let path_ref = path.as_ref();
        let path_disp = path_ref.display().to_string();
        let file = File::open(path_ref)?;
        let mut entries = HashMap::new();
        for (lineno, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: ToolTapeEntry = serde_json::from_str(&line)
                .map_err(|e| std::io::Error::other(format!("{path_disp}:{}: {e}", lineno + 1)))?;
            entries.insert(entry.key.clone(), entry);
        }
        Ok(Self { inner, entries })
    }

    /// In-memory constructor — skips file I/O. Useful when entries
    /// arrive from a replay bundle (`orno replay`) rather than a
    /// standalone tape file on disk.
    #[must_use]
    pub fn from_entries(inner: Arc<dyn ToolHandler>, entries: Vec<ToolTapeEntry>) -> Self {
        let map = entries.into_iter().map(|e| (e.key.clone(), e)).collect();
        Self {
            inner,
            entries: map,
        }
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

    async fn invoke(&self, inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        let key = tool_tape_key(self.inner.name(), inv.call_id, &args);
        let entry = self
            .entries
            .get(&key)
            .ok_or_else(|| ToolError::Invocation {
                name: self.inner.name().to_string(),
                source: Box::new(std::io::Error::other(format!(
                    "tool tape miss for key `{key}` — was this call recorded?"
                ))),
            })?;

        if let Some(content) = &entry.content {
            Ok(content.clone())
        } else if let Some(err_msg) = &entry.error {
            Err(ToolError::Invocation {
                name: self.inner.name().to_string(),
                source: Box::new(std::io::Error::other(err_msg.clone())),
            })
        } else {
            Err(ToolError::Invocation {
                name: self.inner.name().to_string(),
                source: Box::new(std::io::Error::other(
                    "tape entry has neither content nor error",
                )),
            })
        }
    }
}

// ─── Key derivation ──────────────────────────────────────────────────────────

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
fn tool_tape_key(tool_name: &str, call_id: &str, args: &Value) -> String {
    let raw = format!("{tool_name}:{call_id}:{}", canonical_json(args));
    blake3::hash(raw.as_bytes()).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::events::Redactor;
    use crate::tool::ToolInvocation;

    // Minimal stub that always returns a fixed string.
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
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

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
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

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
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

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

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
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

    #[tokio::test]
    async fn corrupt_tape_line_error_includes_path_and_line() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let good = serde_json::to_string(&ToolTapeEntry {
            key: "k".to_string(),
            content: Some("ok".to_string()),
            error: None,
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
}
