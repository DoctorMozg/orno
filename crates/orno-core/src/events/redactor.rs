//! Secret-value redactor for event emission. Rendered `secrets.*`
//! values (ADR 0020) must not reach the event log; the scheduler
//! constructs a `Redactor` from the run's secret map and runs every
//! user-visible string through it before handing the envelope to a
//! sink.
//!
//! The redactor is value-based (not key-based): we substitute raw
//! secret strings inside prompts, node outputs, and other payloads,
//! because the sensitive content flows by value once templates have
//! been rendered. Sorting longest-first guarantees a secret that is a
//! substring of another longer secret cannot sneak through by being
//! matched first.

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde_json::Value;

/// Replaces every occurrence of a known secret value with `"***"`.
///
/// Build once per run from the resolved secret map and share across
/// the engine's emission sites. Empty-valued secrets are dropped — an
/// empty-string match would replace every zero-width position in the
/// haystack, which is worse than not redacting at all.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    values: Vec<String>,
}

impl Redactor {
    /// Build a redactor from the run's `secrets.*` namespace. Duplicate
    /// values collapse to one entry; the final list is sorted
    /// longest-first so overlapping secrets redact in the expected
    /// order (a longer secret is matched before any of its prefixes).
    #[must_use]
    pub fn new(secrets: &BTreeMap<String, String>) -> Self {
        let mut values: Vec<String> = secrets
            .values()
            .filter(|v| !v.is_empty())
            .cloned()
            .collect();
        values.sort();
        values.dedup();
        // Longest first: a secret that is a substring of another secret
        // must not win the replacement race.
        values.sort_by_key(|v| std::cmp::Reverse(v.len()));
        Self { values }
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
        if self.values.is_empty() || !self.values.iter().any(|v| s.contains(v.as_str())) {
            return Cow::Borrowed(s);
        }
        let mut out = s.to_string();
        for v in &self.values {
            if out.contains(v.as_str()) {
                out = out.replace(v.as_str(), "***");
            }
        }
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
