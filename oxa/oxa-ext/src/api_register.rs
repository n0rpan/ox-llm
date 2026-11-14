//! E3: `oxa.*` part 1 (registerTool / Command / Shortcut / Flag, sendMessage, appendEntry, active tools). Owner: task E3. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/core/extensions/types.ts (ExtensionAPI)
use crate::{
    serve_upcall, ExtId, ExtState, Handler, RegisteredCommand, RegisteredFlag, RegisteredShortcut,
    RegisteredTool, Registry, SharedUi,
};
use oxa_proto::{
    codes, Access, CommandSpec, Delivery, Exposure, FlagSpec, HookPoint, HostAction, KeyId, OxaError,
    ShortcutSpec, Source, ThinkingLevel, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Value,
};
use oxa_script::{CallCx, CallbackPort, FnRef, InterruptHandle, Runtime, RuntimeThread, SlotId, Slots};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, MutexGuard};
use std::time::Duration;

/// The upcall `op` a tool's `onUpdate` sends to the caller of `ExtTool::run` (partial output).
const TOOL_UPDATE_OP: &str = "tool_update";
/// The hidden global that `onUpdate` calls inside a tool run.
const TOOL_UPDATE_FN: &str = "__oxa_tool_update";

/// The JS glue of one tool run: `execute(id, params, null, onUpdate, ctx)` where `onUpdate`
/// forwards partial output as an upcall.
const TOOL_GLUE: &str = "(function () { return function (execute, id, params) { \
    var ctx = typeof __oxa_make_ctx === 'function' ? __oxa_make_ctx() : null; \
    return execute(id, params, null, function (partial) { __oxa_tool_update(partial); }, ctx); }; })()";

/// The JS glue that gives `oxa.on` and `oxa.events.on` their disposable return value. The host
/// functions answer the handler's function id, which is the token that removes it again.
const GLUE: &str = "(function () {\
    oxa.on = function (event, handler) {\
        const token = __oxa_on(event, handler);\
        return function () { __oxa_off(event, token); };\
    };\
    oxa.events.on = function (name, handler) {\
        const token = __oxa_bus_on(name, handler);\
        return function () { __oxa_bus_off(name, token); };\
    };\
    return true;\
})()";

/// Bind on `rt` (the runtime of extension `ext`): `oxa.on`, `oxa.registerTool`,
/// `oxa.registerCommand`, `oxa.registerShortcut`, `oxa.registerFlag`, `oxa.getFlag`,
/// `oxa.sendMessage` (-> `HostAction::SendMessage`, delivery modes `steer` / `followUp` /
/// `nextTurn`, `triggerTurn`), `oxa.sendUserMessage`, `oxa.appendEntry`, `oxa.setSessionName`,
/// `oxa.setLabel`, `oxa.getActiveTools`, `oxa.getAllTools`, `oxa.setActiveTools`,
/// `oxa.setModel`, `oxa.setThinkingLevel`, `oxa.events.{on,emit}`, `oxa.addPromptSection`, and
/// the omitted calls (`registerProvider`, `registerVirtualModel`, `registerMcpServer`,
/// `unregisterMcpServer`, `getMcpServers`), which throw `unsupported_in_oxa`.
///
/// Registrations go into `st.registry`. `oxa.on` with an unknown or omitted event name
/// (`before_provider_headers`) throws. Host actions fail with `busy` while no host is attached.
pub fn install(rt: &mut Runtime, st: &Arc<ExtState>, ext: ExtId) {
    let api = Api { st: st.clone(), ext };
    bind(rt, &api, "oxa.registerTool", register_tool);
    bind(rt, &api, "oxa.registerCommand", register_command);
    bind(rt, &api, "oxa.registerShortcut", register_shortcut);
    bind(rt, &api, "oxa.registerFlag", register_flag);
    bind(rt, &api, "oxa.getFlag", get_flag);
    bind(rt, &api, "__oxa_on", on_handler);
    bind(rt, &api, "__oxa_off", off_handler);
    bind(rt, &api, "__oxa_bus_on", bus_on);
    bind(rt, &api, "__oxa_bus_off", bus_off);
    bind(rt, &api, "oxa.events.emit", bus_emit);
    bind(rt, &api, "oxa.sendMessage", send_message);
    bind(rt, &api, "oxa.sendUserMessage", send_user_message);
    bind(rt, &api, "oxa.appendEntry", append_entry);
    bind(rt, &api, "oxa.setSessionName", set_session_name);
    bind(rt, &api, "oxa.setLabel", set_label);
    bind(rt, &api, "oxa.getActiveTools", get_active_tools);
    bind(rt, &api, "oxa.getAllTools", get_all_tools);
    bind(rt, &api, "oxa.setActiveTools", set_active_tools);
    bind(rt, &api, "oxa.setModel", set_model);
    bind(rt, &api, "oxa.setThinkingLevel", set_thinking_level);
    bind(rt, &api, "oxa.addPromptSection", add_prompt_section);
    bind(
        rt,
        &api,
        "oxa.getSessionName",
        |api: &Api, _: &mut CallCx<'_>, _: &[Value]| host_call(api, HostAction::GetSessionName),
    );
    bind(
        rt,
        &api,
        "oxa.getThinkingLevel",
        |api: &Api, _: &mut CallCx<'_>, _: &[Value]| host_call(api, HostAction::GetThinkingLevel),
    );
    bind(
        rt,
        &api,
        "oxa.getSettings",
        |api: &Api, _: &mut CallCx<'_>, _: &[Value]| host_call(api, HostAction::GetSettings),
    );
    bind(
        rt,
        &api,
        "oxa.getCommands",
        |api: &Api, _: &mut CallCx<'_>, _: &[Value]| host_call(api, HostAction::GetCommands),
    );
    bind(rt, &api, TOOL_UPDATE_FN, tool_update);
    for name in [
        "oxa.registerProvider",
        "oxa.unregisterProvider",
        "oxa.registerVirtualModel",
        "oxa.unregisterVirtualModel",
        // O-175: MCP servers come from mcp.json / `oxa mcp add` only.
        "oxa.registerMcpServer",
        "oxa.unregisterMcpServer",
        "oxa.getMcpServers",
    ] {
        bind(rt, &api, name, move |_: &Api, _: &mut CallCx<'_>, _: &[Value]| {
            Err(OxaError::unsupported(name))
        });
    }
    if let Err(e) = rt.eval_script("oxa:e3-glue", GLUE) {
        api.st.push_error(crate::ExtError {
            extension_path: api.path(),
            event: "install".to_string(),
            error: format!("{}: {}", e.code, e.message),
        });
    }
}

/// Validate a tool definition's `parameters` (a JSON Schema object with `type: "object"`) and
/// return its canonical JSON text for `ToolSpec::schema_json` (keys in the definition's order,
/// no whitespace). Errors: `invalid` naming the offending path.
pub fn validate_tool_schema(parameters: &Value) -> Result<String, OxaError> {
    let obj = parameters
        .as_object()
        .ok_or_else(|| invalid("parameters must be a JSON Schema object"))?;
    if obj.get("type").and_then(Value::as_str) != Some("object") {
        return Err(invalid("parameters.type must be \"object\""));
    }
    if let Some(props) = present(parameters, "properties") {
        let props = props
            .as_object()
            .ok_or_else(|| invalid("parameters.properties must be an object"))?;
        for (name, schema) in props {
            if !schema.is_object() {
                return Err(invalid(format!(
                    "parameters.properties.{name} must be a schema object"
                )));
            }
        }
    }
    if let Some(required) = present(parameters, "required") {
        let names = required
            .as_array()
            .ok_or_else(|| invalid("parameters.required must be an array"))?;
        if names.iter().any(|n| !n.is_string()) {
            return Err(invalid("parameters.required must list property names as strings"));
        }
    }
    serde_json::to_string(parameters).map_err(|e| invalid(format!("parameters: {e}")))
}

/// Build the `ToolSpec` of a tool definition `{name, description, parameters, class?,
/// exposure?}`. `class` is an oxa addition (`"read" | "write" | "exec" | "net"`); without it
/// an extension tool is `ToolClass::Exec`, so the ask mode asks. `exposure` defaults to
/// `direct`. Tool names must match `[A-Za-z0-9_-]{1,64}` and must not start with `mcp__`.
pub fn tool_spec_from_def(def: &Value, extension_path: &str) -> Result<ToolSpec, OxaError> {
    if !def.is_object() {
        return Err(invalid("a tool definition must be an object"));
    }
    let name = required_str(def, "name")?;
    if !valid_tool_name(&name) {
        return Err(invalid(format!(
            "tool name {name:?} must match [A-Za-z0-9_-]{{1,64}} and must not start with mcp__"
        )));
    }
    let description = required_str(def, "description")?;
    let parameters =
        present(def, "parameters").ok_or_else(|| invalid(format!("tool {name}: parameters are required")))?;
    let schema_json =
        validate_tool_schema(parameters).map_err(|e| invalid(format!("tool {name}: {}", e.message)))?;
    let class = match opt_str(def, "class")? {
        None => ToolClass::Exec,
        Some("read") => ToolClass::Read,
        Some("write") => ToolClass::Write,
        Some("exec") => ToolClass::Exec,
        Some("net") => ToolClass::Net,
        Some(other) => return Err(invalid(format!("tool {name}: unknown class {other:?}"))),
    };
    // pi's `executionMode`: extension tools may open dialogs, so the loop already runs them
    // one at a time (`Tool::needs_ui`); both values are accepted.
    match opt_str(def, "executionMode")? {
        None | Some("sequential") | Some("parallel") => {}
        Some(other) => return Err(invalid(format!("tool {name}: unknown executionMode {other:?}"))),
    }
    let exposure = match opt_str(def, "exposure")? {
        None => Exposure::Direct,
        // oxa.d.ts names this mode; the model sees it as a direct tool.
        Some("model-only") => Exposure::Direct,
        Some(other) => Exposure::parse(other)
            .ok_or_else(|| invalid(format!("tool {name}: unknown exposure {other:?}")))?,
    };
    Ok(ToolSpec {
        name,
        description,
        schema_json,
        class,
        exposure,
        source: Source::Extension(extension_path.to_string()),
    })
}

/// A registered extension tool as a `dyn Tool`.
pub struct ExtTool {
    pub(crate) reg: RegisteredTool,
    pub(crate) state: Arc<ExtState>,
    pub(crate) thread: Arc<RuntimeThread>,
    pub(crate) ui: Option<SharedUi>,
}

impl ExtTool {
    pub fn new(
        reg: RegisteredTool,
        state: Arc<ExtState>,
        thread: Arc<RuntimeThread>,
        ui: Option<SharedUi>,
    ) -> ExtTool {
        ExtTool {
            reg,
            state,
            thread,
            ui,
        }
    }
}

impl Tool for ExtTool {
    fn spec(&self) -> &ToolSpec {
        &self.reg.spec
    }
    /// Extension tools declare no fine-grained access: the result is one
    /// `Access::Exec { command: "extension tool <name>", net: false }` for class `Exec`, a
    /// `Net { host: "*" }` for class `Net`, a `Write(cwd)` for class `Write`, and nothing for
    /// class `Read`. What the tool then does through `ctx.exec` / `ctx.fs` / `ctx.fetch` is
    /// checked again by the host.
    fn access(&self, _args: &Value, cx: &ToolCtx) -> Result<Vec<Access>, String> {
        Ok(match self.reg.spec.class {
            ToolClass::Read => Vec::new(),
            ToolClass::Write => vec![Access::Write(cx.cwd.to_path_buf())],
            ToolClass::Exec => vec![Access::Exec {
                command: format!("extension tool {}", self.reg.spec.name),
                net: false,
            }],
            ToolClass::Net => vec![Access::Net {
                host: "*".to_string(),
            }],
        })
    }
    /// Call `execute(toolCallId, params, signal, onUpdate, ctx)` on the runtime thread.
    /// `onUpdate(partial)` feeds `live`; `cx.cancel` interrupts the script; a thrown error or
    /// a result with `isError` is `ToolOutput { is_error: true }`. The result's text content
    /// becomes `text`, its `details` becomes `ui` (never on the wire, W4).
    ///
    /// `toolCallId` is the value of `OXA_TOOL_CALL_ID` in `cx.env` (set by the loop; empty when
    /// the tool is run outside a model tool call). `ctx` is a fresh object from
    /// `__oxa_make_ctx()`. `signal` is `null`: an abort interrupts the script instead.
    ///
    /// This form has no caller UI: dialogs go to the UI given with
    /// `ExtensionHost::set_tool_ui`, else they answer `null`. The loop calls
    /// [`Tool::run_with_ui`] instead (see [`Tool::needs_ui`]).
    fn run(&self, args: &Value, cx: &ToolCtx, live: &mut dyn FnMut(&str)) -> ToolOutput {
        match self.ui.clone() {
            Some(shared) => {
                let mut guard = shared.lock().unwrap_or_else(|p| p.into_inner());
                self.run_on(args, cx, live, Some(&mut **guard))
            }
            None => self.run_on(args, cx, live, None),
        }
    }
    /// Extension tools may open dialogs (`ctx.ui`), so the loop runs them serially with the
    /// session's UI.
    fn needs_ui(&self) -> bool {
        true
    }
    fn run_with_ui(
        &self,
        args: &Value,
        cx: &ToolCtx,
        live: &mut dyn FnMut(&str),
        ui: &mut dyn oxa_proto::Ui,
    ) -> ToolOutput {
        self.run_on(args, cx, live, Some(ui))
    }
}

impl ExtTool {
    fn run_on(
        &self,
        args: &Value,
        cx: &ToolCtx,
        live: &mut dyn FnMut(&str),
        mut ui: Option<&mut dyn oxa_proto::Ui>,
    ) -> ToolOutput {
        let slot = self.reg.slot;
        let execute = self.reg.execute;
        // EXT-063: the arguments are checked (and coerced, as pi does) against the tool's
        // schema before `execute` runs; a failure is the tool's error result for the model.
        let params = match serde_json::from_str::<Value>(&self.reg.spec.schema_json) {
            Ok(schema) => match crate::validate_tool_args(&self.reg.spec.name, &schema, args) {
                Ok(v) => v,
                Err(message) => return ToolOutput::error(message),
            },
            Err(_) => args.clone(),
        };
        let call_id = cx
            .env
            .iter()
            .find(|(k, _)| k == oxa_proto::names::ENV_TOOL_CALL_ID)
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let details_id = call_id.clone();
        let cancel = cx.cancel.clone();
        let (handle_tx, handle_rx) = mpsc::channel::<InterruptHandle>();
        let finished = Arc::new(AtomicBool::new(false));
        let watch_finished = finished.clone();
        let outcome = std::thread::scope(|scope| {
            // Cancel watcher: once the run has its runtime's interrupt handle and the token is
            // cancelled, the running script is interrupted (it fails with `script`).
            scope.spawn(move || {
                let mut handle: Option<InterruptHandle> = None;
                let mut sent = false;
                while !watch_finished.load(Ordering::SeqCst) {
                    if handle.is_none() {
                        handle = handle_rx.try_recv().ok();
                    }
                    if let Some(h) = &handle {
                        if !sent && cancel.is_cancelled() {
                            h.interrupt();
                            sent = true;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
            let mut serve = |request: Value, port: &CallbackPort| -> Value {
                if request.get("op").and_then(Value::as_str) == Some(TOOL_UPDATE_OP) {
                    if let Some(text) = request.get("text").and_then(Value::as_str) {
                        live(text);
                    }
                    return Value::Null;
                }
                match ui.as_deref_mut() {
                    Some(ui) => serve_upcall(&request, port, ui),
                    None => Value::Null,
                }
            };
            let r = self.thread.run(
                // A tool runs inside the core's run: its `ctx.waitForIdle()` is refused.
                move |slots: &mut Slots| {
                    crate::api_ctx::in_run_scope(true, || {
                        run_tool_in(slots, slot, execute, call_id, params, handle_tx)
                    })
                },
                &mut serve,
            );
            finished.store(true, Ordering::SeqCst);
            r
        });
        match outcome.and_then(|r| r) {
            Ok(v) => {
                let out = tool_output(&v);
                self.keep_details(&details_id, &out.ui);
                out
            }
            Err(e) => ToolOutput::error(e.to_string()),
        }
    }

    /// pi keeps a tool result's `details` on its `toolResult` message, and extensions rebuild
    /// their state from them (`sessionManager.getBranch()`, e.g. the todo example). oxa's
    /// message entries carry the wire only, so the details go into a custom entry
    /// (`oxa.toolDetails`, `{toolCallId, toolName, details}`) that survives a resume; the core
    /// puts them back on the `message` of the tool result in `getEntries` / `getBranch`.
    fn keep_details(&self, call_id: &str, details: &Value) {
        if call_id.is_empty() || details.is_null() {
            return;
        }
        if let Some(host) = self.state.host() {
            let _ = host.call(HostAction::AppendEntry {
                custom_type: TOOL_DETAILS_ENTRY.to_string(),
                data: json!({
                    "toolCallId": call_id,
                    "toolName": self.reg.spec.name,
                    "details": details,
                }),
            });
        }
    }
}

/// The custom entry type of [`ExtTool::keep_details`] (the core reads it back).
pub const TOOL_DETAILS_ENTRY: &str = "oxa.toolDetails";

// ---------------------------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------------------------

/// What one `oxa.*` host function needs: the shared state and the extension it belongs to.
#[derive(Clone)]
struct Api {
    st: Arc<ExtState>,
    ext: ExtId,
}

impl Api {
    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.st.registry.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The file of this extension, for `Source::Extension` (empty when unknown).
    fn path(&self) -> String {
        self.registry()
            .extension(self.ext)
            .map(|e| e.path.clone())
            .unwrap_or_default()
    }
}

/// The runtime of an extension, read from its registry entry. A registration needs it because
/// every handler, tool and command names the runtime it is called on.
fn slot_of(reg: &Registry, ext: ExtId) -> Result<SlotId, OxaError> {
    reg.extension(ext)
        .and_then(|e| e.slot)
        .ok_or_else(|| OxaError::new(codes::INTERNAL, "the extension has no runtime to register on"))
}

/// Register one host function `path` on `rt`, bound to `api`.
fn bind<F>(rt: &mut Runtime, api: &Api, path: &str, f: F)
where
    F: Fn(&Api, &mut CallCx<'_>, &[Value]) -> Result<Value, OxaError> + Send + 'static,
{
    let api = api.clone();
    let host: oxa_script::HostFn = Box::new(move |cx: &mut CallCx<'_>, args: &[Value]| f(&api, cx, args));
    rt.register(path, host);
}

fn invalid(msg: impl Into<String>) -> OxaError {
    OxaError::new(codes::INVALID, msg)
}

fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or(Value::Null)
}

/// A field that is present and not `null` (JS `undefined` bridges to `null`).
fn present<'a>(obj: &'a Value, key: &str) -> Option<&'a Value> {
    obj.get(key).filter(|v| !v.is_null())
}

/// A required string field of an object.
fn required_str(obj: &Value, key: &str) -> Result<String, OxaError> {
    present(obj, key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| invalid(format!("{key} must be a string")))
}

/// A positional argument that must be a string; `what` names it in the error.
fn text_arg(v: &Value, what: &str) -> Result<String, OxaError> {
    v.as_str()
        .map(str::to_string)
        .ok_or_else(|| invalid(format!("{what} must be a string")))
}

fn opt_str<'a>(obj: &'a Value, key: &str) -> Result<Option<&'a str>, OxaError> {
    match present(obj, key) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| invalid(format!("{key} must be a string"))),
    }
}

fn fn_field(obj: &Value, key: &str) -> Option<FnRef> {
    present(obj, key).and_then(FnRef::from_value)
}

/// Text of a message content: a string, or the text blocks of an array joined by newlines.
/// Blocks without text (images) are dropped.
fn content_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => {
            let parts: Vec<&str> = items
                .iter()
                .filter_map(|i| i.get("text").and_then(Value::as_str))
                .collect();
            Some(parts.join("\n"))
        }
        _ => None,
    }
}

fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && !name.starts_with("mcp__")
}

/// The delivery mode of `opts.deliverAs`; `steer` when absent (pi's default while streaming).
fn delivery(opts: &Value, allow_next_turn: bool) -> Result<Delivery, OxaError> {
    match present(opts, "deliverAs").and_then(Value::as_str) {
        None | Some("steer") => Ok(Delivery::Steer),
        Some("followUp") => Ok(Delivery::FollowUp),
        Some("nextTurn") if allow_next_turn => Ok(Delivery::NextTurn),
        Some(other) => Err(invalid(format!("deliverAs: unknown delivery mode {other:?}"))),
    }
}

/// Call a session action. `busy` while no session is attached.
fn host_call(api: &Api, action: HostAction) -> Result<Value, OxaError> {
    let host = api
        .st
        .host()
        .ok_or_else(|| OxaError::new(codes::BUSY, "no session is attached to the extension host yet"))?;
    host.call(action)
}

/// The result of a tool run as a `ToolOutput`: `content` is the text, `isError` the flag and
/// `details` the display-only value.
fn tool_output(v: &Value) -> ToolOutput {
    ToolOutput {
        text: v.get("content").and_then(content_text).unwrap_or_default(),
        is_error: v.get("isError").and_then(Value::as_bool).unwrap_or(false),
        ui: v.get("details").cloned().unwrap_or(Value::Null),
    }
}

/// Runs on the runtime thread: call the tool's `execute` through the glue. The interrupt handle
/// of the runtime goes back first, so the caller can cancel the run.
fn run_tool_in(
    slots: &mut Slots,
    slot: SlotId,
    execute: FnRef,
    call_id: String,
    params: Value,
    handle: mpsc::Sender<InterruptHandle>,
) -> Result<Value, OxaError> {
    let rt = slots
        .get(slot)
        .ok_or_else(|| OxaError::new(codes::SCRIPT, "the extension is not loaded"))?;
    let _ = handle.send(rt.interrupt_handle());
    let glue = rt.eval_script("oxa:tool-glue", TOOL_GLUE)?;
    let wrapper = FnRef::from_value(&glue)
        .ok_or_else(|| OxaError::new(codes::INTERNAL, "the tool glue is not a function"))?;
    let out = rt.call(&wrapper, &[execute.to_value(), Value::String(call_id), params]);
    rt.release(&wrapper);
    out
}

fn register_tool(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let def = arg(args, 0);
    let path = api.path();
    let spec = tool_spec_from_def(&def, &path)?;
    let name = spec.name.clone();
    let execute = fn_field(&def, "execute")
        .ok_or_else(|| invalid(format!("tool {name}: execute must be a function")))?;
    let mut reg = api.registry();
    let slot = slot_of(&reg, api.ext)?;
    let tool = RegisteredTool {
        ext: api.ext,
        slot,
        spec,
        execute,
        render_call: fn_field(&def, "renderCall"),
        render_result: fn_field(&def, "renderResult"),
    };
    match reg.tools.iter().position(|t| t.spec.name == name) {
        Some(i) => reg.tools[i] = tool,
        None => reg.tools.push(tool),
    }
    Ok(Value::Null)
}

fn register_command(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let name = text_arg(&arg(args, 0), "command name")?;
    if name.is_empty() || name.chars().any(char::is_whitespace) {
        return Err(invalid(format!("command name {name:?} must be one word")));
    }
    let opts = arg(args, 1);
    let handler = fn_field(&opts, "handler")
        .ok_or_else(|| invalid(format!("command {name}: handler must be a function")))?;
    let spec = CommandSpec {
        name: name.clone(),
        description: opt_str(&opts, "description")?.unwrap_or("").to_string(),
        argument_hint: opt_str(&opts, "argumentHint")?.map(str::to_string),
        source: Source::Extension(api.path()),
    };
    let mut reg = api.registry();
    let slot = slot_of(&reg, api.ext)?;
    let cmd = RegisteredCommand {
        ext: api.ext,
        slot,
        spec,
        handler,
        complete: fn_field(&opts, "getArgumentCompletions"),
    };
    match reg.commands.iter().position(|c| c.spec.name == name) {
        Some(i) => reg.commands[i] = cmd,
        None => reg.commands.push(cmd),
    }
    Ok(Value::Null)
}

fn register_shortcut(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let key = KeyId::parse(&text_arg(&arg(args, 0), "shortcut key")?)?;
    let opts = arg(args, 1);
    let handler = fn_field(&opts, "handler")
        .ok_or_else(|| invalid(format!("shortcut {key}: handler must be a function")))?;
    let spec = ShortcutSpec {
        key: key.clone(),
        description: opt_str(&opts, "description")?.unwrap_or("").to_string(),
    };
    let mut reg = api.registry();
    let slot = slot_of(&reg, api.ext)?;
    let sc = RegisteredShortcut {
        ext: api.ext,
        slot,
        spec,
        handler,
    };
    match reg.shortcuts.iter().position(|s| s.spec.key == key) {
        Some(i) => reg.shortcuts[i] = sc,
        None => reg.shortcuts.push(sc),
    }
    Ok(Value::Null)
}

fn register_flag(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let name = text_arg(&arg(args, 0), "flag name")?;
    if name.is_empty() {
        return Err(invalid("a flag needs a name"));
    }
    let opts = arg(args, 1);
    let boolean = match opt_str(&opts, "type")? {
        Some("boolean") => true,
        Some("string") => false,
        Some(other) => return Err(invalid(format!("flag {name}: unknown type {other:?}"))),
        None => {
            return Err(invalid(format!(
                "flag {name}: type must be \"boolean\" or \"string\""
            )))
        }
    };
    let default = present(&opts, "default").cloned().unwrap_or(Value::Null);
    let type_ok = match &default {
        Value::Null => true,
        Value::Bool(_) => boolean,
        Value::String(_) => !boolean,
        _ => false,
    };
    if !type_ok {
        return Err(invalid(format!("flag {name}: default does not match its type")));
    }
    let spec = FlagSpec {
        name: name.clone(),
        description: opt_str(&opts, "description")?.unwrap_or("").to_string(),
        boolean,
        default: default.clone(),
    };
    let mut reg = api.registry();
    let flag = RegisteredFlag {
        ext: api.ext,
        spec,
        value: default,
    };
    match reg.flags.iter().position(|f| f.spec.name == name) {
        Some(i) => reg.flags[i] = flag,
        None => reg.flags.push(flag),
    }
    Ok(Value::Null)
}

fn get_flag(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let name = text_arg(&arg(args, 0), "flag name")?;
    let reg = api.registry();
    Ok(reg
        .flags
        .iter()
        .find(|f| f.spec.name == name)
        .map(|f| f.value.clone())
        .unwrap_or(Value::Null))
}

/// `__oxa_on(event, handler)`: register a handler; answers its id (the disposable's token).
fn on_handler(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let event = text_arg(&arg(args, 0), "event name")?;
    let f = FnRef::from_value(&arg(args, 1))
        .ok_or_else(|| invalid(format!("handler for {event} must be a function")))?;
    let point = match HookPoint::parse(&event) {
        Some(p) => p,
        None if event == "before_provider_headers" => {
            return Err(OxaError::unsupported("the before_provider_headers event"))
        }
        None => return Err(invalid(format!("unknown event: {event}"))),
    };
    let mut reg = api.registry();
    let slot = slot_of(&reg, api.ext)?;
    reg.handlers.entry(point).or_default().push(Handler {
        ext: api.ext,
        slot,
        f,
    });
    Ok(Value::from(f.0))
}

/// `__oxa_off(event, token)`: remove the handler registered with that token.
fn off_handler(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let event = text_arg(&arg(args, 0), "event name")?;
    let token = arg(args, 1).as_u64().unwrap_or(u64::MAX);
    if let Some(point) = HookPoint::parse(&event) {
        let mut reg = api.registry();
        if let Some(list) = reg.handlers.get_mut(&point) {
            list.retain(|h| !(h.ext == api.ext && h.f.0 == token));
        }
    }
    Ok(Value::Null)
}

/// `__oxa_bus_on(name, handler)`: a listener on the inter-extension bus; answers its token.
fn bus_on(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let name = text_arg(&arg(args, 0), "bus event name")?;
    let f = FnRef::from_value(&arg(args, 1)).ok_or_else(|| invalid("bus handler must be a function"))?;
    let mut reg = api.registry();
    let slot = slot_of(&reg, api.ext)?;
    reg.bus_listeners.push((
        name,
        Handler {
            ext: api.ext,
            slot,
            f,
        },
    ));
    Ok(Value::from(f.0))
}

/// `__oxa_bus_off(name, token)`.
fn bus_off(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let name = text_arg(&arg(args, 0), "bus event name")?;
    let token = arg(args, 1).as_u64().unwrap_or(u64::MAX);
    let mut reg = api.registry();
    reg.bus_listeners
        .retain(|(n, h)| !(*n == name && h.ext == api.ext && h.f.0 == token));
    Ok(Value::Null)
}

/// `oxa.events.emit(name, data)`: queued in `ExtState::bus_queue`; the bus delivers it to the
/// listeners of every extension (the emitter's own included) from the session side, right
/// after the handler that emitted it has returned. Delivery is therefore asynchronous, where
/// pi's is synchronous.
fn bus_emit(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let name = text_arg(&arg(args, 0), "bus event name")?;
    api.st
        .bus_queue
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push((name, arg(args, 1)));
    Ok(Value::Null)
}

fn send_message(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let msg = arg(args, 0);
    let opts = arg(args, 1);
    let custom_type = required_str(&msg, "customType")?;
    let content = present(&msg, "content")
        .and_then(content_text)
        .ok_or_else(|| invalid("message content must be a string or content blocks"))?;
    host_call(
        api,
        HostAction::SendMessage {
            custom_type,
            content,
            display: msg.get("display").and_then(Value::as_bool).unwrap_or(true),
            details: msg.get("details").cloned().unwrap_or(Value::Null),
            deliver: delivery(&opts, true)?,
            trigger_turn: opts.get("triggerTurn").and_then(Value::as_bool).unwrap_or(false),
        },
    )?;
    Ok(Value::Null)
}

fn send_user_message(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let text = content_text(&arg(args, 0))
        .ok_or_else(|| invalid("the message content must be a string or content blocks"))?;
    let deliver = delivery(&arg(args, 1), false)?;
    host_call(api, HostAction::SendUserMessage { text, deliver })?;
    Ok(Value::Null)
}

fn append_entry(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let custom_type = text_arg(&arg(args, 0), "entry customType")?;
    host_call(
        api,
        HostAction::AppendEntry {
            custom_type,
            data: arg(args, 1),
        },
    )?;
    Ok(Value::Null)
}

fn set_session_name(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let name = text_arg(&arg(args, 0), "session name")?;
    host_call(api, HostAction::SetSessionName(name))?;
    Ok(Value::Null)
}

fn set_label(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let target = text_arg(&arg(args, 0), "entry id")?;
    let label = match arg(args, 1) {
        Value::Null => None,
        v => Some(
            v.as_str()
                .ok_or_else(|| invalid("label must be a string"))?
                .to_string(),
        ),
    };
    host_call(api, HostAction::SetLabel { target, label })?;
    Ok(Value::Null)
}

fn get_active_tools(api: &Api, _cx: &mut CallCx<'_>, _args: &[Value]) -> Result<Value, OxaError> {
    host_call(api, HostAction::GetActiveTools)
}

fn get_all_tools(api: &Api, _cx: &mut CallCx<'_>, _args: &[Value]) -> Result<Value, OxaError> {
    host_call(api, HostAction::GetAllTools)
}

fn set_active_tools(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let names = arg(args, 0);
    let items = names
        .as_array()
        .ok_or_else(|| invalid("setActiveTools takes an array of tool names"))?;
    let mut list = Vec::with_capacity(items.len());
    for item in items {
        list.push(
            item.as_str()
                .ok_or_else(|| invalid("tool names must be strings"))?
                .to_string(),
        );
    }
    host_call(api, HostAction::SetActiveTools(list))?;
    Ok(Value::Null)
}

/// `oxa.setModel(model)`: `model` is an engine model id (or pi's model object, read by its
/// `id`). A model the engine does not serve throws `not_found` at once; a known one is switched
/// to when the run settles (a new epoch, W3) and the call answers `true`.
fn set_model(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let given = arg(args, 0);
    let model = match given.get("id").and_then(Value::as_str) {
        Some(id) => id.to_string(),
        None => text_arg(&given, "model")?,
    };
    let state = host_call(api, HostAction::GetState)?;
    let current = state.get("model").and_then(Value::as_str).unwrap_or_default();
    let served: Vec<&str> = state
        .get("models")
        .and_then(Value::as_array)
        .map(|m| m.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let known = state.get("models").is_none() || served.contains(&model.as_str());
    if model != current && !known {
        return Err(OxaError::new(
            codes::NOT_FOUND,
            format!("model {model:?} is not served by the engine"),
        )
        .with_hint(format!("available: {}", served.join(", "))));
    }
    host_call(api, HostAction::SetModel(model))?;
    Ok(Value::Bool(true))
}

fn set_thinking_level(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let level = text_arg(&arg(args, 0), "thinking level")?;
    let level =
        ThinkingLevel::parse(&level).ok_or_else(|| invalid(format!("unknown thinking level {level:?}")))?;
    host_call(api, HostAction::SetThinking(level))?;
    Ok(Value::Null)
}

fn add_prompt_section(api: &Api, _cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let name = text_arg(&arg(args, 0), "section name")?;
    let text = text_arg(&arg(args, 1), "section text")?;
    let mut reg = api.registry();
    match reg.prompt_sections.iter().position(|(n, _)| *n == name) {
        Some(i) => reg.prompt_sections[i] = (name, text),
        None => reg.prompt_sections.push((name, text)),
    }
    Ok(Value::Null)
}

/// `__oxa_tool_update(partial)`: forward partial tool output to the caller of the run.
fn tool_update(_api: &Api, cx: &mut CallCx<'_>, args: &[Value]) -> Result<Value, OxaError> {
    let text = arg(args, 0)
        .get("content")
        .and_then(content_text)
        .unwrap_or_default();
    // Outside a run (no upcall link) there is nobody to show the partial output to.
    let _ = cx.upcall(json!({"op": TOOL_UPDATE_OP, "text": text}));
    Ok(Value::Null)
}
