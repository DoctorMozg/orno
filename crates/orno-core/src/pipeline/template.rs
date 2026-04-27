//! `MiniJinja` environment for prompt rendering.
//!
//! `auto_escape` is explicitly forced to `None`. Jinja's default
//! extension-heuristic will HTML-escape anything that looks like a template
//! filename — rendering prompts that then break tool-call JSON downstream.
//!
//! Compiled templates are cached by content-addressable name so the same
//! source string is parsed only once. The cache is LRU-bounded at 128
//! entries so a long-lived engine (e.g. in a multi-pipeline process) cannot
//! accumulate unbounded compiled state. The cache key is a blake3 hash of
//! the source; the same hex string is the `MiniJinja` template name so a
//! cache hit is a direct `get_template` lookup.

use std::num::NonZeroUsize;
use std::sync::Mutex;

use lru::LruCache;
use minijinja::{AutoEscape, Environment, UndefinedBehavior};

use crate::error::PipelineError;

/// Maximum number of bytes accepted for a single template source string.
/// Templates above this threshold are rejected at render time rather than
/// at parse time so the cap is enforced even when the template comes from
/// user YAML (untrusted input). 64 KiB is several orders of magnitude
/// larger than any legitimate prompt template.
const MAX_TEMPLATE_SOURCE_BYTES: usize = 64 * 1024;

/// Maximum number of distinct compiled templates retained in the LRU cache.
/// 128 covers any realistic single-process pipeline library; entries evicted
/// past this bound are re-compiled on next use (a parse cost, not a
/// correctness issue).
const TEMPLATE_CACHE_CAPACITY: usize = 128;

pub struct TemplateEngine {
    inner: Mutex<Inner>,
}

struct Inner {
    env: Environment<'static>,
    /// LRU map from blake3 hash → hex template name. When the cache is at
    /// capacity, the least-recently-used entry is evicted and its compiled
    /// template removed from `env` so memory is bounded.
    cache: LruCache<blake3::Hash, String>,
}

impl TemplateEngine {
    #[must_use]
    pub fn new() -> Self {
        let mut env = Environment::new();
        env.set_auto_escape_callback(|_| AutoEscape::None);
        // Missing `env.FOO` or `secrets.FOO` references must surface
        // as hard render errors, not silent empty strings. Strict
        // undefined applies uniformly across every namespace.
        env.set_undefined_behavior(UndefinedBehavior::Strict);
        let capacity = NonZeroUsize::new(TEMPLATE_CACHE_CAPACITY)
            .expect("TEMPLATE_CACHE_CAPACITY is a non-zero constant");
        Self {
            inner: Mutex::new(Inner {
                env,
                cache: LruCache::new(capacity),
            }),
        }
    }

    pub fn render(
        &self,
        name: &str,
        source: &str,
        ctx: &serde_json::Value,
    ) -> Result<String, PipelineError> {
        if source.len() > MAX_TEMPLATE_SOURCE_BYTES {
            return Err(PipelineError::Validation(format!(
                "template `{name}` source exceeds {MAX_TEMPLATE_SOURCE_BYTES} bytes ({} bytes)",
                source.len(),
            )));
        }

        let hash = blake3::hash(source.as_bytes());

        // Poison recovery matches `InMemorySink` / `StreamingSink` — a
        // panicking task on a sibling render must not starve subsequent
        // template renders. The cache state is set-and-test, so a partial
        // mutation across a panic is bounded: at worst, a hash gets
        // inserted without its template registered (re-insert is
        // idempotent) or vice versa (the next `add_template_owned`
        // returns the same source, then `get_template` succeeds).
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let template_name = if let Some(existing) = inner.cache.get(&hash) {
            existing.clone()
        } else {
            let hex = hash.to_hex().to_string();
            inner
                .env
                .add_template_owned(hex.clone(), source.to_string())
                .map_err(|source| PipelineError::Template {
                    name: name.to_string(),
                    source,
                })?;
            // `LruCache::put` returns the evicted value when the cache is at
            // capacity. Remove the corresponding compiled template from the
            // environment so memory stays bounded.
            if let Some(evicted_name) = inner.cache.put(hash, hex.clone()) {
                inner.env.remove_template(&evicted_name);
            }
            hex
        };

        let tmpl =
            inner
                .env
                .get_template(&template_name)
                .map_err(|source| PipelineError::Template {
                    name: name.to_string(),
                    source,
                })?;
        tmpl.render(ctx).map_err(|source| PipelineError::Template {
            name: name.to_string(),
            source,
        })
    }
}

impl Default for TemplateEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_simple_template() {
        let engine = TemplateEngine::new();
        let out = engine
            .render("greet", "hello {{ name }}", &json!({ "name": "world" }))
            .expect("template renders");
        assert_eq!(out, "hello world");
    }

    #[test]
    fn repeated_render_of_same_source_uses_cache_and_yields_same_output() {
        let engine = TemplateEngine::new();
        let source = "answer is {{ value }}";
        for value in 0..50 {
            let out = engine
                .render("answer", source, &json!({ "value": value }))
                .expect("template renders");
            assert_eq!(out, format!("answer is {value}"));
        }
        // After 50 calls the cache must hold exactly one entry — proves
        // the second-and-onwards renders did not re-parse.
        let inner = engine.inner.lock().expect("mutex");
        assert_eq!(inner.cache.len(), 1);
    }

    #[test]
    fn distinct_sources_get_distinct_cache_entries() {
        let engine = TemplateEngine::new();
        engine
            .render("a", "{{ x }}", &json!({ "x": 1 }))
            .expect("renders");
        engine
            .render("b", "{{ y }}", &json!({ "y": 2 }))
            .expect("renders");
        engine.render("c", "literal", &json!({})).expect("renders");
        let inner = engine.inner.lock().expect("mutex");
        assert_eq!(inner.cache.len(), 3);
    }

    #[test]
    fn template_above_size_cap_is_rejected() {
        let engine = TemplateEngine::new();
        let huge = "a".repeat(MAX_TEMPLATE_SOURCE_BYTES + 1);
        let err = engine
            .render("oversize", &huge, &json!({}))
            .expect_err("must reject");
        match err {
            PipelineError::Validation(msg) => {
                assert!(msg.contains("oversize"), "message must name template");
            },
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn missing_variable_is_strict_error() {
        let engine = TemplateEngine::new();
        let err = engine
            .render("strict", "value: {{ missing }}", &json!({}))
            .expect_err("strict undefined must error");
        assert!(matches!(err, PipelineError::Template { .. }));
    }

    #[test]
    fn invalid_template_syntax_surfaces_template_error() {
        let engine = TemplateEngine::new();
        let err = engine
            .render("broken", "{{ unterminated", &json!({}))
            .expect_err("syntax error must surface");
        assert!(matches!(err, PipelineError::Template { .. }));
    }
}
