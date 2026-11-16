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
        self.validate(text).is_ok()
    }

    /// Validate a JSON answer, returning the first failure's JSON Pointer and reason.
    pub fn validate(&self, text: &str) -> Result<(), OutputValidationError> {
        let value = serde_json::from_str::<Value>(text).map_err(|_| OutputValidationError {
            instance_path: String::new(),
            message: "Invalid JSON",
        })?;
        validate_value(&self.0, &value, "")
    }
}

/// An answer validation failure. The empty pointer denotes the document root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputValidationError {
    pub instance_path: String,
    pub message: &'static str,
}

impl std::fmt::Display for OutputValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at JSON pointer {:?}", self.message, self.instance_path)
    }
}

impl std::error::Error for OutputValidationError {}

fn check_schema(schema: &Value, depth: usize) -> Result<(), &'static str> {
    let bad = "Invalid output schema: unsupported keyword or malformed value (supported: type, properties, items, enum, required, additionalProperties (boolean), minLength, minimum, maximum, $schema (annotation), title, description)";
    let object = schema.as_object().ok_or(bad)?;
    if depth > 64 {
        return Err("Output schema is too deeply nested");
    }
    for (key, value) in object {
        match key.as_str() {
            "type"
                if matches!(
                    value.as_str(),
                    Some("object" | "array" | "string" | "number" | "integer")
                ) => {}
            // Annotations never trigger schema fetching or reference resolution.
            "$schema" | "title" | "description" if value.is_string() => {}
            "additionalProperties" if value.is_boolean() => {}
            "minLength" if value.as_u64().is_some() => {}
            "minimum" | "maximum" if value.is_number() => {}
            "properties" => {
                for child in value.as_object().ok_or(bad)?.values() {
                    check_schema(child, depth + 1)?;
                }
            }
            "items" => check_schema(value, depth + 1)?,
            "enum" if value.as_array().is_some_and(|a| !a.is_empty()) => {}
            "required" => {
                let mut seen = std::collections::HashSet::new();
                for entry in value.as_array().ok_or(bad)? {
                    if !seen.insert(entry.as_str().ok_or(bad)?) {
                        return Err(bad);
                    }
                }
            }
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

// Compare mixed integer/float bounds without rounding large JSON integers to f64.
fn number_cmp(a: &serde_json::Number, b: &serde_json::Number) -> std::cmp::Ordering {
    let integer = |n: &serde_json::Number| n.as_i64().map(i128::from).or_else(|| n.as_u64().map(i128::from));
    match (integer(a), integer(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(a), None) => {
            let b = b.as_f64().expect("JSON number");
            a.cmp(&(b as i128))
                .then_with(|| 0.0_f64.partial_cmp(&b.fract()).unwrap())
        }
        (None, Some(_)) => number_cmp(b, a).reverse(),
        (None, None) => a.as_f64().partial_cmp(&b.as_f64()).unwrap(),
    }
}

fn pointer_child(path: &str, key: &str) -> String {
    format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"))
}

fn validate_value(schema: &Value, value: &Value, path: &str) -> Result<(), OutputValidationError> {
    let fail = |message| OutputValidationError {
        instance_path: path.to_owned(),
        message,
    };
    if let Some(kind) = schema.get("type").and_then(Value::as_str) {
        let valid = match kind {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "number" => value.is_number(),
            "integer" => value
                .as_number()
                .is_some_and(|n| n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|n| n.fract() == 0.0)),
            _ => false,
        };
        if !valid {
            return Err(fail("Wrong value type"));
        }
    }
    if let Some(choices) = schema.get("enum").and_then(Value::as_array) {
        if !choices.iter().any(|choice| schema_equal(choice, value)) {
            return Err(fail("Value is not in enum"));
        }
    }
    if let Some(text) = value.as_str() {
        if schema
            .get("minLength")
            .and_then(Value::as_u64)
            .is_some_and(|min| (text.chars().count() as u64) < min)
        {
            return Err(fail("String is shorter than minLength"));
        }
    }
    if let Some(number) = value.as_number() {
        for (keyword, forbidden) in [
            ("minimum", std::cmp::Ordering::Less),
            ("maximum", std::cmp::Ordering::Greater),
        ] {
            if schema
                .get(keyword)
                .and_then(Value::as_number)
                .is_some_and(|bound| number_cmp(number, bound) == forbidden)
            {
                return Err(fail(if keyword == "minimum" {
                    "Number is below minimum"
                } else {
                    "Number is above maximum"
                }));
            }
        }
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required {
                let key = key.as_str().expect("checked required string");
                if !object.contains_key(key) {
                    return Err(OutputValidationError {
                        instance_path: pointer_child(path, key),
                        message: "Missing required property",
                    });
                }
            }
        }
        let props = schema.get("properties").and_then(Value::as_object);
        for (key, child_value) in object {
            let child_path = pointer_child(path, key);
            if let Some(child_schema) = props.and_then(|props| props.get(key)) {
                validate_value(child_schema, child_value, &child_path)?;
            } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                return Err(OutputValidationError {
                    instance_path: child_path,
                    message: "Unexpected property",
                });
            }
        }
    }
    if let (Some(items), Some(array)) = (schema.get("items"), value.as_array()) {
        for (index, item) in array.iter().enumerate() {
            validate_value(items, item, &pointer_child(path, &index.to_string()))?;
        }
    }
    Ok(())
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
    fn required_entries_must_be_unique_strings() {
        for schema in [
            json!({"type":"object","required":["answer","answer"]}),
            json!({"properties":{"nested":{"required":["a","b","a"]}}}),
            json!({"items":{"required":["answer","answer"]}}),
        ] {
            let error = OutputSchema::new(schema).unwrap_err();
            assert_eq!(error.code, codes::USAGE);
        }
        for required in [json!([]), json!(["answer"]), json!(["a", "b"])] {
            assert!(OutputSchema::new(json!({"type":"object","required":required})).is_ok());
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
        assert!(OutputSchema::new(json!({"type":"boolean"})).is_err());
        assert!(OutputSchema::new(json!({"$ref":"https://example.invalid/schema"})).is_err());
        assert!(OutputSchema::new(json!({"required":[1]})).is_err());
    }
    #[test]
    fn review_keywords_are_checked_and_enforced() {
        let schema = OutputSchema::new(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Example", "description": "An offline schema",
            "type": "object", "additionalProperties": false,
            "properties": {
                "count": {"type": "integer", "minimum": 1, "maximum": 3},
                "name": {"type": "string", "minLength": 2}
            }
        }))
        .unwrap();
        for text in [r#"{"count":1,"name":"é🦀"}"#, r#"{"count":3.0}"#, "{}"] {
            assert!(schema.matches(text), "rejected {text}");
        }
        for text in [
            r#"{"count":0}"#,
            r#"{"count":4}"#,
            r#"{"count":1.5}"#,
            r#"{"count":"2"}"#,
            r#"{"name":"é"}"#,
            r#"{"extra":true}"#,
        ] {
            assert!(!schema.matches(text), "accepted {text}");
        }
        // These keywords only constrain instances of the applicable type.
        let schema = OutputSchema::new(json!({
            "minimum": 1, "maximum": 3, "minLength": 2, "additionalProperties": false
        }))
        .unwrap();
        for text in ["null", "[]", "true", "{}", "2", "\"ok\""] {
            assert!(schema.matches(text), "rejected {text}");
        }
        assert!(OutputSchema::new(json!({"additionalProperties": true}))
            .unwrap()
            .matches(r#"{"extra":true}"#));
        for value in [
            json!({"$schema": false}),
            json!({"title": 1}),
            json!({"description": null}),
            json!({"additionalProperties": {}}),
            json!({"minimum": "0"}),
            json!({"maximum": null}),
            json!({"minLength": -1}),
            json!({"minLength": 1.5}),
            json!({"unknown": true}),
        ] {
            assert!(OutputSchema::new(value.clone()).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn numeric_bounds_preserve_large_integer_precision() {
        for (bound, below, above) in [
            (
                json!(9007199254740993_u64),
                "9007199254740992.0",
                "9007199254740994",
            ),
            (json!(u64::MAX), "18446744073709551614", "18446744073709551616.0"),
            (json!(-1.5), "-2", "-1"),
            (json!(1.5), "1", "2"),
        ] {
            let min = OutputSchema::new(json!({"minimum": bound})).unwrap();
            let max = OutputSchema::new(json!({"maximum": bound})).unwrap();
            assert!(min.matches(&bound.to_string()));
            assert!(max.matches(&bound.to_string()));
            assert!(!min.matches(below), "{below} must be below {bound}");
            assert!(max.matches(below));
            assert!(min.matches(above));
            assert!(!max.matches(above), "{above} must be above {bound}");
        }
    }
}
