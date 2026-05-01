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
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::Value;

/// Replaces every occurrence of a known secret value with `"***"`.
///
/// Build once per run from the resolved secret map and share across
/// the engine's emission sites. Empty-valued secrets are dropped — an
/// empty-string match would replace every zero-width position in the
/// haystack, which is worse than not redacting at all.
///
/// Each secret is registered alongside its base64 (standard and URL-safe
/// no-padding) and lowercase hex encodings so an LLM cannot bypass
/// redaction by encoding a secret before exfiltrating it. The expanded
/// pattern set is fed into an Aho-Corasick automaton so a single pass
/// over the haystack catches every match regardless of how many secrets
/// are registered. `MatchKind::LeftmostLongest` preserves the "longest
/// pattern wins" behavior in the rare case where two patterns overlap.
#[derive(Default)]
pub struct Redactor {
    /// Patterns registered with the automaton — the raw secrets plus
    /// their base64/hex encodings. Retained for `Clone` reconstruction
    /// (the automaton itself does not expose its source patterns).
    values: Vec<String>,
    /// Compiled multi-pattern matcher. `None` when no secrets were
    /// registered so the no-op path costs nothing.
    ac: Option<AhoCorasick>,
}

impl Redactor {
    /// Build a redactor from the run's `secrets.*` namespace. Duplicate
    /// values collapse to one entry; empty values are dropped because a
    /// zero-width match would corrupt every position in the haystack.
    /// Every retained secret also contributes its base64 and hex
    /// encodings to the pattern set.
    #[must_use]
    pub fn new(secrets: &BTreeMap<String, String>) -> Self {
        let raw: Vec<String> = secrets
            .values()
            .filter(|v| !v.is_empty())
            .cloned()
            .collect();
        Self::from_values(Self::expand_encodings(raw))
    }

    /// Expand each raw secret into its literal form plus base64
    /// (standard and URL-safe-no-pad) and lowercase-hex encodings, then
    /// sort and dedup so the automaton sees each unique pattern once.
    fn expand_encodings(raw: Vec<String>) -> Vec<String> {
        let mut patterns: Vec<String> = Vec::with_capacity(raw.len() * 4);
        for v in raw {
            let bytes = v.as_bytes();
            let b64_standard = STANDARD.encode(bytes);
            let b64_url = URL_SAFE_NO_PAD.encode(bytes);
            let hex_val = hex::encode(bytes);
            patterns.push(v);
            // Encodings of an empty input produce empty strings; the
            // earlier non-empty filter on `raw` makes this unreachable
            // for real input, but the guard keeps the contract local.
            if !b64_standard.is_empty() {
                patterns.push(b64_standard);
            }
            if !b64_url.is_empty() {
                patterns.push(b64_url);
            }
            if !hex_val.is_empty() {
                patterns.push(hex_val);
            }
        }
        patterns.sort();
        patterns.dedup();
        patterns
    }

    /// Compile an `AhoCorasick` automaton over `values`. Pulled out so
    /// `Clone` can rebuild the automaton from the retained pattern list
    /// without re-deriving the constructor's deduplication step.
    fn from_values(values: Vec<String>) -> Self {
        let ac = if values.is_empty() {
            None
        } else {
            // `LeftmostLongest` preserves the legacy semantics: when two
            // secrets overlap at the same start position, the longer one
            // wins so a shorter substring secret never slices a longer
            // one mid-token.
            //
            // `expect` here is acceptable: the only documented failure
            // mode of `AhoCorasick::new` over a non-empty `&[String]`
            // input is exhausting the internal state-id space, which
            // requires far more than the handful of secrets a real
            // pipeline registers.
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

    /// Replace every occurrence of any known secret with `"***"`.
    /// Returns `Borrowed` when nothing changes so no-op callers pay no
    /// allocation.
    #[must_use]
    pub fn redact<'a>(&self, s: &'a str) -> Cow<'a, str> {
        let Some(ac) = self.ac.as_ref() else {
            return Cow::Borrowed(s);
        };
        if !ac.is_match(s) {
            return Cow::Borrowed(s);
        }
        let mut out = String::with_capacity(s.len());
        let mut last = 0;
        for mat in ac.find_iter(s) {
            out.push_str(&s[last..mat.start()]);
            out.push_str("***");
            last = mat.end();
        }
        out.push_str(&s[last..]);
        Cow::Owned(out)
    }

    /// Recursively redact string leaves inside a JSON value. Numbers,
    /// booleans, and nulls pass through unchanged; objects and arrays
    /// rebuild themselves with redacted children.
    #[must_use]
    pub fn redact_json(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.redact(s).into_owned()),
            Value::Array(items) => {
                Value::Array(items.iter().map(|i| self.redact_json(i)).collect())
            },
            Value::Object(map) => {
                let mut out = serde_json::Map::with_capacity(map.len());
                for (k, val) in map {
                    out.insert(k.clone(), self.redact_json(val));
                }
                Value::Object(out)
            },
            // Non-string primitives cannot carry a secret by themselves —
            // return unchanged so the caller skips any allocation.
            other => other.clone(),
        }
    }
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
        let value = json!({
            "field": "leak topsecret here",
            "count": 42,
            "flag": true,
            "missing": null,
        });
        let out = r.redact_json(&value);
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
        let value = json!({
            "outer": { "inner": "topsecret" },
            "list": ["topsecret", 42, true],
        });
        let out = r.redact_json(&value);
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
}
