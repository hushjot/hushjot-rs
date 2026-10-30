//! Firestore REST value encoding.
//!
//! Three details are load-bearing. Integers travel as **strings**
//! (`{"integerValue":"42"}`). Ciphertext is `bytesValue` in **standard,
//! padded** base64 — the rules refuse a string, so a client that forgot to
//! encrypt fails loudly instead of uploading readable notes. And timestamps
//! come back with microsecond precision and are truncated **down** to
//! milliseconds: a delta cursor that resumes fractionally early delivers a
//! document twice, while one that resumes late loses it.

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Map, Value};

use hushjot_core::bytes::{from_base64, to_base64};

/// A document's fields.
pub type Fields = Map<String, Value>;

/// `integerValue`.
pub fn int(n: i64) -> Value {
    json!({ "integerValue": n.to_string() })
}

/// `booleanValue`.
pub fn boolean(b: bool) -> Value {
    json!({ "booleanValue": b })
}

/// `bytesValue`, standard padded base64.
pub fn bytes(b: &[u8]) -> Value {
    json!({ "bytesValue": to_base64(b) })
}

/// `stringValue`.
pub fn string(s: &str) -> Value {
    json!({ "stringValue": s })
}

/// `timestampValue` from epoch ms.
pub fn timestamp(ms: i64) -> Value {
    let t: DateTime<Utc> = DateTime::from_timestamp_millis(ms).unwrap_or_default();
    json!({ "timestampValue": t.to_rfc3339_opts(SecondsFormat::Millis, true) })
}

/// `referenceValue`.
pub fn reference(name: &str) -> Value {
    json!({ "referenceValue": name })
}

/// Read an `integerValue` (or a whole `doubleValue`).
pub fn read_int(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(s) = value.get("integerValue").and_then(Value::as_str) {
        return s.parse().ok();
    }
    value.get("doubleValue").and_then(Value::as_f64).map(|f| f as i64)
}

/// Read a `booleanValue`; anything else is false.
pub fn read_bool(value: Option<&Value>) -> bool {
    value.and_then(|v| v.get("booleanValue")).and_then(Value::as_bool).unwrap_or(false)
}

/// Read a `bytesValue`.
pub fn read_bytes(value: Option<&Value>) -> Option<Vec<u8>> {
    from_base64(value?.get("bytesValue")?.as_str()?).ok()
}

/// Read a `stringValue`.
pub fn read_string(value: Option<&Value>) -> Option<String> {
    value?.get("stringValue")?.as_str().map(str::to_string)
}

/// Read a `timestampValue` as epoch ms, truncated down.
pub fn read_timestamp(value: Option<&Value>) -> Option<i64> {
    let text = value?.get("timestampValue")?.as_str()?;
    DateTime::parse_from_rfc3339(text).ok().map(|t| t.timestamp_millis())
}

/// Flatten a Firestore value into plain JSON (maps, arrays, scalars; integers
/// as numbers; bytes as their base64 string).
pub fn to_plain(value: &Value) -> Value {
    if let Some(map) = value.get("mapValue") {
        let mut out = Map::new();
        if let Some(fields) = map.get("fields").and_then(Value::as_object) {
            for (k, v) in fields {
                out.insert(k.clone(), to_plain(v));
            }
        }
        return Value::Object(out);
    }
    if let Some(array) = value.get("arrayValue") {
        return Value::Array(
            array.get("values").and_then(Value::as_array).map(|v| v.iter().map(to_plain).collect()).unwrap_or_default(),
        );
    }
    if let Some(n) = read_int(Some(value)) {
        return json!(n);
    }
    for key in ["stringValue", "booleanValue", "bytesValue", "timestampValue", "doubleValue"] {
        if let Some(v) = value.get(key) {
            return v.clone();
        }
    }
    Value::Null
}

/// A map field's entry, e.g. `hist.202610`.
pub fn read_map_int(value: Option<&Value>, key: &str) -> Option<i64> {
    read_int(value?.get("mapValue")?.get("fields")?.get(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_travel_as_strings() {
        assert_eq!(int(42), json!({"integerValue": "42"}));
        assert_eq!(read_int(Some(&int(-7))), Some(-7));
        assert_eq!(read_int(Some(&json!({"doubleValue": 3.0}))), Some(3));
        assert_eq!(read_int(None), None);
    }

    #[test]
    fn bytes_are_padded_standard_base64() {
        assert_eq!(bytes(&[0xfb, 0xff]), json!({"bytesValue": "+/8="}));
        assert_eq!(read_bytes(Some(&bytes(&[1, 2, 3]))), Some(vec![1, 2, 3]));
        assert_eq!(read_bytes(Some(&string("AQID"))), None, "a string is never ciphertext");
    }

    #[test]
    fn timestamps_round_trip_and_truncate_down() {
        assert_eq!(timestamp(1_790_035_200_123), json!({"timestampValue": "2026-09-22T00:00:00.123Z"}));
        assert_eq!(
            read_timestamp(Some(&json!({"timestampValue": "2026-09-22T00:00:00.123999Z"}))),
            Some(1_790_035_200_123)
        );
        assert_eq!(read_timestamp(Some(&json!({"timestampValue": "garbage"}))), None);
    }

    #[test]
    fn maps_flatten_to_plain_json() {
        let value = json!({"mapValue": {"fields": {
            "version": {"integerValue": "1"},
            "name": {"stringValue": "argon2id"},
            "nested": {"mapValue": {"fields": {"ok": {"booleanValue": true}}}},
            "list": {"arrayValue": {"values": [{"integerValue": "2"}]}}
        }}});
        assert_eq!(to_plain(&value), json!({"version": 1, "name": "argon2id", "nested": {"ok": true}, "list": [2]}));
        assert_eq!(
            read_map_int(Some(&json!({"mapValue": {"fields": {"202610": {"integerValue": "5"}}}})), "202610"),
            Some(5)
        );
    }
}
