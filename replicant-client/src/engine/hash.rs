use serde_json::Value;

/// Canonical content hash shared with the server: compact key-sorted JSON, SHA-256, lowercase hex.
pub fn content_hash(content: &Value) -> String {
    replicant_core::patches::calculate_checksum(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    const HASH_FIXTURE_JSON: &str =
        include_str!("../../tests/fixtures/server_v2/content_hash_fixture.json");

    #[derive(Debug, Deserialize)]
    struct Case {
        name: String,
        content: Value,
        hash: String,
    }

    // Shared pins with replicant-server: test/fixtures/content_hash_fixture.json.
    #[test]
    fn matches_shared_content_hash_fixture() {
        let cases: Vec<Case> = serde_json::from_str(HASH_FIXTURE_JSON).unwrap();
        assert_eq!(cases.len(), 8);
        for case in &cases {
            assert_eq!(content_hash(&case.content), case.hash, "case {}", case.name);
        }
    }

    #[test]
    fn key_order_does_not_matter() {
        let a: Value = serde_json::from_str(r#"{"b":1,"a":2}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":2,"b":1}"#).unwrap();
        assert_eq!(content_hash(&a), content_hash(&b));
    }
}
