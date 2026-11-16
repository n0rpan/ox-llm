//! Opt-in final-answer validation. Deliberately a small, offline JSON Schema subset.
use oxa_proto::{codes, OxaError, Value};

#[derive(Debug, Clone)]
pub struct OutputSchema(Value);

impl OutputSchema {
    pub fn new(value: Value) -> Result<Self, OxaError> {
        check_schema(&value, 0).map_err(|why| OxaError::new(codes::USAGE, why))?;
        Ok(Self(value))
    }

    pub fn value(&self) -> &Value {
        &self.0
    }

    pub fn matches(&self, text: &str) -> bool {
        serde_json::from_str::<Value>(text).is_ok_and(|value| matches_value(&self.0, &value))
    }
}

fn check_schema(schema: &Value, depth: usize) -> Result<(), &'static str> {
    let bad = "Invalid output schema: supported keywords are type, properties, items, enum and required (object, array, string, number)";
    let object = schema.as_object().ok_or(bad)?;
    if depth > 64 {
        return Err("Output schema is too deeply nested");
    }
    for (key, value) in object {
        match key.as_str() {
            "type" if matches!(value.as_str(), Some("object" | "array" | "string" | "number")) => {}
            "properties" => {
                for child in value.as_object().ok_or(bad)?.values() {
                    check_schema(child, depth + 1)?;
                }
            }
            "items" => check_schema(value, depth + 1)?,
            "enum" if value.as_array().is_some_and(|a| !a.is_empty()) => {}
            "required" if value.as_array().is_some_and(|a| a.iter().all(Value::is_string)) => {}
            _ => return Err(bad),
        }
    }
    Ok(())
}

// JSON Schema compares numbers by value, not by integer/float representation, also
// inside containers. Do not round integers to f64: distinct large integers would alias.
fn schema_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => {
            let integer =
                |n: &serde_json::Number| n.as_i64().map(i128::from).or_else(|| n.as_u64().map(i128::from));
            match (integer(a), integer(b)) {
                (Some(a), Some(b)) => a == b,
                (Some(i), None) => b.as_f64().is_some_and(|f| f.fract() == 0.0 && f as i128 == i),
                (None, Some(i)) => a.as_f64().is_some_and(|f| f.fract() == 0.0 && f as i128 == i),
                (None, None) => a == b,
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| schema_equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|b| schema_equal(value, b)))
        }
        _ => left == right,
    }
}

fn matches_value(schema: &Value, value: &Value) -> bool {
    if let Some(kind) = schema.get("type").and_then(Value::as_str) {
        let valid = match kind {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "number" => value.is_number(),
            _ => false,
        };
        if !valid {
            return false;
        }
    }
    if let Some(choices) = schema.get("enum").and_then(Value::as_array) {
        if !choices.iter().any(|choice| schema_equal(choice, value)) {
            return false;
        }
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            if !required
                .iter()
                .all(|key| object.contains_key(key.as_str().unwrap_or_default()))
            {
                return false;
            }
        }
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            if !props
                .iter()
                .all(|(key, child)| object.get(key).is_none_or(|v| matches_value(child, v)))
            {
                return false;
            }
        }
    }
    if let (Some(items), Some(array)) = (schema.get("items"), value.as_array()) {
        if !array.iter().all(|item| matches_value(items, item)) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn enum_uses_numeric_value_equality_recursively() {
        for (choice, answer) in [
            ("1", "1.0"),
            ("1.0", "1"),
            ("-2", "-2e0"),
            ("0", "-0.0"),
            (r#"{"nested":[1,{"n":2.0}]}"#, r#"{"nested":[1.0,{"n":2}]}"#),
        ] {
            let schema =
                OutputSchema::new(serde_json::from_str(&format!(r#"{{"enum":[{choice}]}}"#)).unwrap())
                    .unwrap();
            assert!(schema.matches(answer), "{choice} must equal {answer}");
        }
        for (choice, answer) in [
            ("1", "1.5"),
            ("1", "\"1\""),
            ("9007199254740993", "9007199254740992.0"),
            ("18446744073709551615", "18446744073709551616.0"),
            ("-9223372036854775808", "9223372036854775808.0"),
            ("[1,2]", "[2.0,1.0]"),
            (r#"{"n":1}"#, r#"{"n":1.0,"extra":0}"#),
        ] {
            let schema =
                OutputSchema::new(serde_json::from_str(&format!(r#"{{"enum":[{choice}]}}"#)).unwrap())
                    .unwrap();
            assert!(!schema.matches(answer), "{choice} must differ from {answer}");
        }
    }

    #[test]
    fn subset_nested_types_required_and_enum() {
        let schema = OutputSchema::new(json!({"type":"object", "required":["rows"],
            "properties":{"rows":{"type":"array","items":{"type":"object",
                "required":["name","n"], "properties":{"name":{"type":"string","enum":["ok"]},"n":{"type":"number"}}}}}})).unwrap();
        assert!(schema.matches(r#"{"rows":[{"name":"ok","n":1.5}]}"#));
        for text in [
            "not json",
            "{}",
            r#"{"rows":{}}"#,
            r#"{"rows":[{"name":"no","n":1}]}"#,
            r#"{"rows":[{"name":"ok","n":"1"}]}"#,
        ] {
            assert!(!schema.matches(text), "{text}");
        }
        assert!(OutputSchema::new(json!({"type":"integer"})).is_err());
        assert!(OutputSchema::new(json!({"$ref":"https://example.invalid/schema"})).is_err());
        assert!(OutputSchema::new(json!({"required":[1]})).is_err());
    }
}
