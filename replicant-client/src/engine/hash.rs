use serde_json::Value;
use sha2::{Digest, Sha256};

/// Canonical content hash shared with the server: compact key-sorted JSON, SHA-256, lowercase hex.
pub fn content_hash(content: &Value) -> String {
    format!("{:x}", Sha256::digest(content.to_string().as_bytes()))
}

/// Makes every integral float an integer (`1200.0` → `1200`, `-0.0` → `0`). The server's
/// round trip returns integers unchanged, so canonical content reads back as it was written.
pub fn canonicalise_numbers(value: &mut Value) {
    match value {
        Value::Number(number) => {
            let integer = number
                .as_f64()
                .filter(|_| number.is_f64())
                .and_then(integral);
            if let Some(integer) = integer {
                *value = integer;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(canonicalise_numbers),
        Value::Object(fields) => fields.values_mut().for_each(canonicalise_numbers),
        _ => {}
    }
}

/// Beyond u64 the float stays: the server's integer parses back into that same float.
fn integral(float: f64) -> Option<Value> {
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    const TWO_POW_64: f64 = 18_446_744_073_709_551_616.0;
    if float.fract() != 0.0 {
        None
    } else if (-TWO_POW_63..TWO_POW_63).contains(&float) {
        Some(Value::from(float as i64))
    } else if (0.0..TWO_POW_64).contains(&float) {
        Some(Value::from(float as u64))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

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

    #[test]
    fn parsed_objects_are_key_sorted_so_preserve_order_must_stay_off() {
        let a: Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
        assert_eq!(content_hash(&a), content_hash(&b));
        assert_eq!(b.to_string(), r#"{"a":1,"b":2}"#);
    }

    fn canonical(mut value: Value) -> Value {
        canonicalise_numbers(&mut value);
        value
    }

    #[test]
    fn integral_floats_become_integers() {
        assert_eq!(
            canonical(
                json!({"n": 1200.0, "neg": -1200.0, "zero": -0.0, "list": [2.0, {"deep": 1.0e16}]})
            ),
            json!({"n": 1200, "neg": -1200, "zero": 0, "list": [2, {"deep": 10_000_000_000_000_000_i64}]})
        );
        assert_eq!(
            canonical(json!(1.0e19)),
            json!(10_000_000_000_000_000_000_u64)
        );
        assert!(canonical(json!(-0.0)).is_u64());
    }

    #[test]
    fn other_numbers_and_strings_are_unchanged() {
        for value in [
            json!(1.5),
            json!(1200.5),
            json!(1.0e-5),
            json!(1.0e20),
            json!(-1.0e19),
            json!(u64::MAX),
            json!(i64::MIN),
            json!("1200.0"),
        ] {
            let after = canonical(value.clone());
            assert_eq!(after, value);
            assert_eq!(after.is_f64(), value.is_f64(), "{value}");
        }
    }

    #[test]
    fn canonical_content_hashes_as_the_integer_form() {
        assert_eq!(
            content_hash(&canonical(json!({"n": 1200.0}))),
            content_hash(&json!({"n": 1200}))
        );
        assert_ne!(
            content_hash(&json!({"n": 1200.0})),
            content_hash(&json!({"n": 1200})),
            "content_hash itself hashes the value as given, like the server"
        );
    }
}
