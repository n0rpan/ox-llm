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
        let (shape, numbers) = number_views(text);
        let invalid = || OutputValidationError {
            instance_path: String::new(),
            message: "Invalid JSON",
        };
        let value = serde_json::from_str::<Value>(&shape).map_err(|_| invalid())?;
        let numbers = serde_json::from_str::<Value>(&numbers).map_err(|_| invalid())?;
        validate_value(&self.0, &value, &numbers, "")
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

// Preserve numeric lexemes before serde_json can round them (including builds
// without arbitrary_precision). Both views use serde_json's syntax/depth checks:
// numbers become zero in the shape and quoted lexemes in the numeric view.
// Strings/escapes are copied untouched. This is lexical preservation, not a second
// schema validator; all constraint decisions remain in validate_value below.
fn number_views(text: &str) -> (String, String) {
    let bytes = text.as_bytes();
    let (mut shape, mut numbers) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() {
                let c = bytes[i];
                i += 1;
                if c == b'\\' {
                    i = (i + 1).min(bytes.len());
                } else if c == b'"' {
                    break;
                }
            }
        } else if bytes[i] == b'-' || bytes[i].is_ascii_digit() {
            i += 1;
            while i < bytes.len() && matches!(bytes[i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                i += 1;
            }
            let token = &text[start..i];
            // A number's grammar must be checked before replacing it; parsing a
            // single token also rejects leading zeroes/trailing numeric junk.
            if !decimal_syntax(token) {
                return (String::new(), String::new());
            }
            shape.push(b'0');
            numbers.push(b'"');
            numbers.extend_from_slice(token.as_bytes());
            numbers.push(b'"');
            continue;
        } else {
            i += 1;
        }
        shape.extend_from_slice(&bytes[start..i]);
        numbers.extend_from_slice(&bytes[start..i]);
    }
    // Copies preserve UTF-8; replacements contain only ASCII.
    (
        String::from_utf8(shape).unwrap(),
        String::from_utf8(numbers).unwrap(),
    )
}

fn decimal_syntax(text: &str) -> bool {
    let b = text.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    match b.get(i) {
        Some(b'0') => i += 1,
        Some(b'1'..=b'9') => {
            i += 1;
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        }
        _ => return false,
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    i == b.len()
}

// Signed decimal addition for the exponent only. Its work is proportional to
// input length, never to the exponent's value (no expanded powers of ten).
fn exponent_add(a: &str, delta: i64) -> String {
    let negative = a.starts_with('-');
    let a = a.trim_start_matches(['-', '+']).trim_start_matches('0');
    let b = delta.unsigned_abs().to_string();
    let same = negative == (delta < 0);
    let cmp = a.len().cmp(&b.len()).then_with(|| a.cmp(&b));
    let (large, small, sign) = if !cmp.is_lt() {
        (a, b.as_str(), negative)
    } else {
        (b.as_str(), a, delta < 0)
    };
    let mut out = Vec::new();
    let mut carry = 0i16;
    let mut small = small.bytes().rev();
    for digit in large.bytes().rev() {
        let x = i16::from(digit - b'0');
        let y = small.next().map_or(0, |c| i16::from(c - b'0'));
        let n = if same { x + y + carry } else { x - y + carry };
        out.push((n.rem_euclid(10) as u8) + b'0');
        carry = if same {
            n / 10
        } else if n < 0 {
            -1
        } else {
            0
        };
    }
    if carry > 0 {
        out.push(b'1');
    }
    while out.last() == Some(&b'0') {
        out.pop();
    }
    if out.is_empty() {
        return "0".into();
    }
    if sign {
        out.push(b'-');
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

fn signed_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let an = a.starts_with('-');
    let bn = b.starts_with('-');
    if an != bn {
        return bn.cmp(&an);
    }
    let a = a.trim_start_matches('-');
    let b = b.trim_start_matches('-');
    let cmp = a.len().cmp(&b.len()).then_with(|| a.cmp(b));
    if an {
        cmp.reverse()
    } else {
        cmp
    }
}

struct Decimal {
    negative: bool,
    digits: String,
    order: String,
}
impl Decimal {
    fn new(raw: &str) -> Self {
        let negative = raw.starts_with('-');
        let raw = raw.trim_start_matches('-');
        let (mantissa, exp) = raw.split_once(['e', 'E']).unwrap_or((raw, "0"));
        let fraction = mantissa.split_once('.').map_or(0, |(_, f)| f.len());
        let digits = mantissa.replace('.', "");
        let digits = digits.trim_start_matches('0');
        if digits.is_empty() {
            return Self {
                negative: false,
                digits: "0".into(),
                order: "0".into(),
            };
        }
        let order = exponent_add(exp, digits.len() as i64 - fraction as i64);
        Self {
            negative,
            digits: digits.trim_end_matches('0').into(),
            order,
        }
    }
    fn integer(&self) -> bool {
        self.digits == "0" || signed_cmp(&self.order, &self.digits.len().to_string()).is_ge()
    }
    fn cmp(&self, b: &Self) -> std::cmp::Ordering {
        if self.negative != b.negative {
            return b.negative.cmp(&self.negative);
        }
        let cmp = if self.digits == "0" || b.digits == "0" {
            (self.digits != "0").cmp(&(b.digits != "0"))
        } else {
            signed_cmp(&self.order, &b.order).then_with(|| {
                let n = self.digits.len().max(b.digits.len());
                self.digits
                    .bytes()
                    .chain(std::iter::repeat(b'0'))
                    .take(n)
                    .cmp(b.digits.bytes().chain(std::iter::repeat(b'0')).take(n))
            })
        };
        if self.negative {
            cmp.reverse()
        } else {
            cmp
        }
    }
}

fn schema_equal(schema: &Value, value: &Value, raw: &Value) -> bool {
    match (schema, value) {
        (Value::Number(a), Value::Number(_)) => Decimal::new(&a.to_string())
            .cmp(&Decimal::new(raw.as_str().unwrap()))
            .is_eq(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .enumerate()
                    .all(|(i, (a, b))| schema_equal(a, b, &raw[i]))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, a)| b.get(k).is_some_and(|b| schema_equal(a, b, &raw[k])))
        }
        _ => schema == value,
    }
}

fn pointer_child(path: &str, key: &str) -> String {
    format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"))
}

fn validate_value(
    schema: &Value,
    value: &Value,
    raw: &Value,
    path: &str,
) -> Result<(), OutputValidationError> {
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
            "integer" => value.is_number() && Decimal::new(raw.as_str().unwrap()).integer(),
            _ => false,
        };
        if !valid {
            return Err(fail("Wrong value type"));
        }
    }
    if let Some(choices) = schema.get("enum").and_then(Value::as_array) {
        if !choices.iter().any(|choice| schema_equal(choice, value, raw)) {
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
    if value.is_number() {
        for (keyword, forbidden) in [
            ("minimum", std::cmp::Ordering::Less),
            ("maximum", std::cmp::Ordering::Greater),
        ] {
            if schema
                .get(keyword)
                .and_then(Value::as_number)
                .is_some_and(|bound| {
                    Decimal::new(raw.as_str().unwrap()).cmp(&Decimal::new(&bound.to_string())) == forbidden
                })
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
                validate_value(child_schema, child_value, &raw[key], &child_path)?;
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
            validate_value(items, item, &raw[index], &pointer_child(path, &index.to_string()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decimal_order_matches_exact_scaled_integer_arithmetic() {
        let samples: Vec<_> = (-9i128..=9)
            .flat_map(|n| (-3i32..=3).map(move |e| (format!("{n}e{e}"), n * 10i128.pow((e + 3) as u32))))
            .collect();
        for (a, scaled_a) in &samples {
            assert_eq!(Decimal::new(a).integer(), scaled_a % 1000 == 0, "{a}");
            for (b, scaled_b) in &samples {
                assert_eq!(
                    Decimal::new(a).cmp(&Decimal::new(b)),
                    scaled_a.cmp(scaled_b),
                    "{a} versus {b}"
                );
            }
        }
        assert_eq!(exponent_add("999999999999999999999", 1), "1000000000000000000000");
        assert_eq!(
            exponent_add("-1000000000000000000000", 1),
            "-999999999999999999999"
        );
        assert_eq!(
            exponent_add("-999999999999999999999", -1),
            "-1000000000000000000000"
        );
        assert_eq!(
            Decimal::new("1e999999999999999999999").cmp(&Decimal::new("10e999999999999999999998")),
            std::cmp::Ordering::Equal
        );
    }

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
