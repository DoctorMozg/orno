//! Combined record/replay bundle: pipeline YAML + LLM tape entries +
//! tool tape entries packed into a single NDJSON file.
//!
//! Format: one JSON object per line, internally tagged on `"type"`.
//! ```text
//! {"type":"bundle_header","format_version":1}
//! {"type":"pipeline_yaml","content":"version: 1\n..."}
//! {"type":"llm_entry","req":{...},"res":{...}}
//! {"type":"tool_entry","key":"<hex>","content":"...","error":null}
//! ```
//! `LlmEntry` and `ToolEntry` use newtype variants; serde's internal
//! tagging flattens the inner struct's fields alongside `"type"`, so a
//! bundle line carries both the discriminator and the entry payload at
//! the same JSON level — no nested `"data": {...}` wrapper.
//!
//! `read_bundle` is forgiving on unknown future entry types via
//! `#[non_exhaustive]` on the enum, but a missing `pipeline_yaml`
//! section is a hard error: `orno replay` cannot proceed without one.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::tool::ToolTapeEntry;

use super::recording::TapeEntry;

/// One line in a record/replay bundle. Internally tagged on `"type"`,
/// so each variant lands as a flat object on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum BundleEntry {
    /// First line of every bundle. `format_version` is bumped when the
    /// wire format changes incompatibly so a future reader can refuse
    /// rather than misinterpret an older bundle.
    BundleHeader {
        /// Bundle wire-format version. Currently `1`.
        format_version: u32,
    },
    /// Verbatim pipeline YAML the original run was driven from. Embedded
    /// rather than referenced by path so bundles are self-contained.
    PipelineYaml {
        /// Pipeline YAML body, exactly as the original `orno run` saw it.
        content: String,
    },
    /// One recorded LLM `(request, response)` pair from the run.
    LlmEntry(TapeEntry),
    /// One recorded tool invocation from the run.
    ToolEntry(ToolTapeEntry),
}

/// Parsed contents of a bundle file. The two entry vectors preserve the
/// order they were written in; `orno replay` re-keys them through
/// `ReplayTransport::from_entries` / `ReplayToolHandler::from_entries`
/// where order does not matter, but the deterministic order helps
/// diffs and human inspection.
#[derive(Debug)]
pub struct BundleContents {
    /// Pipeline YAML body extracted from the `pipeline_yaml` line.
    pub pipeline_yaml: String,
    /// LLM tape entries in the order they appeared in the bundle.
    pub llm_entries: Vec<TapeEntry>,
    /// Tool tape entries in the order they appeared in the bundle.
    pub tool_entries: Vec<ToolTapeEntry>,
}

/// Errors raised while writing or reading a bundle file.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BundleError {
    /// Underlying I/O error (file open, read, write).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Bundle did not contain a `pipeline_yaml` section. Replay cannot
    /// reconstruct the run without it.
    #[error("bundle is missing pipeline YAML section")]
    MissingPipelineYaml,
    /// A bundle line could not be deserialized as a `BundleEntry`.
    #[error("bundle parse error on line {line}: {msg}")]
    ParseError {
        /// 1-based line number in the source bundle file.
        line: usize,
        /// Human-readable parser error message.
        msg: String,
    },
}

/// Write a bundle combining the pipeline YAML body with previously
/// recorded LLM and tool tape files.
///
/// The tape files are read line-by-line and re-emitted as `LlmEntry` /
/// `ToolEntry` lines in the bundle. Pipeline YAML is embedded verbatim
/// without re-parsing — the bundle preserves whatever the original
/// `orno run` saw, including comments and trailing whitespace.
///
/// Either tape path may be `None` when no entries of that kind were
/// recorded; the bundle will simply omit those lines.
pub fn write_bundle(
    pipeline_yaml: &str,
    llm_tape_path: Option<&Path>,
    tool_tape_path: Option<&Path>,
    out: &Path,
) -> Result<(), BundleError> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(out)?;
    let mut writer = BufWriter::new(file);

    write_entry(
        &mut writer,
        &BundleEntry::BundleHeader { format_version: 1 },
    )?;
    write_entry(
        &mut writer,
        &BundleEntry::PipelineYaml {
            content: pipeline_yaml.to_string(),
        },
    )?;

    if let Some(path) = llm_tape_path {
        copy_tape_file::<TapeEntry>(path, &mut writer, BundleEntry::LlmEntry)?;
    }

    if let Some(path) = tool_tape_path {
        copy_tape_file::<ToolTapeEntry>(path, &mut writer, BundleEntry::ToolEntry)?;
    }

    writer.flush()?;
    Ok(())
}

/// Read a bundle file back into its components.
///
/// Returns [`BundleError::MissingPipelineYaml`] when no `pipeline_yaml`
/// line was found. Corrupt lines surface as
/// [`BundleError::ParseError`] carrying the 1-based line number.
pub fn read_bundle(path: &Path) -> Result<BundleContents, BundleError> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut pipeline_yaml: Option<String> = None;
    let mut llm_entries: Vec<TapeEntry> = Vec::new();
    let mut tool_entries: Vec<ToolTapeEntry> = Vec::new();

    for (idx, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: BundleEntry =
            serde_json::from_str(&line).map_err(|e| BundleError::ParseError {
                line: idx + 1,
                msg: e.to_string(),
            })?;
        match entry {
            BundleEntry::BundleHeader { .. } => {},
            BundleEntry::PipelineYaml { content } => pipeline_yaml = Some(content),
            BundleEntry::LlmEntry(e) => llm_entries.push(e),
            BundleEntry::ToolEntry(e) => tool_entries.push(e),
        }
    }

    let pipeline_yaml = pipeline_yaml.ok_or(BundleError::MissingPipelineYaml)?;

    Ok(BundleContents {
        pipeline_yaml,
        llm_entries,
        tool_entries,
    })
}

/// Serialize one `BundleEntry` and write it as a single NDJSON line.
fn write_entry<W: Write>(writer: &mut W, entry: &BundleEntry) -> Result<(), BundleError> {
    let line = serde_json::to_string(entry).map_err(|e| BundleError::ParseError {
        line: 0,
        msg: e.to_string(),
    })?;
    writeln!(writer, "{line}")?;
    Ok(())
}

/// Read a tape file (`TapeEntry` or `ToolTapeEntry` lines) and re-emit
/// each entry as a `BundleEntry` through the supplied wrapper. Empty
/// lines are skipped so a manually edited tape with trailing newlines
/// still round-trips.
fn copy_tape_file<T>(
    path: &Path,
    writer: &mut BufWriter<File>,
    wrap: impl Fn(T) -> BundleEntry,
) -> Result<(), BundleError>
where
    T: for<'de> Deserialize<'de>,
{
    let file = File::open(path)?;
    for (idx, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed: T = serde_json::from_str(&line).map_err(|e| BundleError::ParseError {
            line: idx + 1,
            msg: format!("tape `{}`: {}", path.display(), e),
        })?;
        write_entry(writer, &wrap(parsed))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmRequest, LlmResponse, Usage};
    use std::io::Write;

    fn sample_tape_entry(prompt: &str) -> TapeEntry {
        TapeEntry {
            req: LlmRequest::from_prompt(
                "openai".into(),
                "gpt-5".into(),
                prompt.into(),
                None,
                None,
                None,
            ),
            res: LlmResponse {
                content: format!("answer to {prompt}"),
                finish_reason: Some("stop".into()),
                usage: Some(Usage {
                    prompt_tokens: 1,
                    completion_tokens: 2,
                    total_tokens: 3,
                }),
                tool_calls: Vec::new(),
            },
        }
    }

    fn sample_tool_entry(key: &str, content: &str) -> ToolTapeEntry {
        ToolTapeEntry {
            key: key.to_string(),
            content: Some(content.to_string()),
            error: None,
        }
    }

    fn write_tape<T: Serialize>(entries: &[T]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for e in entries {
            writeln!(f, "{}", serde_json::to_string(e).unwrap()).unwrap();
        }
        f.flush().unwrap();
        f
    }

    #[test]
    fn round_trip_preserves_pipeline_and_entries() {
        let yaml_body = "version: 1\nnodes:\n  - id: n\n    kind: shell\n    command: true\n";
        let llm = vec![sample_tape_entry("hello"), sample_tape_entry("world")];
        let tools = vec![
            sample_tool_entry("aaaa", "first"),
            sample_tool_entry("bbbb", "second"),
        ];

        let llm_tape = write_tape(&llm);
        let tool_tape = write_tape(&tools);
        let out = tempfile::NamedTempFile::new().unwrap();

        write_bundle(
            yaml_body,
            Some(llm_tape.path()),
            Some(tool_tape.path()),
            out.path(),
        )
        .expect("bundle write must succeed");

        let read = read_bundle(out.path()).expect("bundle read must succeed");
        assert_eq!(read.pipeline_yaml, yaml_body);
        assert_eq!(read.llm_entries.len(), 2);
        assert_eq!(read.llm_entries[0].req.prompt, "hello");
        assert_eq!(read.llm_entries[1].req.prompt, "world");
        assert_eq!(read.tool_entries.len(), 2);
        assert_eq!(read.tool_entries[0].key, "aaaa");
        assert_eq!(read.tool_entries[1].key, "bbbb");
        assert_eq!(read.tool_entries[0].content.as_deref(), Some("first"));
    }

    #[test]
    fn write_with_no_tapes_emits_only_header_and_yaml() {
        let yaml_body = "version: 1\n";
        let out = tempfile::NamedTempFile::new().unwrap();

        write_bundle(yaml_body, None, None, out.path()).unwrap();

        let read = read_bundle(out.path()).unwrap();
        assert_eq!(read.pipeline_yaml, yaml_body);
        assert!(read.llm_entries.is_empty());
        assert!(read.tool_entries.is_empty());
    }

    #[test]
    fn missing_pipeline_yaml_is_rejected() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, r#"{{"type":"bundle_header","format_version":1}}"#).unwrap();
        f.flush().unwrap();

        let err = read_bundle(f.path()).expect_err("must reject bundle without pipeline_yaml");
        assert!(matches!(err, BundleError::MissingPipelineYaml));
    }

    #[test]
    fn corrupt_line_is_rejected_with_line_number() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, r#"{{"type":"bundle_header","format_version":1}}"#).unwrap();
        writeln!(f, "{{not json").unwrap();
        f.flush().unwrap();

        let err = read_bundle(f.path()).expect_err("corrupt line must be rejected");
        match err {
            BundleError::ParseError { line, .. } => assert_eq!(line, 2),
            other => panic!("expected ParseError, got {other:?}"),
        }
    }
}
