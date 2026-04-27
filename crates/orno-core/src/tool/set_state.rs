//! `SetState` tool — write a single top-level key under
//! `nodes.<self>.state.*`. Effect class
//! [`ToolEffect::ContextSelf`]; gate in [`LoopAgent`](crate::agent::LoopAgent)
//! pre-clears the call against `allow_context_writes`.
//!
//! Semantics are deliberately minimal: one key per call, single-level
//! only (no dotted paths), whole-value replacement. A second call with
//! the same key overwrites the prior value wholesale — no merge. The
//! whole `state` tree is re-serialized after every write and compared
//! against [`ToolError::StateTooLarge`]'s cap; oversize writes are
//! rolled back before the lock is released so the previous state
//! survives intact.

use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};
use tracing::{debug, instrument};

use super::{ToolEffect, ToolHandler, ToolInvocation};
use crate::error::ToolError;
use crate::events::Redactor;

const TOOL_NAME: &str = "SetState";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetStateArgs {
    #[schemars(
        description = "Top-level identifier under `state`. No dots.",
        regex(pattern = r"^[A-Za-z_][A-Za-z0-9_]*$")
    )]
    key: String,
    #[schemars(description = "JSON value to store at `state.<key>`. Any type.")]
    value: Value,
}

/// Handler for the `SetState` builtin. Holds the per-run redactor and
/// the size cap so `invoke` has everything it needs without reaching
/// for globals. The cap is normally `EngineConfig.max_output_bytes`
/// (shared with excerpt caps).
#[derive(Debug, Clone)]
pub struct SetStateHandler {
    redactor: Arc<Redactor>,
    max_bytes: usize,
}

impl SetStateHandler {
    /// Build a handler bound to this run's redactor and the engine's
    /// byte cap. `Arc<Redactor>` is cloned from the one `LoopAgent`
    /// already holds; there is one redactor per run.
    #[must_use]
    pub fn new(redactor: Arc<Redactor>, max_bytes: usize) -> Self {
        Self {
            redactor,
            max_bytes,
        }
    }
}

#[async_trait]
impl ToolHandler for SetStateHandler {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        "Set a single top-level key under this node's `state` tree. \
         `key` must match [A-Za-z_][A-Za-z0-9_]*. `value` is any JSON \
         value and replaces the prior value at that key wholesale. \
         Visible to downstream nodes as `nodes.<this-node>.state.<key>`."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(SetStateArgs)).expect("static schema")
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ContextSelf
    }

    #[instrument(
        skip(self, args),
        fields(tool.name = TOOL_NAME, tool.call_id = %inv.call_id, node.id = %inv.node_id),
    )]
    async fn invoke(&self, inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        let SetStateArgs { key, value } =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
                name: TOOL_NAME.to_string(),
                message: e.to_string(),
            })?;
        validate_key(&key)?;

        // Redact secret leaves before storage so downstream template
        // renders never re-materialize a secret that happened to be in
        // the written payload.
        let redacted = self.redactor.redact_json(&value);

        let handle = inv.state_handle.ok_or_else(|| ToolError::Invocation {
            name: TOOL_NAME.to_string(),
            source: "no state_handle on ToolInvocation — LoopAgent did not plumb the per-node \
                     state buffer (policy gate should have denied first)"
                .into(),
        })?;

        // Lock is synchronous and released before the function returns;
        // it is never held across an `.await`. Poisoning means an earlier
        // SetState panicked mid-write — surface it instead of silently
        // recovering, since the buffer may be in an inconsistent shape.
        let mut guard = handle.buf.lock().map_err(|_| ToolError::Invocation {
            name: TOOL_NAME.to_string(),
            source: "state buffer mutex poisoned".into(),
        })?;

        let map = as_object_mut(&mut guard).ok_or_else(|| ToolError::Invocation {
            name: TOOL_NAME.to_string(),
            source: "state buffer is not a JSON object — LoopAgent initializer invariant violated"
                .into(),
        })?;

        // Insert first so we can measure the tentative new size, then
        // roll back if it exceeds the cap. Keeping the rollback inline
        // avoids a full clone of the map on the happy path.
        let previous = map.insert(key.clone(), redacted.clone());
        let serialized_len = serde_json::to_string(&*guard)
            .map(|s| s.len())
            .map_err(|e| ToolError::Invocation {
                name: TOOL_NAME.to_string(),
                source: Box::new(e),
            })?;

        if serialized_len > self.max_bytes {
            // Re-borrow as object for rollback; we already validated shape.
            if let Some(map) = as_object_mut(&mut guard) {
                match previous {
                    Some(prev) => {
                        map.insert(key.clone(), prev);
                    },
                    None => {
                        map.remove(&key);
                    },
                }
            }
            return Err(ToolError::StateTooLarge {
                name: TOOL_NAME.to_string(),
                bytes: serialized_len,
                cap: self.max_bytes,
            });
        }

        drop(guard);

        // Return the written value so the model has immediate feedback
        // without needing a separate Read tool.
        let value_render =
            serde_json::to_string(&redacted).unwrap_or_else(|_| "<unrenderable>".to_string());
        debug!(
            state.key = %key,
            state.bytes = serialized_len,
            "wrote node-scoped state key",
        );
        Ok(format!("ok: wrote `{key}` = {value_render}"))
    }
}

fn validate_key(k: &str) -> Result<(), ToolError> {
    let mut chars = k.chars();
    let Some(first) = chars.next() else {
        return Err(ToolError::InvalidArgs {
            name: TOOL_NAME.to_string(),
            message: "key must be non-empty".to_string(),
        });
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(ToolError::InvalidArgs {
            name: TOOL_NAME.to_string(),
            message: format!("key must start with an ASCII letter or underscore, got `{first}`"),
        });
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return Err(ToolError::InvalidArgs {
                name: TOOL_NAME.to_string(),
                message: format!(
                    "key `{k}` has invalid character `{c}` — only [A-Za-z0-9_] allowed, no dots"
                ),
            });
        }
    }
    Ok(())
}

fn as_object_mut(v: &mut Value) -> Option<&mut Map<String, Value>> {
    match v {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;
    use crate::tool::StateHandle;

    fn handler(redactor: Redactor, cap: usize) -> SetStateHandler {
        SetStateHandler::new(Arc::new(redactor), cap)
    }

    fn empty_buffer() -> Mutex<Value> {
        Mutex::new(Value::Object(Map::new()))
    }

    fn invocation_with<'a>(buf: &'a Mutex<Value>, call_id: &'a str) -> ToolInvocation<'a> {
        ToolInvocation {
            run_id: "run_test",
            node_id: "n",
            call_id,
            depth: 0,
            state_handle: Some(StateHandle::new(buf)),
            token_budget_share: None,
            roots: &[],
        }
    }

    #[tokio::test]
    async fn writes_key_and_value() {
        let buf = empty_buffer();
        let h = handler(Redactor::default(), 1024);

        let out = h
            .invoke(
                invocation_with(&buf, "c1"),
                json!({ "key": "plan", "value": { "status": "ready" } }),
            )
            .await
            .expect("happy path must succeed");

        assert!(
            out.starts_with("ok: wrote `plan`"),
            "unexpected output: {out}"
        );
        let guard = buf.lock().unwrap();
        assert_eq!(
            guard.get("plan").expect("plan key present"),
            &json!({ "status": "ready" })
        );
    }

    #[tokio::test]
    async fn accepts_null_value() {
        let buf = empty_buffer();
        let h = handler(Redactor::default(), 1024);
        h.invoke(
            invocation_with(&buf, "c1"),
            json!({ "key": "maybe", "value": null }),
        )
        .await
        .expect("null must be an accepted value");
        assert_eq!(buf.lock().unwrap().get("maybe"), Some(&Value::Null));
    }

    #[tokio::test]
    async fn second_write_to_same_key_replaces_wholesale() {
        let buf = empty_buffer();
        let h = handler(Redactor::default(), 1024);

        h.invoke(
            invocation_with(&buf, "c1"),
            json!({ "key": "plan", "value": { "a": 1, "b": 2 } }),
        )
        .await
        .unwrap();
        h.invoke(
            invocation_with(&buf, "c2"),
            json!({ "key": "plan", "value": { "c": 3 } }),
        )
        .await
        .unwrap();

        assert_eq!(
            buf.lock().unwrap().get("plan").unwrap(),
            &json!({ "c": 3 }),
            "second write must replace, not merge"
        );
    }

    #[tokio::test]
    async fn dotted_key_is_invalid_args() {
        let buf = empty_buffer();
        let h = handler(Redactor::default(), 1024);

        let err = h
            .invoke(
                invocation_with(&buf, "c1"),
                json!({ "key": "plan.status", "value": "ready" }),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidArgs { name, message } => {
                assert_eq!(name, TOOL_NAME);
                assert!(message.contains('.') || message.contains("invalid"));
            },
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
        assert!(
            buf.lock().unwrap().as_object().unwrap().is_empty(),
            "invalid key must not mutate state"
        );
    }

    #[tokio::test]
    async fn empty_key_is_invalid_args() {
        let buf = empty_buffer();
        let h = handler(Redactor::default(), 1024);
        let err = h
            .invoke(
                invocation_with(&buf, "c1"),
                json!({ "key": "", "value": 1 }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn digit_leading_key_is_invalid_args() {
        let buf = empty_buffer();
        let h = handler(Redactor::default(), 1024);
        let err = h
            .invoke(
                invocation_with(&buf, "c1"),
                json!({ "key": "1plan", "value": 1 }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn missing_key_is_invalid_args() {
        let buf = empty_buffer();
        let h = handler(Redactor::default(), 1024);
        let err = h
            .invoke(invocation_with(&buf, "c1"), json!({ "value": 1 }))
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidArgs { message, .. } => assert!(message.contains("key")),
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_value_is_invalid_args() {
        let buf = empty_buffer();
        let h = handler(Redactor::default(), 1024);
        let err = h
            .invoke(invocation_with(&buf, "c1"), json!({ "key": "plan" }))
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidArgs { message, .. } => assert!(message.contains("value")),
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn oversize_payload_rejected_and_rolled_back() {
        let buf = empty_buffer();
        // Seed with an acceptable value so rollback has something to restore.
        let h = handler(Redactor::default(), 64);
        h.invoke(
            invocation_with(&buf, "c1"),
            json!({ "key": "plan", "value": "first" }),
        )
        .await
        .unwrap();

        let big = "x".repeat(200);
        let err = h
            .invoke(
                invocation_with(&buf, "c2"),
                json!({ "key": "plan", "value": big }),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::StateTooLarge { name, bytes, cap } => {
                assert_eq!(name, TOOL_NAME);
                assert!(bytes > cap, "bytes {bytes} must exceed cap {cap}");
                assert_eq!(cap, 64);
            },
            other => panic!("expected StateTooLarge, got {other:?}"),
        }

        // Rollback: `plan` still holds the pre-write value.
        assert_eq!(
            buf.lock().unwrap().get("plan").unwrap(),
            &json!("first"),
            "oversize write must leave previous state intact"
        );
    }

    #[tokio::test]
    async fn oversize_on_new_key_removes_it_on_rollback() {
        let buf = empty_buffer();
        let h = handler(Redactor::default(), 32);
        let big = "y".repeat(200);
        let _unused = h
            .invoke(
                invocation_with(&buf, "c1"),
                json!({ "key": "plan", "value": big }),
            )
            .await
            .unwrap_err();
        assert!(
            buf.lock().unwrap().as_object().unwrap().is_empty(),
            "oversize write on a new key must not leave the key behind"
        );
    }

    #[tokio::test]
    async fn secret_in_value_is_redacted_before_storage() {
        let mut secrets = BTreeMap::new();
        secrets.insert("API_KEY".to_string(), "sk-LEAK-12345".to_string());
        let redactor = Redactor::new(&secrets);
        let h = handler(redactor, 1024);

        let buf = empty_buffer();
        h.invoke(
            invocation_with(&buf, "c1"),
            json!({
                "key": "config",
                "value": {
                    "token": "sk-LEAK-12345",
                    "nested": ["prefix sk-LEAK-12345 suffix"]
                }
            }),
        )
        .await
        .unwrap();

        let guard = buf.lock().unwrap();
        let stored = guard.get("config").unwrap();
        assert_eq!(stored.get("token").unwrap(), &json!("***"));
        assert_eq!(
            stored.get("nested").unwrap(),
            &json!(["prefix *** suffix"]),
            "redactor must walk array leaves",
        );
    }

    #[tokio::test]
    async fn missing_state_handle_is_invocation_error() {
        let h = handler(Redactor::default(), 1024);
        let inv = ToolInvocation {
            run_id: "run_test",
            node_id: "n",
            call_id: "c1",
            depth: 0,
            state_handle: None,
            token_budget_share: None,
            roots: &[],
        };
        let err = h
            .invoke(inv, json!({ "key": "plan", "value": 1 }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Invocation { .. }));
    }
}
