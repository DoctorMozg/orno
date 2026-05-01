//! Combined record/replay bundle: pipeline YAML + LLM tape entries +
//! tool tape entries packed into a single NDJSON file.
//!
//! Format: one JSON object per line, internally tagged on `"type"`.
//! ```text
//! {"type":"bundle_header","format_version":1}
//! {"type":"pipeline_yaml","content":"version: 1\n..."}
//! {"type":"llm_entry","req":{...},"outcome":{"kind":"ok","res":{...}}}
//! {"type":"llm_entry","req":{...},"outcome":{"kind":"err","err":{...}}}
//! {"type":"tool_entry","key":"<hex>","content":"...","error":null}
//! ```
//! `LlmEntry` and `ToolEntry` use newtype variants; serde's internal
//! tagging flattens the inner struct's fields alongside `"type"`, so a
//! bundle line carries both the discriminator and the entry payload at
//! the same JSON level — no nested `"data": {...}` wrapper.
//!
//! `read_bundle` is forgiving on unknown future entry types via
//! `#[non_exhaustive]` on the enum, but a missing `pipeline_yaml`
//! section is a hard error: `orno replay` cannot proceed without one,
//! and a `bundle_header` whose `format_version` does not match
//! [`CURRENT_BUNDLE_VERSION`] is also a hard error so a future-version
//! bundle is rejected up front rather than silently misinterpreted.
//!
//! Bearer-token scrub: `write_bundle` walks the parsed pipeline's
//! `mcp_servers` block for HTTP servers with `auth.kind: bearer` and
//! replaces any literal token value with `<REDACTED>` in the embedded
//! YAML. Template values (`{{ secrets.X }}`) are left untouched —
//! they are placeholders, not the secret itself. This is a defense-in-
//! depth measure against bundles being shared as repro artifacts.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use blake3::Hasher as Blake3Hasher;

use serde::{Deserialize, Serialize};

use crate::pipeline::Pipeline;
use crate::pipeline::schema::{McpAuthConfig, McpServerConfig};
use crate::tool::ToolTapeEntry;

use super::recording::TapeEntry;

/// Wire-format version emitted in the `bundle_header` line. Bumped on
/// any incompatible change to the bundle layout. `read_bundle` rejects
/// bundles whose header carries a different value so a stale reader
/// cannot silently misinterpret a newer bundle, and a newer reader
/// does not blindly replay an older bundle whose semantics may have
/// shifted.
pub const CURRENT_BUNDLE_VERSION: u32 = 1;

/// Replacement string substituted for literal bearer tokens during
/// bundle write. Chosen to be visually loud in a diff and to never
/// collide with a real token shape.
const BEARER_REDACTION_PLACEHOLDER: &str = "<REDACTED>";

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
    /// Last line of every bundle. Carries the BLAKE3 hex digest over all
    /// preceding body lines (each written as `{json}\n`), excluding the
    /// trailer line itself. `read_bundle` verifies this digest; a missing
    /// trailer emits a `tracing::warn!` rather than hard-failing so
    /// pre-BS4 bundles still replay.
    BundleTrailer {
        /// Lowercase hex-encoded BLAKE3 digest of all body bytes written
        /// before the trailer.
        blake3: String,
    },
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
    /// Bundle's `bundle_header` carried a `format_version` this build
    /// cannot interpret. Returned only by [`read_bundle`].
    #[error("bundle format version {found} is not supported by this build (expected {expected})")]
    IncompatibleVersion {
        /// `format_version` value read from the bundle header.
        found: u32,
        /// Version this build is compiled against.
        expected: u32,
    },
    /// `pipeline_yaml` body could not be parsed before scrubbing
    /// secrets on write. Returned only by [`write_bundle`]; an unparsable
    /// body must not be embedded verbatim because secrets cannot be
    /// located without a parsed `mcp_servers` block.
    #[error("pipeline YAML parse error during bundle write: {0}")]
    PipelineParse(String),
    /// A bundle line could not be deserialized as a `BundleEntry`.
    #[error("bundle parse error on line {line}: {msg}")]
    ParseError {
        /// 1-based line number in the source bundle file.
        line: usize,
        /// Human-readable parser error message.
        msg: String,
    },
    /// The `bundle_trailer` BLAKE3 digest does not match the digest
    /// recomputed over the body lines. Indicates on-disk corruption or
    /// tampering after `write_bundle` wrote the file.
    #[error("bundle integrity check failed: expected {expected}, got {actual}")]
    IntegrityMismatch {
        /// Digest encoded in the `bundle_trailer` line.
        expected: String,
        /// Digest recomputed from the body lines during `read_bundle`.
        actual: String,
    },
    /// Content was found after the integrity trailer, indicating potential tampering.
    #[error(
        "bundle contains {line_count} line(s) after the integrity trailer — possible tampering"
    )]
    ExtraContentAfterTrailer {
        /// Number of extra lines found.
        line_count: usize,
    },
}

/// A `Write` wrapper that accumulates a BLAKE3 digest over every byte
/// written. Used by `write_bundle` to hash the body lines so the trailer
/// digest covers exactly the same bytes the reader will hash on verify.
struct HashAccumWriter<W: Write> {
    inner: W,
    hasher: Blake3Hasher,
}

impl<W: Write> HashAccumWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Blake3Hasher::new(),
        }
    }

    /// Flush the inner writer, finalize the hash, and return both the
    /// inner writer and the lowercase hex digest so the caller can write
    /// the trailer without going through the hasher.
    fn finalize(mut self) -> std::io::Result<(W, String)> {
        self.inner.flush()?;
        let digest = hex::encode(self.hasher.finalize().as_bytes());
        Ok((self.inner, digest))
    }
}

impl<W: Write> Write for HashAccumWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Write a bundle combining the pipeline YAML body with previously
/// recorded LLM and tool tape files.
///
/// The tape files are read line-by-line and re-emitted as `LlmEntry` /
/// `ToolEntry` lines in the bundle. Pipeline YAML is embedded after
/// passing through [`scrub_pipeline_secrets`]: pipelines with no
/// literal bearer tokens round-trip byte-for-byte (so comments and
/// whitespace survive for human inspection), while pipelines that
/// carry literal credentials are parsed, walked, and re-emitted with
/// `<REDACTED>` substituted in. `orno replay` still sees the YAML the
/// original `orno run` did, modulo the redacted secrets.
///
/// Either tape path may be `None` when no entries of that kind were
/// recorded; the bundle will simply omit those lines.
///
/// A `bundle_trailer` line carrying a BLAKE3 hex digest over all body
/// lines is appended last. `read_bundle` verifies this digest to detect
/// on-disk corruption or tampering.
pub fn write_bundle(
    pipeline_yaml: &str,
    llm_tape_path: Option<&Path>,
    tool_tape_path: Option<&Path>,
    out: &Path,
) -> Result<(), BundleError> {
    let scrubbed_yaml = scrub_pipeline_secrets(pipeline_yaml)?;

    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(out)?;
    // Wrap in HashAccumWriter so every body byte is hashed as it is written.
    let mut writer = HashAccumWriter::new(BufWriter::new(file));

    write_entry(
        &mut writer,
        &BundleEntry::BundleHeader {
            format_version: CURRENT_BUNDLE_VERSION,
        },
    )?;
    write_entry(
        &mut writer,
        &BundleEntry::PipelineYaml {
            content: scrubbed_yaml,
        },
    )?;

    if let Some(path) = llm_tape_path {
        copy_tape_file::<TapeEntry, _>(path, &mut writer, BundleEntry::LlmEntry)?;
    }

    if let Some(path) = tool_tape_path {
        copy_tape_file::<ToolTapeEntry, _>(path, &mut writer, BundleEntry::ToolEntry)?;
    }

    // Flush, extract body digest, then write the trailer directly to the
    // inner BufWriter — bypassing the hasher so the trailer is not
    // self-referential (the reader hashes all non-trailer lines and then
    // compares against the digest in the trailer).
    let (mut inner, blake3_hex) = writer.finalize().map_err(BundleError::Io)?;
    let trailer = serde_json::to_string(&BundleEntry::BundleTrailer { blake3: blake3_hex })
        .map_err(|e| BundleError::ParseError {
            line: 0,
            msg: e.to_string(),
        })?;
    writeln!(inner, "{trailer}")?;
    inner.flush()?;
    Ok(())
}

/// Replace literal MCP bearer tokens in `pipeline_yaml` with
/// `<REDACTED>` so a shared bundle does not leak production credentials.
///
/// The pipeline is parsed first to discover whether any HTTP server is
/// configured with a literal (non-templated) bearer token. When the
/// answer is no, the input is returned verbatim so comments and
/// whitespace round-trip byte-for-byte for human inspection of the
/// bundle. When the answer is yes, the YAML body is parsed into a
/// generic `serde_yaml_ng::Value` tree and walked structurally:
/// string leaves under a credential-shaped key are replaced with the
/// redaction placeholder, then the tree is re-serialized. This avoids
/// the failure modes of a naive `str::replace` loop — which could
/// corrupt unrelated text that happened to share a token's bytes — at
/// the cost of dropping comments and re-normalizing whitespace on the
/// scrub-required path.
///
/// Template values (`{{ secrets.X }}`) are detected at every level and
/// preserved verbatim, because they are placeholders the runtime
/// renderer resolves, not the secret itself. If the structural walk
/// fails (parse error on an otherwise-loadable pipeline body), the
/// scrubber falls back to a literal-token Aho-Corasick replacement so
/// the bundle is still emitted with the credentials redacted.
pub fn scrub_pipeline_secrets(pipeline_yaml: &str) -> Result<String, BundleError> {
    let pipeline: Pipeline = serde_yaml_ng::from_str(pipeline_yaml)
        .map_err(|e| BundleError::PipelineParse(e.to_string()))?;

    let mut tokens: Vec<String> = Vec::new();
    for server in pipeline.mcp_servers.values() {
        if let McpServerConfig::Http(http) = server
            && let Some(McpAuthConfig::Bearer { token }) = &http.auth
        {
            // Template placeholders are not literal secrets — skip
            // them so the rendered YAML still resolves at replay time.
            if token.contains("{{") || token.is_empty() {
                continue;
            }
            tokens.push(token.clone());
        }
    }

    if tokens.is_empty() {
        return Ok(pipeline_yaml.to_string());
    }

    match serde_yaml_ng::from_str::<serde_yaml_ng::Value>(pipeline_yaml) {
        Ok(mut value) => {
            scrub_yaml_value(&mut value);
            serde_yaml_ng::to_string(&value).map_err(|e| BundleError::PipelineParse(e.to_string()))
        },
        Err(parse_err) => {
            // The pipeline parsed as `Pipeline` above, so a generic
            // `Value` parse should not normally fail. If it does, fall
            // back to the literal-token path so the bundle still ships
            // with the credentials redacted — the alternative would be
            // emitting plaintext tokens.
            tracing::warn!(
                error = %parse_err,
                "bundle scrub: generic YAML parse failed; falling back to literal-token replacement",
            );
            Ok(scrub_yaml_with_tokens(pipeline_yaml, &tokens))
        },
    }
}

/// Recursively walk a YAML `Value` tree, replacing string leaves whose
/// keys look like credentials with [`BEARER_REDACTION_PLACEHOLDER`].
/// Template strings (`{{ ... }}`) are left intact at every level so
/// replay can still resolve placeholders against the live secret map.
fn scrub_yaml_value(value: &mut serde_yaml_ng::Value) {
    match value {
        serde_yaml_ng::Value::Mapping(map) => {
            for (key, val) in &mut *map {
                let sensitive = match key {
                    serde_yaml_ng::Value::String(s) => is_sensitive_key(s),
                    _ => false,
                };
                let is_literal_string = sensitive
                    && matches!(val, serde_yaml_ng::Value::String(t) if !t.contains("{{"));
                if is_literal_string {
                    *val = serde_yaml_ng::Value::String(BEARER_REDACTION_PLACEHOLDER.to_string());
                    continue;
                }
                // Recurse on non-leaf values, even when the key is
                // sensitive (e.g. `auth: { token: ... }` — the
                // sensitive content lives one level deeper than `auth`).
                scrub_yaml_value(val);
            }
        },
        serde_yaml_ng::Value::Sequence(seq) => {
            for item in seq.iter_mut() {
                scrub_yaml_value(item);
            }
        },
        _ => {},
    }
}

/// Lower-cases `key` and returns `true` when it carries one of the
/// well-known credential-shaped keywords. Substring match keeps
/// compound names like `api_key` or `private_key` covered.
fn is_sensitive_key(key: &str) -> bool {
    const SENSITIVE_KEYWORDS: &[&str] = &[
        "token",
        "secret",
        "password",
        "api_key",
        "access_key",
        "private_key",
        "credential",
    ];
    let lower = key.to_lowercase();
    SENSITIVE_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

/// Fallback path used only when the structural walk could not parse
/// the YAML body. Replaces literal token bytes with the redaction
/// placeholder via a single Aho-Corasick scan so the multi-pattern
/// match runs in one pass over the haystack.
fn scrub_yaml_with_tokens(pipeline_yaml: &str, tokens: &[String]) -> String {
    use aho_corasick::{AhoCorasick, MatchKind};
    if tokens.is_empty() {
        return pipeline_yaml.to_string();
    }
    let Ok(ac) = AhoCorasick::builder()
        .match_kind(MatchKind::LeftmostLongest)
        .build(tokens)
    else {
        // Should not happen on a non-empty `&[String]` of valid UTF-8;
        // returning the input unchanged is preferable to panicking
        // inside an already-degraded fallback path.
        return pipeline_yaml.to_string();
    };
    let mut out = String::with_capacity(pipeline_yaml.len());
    let mut last = 0;
    for mat in ac.find_iter(pipeline_yaml) {
        out.push_str(&pipeline_yaml[last..mat.start()]);
        out.push_str(BEARER_REDACTION_PLACEHOLDER);
        last = mat.end();
    }
    out.push_str(&pipeline_yaml[last..]);
    out
}

/// Read a bundle file back into its components.
///
/// Returns [`BundleError::MissingPipelineYaml`] when no `pipeline_yaml`
/// line was found, [`BundleError::IncompatibleVersion`] when the
/// `bundle_header` carries a `format_version` other than
/// [`CURRENT_BUNDLE_VERSION`], [`BundleError::ParseError`] for corrupt
/// lines (carrying the 1-based line number), and
/// [`BundleError::IntegrityMismatch`] when a `bundle_trailer` is present
/// but its BLAKE3 digest does not match the digest recomputed over the
/// body lines. A missing trailer emits a `tracing::warn!` but is not an
/// error so pre-BS4 bundles still replay.
#[expect(
    clippy::too_many_lines,
    reason = "sequential parse-hash-dispatch loop with integrity verification and post-trailer detection"
)]
pub fn read_bundle(path: &Path) -> Result<BundleContents, BundleError> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut pipeline_yaml: Option<String> = None;
    let mut llm_entries: Vec<TapeEntry> = Vec::new();
    let mut tool_entries: Vec<ToolTapeEntry> = Vec::new();
    let mut hasher = Blake3Hasher::new();
    let mut trailer_seen = false;
    let mut extra_after_trailer: usize = 0;

    for (idx, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // Guard against post-trailer tampering: any non-empty line that
        // arrives after the integrity trailer is outside the hashed body
        // and must not be parsed or hashed. Counting (rather than breaking)
        // surfaces the violation as a typed error instead of silently
        // dropping the bytes, and placing the check before the JSON parse
        // ensures garbage payloads do not masquerade as `ParseError`.
        if trailer_seen {
            extra_after_trailer += 1;
            continue;
        }
        let entry: BundleEntry =
            serde_json::from_str(&line).map_err(|e| BundleError::ParseError {
                line: idx + 1,
                msg: e.to_string(),
            })?;

        // Hash all body lines before dispatching. The trailer line
        // itself is excluded from the hash so the digest is not
        // self-referential — writers skip the trailer through the
        // same logic (writing it directly to the inner writer, not
        // through the HashAccumWriter).
        if !matches!(entry, BundleEntry::BundleTrailer { .. }) {
            hasher.update(line.as_bytes());
            hasher.update(b"\n");
        }

        match entry {
            BundleEntry::BundleHeader { format_version } => {
                // Reject mismatched versions up front so a future bundle
                // is never fed into an older reader that would silently
                // ignore new fields, and an older bundle is never
                // replayed under semantics it was not recorded against.
                if format_version != CURRENT_BUNDLE_VERSION {
                    return Err(BundleError::IncompatibleVersion {
                        found: format_version,
                        expected: CURRENT_BUNDLE_VERSION,
                    });
                }
            },
            BundleEntry::PipelineYaml { content } => pipeline_yaml = Some(content),
            BundleEntry::LlmEntry(e) => llm_entries.push(e),
            BundleEntry::ToolEntry(e) => tool_entries.push(e),
            BundleEntry::BundleTrailer { blake3 } => {
                let actual = hex::encode(hasher.finalize().as_bytes());
                if actual != blake3 {
                    return Err(BundleError::IntegrityMismatch {
                        expected: blake3,
                        actual,
                    });
                }
                trailer_seen = true;
            },
        }
    }

    if extra_after_trailer > 0 {
        return Err(BundleError::ExtraContentAfterTrailer {
            line_count: extra_after_trailer,
        });
    }

    if !trailer_seen {
        tracing::warn!(
            path = %path.display(),
            "bundle has no integrity trailer; skipping verification \
             (bundle may have been written before BS4 or was truncated)",
        );
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
fn copy_tape_file<T, W: Write>(
    path: &Path,
    writer: &mut W,
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
    use crate::llm::recording::TapeOutcome;
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
            outcome: TapeOutcome::Ok {
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
        let yaml_body = "version: 1\nnodes:\n  - id: n\n    kind: shell\n    command: \"true\"\n";
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
        // Minimal but parseable: `nodes:` is required by the Pipeline
        // struct, and `scrub_pipeline_secrets` parses the YAML as part
        // of locating bearer tokens to redact.
        let yaml_body = "version: 1\nnodes:\n  - id: n\n    kind: shell\n    command: \"true\"\n";
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
        writeln!(
            f,
            r#"{{"type":"bundle_header","format_version":{CURRENT_BUNDLE_VERSION}}}"#,
        )
        .unwrap();
        f.flush().unwrap();

        let err = read_bundle(f.path()).expect_err("must reject bundle without pipeline_yaml");
        assert!(matches!(err, BundleError::MissingPipelineYaml));
    }

    #[test]
    fn corrupt_line_is_rejected_with_line_number() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            f,
            r#"{{"type":"bundle_header","format_version":{CURRENT_BUNDLE_VERSION}}}"#,
        )
        .unwrap();
        writeln!(f, "{{not json").unwrap();
        f.flush().unwrap();

        let err = read_bundle(f.path()).expect_err("corrupt line must be rejected");
        match err {
            BundleError::ParseError { line, .. } => assert_eq!(line, 2),
            other => panic!("expected ParseError, got {other:?}"),
        }
    }

    #[test]
    fn future_format_version_is_rejected() {
        // Defense against silent misinterpretation: a bundle written by
        // a future build with new entry types must not be replayed by an
        // older reader that would skip them as `BundleHeader { .. }`.
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, r#"{{"type":"bundle_header","format_version":999}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"pipeline_yaml","content":"version: 1\nnodes: []\n"}}"#
        )
        .unwrap();
        f.flush().unwrap();

        let err = read_bundle(f.path()).expect_err("must reject incompatible version");
        match err {
            BundleError::IncompatibleVersion { found, expected } => {
                assert_eq!(found, 999);
                assert_eq!(expected, CURRENT_BUNDLE_VERSION);
            },
            other => panic!("expected IncompatibleVersion, got {other:?}"),
        }
    }

    #[test]
    fn header_uses_current_bundle_version_constant() {
        // Pin the wire-format version emitted by `write_bundle` to the
        // public constant so an unintentional bump is caught here
        // before it escapes a release.
        let yaml_body = "version: 1\nnodes:\n  - id: n\n    kind: shell\n    command: \"true\"\n";
        let out = tempfile::NamedTempFile::new().unwrap();
        write_bundle(yaml_body, None, None, out.path()).unwrap();

        let raw = std::fs::read_to_string(out.path()).unwrap();
        let header_line = raw.lines().next().expect("bundle has at least one line");
        assert!(
            header_line.contains(&format!(r#""format_version":{CURRENT_BUNDLE_VERSION}"#)),
            "header line `{header_line}` must encode CURRENT_BUNDLE_VERSION = {CURRENT_BUNDLE_VERSION}",
        );
    }

    #[test]
    fn bearer_token_is_scrubbed_in_bundle() {
        // Real-world threat: a debug bundle gets shared in a bug
        // report, leaking the bearer token verbatim. Scrub on write
        // must replace the literal value with the placeholder.
        let yaml_body = "\
version: 1
mcp_servers:
  prod:
    transport: http
    url: https://mcp.example.com
    auth:
      kind: bearer
      token: sk-very-secret-bearer-abc123
nodes:
  - id: n
    kind: shell
    command: \"true\"
";
        let out = tempfile::NamedTempFile::new().unwrap();
        write_bundle(yaml_body, None, None, out.path()).expect("bundle write");

        let read = read_bundle(out.path()).expect("bundle read");
        assert!(
            !read.pipeline_yaml.contains("sk-very-secret-bearer-abc123"),
            "literal bearer token must not survive bundle write: {}",
            read.pipeline_yaml,
        );
        assert!(
            read.pipeline_yaml.contains(BEARER_REDACTION_PLACEHOLDER),
            "bundle YAML must contain the redaction placeholder: {}",
            read.pipeline_yaml,
        );
    }

    #[test]
    fn templated_bearer_token_is_left_intact() {
        // Templates are placeholders — `secrets.X` resolves at run /
        // replay time. Scrubbing them would break replay because the
        // template would no longer exist for the renderer to fill in.
        let yaml_body = "\
version: 1
secrets:
  - PROD_TOKEN
mcp_servers:
  prod:
    transport: http
    url: https://mcp.example.com
    auth:
      kind: bearer
      token: '{{ secrets.PROD_TOKEN }}'
nodes:
  - id: n
    kind: shell
    command: \"true\"
";
        let out = tempfile::NamedTempFile::new().unwrap();
        write_bundle(yaml_body, None, None, out.path()).expect("bundle write");

        let read = read_bundle(out.path()).expect("bundle read");
        assert!(
            read.pipeline_yaml.contains("{{ secrets.PROD_TOKEN }}"),
            "template-form token must be preserved verbatim: {}",
            read.pipeline_yaml,
        );
        assert!(
            !read.pipeline_yaml.contains(BEARER_REDACTION_PLACEHOLDER),
            "no redaction placeholder should appear when token was templated: {}",
            read.pipeline_yaml,
        );
    }

    #[test]
    fn pipeline_with_no_bearer_auth_is_not_modified() {
        // Common-case no-op: a pipeline with no MCP HTTP bearer auth
        // must round-trip the YAML byte-for-byte so comments and
        // whitespace survive for human inspection of the bundle.
        let yaml_body = "\
# top-of-file comment
version: 1
nodes:
  - id: n
    kind: shell
    command: \"true\"   # trailing comment
";
        let out = tempfile::NamedTempFile::new().unwrap();
        write_bundle(yaml_body, None, None, out.path()).unwrap();

        let read = read_bundle(out.path()).unwrap();
        assert_eq!(
            read.pipeline_yaml, yaml_body,
            "no-bearer pipeline must be embedded verbatim",
        );
    }

    #[test]
    fn bundle_round_trip_integrity_verified() {
        // BS4 contract: a pristine bundle written by `write_bundle` must
        // round-trip through `read_bundle` without tripping the BLAKE3
        // integrity check. Sanity: the pipeline YAML survives intact and
        // both tape vectors come back populated, proving the trailer's
        // digest covers the body lines but does not reject them.
        let yaml_body = "version: 1\nnodes:\n  - id: n\n    kind: shell\n    command: \"true\"\n";
        let llm = vec![sample_tape_entry("alpha"), sample_tape_entry("beta")];
        let tools = vec![sample_tool_entry("aaaa", "out-a")];

        let llm_tape = write_tape(&llm);
        let tool_tape = write_tape(&tools);
        let out = tempfile::NamedTempFile::new().unwrap();

        write_bundle(
            yaml_body,
            Some(llm_tape.path()),
            Some(tool_tape.path()),
            out.path(),
        )
        .expect("bundle write");

        let read = read_bundle(out.path()).expect("bundle read must verify integrity");
        assert_eq!(read.pipeline_yaml, yaml_body);
        assert_eq!(read.llm_entries.len(), 2);
        assert_eq!(read.tool_entries.len(), 1);
    }

    #[test]
    fn bundle_tampered_body_is_rejected() {
        // BS4 anti-tamper guarantee: flipping a single byte inside a body
        // line must surface as `BundleError::IntegrityMismatch` so a
        // shared bundle cannot be silently mutated between record and
        // replay. Mutate the `pipeline_yaml` line so the tampered bundle
        // remains structurally valid JSON-per-line and the reader still
        // recomputes the BLAKE3 hash to compare against the trailer.
        let yaml_body = "version: 1\nnodes:\n  - id: n\n    kind: shell\n    command: \"true\"\n";
        let out = tempfile::NamedTempFile::new().unwrap();
        write_bundle(yaml_body, None, None, out.path()).expect("bundle write");

        let raw = std::fs::read_to_string(out.path()).expect("read raw bundle");
        let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();

        // Find and corrupt the pipeline_yaml line. Replacing a substring
        // that is guaranteed to be present keeps the JSON parseable
        // (so the corruption surfaces as IntegrityMismatch and not as
        // ParseError).
        let yaml_idx = lines
            .iter()
            .position(|l| l.contains(r#""type":"pipeline_yaml""#))
            .expect("pipeline_yaml line must be present");
        lines[yaml_idx] = lines[yaml_idx].replace("version: 1", "version: 2");

        let tampered = lines.join("\n") + "\n";
        std::fs::write(out.path(), tampered).expect("rewrite tampered bundle");

        let err = read_bundle(out.path()).expect_err("tampered bundle must be rejected");
        match err {
            BundleError::IntegrityMismatch { expected, actual } => {
                assert_ne!(
                    expected, actual,
                    "expected vs actual digests must differ on a tampered body",
                );
            },
            other => panic!("expected IntegrityMismatch, got {other:?}"),
        }
    }

    #[test]
    fn bundle_missing_trailer_warns_but_succeeds() {
        // BS4 forward-compat path: pre-BS4 bundles (and bundles truncated
        // mid-write) have no `bundle_trailer` line. The reader logs a
        // `tracing::warn!` and returns `Ok` rather than rejecting, so old
        // bundles still replay. Strip the last line — which is always the
        // trailer for a freshly written bundle — and assert read succeeds.
        let yaml_body = "version: 1\nnodes:\n  - id: n\n    kind: shell\n    command: \"true\"\n";
        let out = tempfile::NamedTempFile::new().unwrap();
        write_bundle(yaml_body, None, None, out.path()).expect("bundle write");

        let raw = std::fs::read_to_string(out.path()).expect("read raw bundle");
        let mut lines: Vec<&str> = raw.lines().collect();
        let last = lines.pop().expect("bundle has at least one line");
        assert!(
            last.contains(r#""type":"bundle_trailer""#),
            "last line of a fresh bundle must be the trailer: {last}",
        );

        let truncated = lines.join("\n") + "\n";
        std::fs::write(out.path(), truncated).expect("rewrite truncated bundle");

        let read = read_bundle(out.path())
            .expect("missing trailer must warn rather than error so pre-BS4 bundles still replay");
        assert_eq!(read.pipeline_yaml, yaml_body);
    }

    #[test]
    fn read_bundle_rejects_post_trailer_content() {
        // Anti-tamper guarantee: lines appearing after a valid integrity
        // trailer are outside the hashed body and must not be silently
        // dropped. An attacker who appends a structurally valid NDJSON
        // entry must surface as `ExtraContentAfterTrailer`, not as the
        // misleading downstream errors that arise from dropping the bytes.
        let yaml_body = "version: 1\nnodes:\n  - id: n\n    kind: shell\n    command: \"true\"\n";
        let llm = vec![sample_tape_entry("alpha")];
        let llm_tape = write_tape(&llm);
        let out = tempfile::NamedTempFile::new().unwrap();
        write_bundle(yaml_body, Some(llm_tape.path()), None, out.path()).expect("bundle write");

        // Append a structurally valid bundle line after the trailer. The
        // appended bytes are not covered by the trailer's BLAKE3 digest,
        // so a permissive reader would silently process them.
        let appended = sample_tape_entry("appended");
        let extra_line =
            serde_json::to_string(&BundleEntry::LlmEntry(appended)).expect("serialize entry");
        let mut existing = std::fs::read_to_string(out.path()).expect("read raw bundle");
        existing.push_str(&extra_line);
        existing.push('\n');
        std::fs::write(out.path(), existing).expect("rewrite bundle with post-trailer line");

        let err = read_bundle(out.path()).expect_err("post-trailer content must be rejected");
        match err {
            BundleError::ExtraContentAfterTrailer { line_count } => {
                assert_eq!(line_count, 1, "exactly one line was appended");
            },
            other => panic!("expected ExtraContentAfterTrailer, got {other:?}"),
        }
    }

    #[test]
    fn read_bundle_rejects_garbage_after_trailer() {
        // The post-trailer guard must fire before the JSON parse so
        // garbage bytes after the trailer surface as
        // `ExtraContentAfterTrailer`, not as `ParseError`. Without this
        // ordering, an attacker appending non-JSON would get a misleading
        // parse error message that hides the integrity violation.
        let yaml_body = "version: 1\nnodes:\n  - id: n\n    kind: shell\n    command: \"true\"\n";
        let out = tempfile::NamedTempFile::new().unwrap();
        write_bundle(yaml_body, None, None, out.path()).expect("bundle write");

        let mut existing = std::fs::read_to_string(out.path()).expect("read raw bundle");
        existing.push_str("not valid json !!!!\n");
        std::fs::write(out.path(), existing).expect("rewrite bundle with garbage trailer line");

        let err = read_bundle(out.path()).expect_err("garbage after trailer must be rejected");
        match err {
            BundleError::ExtraContentAfterTrailer { line_count } => {
                assert_eq!(line_count, 1);
            },
            other => panic!(
                "expected ExtraContentAfterTrailer (guard must fire before JSON parse), got {other:?}",
            ),
        }
    }
}
