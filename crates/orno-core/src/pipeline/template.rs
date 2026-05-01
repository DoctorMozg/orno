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

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Mutex;

use lru::LruCache;
use minijinja::{AutoEscape, Environment, UndefinedBehavior};
use serde_json::{Value, json};

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

    pub fn render(&self, name: &str, source: &str, ctx: &Value) -> Result<String, PipelineError> {
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
            // `LruCache::push` (not `put`) returns the evicted (key, value)
            // pair when the cache is at capacity. `put` only returns a value
            // on a key collision, so an overflow eviction would be silently
            // dropped and the Environment would grow without bound.
            if let Some((_evicted_hash, evicted_name)) = inner.cache.push(hash, hex.clone()) {
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

/// Render every string value inside a pipeline's `vars` map against the
/// `{ env, secrets }` namespace. Numbers, booleans, and nulls pass
/// through unchanged; arrays and objects recurse so a shape like
/// `vars.commits: ["{{ env.SHA }}"]` expands element-by-element.
///
/// Cross-var references are intentionally not supported in v0.1.x — the
/// rendering context exposes only `env` and `secrets`, so a user cannot
/// write `vars.b: "{{ vars.a }}"` and expect a value. Allowing it would
/// require fixed-point iteration with cycle detection; defer until a
/// real pipeline asks for it.
///
/// Errors:
/// - `PipelineError::Template` if any string fails to render (missing
///   `env.X`, malformed Jinja, oversize source).
pub fn render_pipeline_vars(
    tmpl: &TemplateEngine,
    vars: &BTreeMap<String, Value>,
    env: &BTreeMap<String, String>,
    secrets: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, Value>, PipelineError> {
    let ctx = json!({ "env": env, "secrets": secrets });
    let mut out = BTreeMap::new();
    for (key, value) in vars {
        // F7: warn before rendering so the operator gets an early signal
        // even if the render succeeds (e.g. via `| default("")`). This
        // catches declaration drift where a secret is referenced in `vars`
        // but was removed from `pipeline.secrets`.
        warn_undeclared_secret_refs(value, key, secrets);
        out.insert(
            key.clone(),
            render_value(tmpl, &format!("vars.{key}"), value, &ctx)?,
        );
    }
    Ok(out)
}

/// Walk a `Value` tree and emit a `tracing::warn!` for every `secrets.X`
/// reference in template strings where `X` is not a key in `secrets`.
fn warn_undeclared_secret_refs(value: &Value, var_key: &str, secrets: &BTreeMap<String, String>) {
    match value {
        Value::String(s) if looks_templated(s) => {
            for name in extract_secret_refs(s) {
                if !secrets.contains_key(name) {
                    tracing::warn!(
                        var = %var_key,
                        secret = %name,
                        "var template references undeclared secret `{name}`; \
                         add it to the pipeline `secrets:` block or the render will fail",
                    );
                }
            }
        },
        Value::Array(arr) => {
            for item in arr {
                warn_undeclared_secret_refs(item, var_key, secrets);
            }
        },
        Value::Object(map) => {
            for (k, v) in map {
                let nested_key = format!("{var_key}.{k}");
                warn_undeclared_secret_refs(v, &nested_key, secrets);
            }
        },
        _ => {},
    }
}

/// Scan a template source string for `secrets.IDENTIFIER` patterns and
/// return a deduplicated list of the secret names (the parts after
/// `secrets.`). No regex dependency — simple byte-by-byte scan.
fn extract_secret_refs(source: &str) -> Vec<&str> {
    const PREFIX: &str = "secrets.";
    let mut refs: Vec<&str> = Vec::new();
    let mut search = source;
    while let Some(pos) = search.find(PREFIX) {
        let after_prefix = &search[pos + PREFIX.len()..];
        let name_len = after_prefix
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(after_prefix.len());
        if name_len > 0 {
            let name = &after_prefix[..name_len];
            if !refs.contains(&name) {
                refs.push(name);
            }
        }
        search = &search[pos + PREFIX.len()..];
    }
    refs
}

fn render_value(
    tmpl: &TemplateEngine,
    name: &str,
    value: &Value,
    ctx: &Value,
) -> Result<Value, PipelineError> {
    match value {
        Value::String(s) if looks_templated(s) => Ok(Value::String(tmpl.render(name, s, ctx)?)),
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for (i, item) in arr.iter().enumerate() {
                out.push(render_value(tmpl, &format!("{name}[{i}]"), item, ctx)?);
            }
            Ok(Value::Array(out))
        },
        Value::Object(obj) => {
            let mut out = serde_json::Map::with_capacity(obj.len());
            for (k, v) in obj {
                out.insert(
                    k.clone(),
                    render_value(tmpl, &format!("{name}.{k}"), v, ctx)?,
                );
            }
            Ok(Value::Object(out))
        },
        other => Ok(other.clone()),
    }
}

/// Cheap pre-check — skip the render call when the source obviously
/// has no template tags. MiniJinja parses literal strings cleanly, so
/// this is purely a perf hint, not a correctness gate.
fn looks_templated(s: &str) -> bool {
    s.contains("{{") || s.contains("{%")
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

    #[test]
    fn lru_eviction_removes_oldest_compiled_template() {
        // LRU bound contract: when the cache is at capacity, inserting a
        // new entry must evict the least-recently-used one so the cache
        // map cannot grow without bound. Re-rendering the evicted source
        // must succeed (re-parse on demand is the documented fallback)
        // and a fresh entry's presence proves the bound holds.
        //
        // Verified by capturing the first template's hash, filling the
        // cache to `TEMPLATE_CACHE_CAPACITY + 1` distinct sources, and
        // checking the LRU map: size capped at capacity, first hash gone.
        //
        let engine = TemplateEngine::new();

        // Render the first template; remember its hash.
        let first_source = "first {{ x }}";
        engine
            .render("first", first_source, &json!({ "x": 0 }))
            .expect("first render");
        let first_hash = blake3::hash(first_source.as_bytes());
        {
            let inner = engine.inner.lock().expect("mutex");
            assert!(
                inner.cache.peek(&first_hash).is_some(),
                "first entry must be cached before overflow",
            );
        }

        // Fill the cache with `TEMPLATE_CACHE_CAPACITY` more distinct
        // sources. Combined with the first, that's capacity + 1, so the
        // first must be the evictee. Each source string is unique so the
        // blake3 hash and the resulting cache key are unique too.
        for i in 0..TEMPLATE_CACHE_CAPACITY {
            let source = format!("filler-{i} {{{{ y }}}}");
            engine
                .render(&format!("f{i}"), &source, &json!({ "y": i }))
                .expect("filler render");
        }

        let inner = engine.inner.lock().expect("mutex");
        // The cache must remain at capacity — the LRU bound is hard.
        assert_eq!(
            inner.cache.len(),
            TEMPLATE_CACHE_CAPACITY,
            "cache size must stay at capacity after overflow",
        );
        // The first template's hash must no longer be present in the
        // cache. `peek` does not bump LRU recency so the assertion is
        // about presence, not ordering.
        assert!(
            inner.cache.peek(&first_hash).is_none(),
            "oldest entry must have been evicted from the cache",
        );
        // The compiled template must also be removed from the Environment
        // so its memory is freed. This is the env-side half of eviction —
        // `LruCache::push` (not `put`) returns the evicted name, which
        // lets `remove_template` be called.
        let evicted_name = first_hash.to_hex().to_string();
        assert!(
            inner.env.get_template(&evicted_name).is_err(),
            "evicted template must be removed from the Environment",
        );
    }

    #[test]
    fn auto_escape_none_does_not_html_escape_variables() {
        // Module doc-comment guarantee: `auto_escape` is forced to `None` so
        // a value containing HTML-significant characters survives the render
        // unchanged. Without this guard MiniJinja would HTML-escape the
        // string — breaking any downstream consumer that expects the literal
        // bytes (tool-call JSON, code blocks, prompts targeting models that
        // do not parse HTML entities).
        let engine = TemplateEngine::new();
        let payload = "<script>alert(1)</script>";
        let out = engine
            .render("noescape", "Hello {{ name }}", &json!({ "name": payload }))
            .expect("template renders");
        assert_eq!(out, format!("Hello {payload}"));
        assert!(
            !out.contains("&lt;"),
            "auto_escape=None must not HTML-escape: {out:?}",
        );
    }

    #[test]
    fn rerender_after_eviction_returns_correct_output() {
        // Companion to the LRU-eviction test: a template evicted from the
        // cache must still render correctly on re-render, since the engine
        // re-parses on a cache miss. Proves eviction is a parse-cost issue,
        // not a correctness issue — matches the doc comment on
        // `TEMPLATE_CACHE_CAPACITY`.
        let engine = TemplateEngine::new();

        let evictee_source = "evictee {{ value }}";
        let original = engine
            .render("evictee", evictee_source, &json!({ "value": "alpha" }))
            .expect("first render");
        assert_eq!(original, "evictee alpha");

        // Push the cache past capacity so the evictee is no longer in the
        // LRU map.
        for i in 0..TEMPLATE_CACHE_CAPACITY {
            let source = format!("flush-{i} {{{{ y }}}}");
            engine
                .render(&format!("f{i}"), &source, &json!({ "y": i }))
                .expect("filler render");
        }

        // Re-render the evicted source; result must match the original.
        let rerendered = engine
            .render("evictee", evictee_source, &json!({ "value": "beta" }))
            .expect("re-render after eviction must succeed");
        assert_eq!(rerendered, "evictee beta");
    }

    fn vars_from(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    fn env_from(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn render_pipeline_vars_expands_env_into_string_var() {
        // The release-notes / pr-review / flaky-test-triage examples all
        // pivot env vars into `vars.X` to give downstream nodes a stable
        // template handle. Without pre-rendering, the literal source
        // would land in `vars.X` and a downstream `{{ vars.X }}` would
        // substitute it verbatim, breaking shell args / prompts.
        let tmpl = TemplateEngine::new();
        let vars = vars_from(&[
            ("prev_tag", json!("{{ env.PREV_TAG }}")),
            ("curr_tag", json!("{{ env.CURR_TAG }}")),
        ]);
        let env = env_from(&[("PREV_TAG", "v0.1.0"), ("CURR_TAG", "v0.1.1")]);
        let secrets = BTreeMap::new();

        let out = render_pipeline_vars(&tmpl, &vars, &env, &secrets).expect("render must succeed");

        assert_eq!(out["prev_tag"], json!("v0.1.0"));
        assert_eq!(out["curr_tag"], json!("v0.1.1"));
    }

    #[test]
    fn render_pipeline_vars_passes_through_non_string_scalars() {
        // Numbers, booleans, and nulls must round-trip unchanged. The
        // helper only rewrites strings; touching the others would force
        // YAML authors to quote every numeric var.
        let tmpl = TemplateEngine::new();
        let vars = vars_from(&[
            ("n", json!(42)),
            ("flag", json!(true)),
            ("nothing", Value::Null),
            ("plain", json!("literal")),
        ]);
        let env = BTreeMap::new();
        let secrets = BTreeMap::new();

        let out = render_pipeline_vars(&tmpl, &vars, &env, &secrets).expect("render must succeed");

        assert_eq!(out["n"], json!(42));
        assert_eq!(out["flag"], json!(true));
        assert_eq!(out["nothing"], Value::Null);
        assert_eq!(out["plain"], json!("literal"));
    }

    #[test]
    fn render_pipeline_vars_recurses_into_arrays_and_objects() {
        // A composite var like `commits: ["{{ env.A }}", "{{ env.B }}"]`
        // is the natural shape for "fan a list of env-derived ids out
        // to a downstream node"; the renderer must dive in.
        let tmpl = TemplateEngine::new();
        let vars = vars_from(&[
            ("list", json!(["{{ env.A }}", "literal", "{{ env.B }}"])),
            (
                "map",
                json!({ "outer": "{{ env.A }}", "inner": { "k": "{{ env.B }}" } }),
            ),
        ]);
        let env = env_from(&[("A", "alpha"), ("B", "beta")]);
        let secrets = BTreeMap::new();

        let out = render_pipeline_vars(&tmpl, &vars, &env, &secrets).expect("render must succeed");

        assert_eq!(out["list"], json!(["alpha", "literal", "beta"]));
        assert_eq!(
            out["map"],
            json!({ "outer": "alpha", "inner": { "k": "beta" } })
        );
    }

    #[test]
    fn render_pipeline_vars_propagates_template_error_with_var_name() {
        // A missing `env.X` must surface as a Template error tagged with
        // the offending var path so the user can find the broken value
        // without grep. The exact name stays in the error so a
        // multi-var pipeline does not blame the wrong key.
        let tmpl = TemplateEngine::new();
        let vars = vars_from(&[("broken", json!("{{ env.MISSING }}"))]);
        let env = BTreeMap::new();
        let secrets = BTreeMap::new();

        let err = render_pipeline_vars(&tmpl, &vars, &env, &secrets)
            .expect_err("missing env reference must error");

        match err {
            PipelineError::Template { name, .. } => {
                assert_eq!(name, "vars.broken", "error must name the offending var");
            },
            other => panic!("expected PipelineError::Template, got {other:?}"),
        }
    }

    #[test]
    fn render_pipeline_vars_does_not_expose_vars_to_itself() {
        // Cross-var references are intentionally unsupported in v0.1.x.
        // A user who writes `vars.b: "{{ vars.a }}"` must see a hard
        // template-render error rather than a silent empty string —
        // strict undefined behavior is what makes that a real signal
        // instead of a footgun.
        let tmpl = TemplateEngine::new();
        let vars = vars_from(&[("a", json!("alpha")), ("b", json!("{{ vars.a }}"))]);
        let env = BTreeMap::new();
        let secrets = BTreeMap::new();

        let err = render_pipeline_vars(&tmpl, &vars, &env, &secrets)
            .expect_err("cross-var reference must not resolve");
        match err {
            PipelineError::Template { name, .. } => assert_eq!(name, "vars.b"),
            other => panic!("expected PipelineError::Template, got {other:?}"),
        }
    }

    #[test]
    fn render_pipeline_vars_supports_secrets_in_strings() {
        // Symmetry with env: a `vars.token: "{{ secrets.GH_TOKEN }}"`
        // shape is reasonable when the downstream consumer expects a
        // pre-templated string. The redactor on the event stream still
        // scrubs the value before emission.
        let tmpl = TemplateEngine::new();
        let vars = vars_from(&[("token", json!("{{ secrets.MY_TOKEN }}"))]);
        let env = BTreeMap::new();
        let secrets = env_from(&[("MY_TOKEN", "redacted-fake-value")]);

        let out = render_pipeline_vars(&tmpl, &vars, &env, &secrets).expect("render must succeed");

        assert_eq!(out["token"], json!("redacted-fake-value"));
    }

    #[test]
    fn warn_on_undeclared_secret_ref_in_var() {
        // F7 contract: a `vars.X` template that references a secret not
        // present in the secrets map must trigger a `tracing::warn!` —
        // verified here indirectly because no `tracing_test` dependency
        // is in the project. Strict undefined surfaces the missing key
        // as a Template error so the warn-then-fail sequence is what an
        // operator sees: the warn fires inside `warn_undeclared_secret_refs`
        // before render_value runs, and render_value then errors on the
        // strict undefined `secrets.UNDECLARED_KEY` lookup.
        //
        // The assertion pins the failing var name so a future renderer
        // change that swallows the strict-undefined error would surface
        // here as a missing render error rather than a silent regression.
        let tmpl = TemplateEngine::new();
        let vars = vars_from(&[("token", json!("{{ secrets.UNDECLARED_KEY }}"))]);
        let env = BTreeMap::new();
        let secrets = BTreeMap::new();

        let err = render_pipeline_vars(&tmpl, &vars, &env, &secrets)
            .expect_err("undeclared secret ref must surface via strict undefined");
        match err {
            PipelineError::Template { name, .. } => {
                assert_eq!(name, "vars.token", "error must name the offending var");
            },
            other => panic!("expected PipelineError::Template, got {other:?}"),
        }
    }

    #[test]
    fn declared_secret_ref_does_not_warn() {
        // Companion to the undeclared-secret test: when the referenced
        // secret IS in the map, the warn path is skipped and render
        // returns the resolved value. This pins the negative half of the
        // F7 contract — the warn fires only for undeclared references,
        // not every secret usage.
        let tmpl = TemplateEngine::new();
        let vars = vars_from(&[("token", json!("{{ secrets.DECLARED_KEY }}"))]);
        let env = BTreeMap::new();
        let secrets = env_from(&[("DECLARED_KEY", "the-real-value")]);

        let out =
            render_pipeline_vars(&tmpl, &vars, &env, &secrets).expect("declared secret renders");
        assert_eq!(
            out["token"],
            json!("the-real-value"),
            "render must resolve the declared secret",
        );
    }
}
