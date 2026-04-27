//! Small utilities used by more than one subsystem and not large enough
//! to warrant their own module. Currently houses [`canonical_json`],
//! the deterministic JSON serializer the record/replay layer hashes to
//! produce tape keys.

/// Serialize a `serde_json::Value` with object keys in alphabetic order
/// at every nesting level. Arrays preserve their element order; scalars
/// are written via `serde_json::to_string` so escaping matches the
/// canonical JSON profile (UTF-8, `\uXXXX` for control chars).
///
/// # Why this exists
///
/// Tape-key derivation in [`crate::llm::replay`] and
/// [`crate::tool::record`] hashes the request payload to derive a
/// stable replay key. Nested JSON objects in the payload — for example
/// tool argument schemas produced by `schemars` — are not guaranteed
/// to emit keys in the same order across crate versions. Without a
/// canonical form a `schemars` minor bump silently invalidates every
/// existing tape with no migration path. Sorting keys at the
/// canonicalization step pins the hash input to the value's content,
/// not its serialization accident.
///
/// The function is infallible because every leaf path is either an
/// infallible scalar serialization or a recursive descent.
#[must_use]
pub fn canonical_json(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_canonical(&mut out, value);
    out
}

fn write_canonical(out: &mut String, value: &serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => {
            out.push_str(&serde_json::to_string(s).expect("string serialization is infallible"));
        },
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(out, item);
            }
            out.push(']');
        },
        Value::Object(map) => {
            // BTreeMap collapses any insertion-order accident in the
            // input map into deterministic alphabetic order. The
            // collected references avoid cloning keys or values.
            let sorted: std::collections::BTreeMap<&String, &Value> = map.iter().collect();
            out.push('{');
            for (i, (k, v)) in sorted.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(
                    &serde_json::to_string(k).expect("string serialization is infallible"),
                );
                out.push(':');
                write_canonical(out, v);
            }
            out.push('}');
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn object_keys_sorted_alphabetically() {
        let v = json!({"b": 1, "a": 2, "c": 3});
        assert_eq!(canonical_json(&v), r#"{"a":2,"b":1,"c":3}"#);
    }

    #[test]
    fn nested_object_keys_sorted_at_every_level() {
        let v = json!({"outer": {"z": 1, "a": 2}, "abc": {"y": [3, 1, 2]}});
        // Top-level: abc < outer; nested: a < z; arrays preserve order.
        assert_eq!(
            canonical_json(&v),
            r#"{"abc":{"y":[3,1,2]},"outer":{"a":2,"z":1}}"#,
        );
    }

    #[test]
    fn arrays_preserve_element_order() {
        let v = json!([3, 1, 2, "x", null, true]);
        assert_eq!(canonical_json(&v), r#"[3,1,2,"x",null,true]"#);
    }

    #[test]
    fn equivalent_objects_with_different_insertion_order_yield_same_canonical_form() {
        // The whole point of this utility: two payloads that mean the
        // same thing but were built with different key insertion orders
        // must hash to the same tape key.
        let a = json!({"foo": 1, "bar": 2, "baz": {"k": [1, 2]}});
        let b = json!({"baz": {"k": [1, 2]}, "bar": 2, "foo": 1});
        assert_eq!(canonical_json(&a), canonical_json(&b));
    }

    #[test]
    fn string_escaping_matches_serde_json() {
        let v = json!({"msg": "line1\nline2\t\"quoted\""});
        // Compare against serde_json's own escape form for the value.
        let escaped = serde_json::to_string("line1\nline2\t\"quoted\"").unwrap();
        let expected = format!(r#"{{"msg":{escaped}}}"#);
        assert_eq!(canonical_json(&v), expected);
    }

    #[test]
    fn null_bool_number_scalars_are_canonical() {
        assert_eq!(canonical_json(&json!(null)), "null");
        assert_eq!(canonical_json(&json!(true)), "true");
        assert_eq!(canonical_json(&json!(false)), "false");
        assert_eq!(canonical_json(&json!(42)), "42");
        assert_eq!(canonical_json(&json!(-1.5)), "-1.5");
    }

    #[test]
    fn empty_object_and_array_are_minimal() {
        assert_eq!(canonical_json(&json!({})), "{}");
        assert_eq!(canonical_json(&json!([])), "[]");
    }
}
