//! Template-rendering context: pipeline `vars:`, resolved `env.*` and
//! `secrets.*` maps and completed node outputs, kept as the JSON value
//! `MiniJinja` renders against.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

/// One template context. `vars`, `env` and `secrets` are fixed at
/// construction; `nodes` grows through `record_node_output`.
///
/// The render value is held as an `Arc` so each node's render shares it
/// instead of deep-cloning every prior output; `record_node_output` only
/// copies when a still-live render scope holds the previous snapshot.
#[derive(Debug, Clone)]
pub struct Context {
    snapshot: Arc<Value>,
}

impl Context {
    /// Seed from the pipeline's `vars:` plus the `env` and `secrets` maps
    /// the caller already resolved. The process environment is not
    /// inherited; templates see only what the caller passes in.
    #[must_use]
    pub fn new(
        vars: BTreeMap<String, Value>,
        env: BTreeMap<String, String>,
        secrets: BTreeMap<String, String>,
    ) -> Self {
        let strings = |m: BTreeMap<String, String>| -> Value {
            Value::Object(m.into_iter().map(|(k, v)| (k, Value::String(v))).collect())
        };
        Self {
            snapshot: Arc::new(json!({
                "vars": Value::Object(vars.into_iter().collect()),
                "nodes": {},
                "env": strings(env),
                "secrets": strings(secrets),
            })),
        }
    }

    /// Store a completed node's output under `nodes.<id>`. The engine
    /// records failed shell nodes too, so recovery branches can read
    /// their `stderr` and `exit_code`.
    pub fn record_node_output(&mut self, node_id: &str, output: Value) {
        Arc::make_mut(&mut self.snapshot)["nodes"][node_id] = output;
    }

    /// The `{ vars, nodes, env, secrets }` value templates render
    /// against. Secrets render like any namespace; the event redactor is
    /// what keeps them out of the log stream.
    #[must_use]
    pub fn snapshot_for_template(&self) -> Arc<Value> {
        Arc::clone(&self.snapshot)
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
}
