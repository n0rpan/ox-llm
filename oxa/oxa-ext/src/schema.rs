//! Tool argument validation for extension tools (EXT-063): the JSON Schema subset that pi's
//! TypeBox / pi-schema tool schemas use, checked before `execute` runs, with pi's error text for
//! the model (pi `packages/ai/src/utils/validation.ts`, messages of
//! `packages/schema/src/explain.ts`).
//!
//! Supported keywords: `type` (one or a list), `enum`, `const`, `properties`, `required`,
//! `additionalProperties` (`false` or a schema), `items` (a schema or a tuple list),
//! `minItems` / `maxItems`, `minLength` / `maxLength`, `pattern` is ignored, `minimum` /
//! `maximum` / `exclusiveMinimum` / `exclusiveMaximum`, `anyOf` / `oneOf` (one alternative must
//! match). Unknown keywords are ignored. As in pi, arguments are coerced first: a numeric
//! string for a `number` / `integer`, `"true"` / `"false"` for a `boolean`, a single value for
//! an `array`, and `null` for an optional property is dropped.
use oxa_proto::Value;

/// Validate (and coerce) `args` against `schema` for the tool `tool`. `Ok` = the arguments
/// `execute` receives; `Err` = pi's message: `Validation failed for tool "<name>":\n  - <path>:
/// <message>..\n\nReceived arguments:\n<pretty JSON>`.
pub fn validate_tool_args(tool: &str, schema: &Value, args: &Value) -> Result<Value, String> {
    let mut value = args.clone();
    coerce(schema, &mut value);
    let mut errors = Vec::new();
    walk(schema, &value, "", &mut errors);
    if errors.is_empty() {
        return Ok(value);
    }
    let list: Vec<String> = errors
        .iter()
        .map(|(path, msg)| format!("  - {}: {msg}", display_path(path)))
        .collect();
    let received = serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string());
    Err(format!(
        "Validation failed for tool \"{tool}\":\n{}\n\nReceived arguments:\n{received}",
        list.join("\n")
    ))
}

/// pi's `formatValidationPath`: a JSON pointer as a dotted path; the root is `root`.
fn display_path(pointer: &str) -> String {
    if pointer.is_empty() {
        return "root".to_string();
    }
    pointer[1..]
        .split('/')
        .map(|s| s.replace("~1", "/").replace("~0", "~"))
        .collect::<Vec<_>>()
        .join(".")
}

fn pointer(path: &str, key: &str) -> String {
    format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"))
}

fn types_of(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// pi-schema's coercion, for the keywords above.
fn coerce(schema: &Value, v: &mut Value) {
    let types = types_of(schema);
    let wants = |t: &str| types.contains(&t);
    if !types.is_empty() && !types.iter().any(|t| type_ok(t, v)) {
        match v {
            Value::String(s) if wants("integer") || wants("number") => {
                let t = s.trim();
                if let Ok(i) = t.parse::<i64>() {
                    *v = Value::from(i);
                } else if let Ok(f) = t.parse::<f64>() {
                    if wants("number") || f.fract() == 0.0 {
                        if let Some(n) = serde_json::Number::from_f64(f) {
                            *v = Value::Number(n);
                        }
                    }
                }
            }
            Value::String(s) if wants("boolean") && (s == "true" || s == "false") => {
                *v = Value::Bool(s == "true");
            }
            Value::Array(_) | Value::Object(_) | Value::Null => {}
            _ if wants("array") => {
                let one = std::mem::take(v);
                *v = Value::Array(vec![one]);
            }
            _ => {}
        }
    }
    match v {
        Value::Object(map) => {
            let props = schema.get("properties").and_then(Value::as_object);
            let required: Vec<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .map(|r| r.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if let Some(props) = props {
                let keys: Vec<String> = map.keys().cloned().collect();
                for k in keys {
                    let Some(ps) = props.get(&k) else { continue };
                    if map.get(&k).is_some_and(Value::is_null)
                        && !required.contains(&k.as_str())
                        && !type_ok_schema(ps, &Value::Null)
                    {
                        map.remove(&k);
                        continue;
                    }
                    if let Some(item) = map.get_mut(&k) {
                        coerce(ps, item);
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items").filter(|s| s.is_object()) {
                for it in items.iter_mut() {
                    coerce(item_schema, it);
                }
            }
        }
        _ => {}
    }
}

fn type_ok_schema(schema: &Value, v: &Value) -> bool {
    let types = types_of(schema);
    types.is_empty() || types.iter().any(|t| type_ok(t, v))
}

fn type_ok(t: &str, v: &Value) -> bool {
    match t {
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => {
            v.as_i64().is_some() || v.as_u64().is_some() || v.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        "boolean" => v.is_boolean(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        "null" => v.is_null(),
        _ => true,
    }
}

/// pi-schema's `typeName`.
fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0) {
                "integer"
            } else {
                "number"
            }
        }
    }
}

fn walk(schema: &Value, v: &Value, path: &str, out: &mut Vec<(String, String)>) {
    if schema.as_bool() == Some(false) {
        out.push((path.to_string(), "no value is allowed here".to_string()));
        return;
    }
    if !schema.is_object() {
        return;
    }
    let types = types_of(schema);
    if !types.is_empty() && !types.iter().any(|t| type_ok(t, v)) {
        out.push((
            path.to_string(),
            format!("expected {}, got {}", types.join(" or "), type_name(v)),
        ));
        return;
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        if !values.contains(v) {
            let list = serde_json::to_string(values).unwrap_or_default();
            out.push((path.to_string(), format!("expected one of {list}")));
        }
    }
    if let Some(c) = schema.get("const") {
        if c != v {
            out.push((path.to_string(), format!("expected {c}")));
        }
    }
    match v {
        Value::String(s) => {
            let n = s.chars().count() as u64;
            if let Some(min) = schema.get("minLength").and_then(Value::as_u64) {
                if n < min {
                    out.push((path.to_string(), format!("expected at least {min} characters")));
                }
            }
            if let Some(max) = schema.get("maxLength").and_then(Value::as_u64) {
                if n > max {
                    out.push((path.to_string(), format!("expected at most {max} characters")));
                }
            }
        }
        Value::Number(n) => {
            let x = n.as_f64().unwrap_or(0.0);
            let bound = |k: &str| {
                schema
                    .get(k)
                    .and_then(Value::as_f64)
                    .map(|b| (b, schema[k].clone()))
            };
            if let Some((b, raw)) = bound("minimum") {
                if x < b {
                    out.push((path.to_string(), format!("expected >= {raw}")));
                }
            }
            if let Some((b, raw)) = bound("maximum") {
                if x > b {
                    out.push((path.to_string(), format!("expected <= {raw}")));
                }
            }
            if let Some((b, raw)) = bound("exclusiveMinimum") {
                if x <= b {
                    out.push((path.to_string(), format!("expected > {raw}")));
                }
            }
            if let Some((b, raw)) = bound("exclusiveMaximum") {
                if x >= b {
                    out.push((path.to_string(), format!("expected < {raw}")));
                }
            }
        }
        Value::Object(map) => {
            let props = schema.get("properties").and_then(Value::as_object);
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for k in required.iter().filter_map(Value::as_str) {
                    if !map.contains_key(k) {
                        out.push((pointer(path, k), "missing".to_string()));
                    }
                }
            }
            if let Some(props) = props {
                for (k, ps) in props {
                    if let Some(item) = map.get(k) {
                        walk(ps, item, &pointer(path, k), out);
                    }
                }
            }
            if let Some(extra) = schema.get("additionalProperties") {
                for (k, item) in map {
                    if props.is_some_and(|p| p.contains_key(k)) {
                        continue;
                    }
                    if extra.as_bool() == Some(false) {
                        out.push((pointer(path, k), "unknown field".to_string()));
                    } else if extra.is_object() {
                        walk(extra, item, &pointer(path, k), out);
                    }
                }
            }
        }
        Value::Array(items) => {
            match schema.get("items") {
                Some(Value::Array(tuple)) => {
                    for (i, (s, it)) in tuple.iter().zip(items).enumerate() {
                        walk(s, it, &format!("{path}/{i}"), out);
                    }
                }
                Some(s) if s.is_object() => {
                    for (i, it) in items.iter().enumerate() {
                        walk(s, it, &format!("{path}/{i}"), out);
                    }
                }
                _ => {}
            }
            let n = items.len() as u64;
            if let Some(min) = schema.get("minItems").and_then(Value::as_u64) {
                if n < min {
                    out.push((path.to_string(), format!("expected at least {min} items")));
                }
            }
            if let Some(max) = schema.get("maxItems").and_then(Value::as_u64) {
                if n > max {
                    out.push((path.to_string(), format!("expected at most {max} items")));
                }
            }
        }
        _ => {}
    }
    for (key, one) in [("anyOf", false), ("oneOf", true)] {
        if let Some(alts) = schema.get(key).and_then(Value::as_array) {
            let matching = alts
                .iter()
                .filter(|s| {
                    let mut e = Vec::new();
                    walk(s, v, path, &mut e);
                    e.is_empty()
                })
                .count();
            if matching == 0 {
                out.push((path.to_string(), "matches no alternative".to_string()));
            } else if one && matching > 1 {
                out.push((path.to_string(), "matches more than one alternative".to_string()));
            }
        }
    }
}
