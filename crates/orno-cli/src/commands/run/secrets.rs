//! Env and secrets resolution: dotenv parsing, inline `-e KEY=VAL`
//! handling, and the `resolve_inputs` function that builds the two
//! disjoint namespaces the engine receives.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

use orno_core::execution::RunInputs;
use orno_core::pipeline::Pipeline;

use super::RunFlags;

/// Resolve `env` and `secrets` per the documented precedence order.
///
/// - `env.*`: `pass_env:` (process env) < `--env-file` (later files
///   shadow earlier) < `-e KEY=VAL` (last flag wins).
/// - `secrets.*`: process env for names in `secrets:` <
///   `--secrets-file` (later files shadow earlier).
///
/// Classification is by name. A binding whose key is declared in the
/// pipeline's `secrets:` block always lands in `secrets.*`, even when
/// it arrives via an env file. A secret on `-e` is refused outright —
/// `argv` leaks into `HISTFILE` and `ps`.
pub(super) fn resolve_inputs(pipeline: &Pipeline, flags: &RunFlags) -> Result<RunInputs> {
    let declared_secrets: HashSet<&str> = pipeline.secrets.iter().map(String::as_str).collect();

    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut secrets: BTreeMap<String, String> = BTreeMap::new();

    for name in &pipeline.pass_env {
        if let Ok(val) = std::env::var(name) {
            env.insert(name.clone(), val);
        }
    }

    for name in &pipeline.secrets {
        if let Ok(val) = std::env::var(name) {
            secrets.insert(name.clone(), val);
        }
    }

    for file in &flags.env_files {
        for (k, v) in parse_dotenv(file)? {
            if declared_secrets.contains(k.as_str()) {
                secrets.insert(k, v);
            } else {
                env.insert(k, v);
            }
        }
    }

    for file in &flags.secrets_files {
        for (k, v) in parse_dotenv(file)? {
            secrets.insert(k, v);
        }
    }

    for item in &flags.inline_env {
        let (k, v) = parse_inline_env(item)?;
        if declared_secrets.contains(k.as_str()) {
            bail!("refusing to accept secret `{k}` via `-e`; use `--secrets-file` instead");
        }
        env.insert(k, v);
    }

    let mut inputs = RunInputs::default();
    inputs.env = env;
    inputs.secrets = secrets;
    Ok(inputs)
}

fn parse_inline_env(s: &str) -> Result<(String, String)> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| anyhow!("expected `KEY=VAL`, got `{s}`"))?;
    if k.is_empty() {
        bail!("empty key in `-e {s}`");
    }
    if v.is_empty() {
        tracing::warn!(key = %k, "empty value for `-e {k}=` — did you mean to unset this variable?");
    }
    Ok((k.to_string(), v.to_string()))
}

/// Dotenv parser supporting the dialect features common to bash-style
/// `.env` files: `KEY=VAL` per line, blank lines and full-line `#`
/// comments skipped, optional `export` prefix stripped, double-quoted
/// or single-quoted values (literal contents, no escape processing),
/// and inline `# comment` after an unquoted value when preceded by
/// whitespace. Variable expansion (`$OTHER`) is intentionally not
/// supported — the parser must not silently change a secret literal.
/// Later duplicate keys within the same file win on the consumer side
/// via `BTreeMap::insert`.
pub(super) fn parse_dotenv(path: &Path) -> Result<Vec<(String, String)>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading env file `{}`", path.display()))?;

    // Refuse to read an env file whose mode allows group/other access:
    // a secrets-bearing dotenv must be 0600 so a co-tenant on a shared
    // host cannot tail it. The check is Unix-only — Windows ACLs do not
    // map cleanly onto a `mode_t` and a plain bitmask check would either
    // misclassify or impose a meaningless invariant. Operators on
    // Windows are responsible for ACL hygiene out-of-band.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .with_context(|| format!("checking permissions of env file `{}`", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "env file `{}` has permissions {:#o} — secrets files must be 0600 \
                 (run: chmod 600 {})",
                path.display(),
                mode & 0o777,
                path.display()
            );
        }
    }

    // Strip a leading UTF-8 BOM (`U+FEFF`). Some Windows editors prepend
    // one when saving a file as UTF-8; leaving it in front of the first
    // line would make the first key parse as `\u{FEFF}KEY`, which the
    // engine then routes to the wrong env entry.
    let contents = contents
        .strip_prefix('\u{FEFF}')
        .unwrap_or(&contents)
        .to_string();

    let mut out = Vec::new();
    for (lineno, raw) in contents.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Strip the optional `export ` prefix (literal — a single ASCII
        // space). Other whitespace (e.g. tabs) between `export` and the
        // key is uncommon in real .env files and easy to flag as the
        // user's typo by failing the `KEY=VAL` parse below.
        let body = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let (k, raw_value) = body.split_once('=').ok_or_else(|| {
            anyhow!(
                "`{}` line {}: expected `KEY=VAL`, got `{}`",
                path.display(),
                lineno + 1,
                line,
            )
        })?;
        let key = k.trim();
        if key.is_empty() {
            bail!("`{}` line {}: empty key", path.display(), lineno + 1);
        }
        let value = parse_dotenv_value(raw_value).with_context(|| {
            format!(
                "`{}` line {}: malformed value for `{key}`",
                path.display(),
                lineno + 1,
            )
        })?;
        out.push((key.to_string(), value));
    }
    Ok(out)
}

/// Decode the right-hand side of a `KEY=VAL` line according to the
/// dotenv dialect documented on `parse_dotenv`. Single- and
/// double-quoted forms are literal — no escape sequences are
/// interpreted, so a secret containing `\n` round-trips byte-for-byte.
fn parse_dotenv_value(raw: &str) -> Result<String> {
    let trimmed = raw.trim_start();
    // Empty value, or value area is only an inline comment.
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(String::new());
    }

    if let Some(rest) = trimmed.strip_prefix('"') {
        let end = rest
            .find('"')
            .ok_or_else(|| anyhow!("unterminated double-quoted value"))?;
        let value = &rest[..end];
        let tail = rest[end + 1..].trim_start();
        if !tail.is_empty() && !tail.starts_with('#') {
            bail!("trailing content after closing double-quote: `{tail}`");
        }
        return Ok(value.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix('\'') {
        let end = rest
            .find('\'')
            .ok_or_else(|| anyhow!("unterminated single-quoted value"))?;
        let value = &rest[..end];
        let tail = rest[end + 1..].trim_start();
        if !tail.is_empty() && !tail.starts_with('#') {
            bail!("trailing content after closing single-quote: `{tail}`");
        }
        return Ok(value.to_string());
    }

    // Unquoted value. An inline comment starts at the first whitespace
    // followed by `#`; a `#` directly inside the token (e.g. a password
    // with `#` characters) is preserved.
    let cut = trimmed
        .find(" #")
        .or_else(|| trimmed.find("\t#"))
        .unwrap_or(trimmed.len());
    Ok(trimmed[..cut].trim_end().to_string())
}

#[cfg(test)]
mod parse_dotenv_tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// Write `body` to a temp file and parse it through `parse_dotenv`,
    /// returning the parsed (key, value) pairs.
    fn parse(body: &str) -> Result<Vec<(String, String)>> {
        let mut tmp = NamedTempFile::new().expect("temp file");
        tmp.write_all(body.as_bytes()).expect("write");
        parse_dotenv(tmp.path())
    }

    #[test]
    fn plain_key_value_lines_parse() {
        let out = parse("FOO=bar\nBAZ=qux\n").expect("parse");
        assert_eq!(
            out,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "qux".to_string()),
            ]
        );
    }

    #[test]
    fn full_line_comments_and_blank_lines_are_skipped() {
        let out = parse("# comment\n\nFOO=bar\n# another\n").expect("parse");
        assert_eq!(out, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn export_prefix_is_stripped() {
        let out = parse("export FOO=bar\nexport BAZ=qux\n").expect("parse");
        assert_eq!(
            out,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "qux".to_string()),
            ]
        );
    }

    #[test]
    fn double_quoted_value_strips_quotes_and_preserves_internal_whitespace() {
        let out = parse("FOO=\"  spaced value  \"\n").expect("parse");
        assert_eq!(
            out,
            vec![("FOO".to_string(), "  spaced value  ".to_string())]
        );
    }

    #[test]
    fn single_quoted_value_strips_quotes_and_preserves_internal_hash() {
        let out = parse("FOO='val#with#hash'\n").expect("parse");
        assert_eq!(out, vec![("FOO".to_string(), "val#with#hash".to_string())]);
    }

    #[test]
    fn quoted_value_may_be_followed_by_inline_comment() {
        let out = parse("FOO=\"value\" # trailing comment\n").expect("parse");
        assert_eq!(out, vec![("FOO".to_string(), "value".to_string())]);
    }

    #[test]
    fn unquoted_inline_comment_strips_at_first_space_hash() {
        let out = parse("FOO=bar # this is a comment\n").expect("parse");
        assert_eq!(out, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn unquoted_value_keeps_internal_hash_without_preceding_whitespace() {
        // A value like `pa#word` has no space before the `#`, so the
        // `#` is part of the value, not the start of a comment.
        let out = parse("FOO=pa#word\n").expect("parse");
        assert_eq!(out, vec![("FOO".to_string(), "pa#word".to_string())]);
    }

    #[test]
    fn empty_value_is_empty_string() {
        let out = parse("FOO=\n").expect("parse");
        assert_eq!(out, vec![("FOO".to_string(), String::new())]);
    }

    #[test]
    fn value_area_that_is_only_a_comment_yields_empty_string() {
        let out = parse("FOO=  # only comment\n").expect("parse");
        assert_eq!(out, vec![("FOO".to_string(), String::new())]);
    }

    #[test]
    fn unterminated_double_quote_is_rejected() {
        let err = parse("FOO=\"unterminated\n").expect_err("must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unterminated double-quoted"),
            "unexpected error: {msg}"
        );
        assert!(msg.contains("FOO"), "must name the offending key: {msg}");
    }

    #[test]
    fn unterminated_single_quote_is_rejected() {
        let err = parse("FOO='unterminated\n").expect_err("must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unterminated single-quoted"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn trailing_content_after_closing_quote_is_rejected() {
        let err = parse("FOO=\"value\" garbage\n").expect_err("must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("trailing content"), "unexpected error: {msg}");
    }

    #[test]
    fn missing_equals_sign_is_rejected_with_line_number() {
        let err = parse("FOO\n").expect_err("must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("line 1"), "must include line number: {msg}");
        assert!(msg.contains("KEY=VAL"), "must show expected form: {msg}");
    }

    #[test]
    fn empty_key_is_rejected() {
        let err = parse("=value\n").expect_err("must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("empty key"), "unexpected error: {msg}");
    }

    #[test]
    fn value_with_equals_sign_inside_keeps_after_first_equals() {
        let out = parse("FOO=key=value=more\n").expect("parse");
        assert_eq!(out, vec![("FOO".to_string(), "key=value=more".to_string())]);
    }

    #[test]
    fn quoted_value_may_contain_equals_sign() {
        let out = parse("FOO=\"a=b=c\"\n").expect("parse");
        assert_eq!(out, vec![("FOO".to_string(), "a=b=c".to_string())]);
    }

    #[test]
    fn export_prefix_combines_with_quoted_value() {
        let out = parse("export TOKEN=\"sk-abc-123\"\n").expect("parse");
        assert_eq!(out, vec![("TOKEN".to_string(), "sk-abc-123".to_string())]);
    }
}
