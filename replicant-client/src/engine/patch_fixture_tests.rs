//! Shared with replicant-server (`test/fixtures/json_patch_fixture.json`): every patch the
//! client sends is `json_patch::diff` output, and the server's `jsonpatch` must turn each `doc`
//! into the same `result`. Recorded with `RECORD_PATCH_FIXTURE=1`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::backoff::Jitter;

const RECORD: &str = "RECORD_PATCH_FIXTURE";

#[derive(Debug, Serialize, Deserialize)]
struct Case {
    name: String,
    doc: Value,
    patch: Value,
    result: Value,
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/server_v2/json_patch_fixture.json")
}

/// (name, doc, result): the patch is whatever `json_patch::diff` makes of the pair. The client
/// sends canonical content, so only canonical numbers appear in the cases below (and every
/// document is an object, so `diff` never emits a root replace).
fn inputs() -> Vec<(String, Value, Value)> {
    let scale = |pitches: Value| json!({"name": "12-TET", "pitches": pitches});
    let base = scale(json!([0, 200, 400, 500, 700, 900]));
    let mut cases = vec![
        ("replace a scalar", json!({"a": 1}), json!({"a": 2})),
        ("add a key", json!({"a": 1}), json!({"a": 1, "b": [1, 2]})),
        ("remove a key", json!({"a": 1, "b": 2}), json!({"a": 1})),
        (
            "nested object",
            json!({"a": {"b": {"c": 1}}}),
            json!({"a": {"b": {"c": 2, "d": null}}}),
        ),
        ("type change", json!({"a": [1]}), json!({"a": {"k": true}})),
        (
            "retune one degree",
            base.clone(),
            scale(json!([0, 200, 386, 500, 700, 900])),
        ),
        (
            "append",
            base.clone(),
            scale(json!([0, 200, 400, 500, 700, 900, 1000])),
        ),
        (
            "insert in the middle",
            base.clone(),
            scale(json!([0, 200, 400, 450, 500, 700, 900])),
        ),
        (
            "remove from the middle",
            base.clone(),
            scale(json!([0, 200, 500, 700, 900])),
        ),
        (
            "remove several from the end",
            base.clone(),
            scale(json!([0, 200])),
        ),
        ("empty the list", base.clone(), scale(json!([]))),
        (
            "list of objects",
            json!({"tunings": [{"n": "A", "p": [1, 2]}, {"n": "B", "p": [3]}]}),
            json!({"tunings": [{"n": "A", "p": [1, 2, 5]}, {"n": "B2", "p": []}]}),
        ),
        (
            "keys needing escapes",
            json!({"a/b": 1, "m~n": 2}),
            json!({"a/b": 3, "m~n": 4, "x/~y": 5}),
        ),
        ("empty key", json!({"": 1}), json!({"": 2})),
        (
            "unicode",
            json!({"name": "Bohlen–Pierce"}),
            json!({"name": "Bohlen–Pierce ✓", "ñ": "é"}),
        ),
        (
            "numbers",
            json!({"cents": 1.5, "big": 9_007_199_254_740_993_i64, "neg": -3}),
            json!({"cents": 386.3137, "big": 9_007_199_254_740_994_i64, "neg": -0.5}),
        ),
        (
            "exponent-form numbers",
            json!({"a": 1e-5, "b": 1e20, "c": -1e19}),
            json!({"a": 2e-5, "b": -1e20, "c": 1e19, "d": 1.5e-7}),
        ),
        (
            "keys that look like indices",
            json!({"0": 1, "01": 2, "-": 3, "1abc": 4}),
            json!({"0": 5, "01": 6, "-": 7, "1abc": 8, "2": 9}),
        ),
        (
            "index-like keys nested in a list",
            json!({"l": [{"0": [1, 2], "-": 1}]}),
            json!({"l": [{"0": [1, 3, 2], "-": null}]}),
        ),
    ]
    .into_iter()
    .map(|(name, doc, result)| (name.to_string(), doc, result))
    .collect::<Vec<_>>();
    let mut rng = Jitter::new(20_260_929);
    for n in 0..40 {
        let doc = random_object(&mut rng, 3);
        let result = edited(&doc, &mut rng);
        cases.push((format!("random {n}"), doc, result));
    }
    cases
}

fn index_below(rng: &mut Jitter, len: usize) -> usize {
    ((rng.next_unit() * len as f64) as usize).min(len - 1)
}

fn random_scalar(rng: &mut Jitter) -> Value {
    match (rng.next_unit() * 5.0) as u32 {
        0 => Value::Null,
        1 => json!(rng.next_unit() < 0.5),
        2 => json!((rng.next_unit() * 2000.0) as i64 - 1000),
        3 => json!((rng.next_unit() * 1000.0).floor() + 0.5),
        _ => json!(format!("s{}", (rng.next_unit() * 100.0) as u32)),
    }
}

fn random_value(rng: &mut Jitter, depth: u32) -> Value {
    if depth == 0 {
        return random_scalar(rng);
    }
    match (rng.next_unit() * 3.0) as u32 {
        0 => random_scalar(rng),
        1 => {
            let len = (rng.next_unit() * 5.0) as usize;
            Value::Array((0..len).map(|_| random_value(rng, depth - 1)).collect())
        }
        _ => random_object(rng, depth - 1),
    }
}

fn random_object(rng: &mut Jitter, depth: u32) -> Value {
    let mut fields = Map::new();
    for _ in 0..1 + (rng.next_unit() * 4.0) as usize {
        let key = format!("k{}", (rng.next_unit() * 6.0) as u32);
        fields.insert(key, random_value(rng, depth));
    }
    Value::Object(fields)
}

/// Edits `value` the way a user might: keys replaced, added or removed; list elements
/// inserted, removed or appended; scalars changed.
fn edited(value: &Value, rng: &mut Jitter) -> Value {
    match value {
        Value::Object(fields) => {
            let mut out = Map::new();
            for (key, field) in fields {
                match (rng.next_unit() * 6.0) as u32 {
                    0 => {}
                    1 => {
                        out.insert(key.clone(), random_value(rng, 2));
                    }
                    _ => {
                        out.insert(key.clone(), edited(field, rng));
                    }
                }
            }
            if rng.next_unit() < 0.3 {
                let key = format!("new{}", (rng.next_unit() * 6.0) as u32);
                out.insert(key, random_value(rng, 2));
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            let mut out: Vec<Value> = items.iter().map(|item| edited(item, rng)).collect();
            match (rng.next_unit() * 5.0) as u32 {
                0 if !out.is_empty() => {
                    let at = index_below(rng, out.len());
                    out.remove(at);
                }
                1 => {
                    let at = index_below(rng, out.len() + 1);
                    out.insert(at, random_value(rng, 1));
                }
                2 => out.push(random_value(rng, 1)),
                _ => {}
            }
            Value::Array(out)
        }
        _ if rng.next_unit() < 0.3 => random_scalar(rng),
        scalar => scalar.clone(),
    }
}

#[test]
fn every_patch_fixture_case_applies_and_is_what_diff_produces() {
    if std::env::var_os(RECORD).is_some() {
        let cases: Vec<Case> = inputs()
            .into_iter()
            .map(|(name, doc, result)| Case {
                patch: serde_json::to_value(json_patch::diff(&doc, &result)).unwrap(),
                name,
                doc,
                result,
            })
            .collect();
        let text = serde_json::to_string_pretty(&cases).unwrap() + "\n";
        std::fs::write(fixture_path(), text).unwrap();
    }
    let text = std::fs::read_to_string(fixture_path()).expect("the patch fixture exists");
    let cases: Vec<Case> = serde_json::from_str(&text).unwrap();
    let inputs = inputs();
    assert_eq!(
        cases.len(),
        inputs.len(),
        "the fixture lists every input; re-record with {RECORD}=1"
    );
    for (case, (name, doc, result)) in cases.iter().zip(inputs) {
        assert_eq!(
            (&case.name, &case.doc, &case.result),
            (&name, &doc, &result),
            "{name}: re-record with {RECORD}=1"
        );
        assert_eq!(
            serde_json::to_value(json_patch::diff(&case.doc, &case.result)).unwrap(),
            case.patch,
            "{name}: the client would send another patch"
        );
        let patch: json_patch::Patch = serde_json::from_value(case.patch.clone()).unwrap();
        let mut applied = case.doc.clone();
        json_patch::patch(&mut applied, &patch).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(applied, case.result, "{name}");
    }
}
