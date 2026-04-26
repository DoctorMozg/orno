//! Template-rendering context. Threads node outputs, pipeline
//! `vars:`, plus resolved `env.*` and `secrets.*` maps into the
//! `MiniJinja` render call. Designed for per-branch shadowing in
//! parallel execution; v0.1 uses one flat context and grows it as
//! each node succeeds.

use std::collections::BTreeMap;

use serde_json::{Value, json};

/// One template context. `vars`, `env`, and `secrets` are populated
/// at construction and never mutated. `nodes` grows as upstream
/// nodes succeed (via `record_node_output`).
#[derive(Debug, Clone)]
pub struct Context {
    vars: BTreeMap<String, Value>,
    env: BTreeMap<String, String>,
    secrets: BTreeMap<String, String>,
    nodes: BTreeMap<String, Value>,
}

/// A key-level conflict surfaced by `Context::merge`. Does not
/// block the merge — the right-hand context wins; the conflict is
/// advisory (logged as a warning by the scheduler when parallel
/// execution lands).
#[derive(Debug, Clone)]
pub struct ContextConflict {
    /// Dotted path like `nodes.fetch.exit_code` or `vars.foo`.
    pub path: String,
    pub left: Value,
    pub right: Value,
}

impl Context {
    /// Seed from the pipeline's `vars:` map plus `env` and `secrets`
    /// maps already resolved by the caller from CLI flags, `.env`
    /// files, and `pass_env:`. The process environment is
    /// not auto-inherited; only the entries the caller places in
    /// `env` and `secrets` are visible to templates.
    #[must_use]
    pub fn new(
        vars: BTreeMap<String, Value>,
        env: BTreeMap<String, String>,
        secrets: BTreeMap<String, String>,
    ) -> Self {
        Self {
            vars,
            env,
            secrets,
            nodes: BTreeMap::new(),
        }
    }

    /// Bare context with no vars, env, or secrets — for tests and
    /// embedders that construct context programmatically. Prefer
    /// `Context::new` when caller-supplied maps are available.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            vars: BTreeMap::new(),
            env: BTreeMap::new(),
            secrets: BTreeMap::new(),
            nodes: BTreeMap::new(),
        }
    }

    /// Store a completed upstream node's output under `nodes.<id>`.
    /// For shell nodes the output is `{ stdout, stderr, exit_code }`;
    /// for agent nodes it is `{ output, state?, finish_reason, usage }`
    /// where `output` is the final assistant message and `state` is
    /// present only when the agent made at least one `SetState` call
    /// (`SetState` writes node-scoped state). The scheduler is
    /// authoritative for calling this only on success (failed nodes
    /// are `Skipped`, not recorded).
    pub fn record_node_output(&mut self, node_id: &str, output: Value) {
        self.nodes.insert(node_id.to_string(), output);
    }

    /// Serialize the context in the exact shape `MiniJinja` expects:
    /// `{ "vars": {...}, "nodes": {...}, "env": {...}, "secrets": {...} }`.
    /// The secrets map renders here just like any other namespace; the
    /// redactor applied to emitted events is what keeps values from
    /// leaking into the log stream.
    /// Call site: `TemplateEngine::render(name, source, &ctx.snapshot_for_template())`.
    #[must_use]
    pub fn snapshot_for_template(&self) -> Value {
        json!({
            "vars": self.vars,
            "nodes": self.nodes,
            "env": self.env,
            "secrets": self.secrets,
        })
    }

    /// Merge `other` into `self`, last-writer-wins. Returns every
    /// overlapping key as a `ContextConflict` so callers can log the
    /// shadowed value. Unused in v0.1 sequential execution; the API
    /// exists so parallel branches can merge their child contexts
    /// back into the parent DAG context without adding a new method
    /// later.
    pub fn merge(&mut self, other: Context) -> Vec<ContextConflict> {
        let mut conflicts = Vec::new();
        // Merge vars.
        for (k, v) in other.vars {
            if let Some(prev) = self.vars.insert(k.clone(), v.clone())
                && prev != v
            {
                conflicts.push(ContextConflict {
                    path: format!("vars.{k}"),
                    left: prev,
                    right: v,
                });
            }
        }
        // Merge nodes.
        for (k, v) in other.nodes {
            if let Some(prev) = self.nodes.insert(k.clone(), v.clone())
                && prev != v
            {
                conflicts.push(ContextConflict {
                    path: format!("nodes.{k}"),
                    left: prev,
                    right: v,
                });
            }
        }
        // Env and secrets are not merged — each branch keeps its
        // construction-time snapshot.
        conflicts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn new_exposes_vars() {
        let mut vars = BTreeMap::new();
        vars.insert("foo".to_string(), json!("bar"));
        let ctx = Context::new(vars, BTreeMap::new(), BTreeMap::new());

        let snapshot = ctx.snapshot_for_template();
        assert_eq!(snapshot["vars"]["foo"], json!("bar"));
    }

    #[test]
    fn record_node_output_nested_under_nodes() {
        let mut ctx = Context::new(BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
        ctx.record_node_output("n1", json!({"x": 1}));

        let snapshot = ctx.snapshot_for_template();
        assert_eq!(snapshot["nodes"]["n1"]["x"], json!(1));
    }

    #[test]
    fn new_exposes_env_as_given() {
        // env is passed in by the caller — the process
        // environment is not auto-inherited. Verify that exactly the
        // entries handed to `new` show up in the snapshot, nothing more.
        let mut env = BTreeMap::new();
        env.insert(
            "CI_LOG_URL".to_string(),
            "https://ci.example/42".to_string(),
        );
        env.insert("PR_NUMBER".to_string(), "123".to_string());
        let ctx = Context::new(BTreeMap::new(), env.clone(), BTreeMap::new());

        let snapshot = ctx.snapshot_for_template();
        let env_obj = snapshot["env"].as_object().expect("env is a JSON object");
        assert_eq!(env_obj.len(), 2, "only caller-provided entries are exposed");
        for (k, v) in &env {
            assert_eq!(env_obj.get(k), Some(&json!(v)));
        }
    }

    #[test]
    fn new_does_not_inherit_process_env() {
        // Regression guard: an empty env map must produce an
        // empty env snapshot, independent of the current process env.
        let ctx = Context::new(BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
        let snapshot = ctx.snapshot_for_template();
        let env_obj = snapshot["env"].as_object().expect("env is a JSON object");
        assert!(
            env_obj.is_empty(),
            "env must be empty when no entries are passed in",
        );
    }

    #[test]
    fn new_exposes_secrets_as_given() {
        // The `secrets.*` namespace renders in the template
        // context like any other. The redactor on the event stream
        // (not the engine) is what keeps values out of logs.
        let mut secrets = BTreeMap::new();
        secrets.insert("GITHUB_TOKEN".to_string(), "ghp_placeholder".to_string());
        let ctx = Context::new(BTreeMap::new(), BTreeMap::new(), secrets.clone());

        let snapshot = ctx.snapshot_for_template();
        let secrets_obj = snapshot["secrets"]
            .as_object()
            .expect("secrets is a JSON object");
        assert_eq!(secrets_obj.len(), 1);
        assert_eq!(
            secrets_obj.get("GITHUB_TOKEN"),
            Some(&json!("ghp_placeholder")),
        );
    }

    #[test]
    fn snapshot_shape_matches_yaml_spec() {
        let ctx = Context::new(BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
        let snapshot = ctx.snapshot_for_template();

        let obj = snapshot.as_object().expect("snapshot is a JSON object");
        assert_eq!(obj.len(), 4, "snapshot must have exactly four keys");
        assert!(obj.contains_key("vars"));
        assert!(obj.contains_key("nodes"));
        assert!(obj.contains_key("env"));
        assert!(obj.contains_key("secrets"));
    }

    #[test]
    fn merge_with_conflict_reports_every_overlap() {
        let mut left_vars = BTreeMap::new();
        left_vars.insert("k1".to_string(), json!(1));
        let mut left = Context::new(left_vars, BTreeMap::new(), BTreeMap::new());
        left.record_node_output("n1", json!({"a": 1}));

        let mut right_vars = BTreeMap::new();
        right_vars.insert("k1".to_string(), json!(2));
        right_vars.insert("k2".to_string(), json!(3));
        let mut right = Context::new(right_vars, BTreeMap::new(), BTreeMap::new());
        right.record_node_output("n1", json!({"a": 2}));
        right.record_node_output("n2", json!({"b": 5}));

        let conflicts = left.merge(right);
        assert_eq!(conflicts.len(), 2, "expected exactly two conflicts");

        let vars_conflict = conflicts
            .iter()
            .find(|c| c.path == "vars.k1")
            .expect("vars.k1 conflict missing");
        assert_eq!(vars_conflict.left, json!(1));
        assert_eq!(vars_conflict.right, json!(2));

        let nodes_conflict = conflicts
            .iter()
            .find(|c| c.path == "nodes.n1")
            .expect("nodes.n1 conflict missing");
        assert_eq!(nodes_conflict.left, json!({"a": 1}));
        assert_eq!(nodes_conflict.right, json!({"a": 2}));

        let snapshot = left.snapshot_for_template();
        assert_eq!(snapshot["vars"]["k1"], json!(2));
        assert_eq!(snapshot["vars"]["k2"], json!(3));
        assert_eq!(snapshot["nodes"]["n1"]["a"], json!(2));
        assert_eq!(snapshot["nodes"]["n2"]["b"], json!(5));
    }

    #[test]
    fn merge_without_conflict_returns_empty() {
        let mut left_vars = BTreeMap::new();
        left_vars.insert("k1".to_string(), json!(1));
        let mut left = Context::new(left_vars, BTreeMap::new(), BTreeMap::new());

        let mut right_vars = BTreeMap::new();
        right_vars.insert("k2".to_string(), json!(2));
        let right = Context::new(right_vars, BTreeMap::new(), BTreeMap::new());

        let conflicts = left.merge(right);
        assert!(conflicts.is_empty(), "disjoint merge must not conflict");

        let snapshot = left.snapshot_for_template();
        assert_eq!(snapshot["vars"]["k1"], json!(1));
        assert_eq!(snapshot["vars"]["k2"], json!(2));
    }

    #[test]
    fn merge_does_not_merge_env_or_secrets() {
        // env and secrets are per-branch construction-time
        // snapshots; `merge` only flows vars and nodes. Construct with
        // arguments that would produce a clear conflict if merge ever
        // touched them, then assert the left side is untouched.
        let mut left_env = BTreeMap::new();
        left_env.insert("X".to_string(), "left".to_string());
        let mut left_secrets = BTreeMap::new();
        left_secrets.insert("S".to_string(), "left_s".to_string());
        let mut left = Context::new(BTreeMap::new(), left_env, left_secrets);

        let mut right_env = BTreeMap::new();
        right_env.insert("X".to_string(), "right".to_string());
        right_env.insert("Y".to_string(), "right_only".to_string());
        let mut right_secrets = BTreeMap::new();
        right_secrets.insert("S".to_string(), "right_s".to_string());
        let right = Context::new(BTreeMap::new(), right_env, right_secrets);

        drop(left.merge(right));

        let snapshot = left.snapshot_for_template();
        assert_eq!(snapshot["env"]["X"], json!("left"));
        assert!(
            snapshot["env"].get("Y").is_none(),
            "right-only env key must not leak into left",
        );
        assert_eq!(snapshot["secrets"]["S"], json!("left_s"));
    }
}
