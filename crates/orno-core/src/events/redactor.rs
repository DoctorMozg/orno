//! Secret-value redactor for event emission. Rendered `secrets.*`
//! values must not reach the event log; the scheduler constructs a
//! `Redactor` from the run's secret map and runs every user-visible
//! string through it before handing the envelope to a sink.
//!
//! The redactor is value-based (not key-based): we substitute raw
//! secret strings inside prompts, node outputs, and other payloads,
//! because the sensitive content flows by value once templates have
//! been rendered. An Aho-Corasick automaton matches every secret in a
//! single linear scan of the haystack — `leftmost-longest` semantics
//! guarantee that a secret which is a substring of a longer secret
//! cannot sneak through by being matched first.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;

use aho_corasick::{AhoCorasick, MatchKind};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use serde_json::Value;

use super::{truncate_excerpt, truncate_tail};

/// Derived patterns (embedded base64, per-line, escaped forms) shorter
/// than this are dropped; a short fragment would over-redact ordinary text.
const MIN_DERIVED_PATTERN_LEN: usize = 8;

const MARKER: &str = "***";

/// Which end of an over-long string survives bounding.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Keep {
    Head,
    Tail,
}

/// Replaces every occurrence of a known secret value with `"***"`.
///
/// Build once per run from the resolved secret map and share across
/// the engine's emission sites. Empty-valued secrets are dropped; an
/// empty-string match would replace every zero-width position in the
/// haystack, which is worse than not redacting at all.
///
/// Each secret is registered alongside the encodings an LLM could use to
/// smuggle it out: base64 (standard, no-pad, URL-safe; also at byte
/// offsets 1 and 2 so a secret embedded in a larger blob matches),
/// lower/upper hex, percent-encoding and the JSON-escaped form. Each
/// line of a multi-line secret is its own pattern. All patterns feed one
/// Aho-Corasick automaton with `LeftmostLongest` semantics.
#[derive(Default)]
pub struct Redactor {
    /// Patterns registered with the automaton, kept so `Clone` can rebuild it.
    values: Vec<String>,
    /// `None` when no secrets were registered so the no-op path costs nothing.
    ac: Option<AhoCorasick>,
}

impl Redactor {
    /// Build a redactor from the run's `secrets.*` namespace. Duplicate
    /// values collapse to one entry; empty values are dropped because a
    /// zero-width match would corrupt every position in the haystack.
    #[must_use]
    pub fn new(secrets: &BTreeMap<String, String>) -> Self {
        let raw: Vec<&str> = secrets
            .values()
            .map(String::as_str)
            .filter(|v| !v.is_empty())
            .collect();
        Self::from_values(Self::expand_encodings(&raw))
    }

    fn expand_encodings(raw: &[&str]) -> Vec<String> {
        let mut patterns: Vec<String> = Vec::with_capacity(raw.len() * 16);
        for v in raw {
            patterns.push((*v).to_string());
            let bytes = v.as_bytes();
            patterns.push(STANDARD.encode(bytes));
            patterns.push(STANDARD_NO_PAD.encode(bytes));
            patterns.push(URL_SAFE_NO_PAD.encode(bytes));
            let hex_lower = hex::encode(bytes);
            patterns.push(hex_lower.to_uppercase());
            patterns.push(hex_lower);

            for offset in 0..=2 {
                patterns.extend(embedded_base64(bytes, offset));
            }

            let mut derived = vec![percent_encode(v, false), percent_encode(v, true)];
            if let Ok(json) = serde_json::to_string(v) {
                derived.push(json[1..json.len() - 1].to_string());
            }
            if v.contains('\n') {
                derived.extend(v.lines().map(|l| l.trim().to_string()));
            }
            patterns.extend(
                derived
                    .into_iter()
                    .filter(|p| p.len() >= MIN_DERIVED_PATTERN_LEN || p == v),
            );
        }
        patterns.retain(|p| !p.is_empty());
        patterns.sort();
        patterns.dedup();
        patterns
    }

    fn from_values(values: Vec<String>) -> Self {
        let ac = if values.is_empty() {
            None
        } else {
            // Longest-wins keeps a shorter substring secret from slicing a
            // longer one mid-token. Build only fails on state-id exhaustion,
            // far beyond a pipeline's handful of secrets.
            Some(
                AhoCorasick::builder()
                    .match_kind(MatchKind::LeftmostLongest)
                    .build(&values)
                    .expect("Redactor: Aho-Corasick build failed on valid UTF-8 secret values"),
            )
        };
        Self { values, ac }
    }

    /// `true` when the redactor holds no secrets and can be skipped.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.values.is_empty()
    }

    /// Replace every occurrence of any known secret with `"***"` in a
    /// single scan. Returns `Borrowed` when nothing matched.
    #[must_use]
    pub fn redact<'a>(&self, s: &'a str) -> Cow<'a, str> {
        let Some(ac) = self.ac.as_ref() else {
            return Cow::Borrowed(s);
        };
        let mut matches = ac.find_iter(s).peekable();
        if matches.peek().is_none() {
            return Cow::Borrowed(s);
        }
        let mut out = String::with_capacity(s.len());
        let mut last = 0;
        for mat in matches {
            out.push_str(&s[last..mat.start()]);
            out.push_str(MARKER);
            last = mat.end();
        }
        out.push_str(&s[last..]);
        Cow::Owned(out)
    }

    /// Redact first, then bound the length. Bounding first could cut a
    /// secret in half and leave its tail unmatched by every pattern.
    #[must_use]
    pub(crate) fn redact_bounded(&self, s: &str, max_bytes: usize, keep: Keep) -> String {
        let redacted = self.redact(s);
        match keep {
            Keep::Head => truncate_excerpt(&redacted, max_bytes),
            Keep::Tail => truncate_tail(&redacted, max_bytes),
        }
    }

    /// Recursively redact string leaves and object keys in place.
    /// Numbers, booleans and nulls are left untouched, and nothing is
    /// allocated when no secret matches.
    pub fn redact_json(&self, v: &mut Value) {
        if self.ac.is_none() {
            return;
        }
        match v {
            Value::String(s) => {
                if let Cow::Owned(redacted) = self.redact(s) {
                    *s = redacted;
                }
            },
            Value::Array(items) => items.iter_mut().for_each(|i| self.redact_json(i)),
            Value::Object(map) => {
                let leaking: Vec<String> = map
                    .keys()
                    .filter(|k| matches!(self.redact(k), Cow::Owned(_)))
                    .cloned()
                    .collect();
                for key in leaking {
                    if let Some(val) = map.remove(&key) {
                        map.insert(self.redact(&key).into_owned(), val);
                    }
                }
                map.values_mut().for_each(|val| self.redact_json(val));
            },
            _ => {},
        }
    }
}

/// Base64 forms of `bytes` as they appear inside a larger blob when
/// `offset` (0..=2) bytes precede them. Only characters fully determined
/// by the secret's own bits are kept; the boundary characters depend on
/// the adjacent bytes.
fn embedded_base64(bytes: &[u8], offset: usize) -> Vec<String> {
    let mut padded = vec![0u8; offset];
    padded.extend_from_slice(bytes);
    let skip = (offset * 8).div_ceil(6);
    let full = padded.len() * 8 / 6;
    if full <= skip {
        return Vec::new();
    }
    [
        STANDARD_NO_PAD.encode(&padded),
        URL_SAFE_NO_PAD.encode(&padded),
    ]
    .into_iter()
    .map(|enc| enc[skip..full].to_string())
    .filter(|p| p.len() >= MIN_DERIVED_PATTERN_LEN)
    .collect()
}

/// RFC 3986 percent-encoding of everything except unreserved characters.
fn percent_encode(s: &str, lowercase_hex: bool) -> String {
    let digits = if lowercase_hex {
        b"0123456789abcdef"
    } else {
        b"0123456789ABCDEF"
    };
    let mut out = String::with_capacity(s.len() * 3);
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(digits[usize::from(b >> 4)]));
            out.push(char::from(digits[usize::from(b & 0x0f)]));
        }
    }
    out
}

// `AhoCorasick` is not `Debug`, so we omit it from the printed form and
// surface only the secret count. Secret values themselves must never be
// emitted through `Debug` — that would defeat the redactor's purpose.
impl fmt::Debug for Redactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Redactor")
            .field("secret_count", &self.values.len())
            .finish_non_exhaustive()
    }
}

// `AhoCorasick` itself is `Clone` (cheap `Arc` bump), but rebuilding
// from `values` keeps the public field set tight and avoids relying on
// implementation details of the upstream crate.
impl Clone for Redactor {
    fn clone(&self) -> Self {
        Self::from_values(self.values.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn redactor_with(secrets: &[(&str, &str)]) -> Redactor {
        let map: BTreeMap<String, String> = secrets
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        Redactor::new(&map)
    }

    #[test]
    fn noop_when_no_secrets() {
        // Default redactor holds no patterns; redaction must short-circuit
        // on the borrowed path so callers pay no allocation cost.
        let r = Redactor::default();
        let result = r.redact("");
        assert!(matches!(result, Cow::Borrowed(_)));
        assert!(r.is_noop());
    }

    #[test]
    fn single_secret_replaced() {
        // Smoke test for the common path: one registered secret, one
        // occurrence in the haystack, replaced with the canonical marker.
        let r = redactor_with(&[("api_key", "supersecret")]);
        let out = r.redact("token=supersecret end").into_owned();
        assert_eq!(out, "token=*** end");
    }

    #[test]
    fn multiple_secrets_replaced_in_one_pass() {
        // The Aho-Corasick automaton catches every registered secret in a
        // single linear scan. Three distinct secrets must each be replaced
        // independently when they all appear in the same haystack.
        let r = redactor_with(&[("a", "alpha"), ("b", "bravo"), ("c", "charlie")]);
        let out = r.redact("alpha then bravo then charlie done").into_owned();
        assert_eq!(out, "*** then *** then *** done");
    }

    #[test]
    fn longer_secret_wins_over_substring() {
        // `MatchKind::LeftmostLongest` semantics: when two registered secrets
        // overlap at the same start position, the longer one must be the
        // single match. Otherwise the shorter substring would slice the
        // longer secret in half and emit two adjacent markers.
        let r = redactor_with(&[("short", "api"), ("long", "api_key_secret")]);
        let out = r.redact("see api_key_secret here").into_owned();
        assert_eq!(out, "see *** here");
        assert_eq!(out.matches("***").count(), 1);
    }

    #[test]
    fn secret_not_in_string_returns_borrowed() {
        // When the haystack does not contain any registered secret the
        // redactor must return `Cow::Borrowed`. Allocating a fresh String
        // for every clean payload would defeat the no-op fast path.
        let r = redactor_with(&[("k", "supersecret")]);
        let result = r.redact("nothing sensitive here");
        assert!(matches!(result, Cow::Borrowed(_)));
    }

    #[test]
    fn json_redaction_redacts_string_leaves() {
        // Recursive JSON walk: only string leaves are rewritten. Numbers,
        // booleans, and nulls cannot carry a secret by themselves so they
        // must pass through bit-for-bit.
        let r = redactor_with(&[("k", "topsecret")]);
        let mut out = json!({
            "field": "leak topsecret here",
            "count": 42,
            "flag": true,
            "missing": null,
        });
        r.redact_json(&mut out);
        assert_eq!(out["field"], json!("leak *** here"));
        assert_eq!(out["count"], json!(42));
        assert_eq!(out["flag"], json!(true));
        assert_eq!(out["missing"], json!(null));
    }

    #[test]
    fn empty_secret_value_not_registered() {
        // Empty-valued secrets would match every zero-width position in
        // the haystack — strictly worse than no redaction. The constructor
        // must drop them so the resulting redactor is a no-op.
        let r = redactor_with(&[("blank", "")]);
        assert!(r.is_noop());
    }

    #[test]
    fn clone_produces_equivalent_redactor() {
        // `Clone` rebuilds the automaton from the retained pattern list
        // rather than relying on `AhoCorasick`'s own `Clone`. The cloned
        // redactor must redact identically — same patterns, same output.
        let r = redactor_with(&[("k", "abracadabra")]);
        let clone = r.clone();
        let input = "say abracadabra now";
        assert_eq!(r.redact(input), clone.redact(input));
        assert_eq!(clone.redact(input).into_owned(), "say *** now");
    }

    #[test]
    fn repeated_secret_in_string_replaces_all_occurrences() {
        // The Aho-Corasick scan walks the haystack to the end, so a secret
        // that appears multiple times in one string must be redacted at
        // every position — not just the first match.
        let r = redactor_with(&[("k", "secret")]);
        let out = r.redact("token=secret and secret again").into_owned();
        assert_eq!(out, "token=*** and *** again");
    }

    #[test]
    fn redact_json_handles_nested_objects_and_arrays() {
        // Recursive walk must descend into nested objects and arrays without
        // collapsing structure or rewriting non-string leaves. Numbers and
        // booleans flow through unchanged; only string leaves get the
        // redaction marker.
        let r = redactor_with(&[("k", "topsecret")]);
        let mut out = json!({
            "outer": { "inner": "topsecret" },
            "list": ["topsecret", 42, true],
        });
        r.redact_json(&mut out);
        assert_eq!(out["outer"]["inner"], json!("***"));
        assert_eq!(out["list"][0], json!("***"));
        assert_eq!(out["list"][1], json!(42));
        assert_eq!(out["list"][2], json!(true));
    }

    #[test]
    fn redactor_literal_still_redacted() {
        // Sanity: registering encoding-aware patterns must not regress the
        // literal-secret path. `"hunter2"` itself has to stay matchable
        // alongside its base64/hex forms.
        let r = redactor_with(&[("k", "hunter2")]);
        let out = r.redact("password=hunter2 done").into_owned();
        assert_eq!(out, "password=*** done");
    }

    #[test]
    fn redactor_redacts_base64_standard_encoded_secret() {
        // Standard base64 (`+` / `/` alphabet, `=` padding) is the form
        // most LLMs reach for when asked to encode a string. `"hunter2"`
        // encodes to `aHVudGVyMg==` and must collapse to the marker.
        let r = redactor_with(&[("k", "hunter2")]);
        let out = r.redact("leak: aHVudGVyMg== here").into_owned();
        assert_eq!(out, "leak: *** here");
    }

    #[test]
    fn redactor_redacts_base64_url_safe_encoded_secret() {
        // URL-safe-no-pad is the form base64 takes inside JWTs and any
        // "URL-friendly" exfiltration channel. `"hunter2"` encodes to
        // `aHVudGVyMg` (no `=` padding) and must also collapse.
        let r = redactor_with(&[("k", "hunter2")]);
        let out = r.redact("token=aHVudGVyMg next").into_owned();
        assert_eq!(out, "token=*** next");
    }

    #[test]
    fn redactor_redacts_hex_encoded_secret() {
        // Lowercase hex is the third common bypass vector, especially in
        // logs and CTF-style payloads. `"hunter2"` encodes to
        // `68756e74657232` and must collapse to the marker.
        let r = redactor_with(&[("k", "hunter2")]);
        let out = r.redact("hex: 68756e74657232 end").into_owned();
        assert_eq!(out, "hex: *** end");
    }

    #[test]
    fn redacts_uppercase_hex_and_no_pad_base64() {
        let r = redactor_with(&[("k", "hunter2")]);
        assert_eq!(r.redact("x 68756E74657232 y"), "x *** y");
        // `aHVudGVyMg` is the no-pad form; the padded form is covered above.
        assert_eq!(r.redact("x aHVudGVyMg y"), "x *** y");
    }

    #[test]
    fn redacts_base64_of_secret_embedded_at_every_offset() {
        let secret = "s3cr3t-token-value";
        let r = redactor_with(&[("k", secret)]);
        for prefix in ["", "A", "AB"] {
            let blob = format!("{prefix}{secret}trailing-bytes");
            for enc in [STANDARD.encode(&blob), URL_SAFE_NO_PAD.encode(&blob)] {
                let text = format!("data={enc}");
                let out = r.redact(&text);
                assert!(
                    out.contains(MARKER),
                    "offset {} not redacted: {out}",
                    prefix.len()
                );
            }
        }
    }

    #[test]
    fn redacts_percent_encoded_secret() {
        let r = redactor_with(&[("k", "p@ss word/1234")]);
        assert_eq!(r.redact("q=p%40ss%20word%2F1234&x=1"), "q=***&x=1");
        assert_eq!(r.redact("q=p%40ss%20word%2f1234"), "q=***");
    }

    #[test]
    fn redacts_json_escaped_secret() {
        let r = redactor_with(&[("k", "line\"quoted\"secret")]);
        assert_eq!(
            r.redact(r#"{"a":"line\"quoted\"secret"}"#),
            r#"{"a":"***"}"#
        );
    }

    #[test]
    fn multiline_secret_redacts_each_long_line() {
        let r = redactor_with(&[(
            "k",
            "-----BEGIN KEY-----\nabcdefghijkl\nshort\n-----END KEY-----",
        )]);
        let out = r.redact("leak: abcdefghijkl and short");
        assert_eq!(out, "leak: *** and short");
    }

    #[test]
    fn redact_json_redacts_object_keys() {
        let r = redactor_with(&[("k", "topsecret")]);
        let mut v = json!({"topsecret": 1, "ok": {"topsecret": "x"}});
        r.redact_json(&mut v);
        assert_eq!(v, json!({"***": 1, "ok": {"***": "x"}}));
    }

    #[test]
    fn redact_bounded_redacts_secret_straddling_the_cut() {
        let r = redactor_with(&[("k", "supersecretvalue")]);
        let input = "0123456789supersecretvalue";
        // A 10-byte tail window would start mid-secret ("ecretvalue").
        let tail = r.redact_bounded(input, 10, Keep::Tail);
        assert!(!tail.contains("ecretvalue"), "{tail}");
        let head = r.redact_bounded(input, 18, Keep::Head);
        assert!(!head.contains("supersecret"), "{head}");
    }
}
