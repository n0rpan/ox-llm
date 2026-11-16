//! F3: message, entry and tool renderers; Markdown transformers.
use crate::{ExtError, ExtId, ExtState, RegisteredRenderer, Registry};
use oxa_proto::{codes, OxaError, RenderKind, Value};
use oxa_script::{CallCx, FnRef, Runtime, RuntimeThread, SlotId, Slots};
use serde_json::json;
use std::sync::{Arc, MutexGuard};

/// The `key` of a tool renderer resolver (`oxa.registerToolRenderer(resolver)`): it applies to
/// every tool, and the resolver decides per tool name.
const RESOLVER_KEY: &str = "*";

/// Bind on `rt`: `oxa.registerMessageRenderer(customType, fn)`,
/// `oxa.registerEntryRenderer(customType, fn)`, `oxa.registerToolRenderer(resolver)`,
/// `oxa.registerMarkdownTransformer(fn, {messageTypes})`. Registrations go to
/// `Registry::renderers` (tool definitions' own `renderCall` / `renderResult` are already in
/// `RegisteredTool`).
///
/// W0: a no-op so wave E can load extensions before F3 lands.
pub fn install(rt: &mut Runtime, st: &Arc<ExtState>, ext: ExtId) {
    let api = Api { st: st.clone(), ext };
    bind(rt, &api, "oxa.registerMessageRenderer", |api, _cx, args| {
        register_keyed(api, RenderKind::Message, args)
    });
    bind(rt, &api, "oxa.registerEntryRenderer", |api, _cx, args| {
        register_keyed(api, RenderKind::Entry, args)
    });
    bind(rt, &api, "oxa.registerToolRenderer", register_tool_renderer);
    bind(rt, &api, "oxa.registerMarkdownTransformer", register_markdown);
}

/// Draw with the renderer registered for `kind` and the payload's key (`customType`,
/// `toolName`, `messageType`); the last registration for a key wins. The script returns a
/// component or lines; the result is rendered for `width`. `None` when no renderer matches or
/// the renderer throws (the built-in view is then used; the error is pushed to
/// `ExtState::errors`). For `RenderKind::Markdown` all matching transformers run in
/// registration order and the result is the new Markdown source split into lines.
///
/// Arguments, as `oxa.d.ts` declares them:
/// * `Message` / `Entry`: `renderer(payload, {width, expanded})`.
/// * `ToolCall`: `renderCall(args, {toolName, width})`, from the resolver registered with
///   `registerToolRenderer` first, then the tool definition's own `renderCall`.
/// * `ToolResult`: `renderResult(result, {width, expanded, isError, partial}, {toolName, args})`,
///   resolved the same way.
/// * `Markdown`: `transformer(markdown, {messageType})`, each one fed the previous output.
///
/// A renderer that returns `undefined` (or `null`) declines: the built-in view is used.
pub fn render(
    st: &Arc<ExtState>,
    thread: &Arc<RuntimeThread>,
    kind: RenderKind,
    payload: &Value,
    width: u16,
) -> Option<Vec<String>> {
    if kind == RenderKind::Markdown {
        return render_markdown(st, thread, payload);
    }
    let plan = plan(st, kind, payload, width);
    if plan.candidates.is_empty() {
        return None;
    }
    let paths: Vec<String> = plan.candidates.iter().map(|c| c.path.clone()).collect();
    let candidates = plan.candidates;
    let args = plan.args;
    let tool = plan.tool;
    let out = thread.run(
        move |slots: &mut Slots| -> Result<Value, (usize, OxaError)> {
            for (i, c) in candidates.iter().enumerate() {
                let fail = |e: OxaError| (i, e);
                let rt = slots
                    .get(c.slot)
                    .ok_or_else(|| (i, OxaError::new(codes::SCRIPT, "the extension is not loaded")))?;
                match c.target {
                    Target::Fn(f) => return rt.call(&f, &args).map_err(fail),
                    Target::Resolver(r) => {
                        let obj = rt.call(&r, &[Value::String(tool.clone())]).map_err(fail)?;
                        let field = resolver_field(kind);
                        if let Some(fr) = obj.get(field).and_then(FnRef::from_value) {
                            let out = rt.call(&fr, &args);
                            rt.release(&fr);
                            return out.map_err(fail);
                        }
                    }
                }
            }
            Ok(Value::Null)
        },
        &mut |_: Value, _: &oxa_script::CallbackPort| Value::Null,
    );
    match out {
        Ok(Ok(v)) => to_lines(&v),
        Ok(Err((i, e))) => {
            report(st, paths.get(i).cloned().unwrap_or_default(), kind, e);
            None
        }
        Err(e) => {
            report(st, paths.first().cloned().unwrap_or_default(), kind, e);
            None
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Markdown transformers
// ---------------------------------------------------------------------------------------------

fn render_markdown(st: &Arc<ExtState>, thread: &Arc<RuntimeThread>, payload: &Value) -> Option<Vec<String>> {
    let message_type = text_field(payload, "messageType").to_string();
    let markdown = text_field(payload, "markdown").to_string();
    let chain: Vec<(SlotId, FnRef, String)> = {
        let reg = lock(st);
        let mut out = Vec::new();
        for r in reg.renderers.iter() {
            if r.kind == RenderKind::Markdown && key_matches(&r.key, &message_type) {
                out.push((r.slot, r.f, path_of(&reg, r.ext)));
            }
        }
        out
    };
    if chain.is_empty() {
        return None;
    }
    let paths: Vec<String> = chain.iter().map(|c| c.2.clone()).collect();
    let steps: Vec<(SlotId, FnRef)> = chain.iter().map(|c| (c.0, c.1)).collect();
    let mt = message_type.clone();
    let out = thread.run(
        move |slots: &mut Slots| -> (String, Vec<(usize, OxaError)>) {
            let mut current = markdown;
            let mut errors = Vec::new();
            for (i, (slot, f)) in steps.iter().enumerate() {
                let Some(rt) = slots.get(*slot) else {
                    errors.push((i, OxaError::new(codes::SCRIPT, "the extension is not loaded")));
                    continue;
                };
                match rt.call(f, &[Value::String(current.clone()), json!({"messageType": mt})]) {
                    Ok(Value::String(s)) => current = s,
                    Ok(Value::Array(items)) => {
                        let lines: Vec<String> = items
                            .iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect();
                        current = lines.join("\n");
                    }
                    // `undefined` / `null` (or anything else): the text is kept.
                    Ok(_) => {}
                    Err(e) => errors.push((i, e)),
                }
            }
            (current, errors)
        },
        &mut |_: Value, _: &oxa_script::CallbackPort| Value::Null,
    );
    match out {
        Ok((text, errors)) => {
            for (i, e) in errors {
                let path = paths.get(i).cloned().unwrap_or_default();
                report(st, path, RenderKind::Markdown, e);
            }
            Some(text.split('\n').map(str::to_string).collect())
        }
        Err(e) => {
            report(
                st,
                paths.first().cloned().unwrap_or_default(),
                RenderKind::Markdown,
                e,
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Planning a render: which renderers apply, and their arguments
// ---------------------------------------------------------------------------------------------

/// What runs for one candidate renderer.
#[derive(Clone, Copy)]
enum Target {
    /// A renderer function, called with the render arguments.
    Fn(FnRef),
    /// A `registerToolRenderer` resolver: called with the tool name; its answer's `renderCall` /
    /// `renderResult` is then called with the render arguments.
    Resolver(FnRef),
}

struct Candidate {
    slot: SlotId,
    target: Target,
    path: String,
}

struct Plan {
    /// In priority order.
    candidates: Vec<Candidate>,
    args: Vec<Value>,
    tool: String,
}

fn plan(st: &Arc<ExtState>, kind: RenderKind, payload: &Value, width: u16) -> Plan {
    let reg = lock(st);
    let expanded = payload.get("expanded").and_then(Value::as_bool).unwrap_or(false);
    let mut tool = String::new();
    let mut args = Vec::new();
    let mut candidates = Vec::new();
    match kind {
        RenderKind::Message | RenderKind::Entry => {
            let key = text_field(payload, "customType");
            if let Some(r) = reg
                .renderers
                .iter()
                .rev()
                .find(|r| r.kind == kind && r.key == key)
            {
                candidates.push(Candidate {
                    slot: r.slot,
                    target: Target::Fn(r.f),
                    path: path_of(&reg, r.ext),
                });
            }
            args = vec![payload.clone(), json!({"width": width, "expanded": expanded})];
        }
        RenderKind::ToolCall | RenderKind::ToolResult => {
            tool = text_field(payload, "toolName").to_string();
            let call_args = payload.get("args").cloned().unwrap_or(Value::Null);
            args = match kind {
                RenderKind::ToolCall => vec![call_args.clone(), json!({"toolName": tool, "width": width})],
                _ => vec![
                    payload.get("result").cloned().unwrap_or(Value::Null),
                    json!({
                        "width": width,
                        "expanded": expanded,
                        "isError": payload.get("isError").and_then(Value::as_bool).unwrap_or(false),
                        "partial": payload.get("partial").and_then(Value::as_bool).unwrap_or(false),
                    }),
                    json!({"toolName": tool, "args": call_args}),
                ],
            };
            for r in reg.renderers.iter().rev() {
                if r.kind == kind && r.key == RESOLVER_KEY {
                    candidates.push(Candidate {
                        slot: r.slot,
                        target: Target::Resolver(r.f),
                        path: path_of(&reg, r.ext),
                    });
                }
            }
            if let Some(t) = reg.tools.iter().find(|t| t.spec.name == tool) {
                let f = match kind {
                    RenderKind::ToolCall => t.render_call,
                    _ => t.render_result,
                };
                if let Some(f) = f {
                    candidates.push(Candidate {
                        slot: t.slot,
                        target: Target::Fn(f),
                        path: path_of(&reg, t.ext),
                    });
                }
            }
        }
        RenderKind::Markdown => {}
    }
    Plan {
        candidates,
        args,
        tool,
    }
}

/// The field of a resolver's answer that holds the renderer for `kind`.
fn resolver_field(kind: RenderKind) -> &'static str {
    match kind {
        RenderKind::ToolCall => "renderCall",
        _ => "renderResult",
    }
}

/// Turn a renderer's answer into lines. `undefined` / `null` and anything that is not text or
/// a list of text is a decline (`None`).
fn to_lines(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::String(s) => Some(s.split('\n').map(str::to_string).collect()),
        Value::Array(items) => items
            .iter()
            .map(|i| match i {
                Value::String(s) => Some(s.clone()),
                Value::Number(_) | Value::Bool(_) => Some(i.to_string()),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// Record a renderer error (it is pushed to `ExtState::errors`; the built-in view is used).
fn report(st: &Arc<ExtState>, extension_path: String, kind: RenderKind, e: OxaError) {
    st.push_error(ExtError {
        extension_path,
        event: event_name(kind).to_string(),
        error: format!("{}: {}", e.code, e.message),
    });
}

fn event_name(kind: RenderKind) -> &'static str {
    match kind {
        RenderKind::Message => "render:message",
        RenderKind::Entry => "render:entry",
        RenderKind::ToolCall => "render:toolCall",
        RenderKind::ToolResult => "render:toolResult",
        RenderKind::Markdown => "render:markdown",
    }
}

/// A Markdown transformer key applies to a message type when it is `*` or lists the type in
/// its comma-separated `messageTypes`.
fn key_matches(key: &str, message_type: &str) -> bool {
    key == RESOLVER_KEY || key.split(',').any(|k| k.trim() == message_type)
}

fn text_field<'a>(obj: &'a Value, key: &str) -> &'a str {
    obj.get(key).and_then(Value::as_str).unwrap_or("")
}

fn path_of(reg: &Registry, ext: ExtId) -> String {
    reg.extension(ext).map(|e| e.path.clone()).unwrap_or_default()
}

fn lock(st: &Arc<ExtState>) -> MutexGuard<'_, Registry> {
    st.registry.lock().unwrap_or_else(|p| p.into_inner())
}

// ---------------------------------------------------------------------------------------------
// Registration (runs on the runtime thread, inside the extension's module evaluation)
// ---------------------------------------------------------------------------------------------

/// What one `oxa.register*` host function needs: the shared state and the extension.
#[derive(Clone)]
struct Api {
    st: Arc<ExtState>,
    ext: ExtId,
}

impl Api {
    fn registry(&self) -> MutexGuard<'_, Registry> {
        lock(&self.st)
    }
}

fn bind<F>(rt: &mut Runtime, api: &Api, path: &str, f: F)
where
    F: Fn(&Api, &mut CallCx<'_>, &[Value]) -> Result<Value, OxaError> + Send + 'static,
{
    let api = api.clone();
    let host: oxa_script::HostFn = Box::new(move |cx: &mut CallCx<'_>, args: &[Value]| f(&api, cx, args));
    rt.register(path, host);
}

fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or(Value::Null)
}

fn invalid(msg: impl Into<String>) -> OxaError {
    OxaError::new(codes::INVALID, msg)
}

fn slot_of(reg: &Registry, ext: ExtId) -> Result<SlotId, OxaError> {
    reg.extension(ext)
        .and_then(|e| e.slot)
        .ok_or_else(|| OxaError::new(codes::INTERNAL, "the extension has no runtime to register on"))
}

/// `oxa.registerMessageRenderer(customType, fn)` and `oxa.registerEntryRenderer(customType, fn)`.
/// A later registration for the same key replaces the earlier one.
fn register_keyed(api: &Api, kind: RenderKind, args: &[Value]) -> Result<Value, OxaError> {
    let what = match kind {
        RenderKind::Entry => "entry renderer",
        _ => "message renderer",
    };
    let key = match arg(args, 0) {
        Value::String(s) if !s.is_empty() => s,
        _ => return Err(invalid(format!("{what}: customType must be a non-empty string"))),
    };
    let f = FnRef::from_value(&arg(args, 1))
        .ok_or_else(|| invalid(format!("{what} {key}: renderer must be a function")))?;
    let mut reg = api.registry();
    let slot = slot_of(&reg, api.ext)?;
    reg.renderers.retain(|r| !(r.kind == kind && r.key == key));
    reg.renderers.push(RegisteredRenderer {
        ext: api.ext,
        slot,
        kind,
        key,
        f,
    });
    Ok(Value::Null)
}

/// `oxa.registerToolRenderer(resolver)`: the resolver is stored once per kind (`ToolCall`,
/// `ToolResult`) under the wildcard key. A later registration by the same extension replaces
/// the earlier one.
fn register_tool_renderer(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let f = FnRef::from_value(&arg(args, 0))
        .ok_or_else(|| invalid("registerToolRenderer: the resolver must be a function"))?;
    let mut reg = api.registry();
    let slot = slot_of(&reg, api.ext)?;
    reg.renderers.retain(|r| {
        !(r.ext == api.ext
            && (r.kind == RenderKind::ToolCall || r.kind == RenderKind::ToolResult)
            && r.key == RESOLVER_KEY)
    });
    for kind in [RenderKind::ToolCall, RenderKind::ToolResult] {
        reg.renderers.push(RegisteredRenderer {
            ext: api.ext,
            slot,
            kind,
            key: RESOLVER_KEY.to_string(),
            f,
        });
    }
    Ok(Value::Null)
}

/// `oxa.registerMarkdownTransformer(fn, {messageTypes})`: `messageTypes` is a comma-separated
/// string (or a list of strings); absent or `*` means every message type. Transformers stack in
/// registration order.
fn register_markdown(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let f = FnRef::from_value(&arg(args, 0))
        .ok_or_else(|| invalid("registerMarkdownTransformer: the transformer must be a function"))?;
    let opts = arg(args, 1);
    let raw: Vec<String> = match opts.get("messageTypes") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| {
                i.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| invalid("messageTypes must list strings"))
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(invalid("messageTypes must be a string")),
    };
    let names: Vec<String> = raw
        .iter()
        .flat_map(|s| s.split(','))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let key = if names.is_empty() || names.iter().any(|n| n == RESOLVER_KEY) {
        RESOLVER_KEY.to_string()
    } else {
        names.join(",")
    };
    let mut reg = api.registry();
    let slot = slot_of(&reg, api.ext)?;
    reg.renderers.push(RegisteredRenderer {
        ext: api.ext,
        slot,
        kind: RenderKind::Markdown,
        key,
        f,
    });
    Ok(Value::Null)
}
