//! Combined record/replay bundle: pipeline YAML + run inputs + LLM tape
//! entries + tool tape entries packed into a single NDJSON file.
//!
//! Format: one JSON object per line, internally tagged on `"type"`.
//! ```text
//! {"type":"bundle_header","format_version":2}
//! {"type":"pipeline_yaml","content":"version: 1\n..."}
//! {"type":"run_inputs","vars":{...},"env":{...},"secrets":{"KEY":"***"}}
//! {"type":"llm_entry","key":"<provider:model:hash>","seq":0,"outcome":{"kind":"ok","res":{...}}}
//! {"type":"tool_entry","key":"<hex>","seq":0,"outcome":{"kind":"ok","content":"..."}}
//! {"type":"bundle_trailer","blake3":"<hex>"}
//! ```
//! `LlmEntry` and `ToolEntry` use newtype variants; serde's internal
//! tagging flattens the inner struct's fields alongside `"type"`, so a
//! bundle line carries both the discriminator and the entry payload at
//! the same JSON level — no nested `"data": {...}` wrapper.
//!
//! `read_bundle` is strict: the header comes first and its
//! `format_version` must equal [`CURRENT_BUNDLE_VERSION`] (older and
//! newer bundles are refused, naming both versions), the trailer comes
//! last and its digest must match, and `pipeline_yaml` and `run_inputs`
//! each appear exactly once. A truncated or hand-edited bundle is
//! therefore rejected rather than replayed partially.
//!
//! `run_inputs` holds what templates saw at record time: the rendered
//! `vars`, the `env` namespace, and the `secrets` namespace with every
//! value replaced by the redaction placeholder, so replay renders the
//! same prompts without the bundle containing a real secret.
//!
//! Bearer-token scrub: `write_bundle` walks the parsed pipeline's
//! `mcp_servers` block for HTTP servers with `auth.kind: bearer` and
//! replaces any literal token value with `<REDACTED>` in the embedded
//! YAML. Template values (`{{ secrets.X }}`) are left untouched —
//! they are placeholders, not the secret itself. This is a defense-in-
//! depth measure against bundles being shared as repro artifacts.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use blake3::Hasher as Blake3Hasher;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::events::Redactor;
use crate::events::redactor::REDACTION_MARKER;
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
pub const CURRENT_BUNDLE_VERSION: u32 = 2;

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
        /// Bundle wire-format version. Currently `2`.
        format_version: u32,
    },
    /// Verbatim pipeline YAML the original run was driven from. Embedded
    /// rather than referenced by path so bundles are self-contained.
    PipelineYaml {
        /// Pipeline YAML body, exactly as the original `orno run` saw it.
        content: String,
    },
    /// What templates saw at record time, with secrets replaced by a placeholder.
    RunInputs(BundleRunInputs),
    /// One recorded LLM call from the run.
    LlmEntry(TapeEntry),
    /// One recorded tool invocation from the run.
    ToolEntry(ToolTapeEntry),
    /// Last line of every bundle. Carries the BLAKE3 hex digest over all
    /// preceding body lines (each written as `{json}\n`), excluding the
    /// trailer line itself. `read_bundle` verifies this digest and
    /// rejects a bundle without a trailer.
    BundleTrailer {
        /// Lowercase hex-encoded BLAKE3 digest of all body bytes written
        /// before the trailer.
        blake3: String,
    },
}

/// The template namespaces of a recorded run, as `orno replay` needs to
/// rebuild them. Holds no real secret value.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BundleRunInputs {
    /// Pipeline `vars:` after rendering, redacted. Replay uses these as
    /// they are instead of rendering again.
    pub vars: BTreeMap<String, Value>,
    /// The `env.*` namespace, redacted.
    pub env: BTreeMap<String, String>,
    /// The `secrets.*` namespace: every key kept, every value replaced
    /// by the redaction placeholder so templates still render.
    pub secrets: BTreeMap<String, String>,
}

impl BundleRunInputs {
    /// Snapshot the namespaces of a run, scrubbing `vars` and `env`
    /// with `redactor` and masking every secret value.
    #[must_use]
    pub fn capture(
        vars: &BTreeMap<String, Value>,
        env: &BTreeMap<String, String>,
        secrets: &BTreeMap<String, String>,
        redactor: &Redactor,
    ) -> Self {
        let vars = vars
            .iter()
            .map(|(k, v)| {
                let mut v = v.clone();
                redactor.redact_json(&mut v);
                (k.clone(), v)
            })
            .collect();
        let env = env
            .iter()
            .map(|(k, v)| (k.clone(), redactor.redact(v).into_owned()))
            .collect();
        let secrets = secrets
            .keys()
            .map(|k| (k.clone(), REDACTION_MARKER.to_string()))
            .collect();
        Self { vars, env, secrets }
    }
}

/// Everything `write_bundle` packs into a bundle.
#[derive(Debug, Clone, Copy)]
pub struct BundleSources<'a> {
    /// Pipeline YAML exactly as the run loaded it.
    pub pipeline_yaml: &'a str,
    /// Template namespaces the run rendered against.
    pub run_inputs: &'a BundleRunInputs,
    /// LLM tape written by `RecordingTransport`, if any.
    pub llm_tape: Option<&'a Path>,
    /// Tool tape written by `RecordingToolHandler`, if any.
    pub tool_tape: Option<&'a Path>,
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
    /// Template namespaces captured at record time.
    pub run_inputs: BundleRunInputs,
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
    /// A required section (`pipeline_yaml` or `run_inputs`) is absent.
    /// Replay cannot reconstruct the run without it.
    #[error("bundle is missing its `{section}` section")]
    MissingSection {
        /// Wire name of the absent section.
        section: &'static str,
    },
    /// A section that must appear once appeared again.
    #[error("bundle line {line}: duplicate `{section}` section")]
    DuplicateSection {
        /// Wire name of the repeated section.
        section: &'static str,
        /// 1-based line number of the second occurrence.
        line: usize,
    },
    /// The first line of the bundle is not a `bundle_header`.
    #[error("bundle does not start with a `bundle_header` line")]
    MissingHeader,
    /// The last line of the bundle is not a `bundle_trailer`: the file
    /// was truncated or never finished writing.
    #[error("bundle has no `bundle_trailer` line (truncated or unfinished write?)")]
    MissingTrailer,
    /// Bundle's `bundle_header` carried a `format_version` this build
    /// cannot interpret. Returned only by [`read_bundle`].
    #[error(
        "bundle format version {found} is not supported by this build (expected {expected}); \
         re-record the run with this build"
    )]
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
        let digest = self.hasher.finalize().to_hex().to_string();
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

/// Write a bundle combining the pipeline YAML body and run inputs with
/// previously recorded LLM and tool tape files.
///
/// The tape files are read line-by-line and re-emitted as `LlmEntry` /
/// `ToolEntry` lines in the bundle. Pipeline YAML is embedded after
/// passing through [`scrub_pipeline_secrets`]: pipelines with no
/// literal bearer tokens round-trip byte-for-byte (so comments and
/// whitespace survive for human inspection), while pipelines that
/// carry literal credentials are parsed, walked, and re-emitted with
/// `<REDACTED>` substituted in.
///
/// Either tape path may be `None` when no entries of that kind were
/// recorded; the bundle will simply omit those lines. A
/// `bundle_trailer` line carrying a BLAKE3 hex digest over all body
/// lines is appended last.
///
/// The bundle is written to a temporary file in the target directory,
/// synced, and renamed into place, so a crash or error never leaves a
/// partial bundle at `out`.
pub fn write_bundle(sources: &BundleSources<'_>, out: &Path) -> Result<(), BundleError> {
    let scrubbed_yaml = scrub_pipeline_secrets(sources.pipeline_yaml)?;

    let dir = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temp = tempfile::Builder::new()
        .prefix(".orno-bundle-")
        .suffix(".tmp")
        .tempfile_in(dir)?;

    // Wrap in HashAccumWriter so every body byte is hashed as it is written.
    let mut writer = HashAccumWriter::new(BufWriter::new(temp.as_file()));
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
    write_entry(
        &mut writer,
        &BundleEntry::RunInputs(sources.run_inputs.clone()),
    )?;
    if let Some(path) = sources.llm_tape {
        copy_tape_file::<TapeEntry, _>(path, &mut writer, BundleEntry::LlmEntry)?;
    }
    if let Some(path) = sources.tool_tape {
        copy_tape_file::<ToolTapeEntry, _>(path, &mut writer, BundleEntry::ToolEntry)?;
    }

    // Write the trailer directly to the inner writer — bypassing the
    // hasher so the trailer is not self-referential (the reader hashes
    // all non-trailer lines and compares against the trailer's digest).
    let (mut inner, blake3_hex) = writer.finalize().map_err(BundleError::Io)?;
    let trailer = serde_json::to_string(&BundleEntry::BundleTrailer { blake3: blake3_hex })
        .map_err(|e| BundleError::ParseError {
            line: 0,
            msg: e.to_string(),
        })?;
    writeln!(inner, "{trailer}")?;
    inner.flush()?;
    drop(inner);

    temp.as_file().sync_all()?;
    temp.persist(out).map_err(|e| BundleError::Io(e.error))?;
    Ok(())
}

/// Replace literal MCP credentials in `pipeline_yaml` with `<REDACTED>`
/// so a shared bundle does not leak production secrets.
///
/// The YAML is always parsed into a generic tree and walked
/// structurally. When something was scrubbed the tree is re-serialized
/// (comments and whitespace are lost); otherwise the input comes back
/// verbatim. String
/// leaves under a credential-shaped key are replaced, and so is every
/// literal value under an MCP server's `env`, every `headers` value and
/// the `token` / `password` of its `auth` block. Template values
/// (`{{ secrets.X }}`) are kept verbatim because the runtime resolves
/// them; they are placeholders, not the secret. If the generic parse
/// fails on an otherwise-loadable pipeline, the literal credentials are
/// replaced by an Aho-Corasick scan of the raw text instead.
pub fn scrub_pipeline_secrets(pipeline_yaml: &str) -> Result<String, BundleError> {
    let pipeline: Pipeline = serde_yaml_ng::from_str(pipeline_yaml)
        .map_err(|e| BundleError::PipelineParse(e.to_string()))?;

    match serde_yaml_ng::from_str::<serde_yaml_ng::Value>(pipeline_yaml) {
        Ok(mut value) => {
            let original = value.clone();
            scrub_yaml_value(&mut value);
            scrub_mcp_servers(&mut value);
            // Untouched input keeps its comments and formatting.
            if value == original {
                return Ok(pipeline_yaml.to_string());
            }
            serde_yaml_ng::to_string(&value).map_err(|e| BundleError::PipelineParse(e.to_string()))
        },
        Err(parse_err) => {
            // Shipping plaintext credentials is worse than a degraded scrub.
            tracing::warn!(
                error = %parse_err,
                "bundle scrub: generic YAML parse failed; falling back to literal replacement",
            );
            Ok(scrub_yaml_with_tokens(
                pipeline_yaml,
                &literal_mcp_credentials(&pipeline),
            ))
        },
    }
}

fn is_literal(value: &str) -> bool {
    !value.is_empty() && !value.contains("{{")
}

fn literal_mcp_credentials(pipeline: &Pipeline) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for server in pipeline.mcp_servers.values() {
        match server {
            McpServerConfig::Stdio(stdio) => out.extend(stdio.env.values().cloned()),
            McpServerConfig::Http(http) => {
                out.extend(http.headers.values().cloned());
                match &http.auth {
                    Some(McpAuthConfig::Bearer { token }) => out.push(token.clone()),
                    Some(McpAuthConfig::Basic { password, .. }) => out.push(password.clone()),
                    _ => {},
                }
            },
        }
    }
    out.retain(|v| is_literal(v));
    out
}

/// Replace every literal string under MCP `env`, `headers` and the
/// credential fields of `auth`, whatever the key is called.
fn scrub_mcp_servers(root: &mut serde_yaml_ng::Value) {
    use serde_yaml_ng::Value;
    let Some(servers) = root.get_mut("mcp_servers").and_then(Value::as_mapping_mut) else {
        return;
    };
    let scrub_leaf = |leaf: &mut Value| {
        if matches!(leaf, Value::String(t) if is_literal(t)) {
            *leaf = Value::String(BEARER_REDACTION_PLACEHOLDER.to_string());
        }
    };
    for (_, server) in servers.iter_mut() {
        for section in ["env", "headers"] {
            if let Some(map) = server.get_mut(section).and_then(Value::as_mapping_mut) {
                map.iter_mut().for_each(|(_, v)| scrub_leaf(v));
            }
        }
        if let Some(auth) = server.get_mut("auth").and_then(Value::as_mapping_mut) {
            for field in ["token", "password"] {
                if let Some(v) = auth.get_mut(field) {
                    scrub_leaf(v);
                }
            }
        }
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
/// Fails with [`BundleError::MissingHeader`] unless the first line is a
/// `bundle_header`, [`BundleError::IncompatibleVersion`] when its
/// `format_version` is not [`CURRENT_BUNDLE_VERSION`] (checked before any
/// other line is parsed, so an old-format bundle gets this error rather
/// than a parse error), [`BundleError::MissingTrailer`] unless the last
/// line is a `bundle_trailer`, [`BundleError::IntegrityMismatch`] when
/// the trailer digest does not match the body,
/// [`BundleError::DuplicateSection`] / [`BundleError::MissingSection`]
/// unless `pipeline_yaml` and `run_inputs` each appear exactly once, and
/// [`BundleError::ParseError`] for corrupt lines (with the 1-based line
/// number).
pub fn read_bundle(path: &Path) -> Result<BundleContents, BundleError> {
    let mut parsed = ParsedBundle::default();
    let mut extra_after_trailer: usize = 0;

    for (idx, line) in BufReader::new(File::open(path)?).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // Anything after the trailer is outside the hashed body. The
        // check precedes the JSON parse so garbage payloads surface as
        // tampering, not as a misleading `ParseError`.
        if parsed.trailer_seen {
            extra_after_trailer += 1;
            continue;
        }
        let entry: BundleEntry =
            serde_json::from_str(&line).map_err(|e| BundleError::ParseError {
                line: idx + 1,
                msg: e.to_string(),
            })?;
        parsed.accept(entry, &line, idx + 1)?;
    }

    if extra_after_trailer > 0 {
        return Err(BundleError::ExtraContentAfterTrailer {
            line_count: extra_after_trailer,
        });
    }
    parsed.finish()
}

/// Accumulates bundle lines while enforcing the section rules.
#[derive(Default)]
struct ParsedBundle {
    header_seen: bool,
    trailer_seen: bool,
    pipeline_yaml: Option<String>,
    run_inputs: Option<BundleRunInputs>,
    llm_entries: Vec<TapeEntry>,
    tool_entries: Vec<ToolTapeEntry>,
    hasher: Blake3Hasher,
}

impl ParsedBundle {
    fn accept(&mut self, entry: BundleEntry, raw: &str, line: usize) -> Result<(), BundleError> {
        // The header must open the file, and its version is checked
        // before anything else so an old bundle fails on the version.
        if !self.header_seen {
            let BundleEntry::BundleHeader { format_version } = entry else {
                return Err(BundleError::MissingHeader);
            };
            if format_version != CURRENT_BUNDLE_VERSION {
                return Err(BundleError::IncompatibleVersion {
                    found: format_version,
                    expected: CURRENT_BUNDLE_VERSION,
                });
            }
            self.header_seen = true;
            self.hash_line(raw);
            return Ok(());
        }

        // The trailer line is excluded from the digest; writers emit it
        // outside the hashing writer for the same reason.
        if !matches!(entry, BundleEntry::BundleTrailer { .. }) {
            self.hash_line(raw);
        }
        match entry {
            BundleEntry::BundleHeader { .. } => {
                return Err(BundleError::DuplicateSection {
                    section: "bundle_header",
                    line,
                });
            },
            BundleEntry::PipelineYaml { content } => {
                Self::set_once(&mut self.pipeline_yaml, content, "pipeline_yaml", line)?;
            },
            BundleEntry::RunInputs(inputs) => {
                Self::set_once(&mut self.run_inputs, inputs, "run_inputs", line)?;
            },
            BundleEntry::LlmEntry(e) => self.llm_entries.push(e),
            BundleEntry::ToolEntry(e) => self.tool_entries.push(e),
            BundleEntry::BundleTrailer { blake3 } => {
                let actual = self.hasher.finalize().to_hex().to_string();
                if actual != blake3 {
                    return Err(BundleError::IntegrityMismatch {
                        expected: blake3,
                        actual,
                    });
                }
                self.trailer_seen = true;
            },
        }
        Ok(())
    }

    fn hash_line(&mut self, raw: &str) {
        self.hasher.update(raw.as_bytes());
        self.hasher.update(b"\n");
    }

    fn set_once<T>(
        slot: &mut Option<T>,
        value: T,
        section: &'static str,
        line: usize,
    ) -> Result<(), BundleError> {
        if slot.replace(value).is_some() {
            return Err(BundleError::DuplicateSection { section, line });
        }
        Ok(())
    }

    fn finish(self) -> Result<BundleContents, BundleError> {
        if !self.header_seen {
            return Err(BundleError::MissingHeader);
        }
        if !self.trailer_seen {
            return Err(BundleError::MissingTrailer);
        }
        Ok(BundleContents {
            pipeline_yaml: self.pipeline_yaml.ok_or(BundleError::MissingSection {
                section: "pipeline_yaml",
            })?,
            run_inputs: self.run_inputs.ok_or(BundleError::MissingSection {
                section: "run_inputs",
            })?,
            llm_entries: self.llm_entries,
            tool_entries: self.tool_entries,
        })
    }
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
    use crate::llm::LlmResponse;
    use crate::llm::recording::TapeOutcome;
    use crate::tool::record::ToolTapeOutcome;
    use std::io::Write;

    const YAML: &str = "version: 1\nnodes:\n  - id: n\n    kind: shell\n    command: \"true\"\n";

    fn llm_entry(key: &str, content: &str) -> TapeEntry {
        TapeEntry {
            key: key.to_string(),
            seq: 0,
            tools: Vec::new(),
            outcome: TapeOutcome::Ok {
                res: LlmResponse {
                    content: content.to_string(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                    tool_calls: Vec::new(),
                },
            },
        }
    }

    fn tool_entry(key: &str, content: &str) -> ToolTapeEntry {
        ToolTapeEntry {
            key: key.to_string(),
            seq: 0,
            outcome: ToolTapeOutcome::Ok {
                content: content.to_string(),
            },
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

    fn write(
        yaml: &str,
        llm: Option<&Path>,
        tool: Option<&Path>,
        out: &Path,
    ) -> Result<(), BundleError> {
        write_bundle(
            &BundleSources {
                pipeline_yaml: yaml,
                run_inputs: &BundleRunInputs::default(),
                llm_tape: llm,
                tool_tape: tool,
            },
            out,
        )
    }

    fn bundle_lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn rewrite(path: &Path, lines: &[String]) {
        std::fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    fn fresh_bundle() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("bundle.ndjson");
        write(YAML, None, None, &path).unwrap();
        (dir, path)
    }

    #[test]
    fn round_trip_preserves_pipeline_inputs_and_entries() {
        let llm_tape = write_tape(&[llm_entry("k1", "hello"), llm_entry("k2", "world")]);
        let tool_tape = write_tape(&[tool_entry("aaaa", "first"), tool_entry("bbbb", "second")]);
        let inputs = BundleRunInputs {
            vars: BTreeMap::from([("tag".to_string(), serde_json::json!("v1"))]),
            env: BTreeMap::from([("HOME".to_string(), "/h".to_string())]),
            secrets: BTreeMap::from([("T".to_string(), "***".to_string())]),
        };
        let dir = tempfile::TempDir::new().unwrap();
        let out = dir.path().join("bundle.ndjson");

        write_bundle(
            &BundleSources {
                pipeline_yaml: YAML,
                run_inputs: &inputs,
                llm_tape: Some(llm_tape.path()),
                tool_tape: Some(tool_tape.path()),
            },
            &out,
        )
        .expect("bundle write must succeed");

        let read = read_bundle(&out).expect("bundle read must succeed");
        assert_eq!(read.pipeline_yaml, YAML);
        assert_eq!(read.run_inputs.vars, inputs.vars);
        assert_eq!(read.run_inputs.env, inputs.env);
        assert_eq!(read.run_inputs.secrets, inputs.secrets);
        assert_eq!(read.llm_entries.len(), 2);
        assert_eq!(read.llm_entries[0].key, "k1");
        assert_eq!(read.llm_entries[1].key, "k2");
        assert_eq!(read.tool_entries.len(), 2);
        assert_eq!(read.tool_entries[0].key, "aaaa");
        assert_eq!(
            read.tool_entries[0].outcome,
            ToolTapeOutcome::Ok {
                content: "first".into()
            }
        );
    }

    #[test]
    fn capture_masks_secrets_and_redacts_vars_and_env() {
        let secrets = BTreeMap::from([("T".to_string(), "sk-live-123456".to_string())]);
        let redactor = Redactor::new(&secrets);
        let vars = BTreeMap::from([(
            "auth".to_string(),
            serde_json::json!("Bearer sk-live-123456"),
        )]);
        let env = BTreeMap::from([("X".to_string(), "pre sk-live-123456".to_string())]);

        let captured = BundleRunInputs::capture(&vars, &env, &secrets, &redactor);

        let json = serde_json::to_string(&captured).unwrap();
        assert!(!json.contains("sk-live-123456"), "{json}");
        assert_eq!(captured.secrets["T"], "***");
        assert_eq!(captured.vars["auth"], "Bearer ***");
        assert_eq!(captured.env["X"], "pre ***");
    }

    #[test]
    fn write_with_no_tapes_emits_header_yaml_inputs_trailer() {
        let (_dir, path) = fresh_bundle();

        let read = read_bundle(&path).unwrap();
        assert_eq!(read.pipeline_yaml, YAML);
        assert!(read.llm_entries.is_empty());
        assert!(read.tool_entries.is_empty());
        assert_eq!(bundle_lines(&path).len(), 4);
    }

    #[test]
    fn truncated_bundle_without_trailer_is_rejected() {
        let (_dir, path) = fresh_bundle();
        let mut lines = bundle_lines(&path);
        let last = lines.pop().unwrap();
        assert!(last.contains(r#""type":"bundle_trailer""#), "{last}");
        rewrite(&path, &lines);

        let err = read_bundle(&path).expect_err("no trailer must be an error");
        assert!(matches!(err, BundleError::MissingTrailer), "{err:?}");
    }

    #[test]
    fn bundle_without_header_is_rejected() {
        let (_dir, path) = fresh_bundle();
        let mut lines = bundle_lines(&path);
        lines.remove(0);
        rewrite(&path, &lines);

        let err = read_bundle(&path).expect_err("header-less bundle must be rejected");
        assert!(matches!(err, BundleError::MissingHeader), "{err:?}");
    }

    #[test]
    fn duplicate_pipeline_yaml_is_rejected() {
        let (_dir, path) = fresh_bundle();
        let mut lines = bundle_lines(&path);
        let yaml_line = lines[1].clone();
        lines.insert(2, yaml_line);
        rewrite(&path, &lines);

        let err = read_bundle(&path).expect_err("two pipeline_yaml lines must be rejected");
        assert!(
            matches!(
                err,
                BundleError::DuplicateSection {
                    section: "pipeline_yaml",
                    line: 3
                }
            ),
            "{err:?}",
        );
    }

    #[test]
    fn missing_pipeline_yaml_is_rejected() {
        let (_dir, path) = fresh_bundle();
        let mut lines = bundle_lines(&path);
        lines.remove(1);
        // Dropping a body line also breaks the digest, so re-seal the trailer.
        lines.pop();
        let mut hasher = Blake3Hasher::new();
        for l in &lines {
            hasher.update(l.as_bytes());
            hasher.update(b"\n");
        }
        let trailer = serde_json::to_string(&BundleEntry::BundleTrailer {
            blake3: hasher.finalize().to_hex().to_string(),
        })
        .unwrap();
        lines.push(trailer);
        rewrite(&path, &lines);

        let err = read_bundle(&path).expect_err("must reject bundle without pipeline_yaml");
        assert!(
            matches!(
                err,
                BundleError::MissingSection {
                    section: "pipeline_yaml"
                }
            ),
            "{err:?}",
        );
    }

    #[test]
    fn old_format_bundle_is_refused_naming_both_versions() {
        // An old bundle's later lines no longer parse; the version check
        // on the header must fire first.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("old.ndjson");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"bundle_header","format_version":1}"#,
                "\n",
                r#"{"type":"llm_entry","req":{"provider":"x"},"outcome":{"kind":"ok"}}"#,
                "\n",
            ),
        )
        .unwrap();

        let err = read_bundle(&path).expect_err("v1 bundle must be refused");
        match &err {
            BundleError::IncompatibleVersion { found, expected } => {
                assert_eq!((*found, *expected), (1, CURRENT_BUNDLE_VERSION));
            },
            other => panic!("expected IncompatibleVersion, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains('1') && msg.contains(&CURRENT_BUNDLE_VERSION.to_string()),
            "{msg}"
        );
    }

    #[test]
    fn future_format_version_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("future.ndjson");
        std::fs::write(
            &path,
            "{\"type\":\"bundle_header\",\"format_version\":999}\n",
        )
        .unwrap();

        let err = read_bundle(&path).expect_err("must reject incompatible version");
        assert!(
            matches!(err, BundleError::IncompatibleVersion { found: 999, .. }),
            "{err:?}",
        );
    }

    #[test]
    fn corrupt_line_is_rejected_with_line_number() {
        let (_dir, path) = fresh_bundle();
        let mut lines = bundle_lines(&path);
        lines.insert(1, "{not json".to_string());
        rewrite(&path, &lines);

        match read_bundle(&path).expect_err("corrupt line must be rejected") {
            BundleError::ParseError { line, .. } => assert_eq!(line, 2),
            other => panic!("expected ParseError, got {other:?}"),
        }
    }

    #[test]
    fn header_uses_current_bundle_version_constant() {
        let (_dir, path) = fresh_bundle();
        let lines = bundle_lines(&path);
        assert!(
            lines[0].contains(&format!(r#""format_version":{CURRENT_BUNDLE_VERSION}"#)),
            "header line `{}` must encode CURRENT_BUNDLE_VERSION = {CURRENT_BUNDLE_VERSION}",
            lines[0],
        );
    }

    #[test]
    fn failed_write_leaves_no_file_at_target() {
        let dir = tempfile::TempDir::new().unwrap();
        let out = dir.path().join("bundle.ndjson");
        let missing_tape = dir.path().join("missing.ndjson");

        write(YAML, Some(&missing_tape), None, &out).expect_err("missing tape must fail");

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            leftovers.is_empty(),
            "partial output left behind: {leftovers:?}"
        );
    }

    #[test]
    fn failed_write_keeps_the_previous_bundle_intact() {
        let (dir, path) = fresh_bundle();
        let before = std::fs::read(&path).unwrap();

        let missing_tape = dir.path().join("missing.ndjson");
        write(YAML, Some(&missing_tape), None, &path).expect_err("missing tape must fail");

        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn bearer_token_is_scrubbed_in_bundle() {
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
        let dir = tempfile::TempDir::new().unwrap();
        let out = dir.path().join("bundle.ndjson");
        write(yaml_body, None, None, &out).expect("bundle write");

        let read = read_bundle(&out).expect("bundle read");
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
    fn stdio_env_headers_and_basic_password_are_scrubbed() {
        let yaml_body = "\
version: 1
mcp_servers:
  local:
    transport: stdio
    command: [srv]
    env:
      API_THING: literal-env-value
  remote:
    transport: http
    url: https://mcp.example.com
    headers:
      Authorization: Bearer literal-header-value
    auth:
      kind: basic
      user: bob
      password: literal-basic-pw
nodes:
  - id: n
    kind: shell
    command: \"true\"
";
        let scrubbed = scrub_pipeline_secrets(yaml_body).expect("scrub");
        for leaked in [
            "literal-env-value",
            "literal-header-value",
            "literal-basic-pw",
        ] {
            assert!(!scrubbed.contains(leaked), "{leaked} leaked: {scrubbed}");
        }
        assert!(scrubbed.contains("bob"), "username is not a credential");
    }

    #[test]
    fn templated_bearer_token_is_left_intact() {
        // Templates are placeholders — `secrets.X` resolves at run time.
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
        let dir = tempfile::TempDir::new().unwrap();
        let out = dir.path().join("bundle.ndjson");
        write(yaml_body, None, None, &out).expect("bundle write");

        let read = read_bundle(&out).expect("bundle read");
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
        // Comments and whitespace survive for human inspection.
        let yaml_body = "\
# top-of-file comment
version: 1
nodes:
  - id: n
    kind: shell
    command: \"true\"   # trailing comment
";
        let dir = tempfile::TempDir::new().unwrap();
        let out = dir.path().join("bundle.ndjson");
        write(yaml_body, None, None, &out).unwrap();

        let read = read_bundle(&out).unwrap();
        assert_eq!(
            read.pipeline_yaml, yaml_body,
            "no-bearer pipeline must be embedded verbatim",
        );
    }

    #[test]
    fn bundle_tampered_body_is_rejected() {
        let (_dir, path) = fresh_bundle();
        let mut lines = bundle_lines(&path);
        let yaml_idx = lines
            .iter()
            .position(|l| l.contains(r#""type":"pipeline_yaml""#))
            .expect("pipeline_yaml line must be present");
        lines[yaml_idx] = lines[yaml_idx].replace("version: 1", "version: 2");
        rewrite(&path, &lines);

        match read_bundle(&path).expect_err("tampered bundle must be rejected") {
            BundleError::IntegrityMismatch { expected, actual } => assert_ne!(expected, actual),
            other => panic!("expected IntegrityMismatch, got {other:?}"),
        }
    }

    #[test]
    fn read_bundle_rejects_post_trailer_content() {
        let llm_tape = write_tape(&[llm_entry("k", "a")]);
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("bundle.ndjson");
        write(YAML, Some(llm_tape.path()), None, &path).expect("bundle write");

        let extra = serde_json::to_string(&BundleEntry::LlmEntry(llm_entry("late", "b"))).unwrap();
        let mut existing = std::fs::read_to_string(&path).unwrap();
        existing.push_str(&extra);
        existing.push('\n');
        std::fs::write(&path, existing).unwrap();

        match read_bundle(&path).expect_err("post-trailer content must be rejected") {
            BundleError::ExtraContentAfterTrailer { line_count } => assert_eq!(line_count, 1),
            other => panic!("expected ExtraContentAfterTrailer, got {other:?}"),
        }
    }

    #[test]
    fn read_bundle_rejects_garbage_after_trailer() {
        // The guard fires before the JSON parse, so garbage surfaces as
        // tampering rather than a parse error.
        let (_dir, path) = fresh_bundle();
        let mut existing = std::fs::read_to_string(&path).unwrap();
        existing.push_str("not valid json !!!!\n");
        std::fs::write(&path, existing).unwrap();

        match read_bundle(&path).expect_err("garbage after trailer must be rejected") {
            BundleError::ExtraContentAfterTrailer { line_count } => assert_eq!(line_count, 1),
            other => panic!("expected ExtraContentAfterTrailer, got {other:?}"),
        }
    }
}
