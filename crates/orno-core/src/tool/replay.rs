//! `ReplayToolHandler` — load a tape written by
//! [`super::record::RecordingToolHandler`] and return tool results
//! deterministically without invoking the inner handler. A tape miss
//! is a hard error: replay never falls through to a live call, which
//! is what makes it usable as the bounded-non-determinism guarantee
//! in the strictness contract.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use super::record::{ToolTapeEntry, tool_tape_key};
use super::{ToolEffect, ToolHandler, ToolInvocation};
use crate::error::ToolError;

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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
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
