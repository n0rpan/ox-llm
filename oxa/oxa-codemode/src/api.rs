//! E6: the script globals (`tools`, `text`, `store`, `searchTools`, ..).
// ported from pi packages/codemode/src/{declarations,identifier,portable}.ts
use crate::tool::{MAX_OUTPUT_CHARS, MAX_OUTPUT_ITEMS, MAX_STORE_TOTAL_CHARS, MAX_STORE_VALUE_CHARS};
use crate::{Namespace, Shared, CODEMODE_TOOL, STORE_ENTRY_TYPE};
use oxa_proto::{codes, Host, HostAction, OxaError, ToolSpec, Value};
use oxa_script::{CallCx, HostFn, InterruptHandle, Runtime};
use serde_json::{json, Map};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

/// What a running script accumulates; read by the tool after the script ends.
#[derive(Debug, Default)]
pub struct ScriptSink {
    pub texts: Vec<String>,
    pub console: Vec<String>,
    pub store_writes: Vec<(String, Value)>,
    /// Set by `exit()`.
    pub exited: bool,
    /// Total characters of output so far (limit `MAX_OUTPUT_CHARS`).
    pub chars: usize,
}

/// Install the globals of one script run on `rt`:
///
/// * `tools.<ident>(args)` for every catalog tool except `codemode` itself (scripts cannot
///   start scripts), named by [`js_identifier`]. Async (`Runtime::register_async`): resolves to
///   the tool's text output, or for `bash` to `{output, exit_code}`, or for an MCP tool to its
///   `CallToolResult` JSON; rejects with an `Error` carrying the tool's error text when the
///   call fails, is blocked or has invalid arguments. Calls go through
///   `Host::call(HostAction::CallTool)`.
/// * `text(value)`: strings as is, other values as JSON. `console.{log,info,warn,error,debug}`.
/// * `exit()`: end the script successfully.
/// * `store(key, value)` / `load(key)`: limits `MAX_STORE_VALUE_CHARS`, `MAX_STORE_TOTAL_CHARS`;
///   storing `undefined` deletes. Reads see this script's own writes.
/// * `ALL_TOOLS`: `[{name, description}]`. `searchTools(query, {limit?, namespace?})` ->
///   [`crate::rank`]. `describeTool(name)` -> `{description, declaration}` | `undefined` with
///   [`ts_declaration`]. `describeNamespace(name)`.
/// * `image()`, `models.*`: throw `unsupported_in_oxa`.
pub fn install_globals(
    rt: &mut Runtime,
    shared: &Arc<Shared>,
    sink: &Arc<Mutex<ScriptSink>>,
) -> Result<(), OxaError> {
    let interrupt = rt.interrupt_handle();
    let specs = callable_specs(shared);

    let _ = rt.set_global("tools", &Value::Object(Map::new()));
    let mut idents = BTreeSet::new();
    for spec in &specs {
        let ident = js_identifier(&spec.name);
        if !idents.insert(ident.clone()) {
            continue;
        }
        let name = spec.name.clone();
        let sh = shared.clone();
        rt.register_async(
            &format!("tools.{ident}"),
            host_fn(move |_cx, args| call_tool(&sh, &name, args)),
        );
    }

    let all: Vec<Value> = specs
        .iter()
        .map(|s| json!({"name": s.name, "description": s.description}))
        .collect();
    let _ = rt.set_global("ALL_TOOLS", &Value::Array(all));

    {
        let sk = sink.clone();
        let int = interrupt.clone();
        rt.register(
            "text",
            host_fn(move |_cx, args| {
                let item = args.first().map(text_of).unwrap_or_default();
                push_output(&sk, &int, false, item);
                Ok(Value::Null)
            }),
        );
    }
    {
        let sk = sink.clone();
        rt.register(
            "exit",
            host_fn(move |_cx, _args| {
                sk.lock().unwrap_or_else(|p| p.into_inner()).exited = true;
                Err(OxaError::new(codes::SCRIPT, "script exited"))
            }),
        );
    }
    {
        let sk = sink.clone();
        let sh = shared.clone();
        rt.register(
            "store",
            host_fn(move |_cx, args| {
                let key = args
                    .first()
                    .and_then(Value::as_str)
                    .ok_or_else(|| OxaError::new(codes::INVALID, "store needs a string key"))?
                    .to_string();
                let value = args.get(1).cloned().unwrap_or(Value::Null);
                check_store_write(&sh, &sk, &key, &value)?;
                sk.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .store_writes
                    .push((key, value));
                Ok(Value::Null)
            }),
        );
    }
    {
        let sk = sink.clone();
        let sh = shared.clone();
        rt.register(
            "__cm_load",
            host_fn(move |_cx, args| {
                let key = args.first().and_then(Value::as_str).unwrap_or_default();
                Ok(store_view(&sh, &sk).remove(key).unwrap_or(Value::Null))
            }),
        );
    }
    {
        let sh = shared.clone();
        rt.register(
            "searchTools",
            host_fn(move |_cx, args| {
                let query = args.first().and_then(Value::as_str).unwrap_or_default();
                let opts = args.get(1).and_then(Value::as_object);
                let limit = opts
                    .and_then(|o| o.get("limit"))
                    .and_then(Value::as_u64)
                    .filter(|n| *n > 0)
                    .unwrap_or(DEFAULT_LIMIT as u64) as usize;
                let namespace = opts.and_then(|o| o.get("namespace")).and_then(Value::as_str);
                let specs = callable_specs(&sh);
                let hits = crate::rank(query, &specs, &(sh.namespaces)(), limit, namespace);
                Ok(Value::Array(
                    hits.into_iter()
                        .map(|h| json!({"name": h.name, "description": h.description}))
                        .collect(),
                ))
            }),
        );
    }
    {
        let sh = shared.clone();
        rt.register(
            "__cm_describeTool",
            host_fn(move |_cx, args| {
                let name = args.first().and_then(Value::as_str).unwrap_or_default();
                let found = callable_specs(&sh).into_iter().find(|s| s.name == name);
                Ok(match found {
                    Some(spec) => json!({
                        "description": spec.description,
                        "declaration": ts_declaration(&spec),
                    }),
                    None => Value::Null,
                })
            }),
        );
    }
    {
        let sh = shared.clone();
        rt.register(
            "__cm_describeNamespace",
            host_fn(move |_cx, args| {
                let name = args.first().and_then(Value::as_str).unwrap_or_default();
                let ns = (sh.namespaces)().into_iter().find(|n| n.name == name);
                Ok(match ns {
                    Some(ns) => namespace_value(&ns, &callable_specs(&sh)),
                    None => Value::Null,
                })
            }),
        );
    }
    rt.register(
        "image",
        host_fn(|_cx, _args| Err(OxaError::unsupported("image()"))),
    );
    let _ = rt.set_global("models", &Value::Object(Map::new()));
    for method in [
        "getModelsOfType",
        "getAvailableOfType",
        "getModelOfType",
        "classify",
        "generateImages",
    ] {
        rt.register(
            &format!("models.{method}"),
            host_fn(move |_cx, _args| Err(OxaError::unsupported(&format!("models.{method}()")))),
        );
    }

    let sk = sink.clone();
    rt.set_console(Box::new(move |_level, line| {
        push_output(&sk, &interrupt, true, line.to_string());
    }));
    rt.eval_script("codemode:globals", UNDEFINED_WRAPPER)?;
    Ok(())
}

/// Installed after the host functions: the lookups that answer "no such key / name" with `null`
/// in the bridge (`load`, `describeTool`, `describeNamespace`) answer JS `undefined`, as pi
/// documents. The raw functions are registered as `__cm_*` and removed from the global object.
/// `null` is not changed anywhere else (`oxa-script` maps `Value::Null` to `null`).
const UNDEFINED_WRAPPER: &str = r#"(function () {
  const undef = (f) => (...args) => {
    const v = f(...args);
    return v === null ? undefined : v;
  };
  for (const n of ["load", "describeTool", "describeNamespace"]) {
    const raw = globalThis["__cm_" + n];
    delete globalThis["__cm_" + n];
    globalThis[n] = undef(raw);
  }
})();"#;

const DEFAULT_LIMIT: usize = 8;

/// Adapts a closure to the host function type of `oxa_script`.
fn host_fn<F>(f: F) -> HostFn
where
    F: Fn(&mut CallCx, &[Value]) -> Result<Value, OxaError> + Send + 'static,
{
    Box::new(f)
}

/// The catalog without `codemode` itself, sorted by name (the catalog is sorted already).
fn callable_specs(shared: &Arc<Shared>) -> Vec<ToolSpec> {
    let mut specs: Vec<ToolSpec> = (shared.catalog)()
        .into_iter()
        .filter(|s| s.name != CODEMODE_TOOL)
        .collect();
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs
}

/// One `tools.<ident>(args)` call: `CallTool` through the host; text on success, an `Error`
/// carrying the tool's text on failure.
fn call_tool(shared: &Arc<Shared>, name: &str, args: &[Value]) -> Result<Value, OxaError> {
    let host = shared.host()?;
    let arg = match args.first() {
        Some(v) if !v.is_null() => v.clone(),
        _ => Value::Object(Map::new()),
    };
    let reply = host.call(HostAction::CallTool {
        name: name.to_string(),
        args: arg,
    })?;
    let text = reply
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if reply.get("isError").and_then(Value::as_bool).unwrap_or(false) {
        let message = if text.is_empty() {
            format!("tool {name} failed")
        } else {
            text
        };
        return Err(OxaError::new(codes::SCRIPT, message));
    }
    Ok(Value::String(text))
}

/// Text of a script value: strings as is, anything else as JSON.
fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Add one output line (`text()` when `console` is false, a console call otherwise). The line
/// is always recorded; once a limit is passed the script is interrupted, and the tool reports
/// the limit as the failure.
fn push_output(sink: &Arc<Mutex<ScriptSink>>, interrupt: &InterruptHandle, console: bool, line: String) {
    let mut s = sink.lock().unwrap_or_else(|p| p.into_inner());
    s.chars += line.chars().count();
    if console {
        s.console.push(line);
    } else {
        s.texts.push(line);
    }
    let over = s.chars > MAX_OUTPUT_CHARS || s.texts.len() + s.console.len() > MAX_OUTPUT_ITEMS;
    drop(s);
    if over {
        interrupt.interrupt();
    }
}

/// Reject a store write that passes the value or the total limit.
fn check_store_write(
    shared: &Arc<Shared>,
    sink: &Arc<Mutex<ScriptSink>>,
    key: &str,
    value: &Value,
) -> Result<(), OxaError> {
    if value.is_null() {
        return Ok(());
    }
    let value_chars = value.to_string().chars().count();
    if value_chars > MAX_STORE_VALUE_CHARS {
        return Err(OxaError::new(
            codes::INVALID,
            format!("store value for {key} exceeds {MAX_STORE_VALUE_CHARS} characters of JSON"),
        ));
    }
    let mut view = store_view(shared, sink);
    view.insert(key.to_string(), value.clone());
    let total: usize = view.values().map(|v| v.to_string().chars().count()).sum();
    if total > MAX_STORE_TOTAL_CHARS {
        return Err(OxaError::new(
            codes::INVALID,
            format!("store values exceed {MAX_STORE_TOTAL_CHARS} characters of JSON in total"),
        ));
    }
    Ok(())
}

/// The session store as this script sees it: the shared store with the script's own writes
/// applied in order (`Null` removes).
fn store_view(shared: &Arc<Shared>, sink: &Arc<Mutex<ScriptSink>>) -> BTreeMap<String, Value> {
    let mut view = shared.store.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let s = sink.lock().unwrap_or_else(|p| p.into_inner());
    for (key, value) in &s.store_writes {
        if value.is_null() {
            view.remove(key);
        } else {
            view.insert(key.clone(), value.clone());
        }
    }
    view
}

/// `describeNamespace` result: name, optional description and instructions, and the names of
/// the callable tools of the namespace (`mcp__<name>__*`).
fn namespace_value(ns: &Namespace, specs: &[ToolSpec]) -> Value {
    let prefix = format!("mcp__{}__", ns.name);
    let tools: Vec<String> = specs
        .iter()
        .filter(|s| s.name.starts_with(&prefix))
        .map(|s| s.name.clone())
        .collect();
    let mut m = Map::new();
    m.insert("name".into(), json!(ns.name));
    if let Some(d) = &ns.description {
        m.insert("description".into(), json!(d));
    }
    if let Some(i) = &ns.instructions {
        m.insert("instructions".into(), json!(i));
    }
    m.insert("tools".into(), json!(tools));
    Value::Object(m)
}

/// pi's identifier rule: every character that is not valid in a JavaScript identifier becomes
/// `_` (`mcp__dev-radius__search` -> `mcp__dev_radius__search`); a leading digit gets a `_`
/// prefix.
pub fn js_identifier(tool_name: &str) -> String {
    let mut out = String::new();
    for c in tool_name.chars() {
        if out.is_empty() && c.is_ascii_digit() {
            out.push('_');
            out.push(c);
            continue;
        }
        let valid = if out.is_empty() {
            c.is_ascii_alphabetic() || c == '_' || c == '$'
        } else {
            c.is_ascii_alphanumeric() || c == '_' || c == '$'
        };
        out.push(if valid { c } else { '_' });
    }
    if out.is_empty() {
        "_".to_string()
    } else {
        out
    }
}

/// A TypeScript declaration of a tool from its JSON Schema, for `describeTool`:
/// `declare function <ident>(args: { .. }): Promise<..>;` with `$ref`s resolved, property
/// order as in the schema. Deterministic. The result is text (`Promise<string>`): the host
/// returns tool output as text.
pub fn ts_declaration(spec: &ToolSpec) -> String {
    let schema: Value = serde_json::from_str(&spec.schema_json).unwrap_or(Value::Null);
    let args = schema_type(&schema, &schema, 0);
    format!(
        "declare function {}(args: {}): Promise<string>;",
        js_identifier(&spec.name),
        args
    )
}

const MAX_TYPE_DEPTH: usize = 16;

fn schema_type(root: &Value, schema: &Value, depth: usize) -> String {
    if depth > MAX_TYPE_DEPTH {
        return "unknown".to_string();
    }
    let Some(obj) = schema.as_object() else {
        return "unknown".to_string();
    };
    if let Some(r) = obj.get("$ref").and_then(Value::as_str) {
        return match root.pointer(r.strip_prefix('#').unwrap_or("")) {
            Some(target) if r.starts_with('#') => schema_type(root, target, depth + 1),
            _ => "unknown".to_string(),
        };
    }
    if let Some(c) = obj.get("const") {
        return literal(c);
    }
    if let Some(values) = obj.get("enum").and_then(Value::as_array) {
        if values.is_empty() {
            return "never".to_string();
        }
        return values.iter().map(literal).collect::<Vec<_>>().join(" | ");
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(alts) = obj.get(key).and_then(Value::as_array) {
            let parts: Vec<String> = alts.iter().map(|a| schema_type(root, a, depth + 1)).collect();
            return if parts.is_empty() {
                "never".to_string()
            } else {
                parts.join(" | ")
            };
        }
    }
    if let Some(all) = obj.get("allOf").and_then(Value::as_array) {
        let parts: Vec<String> = all
            .iter()
            .map(|a| wrap(&schema_type(root, a, depth + 1), " | "))
            .collect();
        return if parts.is_empty() {
            "unknown".to_string()
        } else {
            parts.join(" & ")
        };
    }
    match obj.get("type") {
        Some(Value::String(t)) => base_type(root, obj, t, depth),
        Some(Value::Array(ts)) => {
            let parts: Vec<String> = ts
                .iter()
                .filter_map(Value::as_str)
                .map(|t| base_type(root, obj, t, depth))
                .collect();
            if parts.is_empty() {
                "unknown".to_string()
            } else {
                parts.join(" | ")
            }
        }
        _ => {
            if obj.contains_key("properties") {
                object_type(root, obj, depth)
            } else if obj.contains_key("items") {
                base_type(root, obj, "array", depth)
            } else {
                "unknown".to_string()
            }
        }
    }
}

fn base_type(root: &Value, obj: &Map<String, Value>, t: &str, depth: usize) -> String {
    match t {
        "string" => "string".to_string(),
        "number" | "integer" => "number".to_string(),
        "boolean" => "boolean".to_string(),
        "null" => "null".to_string(),
        "array" => match obj.get("items") {
            Some(items) => format!("{}[]", wrap(&schema_type(root, items, depth + 1), " | ")),
            None => "unknown[]".to_string(),
        },
        "object" => object_type(root, obj, depth),
        _ => "unknown".to_string(),
    }
}

fn object_type(root: &Value, obj: &Map<String, Value>, depth: usize) -> String {
    let required: Vec<&str> = obj
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    match obj.get("properties").and_then(Value::as_object) {
        Some(props) if !props.is_empty() => {
            let members: Vec<String> = props
                .iter()
                .map(|(name, schema)| {
                    let optional = if required.contains(&name.as_str()) {
                        ""
                    } else {
                        "?"
                    };
                    format!(
                        "{}{}: {}",
                        property_key(name),
                        optional,
                        schema_type(root, schema, depth + 1)
                    )
                })
                .collect();
            format!("{{ {} }}", members.join("; "))
        }
        _ => match obj.get("additionalProperties") {
            Some(extra) if extra.is_object() => {
                format!("{{ [key: string]: {} }}", schema_type(root, extra, depth + 1))
            }
            _ => "Record<string, unknown>".to_string(),
        },
    }
}

fn property_key(name: &str) -> String {
    let plain = !name.is_empty()
        && name.chars().enumerate().all(|(i, c)| {
            if i == 0 {
                c.is_ascii_alphabetic() || c == '_' || c == '$'
            } else {
                c.is_ascii_alphanumeric() || c == '_' || c == '$'
            }
        });
    if plain {
        name.to_string()
    } else {
        serde_json::to_string(name).unwrap_or_else(|_| "\"\"".to_string())
    }
}

fn literal(v: &Value) -> String {
    match v {
        Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => v.to_string(),
        _ => "unknown".to_string(),
    }
}

/// Parenthesise a union before it is used inside an array or intersection.
fn wrap(t: &str, union_marker: &str) -> String {
    if t.contains(union_marker) {
        format!("({t})")
    } else {
        t.to_string()
    }
}

/// The store as of the active branch: the `data` of the newest `codemode-store` custom entry
/// (`HostAction::GetBranch`), or empty. Never fails (problems = empty store).
pub fn load_store(host: &dyn Host) -> BTreeMap<String, Value> {
    let Ok(Value::Array(entries)) = host.call(HostAction::GetBranch) else {
        return BTreeMap::new();
    };
    for entry in entries.iter().rev() {
        let is_store = entry.get("type").and_then(Value::as_str) == Some("custom")
            && entry.get("customType").and_then(Value::as_str) == Some(STORE_ENTRY_TYPE);
        if !is_store {
            continue;
        }
        return match entry.get("data").and_then(Value::as_object) {
            Some(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            None => BTreeMap::new(),
        };
    }
    BTreeMap::new()
}
