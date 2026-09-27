use serde_json::Value;

/// Canonical content hash shared with the server: compact key-sorted JSON, SHA-256, lowercase hex.
pub fn content_hash(content: &Value) -> String {
    replicant_core::patches::calculate_checksum(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map};

    // Same inputs and expected hashes as replicant-server
    // test/replicant_server/documents_test.exs ("pins the exact hash ...").
    #[test]
    fn matches_server_pin_for_42_key_map_with_floats_and_unicode() {
        let mut m = Map::new();
        for i in 1..=35 {
            m.insert(format!("field_{:02}", i), json!(i));
        }
        m.insert("whole".into(), json!(1.0));
        m.insert("large".into(), json!(1.0e10));
        m.insert("small".into(), json!(1.0e-7));
        m.insert("tenth".into(), json!(0.1));
        m.insert("negative".into(), json!(-2.5));
        m.insert("unicode_key_🎵".into(), json!("café résumé 音楽"));
        m.insert(
            "nested".into(),
            json!({"z": [3, 2, 1], "a": "x", "deep": {"tags": ["b", "a", "c"]}}),
        );
        assert_eq!(m.len(), 42);
        assert_eq!(
            content_hash(&Value::Object(m)),
            "7d0576a13a06288152611ed9957f3679e33c1e4f3c44c585b09f17c2521e995a"
        );
    }

    #[test]
    fn matches_server_pin_for_nested_40_key_child() {
        let mut child = Map::new();
        for i in 1..=40 {
            child.insert(format!("child_field_{:02}", i), json!(i));
        }
        let content = json!({"title": "Parent Doc", "count": 3, "child": Value::Object(child)});
        assert_eq!(
            content_hash(&content),
            "4460584450427fd9acbd2d2ecc56eb8fb11b40f96eef23172bb1a3655fca2026"
        );
    }

    #[test]
    fn key_order_does_not_matter() {
        let a: Value = serde_json::from_str(r#"{"b":1,"a":2}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":2,"b":1}"#).unwrap();
        assert_eq!(content_hash(&a), content_hash(&b));
    }
}
