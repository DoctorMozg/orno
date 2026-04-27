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
use serde_json::Value;

/// Replaces every occurrence of a known secret value with `"***"`.
///
/// Build once per run from the resolved secret map and share across
/// the engine's emission sites. Empty-valued secrets are dropped — an
/// empty-string match would replace every zero-width position in the
/// haystack, which is worse than not redacting at all.
///
/// Internally, the secret list is compiled into an Aho-Corasick
/// automaton so a single pass over the haystack catches every match
/// regardless of how many secrets are registered. `MatchKind::LeftmostLongest`
/// preserves the previous "longest secret wins" behavior in the rare
/// case where two secrets overlap.
#[derive(Default)]
pub struct Redactor {
    /// Original secret values, retained for `Clone` reconstruction
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
    #[must_use]
    pub fn new(secrets: &BTreeMap<String, String>) -> Self {
        let mut values: Vec<String> = secrets
            .values()
            .filter(|v| !v.is_empty())
            .cloned()
            .collect();
        values.sort();
        values.dedup();
        Self::from_values(values)
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
