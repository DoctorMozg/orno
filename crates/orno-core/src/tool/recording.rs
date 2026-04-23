//! Tool-result tape: record every `invoke` response to NDJSON, replay
//! bit-for-bit on a second run. Implements the non-determinism
//! dimension of ADR 0005 §5 for the tool layer.
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
        })
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
            content: result.as_ref().ok().cloned(),
            error: result.as_ref().err().map(ToString::to_string),
        };
        let line = serde_json::to_string(&entry)
            .unwrap_or_else(|e| format!("{{\"serialize_err\":\"{e}\"}}"));
        {
            let mut guard = self.tape.lock().expect("tape mutex poisoned");
            // Write errors are non-fatal — we don't want a tape-write failure
            // to kill the agent run. Log at debug and continue.
            if let Err(e) = writeln!(*guard, "{line}") {
                tracing::debug!(error = %e, tool = self.inner.name(), "tool tape write failed");
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
        let file = File::open(path.as_ref())?;
        let mut entries = HashMap::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: ToolTapeEntry =
                serde_json::from_str(&line).map_err(|e| std::io::Error::other(e.to_string()))?;
            entries.insert(entry.key.clone(), entry);
        }
        Ok(Self { inner, entries })
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

fn tool_tape_key(tool_name: &str, call_id: &str, args: &Value) -> String {
    let raw = format!(
        "{}:{}:{}",
        tool_name,
        call_id,
        serde_json::to_string(args).unwrap_or_default()
    );
    blake3::hash(raw.as_bytes()).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
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

        let rec = RecordingToolHandler::create(Arc::new(EchoHandler), &path).unwrap();
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

        let rec = RecordingToolHandler::create(Arc::new(AlwaysErrHandler), &path).unwrap();
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
        let rec = RecordingToolHandler::create(Arc::new(EchoHandler), &path).unwrap();
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
}
