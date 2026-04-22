//! `RecordingTransport` — decorator that writes `(request, response)`
//! pairs to an NDJSON tape while passing the call through to an inner
//! transport. Pairs with [`super::ReplayTransport`] for the
//! determinism guarantee in ADR 0005 dimension 5.
//!
//! Tape format: one JSON object per line with `{ "req": …, "res": … }`.
//! Portable DTOs only — no `genai::*` types touch the tape, so tapes
//! remain valid across library version bumps.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::LlmError;

use super::{LlmRequest, LlmResponse, LlmTransport};

pub struct RecordingTransport {
    inner: Arc<dyn LlmTransport>,
    tape: Arc<Mutex<BufWriter<File>>>,
    path: PathBuf,
}

/// Single tape entry. Kept public so `ReplayTransport` and external
/// tools (e.g. a future `orno tape inspect`) can deserialize without
/// re-implementing the shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TapeEntry {
    pub req: LlmRequest,
    pub res: LlmResponse,
}

impl RecordingTransport {
    /// Create or truncate the tape file at `path` and wrap `inner`.
    /// Parent directory must exist — the tape file itself is created
    /// or truncated by this call.
    pub fn create(
        inner: Arc<dyn LlmTransport>,
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
}

#[async_trait]
impl LlmTransport for RecordingTransport {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        let res = self.inner.complete(req.clone()).await?;

        // Serialize + write without crossing an `.await`. The mutex
        // is held only for the duration of one line write; flush is
        // deferred to either the caller or `Drop`.
        let line = serde_json::to_string(&TapeEntry {
            req,
            res: res.clone(),
        })
        .map_err(|e| LlmError::ParseError(e.to_string()))?;
        {
            let mut guard = self.tape.lock().expect("tape mutex poisoned");
            writeln!(*guard, "{line}").map_err(|e| LlmError::Transport(Box::new(e)))?;
        }
        Ok(res)
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
        let rec = RecordingTransport::create(Arc::new(DummyTransport), &path).unwrap();

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
            assert!(!entry.res.content.is_empty());
        }
    }
}
