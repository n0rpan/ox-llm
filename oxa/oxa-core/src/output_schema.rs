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
        if !choices.contains(value) {
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
