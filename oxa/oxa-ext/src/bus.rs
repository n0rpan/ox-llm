//! E2: the extension bus, `ExtensionHost` behind `dyn Hooks`.
// ported from pi packages/coding-agent/src/core/extensions/runner.ts
use crate::guard::{self, GuardVerdict};
use crate::{ExtError, ExtId, ExtState, ExtTool, Handler, LoadOpts, LoadedExtension, Registry};
use oxa_proto::{
    codes, CommandSpec, FlagSpec, GuardClass, HookPoint, HookReply, Host, KeyId, Level, OxaError, RenderKind,
    Role, ShortcutSpec, Tool, ToolCall, ToolOutput, Ui, Value, WireMsg,
};
use oxa_script::{CallbackPort, RuntimeThread, Slots};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

/// A UI that extension *tools* may use while they execute (`Tool::run` carries no `Ui`). Set by
/// the front end; without one, dialogs opened from a tool answer `null`.
pub type SharedUi = Arc<Mutex<Box<dyn Ui>>>;

/// All JavaScript extensions of a session, as one `Hooks` source.
pub struct ExtensionHost {
    pub(crate) state: Arc<ExtState>,
    pub(crate) thread: Arc<RuntimeThread>,
    pub(crate) tool_ui: Mutex<Option<SharedUi>>,
    /// The current wire as last reported by the core (payload field `messages` of the
    /// prefix-checked points); the guard compares against it.
    pub(crate) last_wire: Mutex<Vec<WireMsg>>,
}

/// How many rounds of re-emission one bus delivery follows.
const BUS_MAX_ROUNDS: usize = 16;

/// Applies a handler with a fresh `ctx` inserted at position `at` of its arguments.
const INVOKE_GLUE: &str = "(function () { return function (f, args, at) { \
    var ctx = typeof __oxa_make_ctx === 'function' ? __oxa_make_ctx() : null; \
    args.splice(at, 0, ctx); \
    return f.apply(undefined, args); }; })()";

/// Lock a mutex, recovering from poisoning (a panicking extension must not wedge the session).
fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The file of an extension, for error events (empty when it is unknown).
fn path_of(reg: &Registry, ext: ExtId) -> String {
    reg.extension(ext).map(|e| e.path.clone()).unwrap_or_default()
}

impl ExtensionHost {
    /// Start the runtime thread with no extensions loaded.
    pub fn new(cwd: PathBuf) -> Result<ExtensionHost, OxaError> {
        let thread = RuntimeThread::spawn("extensions")?;
        Ok(ExtensionHost {
            state: Arc::new(ExtState::new(cwd)),
            thread: Arc::new(thread),
            tool_ui: Mutex::new(None),
            last_wire: Mutex::new(Vec::new()),
        })
    }

    /// Load extensions (see [`crate::load_extensions`]). Returns this pass's extensions,
    /// including the failed ones.
    pub fn load(&self, paths: &[PathBuf], opts: &LoadOpts) -> Vec<LoadedExtension> {
        crate::load_extensions(&self.state, &self.thread, paths, opts)
    }

    /// Load a built-in or test extension from source.
    pub fn load_source(&self, name: &str, source: &str, opts: &LoadOpts) -> LoadedExtension {
        crate::load_source(&self.state, &self.thread, name, source, opts)
    }

    /// `/reload`: fire nothing, drop every context, load `paths` again. The attached host is
    /// kept.
    pub fn reload(&self, paths: &[PathBuf], opts: &LoadOpts) -> Vec<LoadedExtension> {
        *locked(&self.last_wire) = Vec::new();
        crate::unload_all(&self.state, &self.thread);
        crate::load_extensions(&self.state, &self.thread, paths, opts)
    }

    /// All extensions, loaded and failed, in load order.
    pub fn extensions(&self) -> Vec<LoadedExtension> {
        locked(&self.state.registry).extensions.clone()
    }

    pub fn state(&self) -> &Arc<ExtState> {
        &self.state
    }

    /// The run mode extensions see as `ctx.mode` (pi's `"tui"`, `"rpc"`, `"json"`, `"print"`).
    pub fn set_mode(&self, mode: &str) {
        *locked(&self.state.mode) = Some(mode.to_string());
    }

    /// Run the pending `ctx.compact` callbacks: `onComplete(result)` after `session_compact`
    /// (pi's `CompactionResult`: `summary`, `firstKeptEntryId`, `tokensBefore`), `onError(error)`
    /// after `session_compact_failed`. Each waiter runs once.
    fn run_compact_waiters(&self, p: HookPoint, payload: &Value, ui: &mut dyn Ui) {
        let waiters = std::mem::take(&mut *locked(&self.state.compact_waiters));
        for w in waiters {
            let (f, arg) = if p == HookPoint::SessionCompact {
                let result = serde_json::json!({
                    "summary": payload.get("summary").cloned().unwrap_or(Value::Null),
                    "firstKeptEntryId": payload.get("firstKeptEntryId").cloned().unwrap_or(Value::Null),
                    "tokensBefore": payload.get("tokensBefore").cloned().unwrap_or(Value::Null),
                    "details": payload.get("details").cloned().unwrap_or(Value::Null),
                });
                (w.on_complete, result)
            } else {
                let message = payload
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("compaction failed");
                let error = serde_json::json!({ "$error": { "name": "Error", "message": message } });
                (w.on_error, error)
            };
            if let Some(f) = f {
                let h = Handler {
                    ext: w.ext,
                    slot: w.slot,
                    f,
                };
                if let Err(e) = self.call_raw(h, vec![arg], None, true, ui) {
                    let path = path_of(&locked(&self.state.registry), w.ext);
                    self.report(path, "compact".to_string(), e, ui);
                }
            }
            // The callbacks were captured for this one compaction.
            let fns: Vec<oxa_script::FnRef> = w.on_complete.into_iter().chain(w.on_error).collect();
            let slot = w.slot;
            let _ = self.thread.run(
                move |slots: &mut Slots| {
                    if let Some(rt) = slots.get(slot) {
                        for f in &fns {
                            rt.release(f);
                        }
                    }
                },
                &mut |_: Value, _: &CallbackPort| Value::Null,
            );
        }
    }

    /// Give extension tools a UI (see [`SharedUi`]); `None` removes it.
    pub fn set_tool_ui(&self, ui: Option<SharedUi>) {
        *locked(&self.tool_ui) = ui;
    }

    /// Emit on the inter-extension bus (`oxa.events.emit`), from Rust. Events the listeners
    /// emit in turn are delivered too (see [`ExtensionHost::drain_bus`]).
    pub fn emit_bus(&self, name: &str, data: &Value, ui: &mut dyn Ui) {
        locked(&self.state.bus_queue).push((name.to_string(), data.clone()));
        self.drain_bus(false, ui);
    }

    /// Deliver the events queued by `oxa.events.emit` to their listeners, in emission order.
    /// A listener may emit again; the chain is cut after [`BUS_MAX_ROUNDS`] rounds so two
    /// extensions cannot keep each other busy for ever.
    fn drain_bus(&self, run_scope: bool, ui: &mut dyn Ui) {
        for _ in 0..BUS_MAX_ROUNDS {
            let queued = std::mem::take(&mut *locked(&self.state.bus_queue));
            if queued.is_empty() {
                return;
            }
            for (name, data) in queued {
                let listeners: Vec<(Handler, String)> = {
                    let reg = locked(&self.state.registry);
                    reg.bus_listeners
                        .iter()
                        .filter(|(n, _)| *n == name)
                        .map(|(_, h)| (*h, path_of(&reg, h.ext)))
                        .collect()
                };
                for (h, path) in listeners {
                    if let Err(e) = self.call_raw(h, vec![data.clone()], None, run_scope, ui) {
                        self.report(path, format!("bus:{name}"), e, ui);
                    }
                }
            }
        }
        let dropped = std::mem::take(&mut *locked(&self.state.bus_queue)).len();
        if dropped > 0 {
            self.state.log(
                Level::Warning,
                &format!(
                    "oxa.events: {dropped} event(s) dropped after {BUS_MAX_ROUNDS} rounds of re-emission"
                ),
            );
        }
    }

    /// Call one script function on the runtime thread, then deliver what it emitted on the
    /// bus. `ctx_at` = the position at which a fresh `ctx` object (`__oxa_make_ctx()`) is
    /// inserted into `args` (pi passes `ctx` as the last argument of handlers, commands and
    /// shortcuts). `run_scope`: the call is an event handler the core's run (or the operation
    /// holding the session) waits for; its `ctx.waitForIdle()` is refused (see
    /// [`crate::api_ctx::in_run_scope`]).
    fn call_handler(
        &self,
        h: Handler,
        args: Vec<Value>,
        ctx_at: Option<usize>,
        run_scope: bool,
        ui: &mut dyn Ui,
    ) -> Result<Value, OxaError> {
        let out = self.call_raw(h, args, ctx_at, run_scope, ui);
        self.drain_bus(run_scope, ui);
        out
    }

    /// Upcalls the function makes are answered with `ui`. A missing slot (the extension was
    /// unloaded) is `script`; a runaway call is interrupted by the runtime's deadline and comes
    /// back as `script`.
    fn call_raw(
        &self,
        h: Handler,
        args: Vec<Value>,
        ctx_at: Option<usize>,
        run_scope: bool,
        ui: &mut dyn Ui,
    ) -> Result<Value, OxaError> {
        let (slot, f) = (h.slot, h.f);
        self.thread
            .run(
                move |slots: &mut Slots| {
                    crate::api_ctx::in_run_scope(run_scope, || {
                        let Some(rt) = slots.get(slot) else {
                            return Err(OxaError::new(codes::SCRIPT, "the extension is not loaded"));
                        };
                        let Some(at) = ctx_at else {
                            return rt.call(&f, &args);
                        };
                        // The `ctx` object holds functions, so it cannot cross the JSON bridge: a
                        // small script function builds it and applies the handler.
                        let glue = rt.eval_script("oxa:invoke-glue", INVOKE_GLUE)?;
                        let invoke = oxa_script::FnRef::from_value(&glue).ok_or_else(|| {
                            OxaError::new(codes::INTERNAL, "the invoke glue is not a function")
                        })?;
                        let out = rt.call(
                            &invoke,
                            &[f.to_value(), Value::Array(args), Value::from(at as u64)],
                        );
                        rt.release(&invoke);
                        out
                    })
                },
                &mut |request: Value, port: &CallbackPort| serve_upcall(&request, port, &mut *ui),
            )
            .and_then(|r| r)
    }

    /// Record an extension error (kept for `ExtState::take_errors`, written to the log sink).
    /// The core drains it through `Hooks::take_errors` and reports it as an `extension_error`
    /// event (pi), which every front end shows; it is not a UI notification.
    fn report(&self, extension_path: String, event: String, e: OxaError, _ui: &mut dyn Ui) {
        let error = format!("{}: {}", e.code, e.message);
        self.state.push_error(ExtError {
            extension_path,
            event,
            error,
        });
    }

    /// The handlers of `p` in registration order, with the file and declared rebase of each.
    fn snapshot(&self, p: HookPoint) -> Vec<(Handler, String, bool)> {
        let reg = locked(&self.state.registry);
        let Some(list) = reg.handlers.get(&p) else {
            return Vec::new();
        };
        list.iter()
            .map(|h| match reg.extension(h.ext) {
                Some(e) => (*h, e.path.clone(), e.declared_rebase),
                None => (*h, String::new(), false),
            })
            .collect()
    }
}

/// The event object a handler receives: the core's payload with pi's `type` field (the event
/// name) added when it is missing; a payload that is not an object becomes `{type}`.
fn event_object(p: HookPoint, payload: &Value) -> Value {
    let mut event = match payload {
        Value::Object(_) => payload.clone(),
        _ => Value::Object(serde_json::Map::new()),
    };
    if let Some(m) = event.as_object_mut() {
        m.entry("type")
            .or_insert_with(|| Value::String(p.as_str().to_string()));
    }
    event
}

/// Set one field of an event object (no-op when the event is not an object).
fn set_field(event: &mut Value, key: &str, val: Value) {
    if let Some(m) = event.as_object_mut() {
        m.insert(key.to_string(), val);
    }
}

/// The text of a content value: a string, or an array of `{type, text}` blocks joined by newlines.
fn text_of(v: &Value) -> Option<String> {
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

/// One message of a JS message list (`{role, content, reasoning?, toolCallId?, toolCalls?}`).
/// `None` when the role is unknown or the content is not text.
fn wire_from_json(v: &Value) -> Option<WireMsg> {
    let o = v.as_object()?;
    let role = Role::parse(o.get("role")?.as_str()?)?;
    let content = match o.get("content") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return None,
    };
    let reasoning = o.get("reasoning").and_then(Value::as_str).map(str::to_string);
    let tool_call_id = o.get("toolCallId").and_then(Value::as_str).map(str::to_string);
    let mut tool_calls = Vec::new();
    if let Some(calls) = o.get("toolCalls").and_then(Value::as_array) {
        for c in calls {
            let id = c.get("id").and_then(Value::as_str)?.to_string();
            let name = c.get("name").and_then(Value::as_str)?.to_string();
            let args_raw = match c.get("args") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => "{}".to_string(),
            };
            tool_calls.push(ToolCall { id, name, args_raw });
        }
    }
    Some(WireMsg {
        role,
        content,
        reasoning,
        tool_calls,
        tool_call_id,
    })
}

/// A whole JS message list; `None` if any element is malformed.
fn wire_list_from_json(v: &Value) -> Option<Vec<WireMsg>> {
    v.as_array()?.iter().map(wire_from_json).collect()
}

/// The current wire the core reported in the payload field `messages` (empty when absent or
/// malformed; malformed elements are skipped).
fn wire_from_payload(payload: &Value) -> Vec<WireMsg> {
    payload
        .get("messages")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(wire_from_json).collect())
        .unwrap_or_default()
}

/// Convert what a handler returned (bridged to JSON; `null` = nothing) into a reply, using pi's
/// result shapes for `point` (extensions.md):
/// `{cancel: true, reason?}` -> `Cancel`; `{block: true, reason?}` -> `Block`;
/// `input`: `{action: "transform", text}` -> `ReplaceInput`, `{action: "handled"}` -> `Handled`;
/// `tool_call`: `{input: {...}}` -> `ReplaceArgs(<JSON text>)`;
/// `tool_result`: `{content, isError?, details?}` -> `ReplaceResult`;
/// `before_agent_start`: `{message: {customType, content, display}}` -> `Append` (user role);
/// `context*` / `before_provider_request`: `{messages: [...]}` -> `Rebase`;
/// `session_before_compact` / `session_before_tree`: `{summary: {summary, details?}}` ->
/// `Summary`; `project_trust`: pi's `{trusted: "yes" | "no", remember?}` (or `{trust: bool}`)
/// -> `Trust` (`TrustRemembered` with `remember: true`), `"undecided"` -> `Continue`; `agent_before_settle`: pi's `{entries: [{type:
/// "custom_message", content}, ..]}` (or `{message: {content}}`) -> `Append` (user role).
/// Anything else -> `Continue`.
/// Pure; the guard runs after this.
pub fn reply_from_js(point: HookPoint, returned: &Value) -> HookReply {
    let Some(obj) = returned.as_object() else {
        return HookReply::Continue;
    };
    let reason = obj
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if obj.get("cancel").and_then(Value::as_bool) == Some(true) {
        return HookReply::Cancel(reason);
    }
    if obj.get("block").and_then(Value::as_bool) == Some(true) {
        return HookReply::Block(reason);
    }
    match point {
        HookPoint::Input => match obj.get("action").and_then(Value::as_str) {
            Some("transform") => match obj.get("text").and_then(Value::as_str) {
                Some(text) => HookReply::ReplaceInput(text.to_string()),
                None => HookReply::Continue,
            },
            Some("handled") => HookReply::Handled,
            _ => HookReply::Continue,
        },
        HookPoint::ToolCall => match obj.get("input") {
            Some(input) => HookReply::ReplaceArgs(input.to_string()),
            None => HookReply::Continue,
        },
        HookPoint::ToolResult => {
            let Some(text) = obj.get("content").and_then(text_of) else {
                return HookReply::Continue;
            };
            HookReply::ReplaceResult(ToolOutput {
                text,
                is_error: obj.get("isError").and_then(Value::as_bool).unwrap_or(false),
                ui: obj.get("details").cloned().unwrap_or(Value::Null),
            })
        }
        HookPoint::BeforeAgentStart => {
            let content = obj
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(text_of);
            match content {
                Some(text) => HookReply::Append(vec![WireMsg::user(text)]),
                None => HookReply::Continue,
            }
        }
        HookPoint::Context | HookPoint::ContextWithSystem | HookPoint::BeforeProviderRequest => {
            match obj.get("messages").and_then(wire_list_from_json) {
                Some(list) => HookReply::Rebase(list),
                None => HookReply::Continue,
            }
        }
        HookPoint::SessionBeforeCompact | HookPoint::SessionBeforeTree => match obj.get("summary") {
            Some(s) => match s.get("summary").and_then(Value::as_str) {
                Some(text) => HookReply::Summary {
                    text: text.to_string(),
                    details: s.get("details").cloned().unwrap_or(Value::Null),
                },
                None => HookReply::Continue,
            },
            None => HookReply::Continue,
        },
        HookPoint::ProjectTrust => {
            let decided = match obj.get("trusted").and_then(Value::as_str) {
                Some("yes") => Some(true),
                Some("no") => Some(false),
                _ => obj.get("trust").and_then(Value::as_bool),
            };
            let remember = obj.get("remember").and_then(Value::as_bool) == Some(true);
            match decided {
                Some(t) if remember => HookReply::TrustRemembered(t),
                Some(t) => HookReply::Trust(t),
                None => HookReply::Continue,
            }
        }
        HookPoint::AgentBeforeSettle => {
            // pi's boundary drafts: only `custom_message` entries reach the context, so they
            // are what this point can append (W1: appends only; other drafts are ignored).
            let mut texts: Vec<String> = obj
                .get("entries")
                .and_then(Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .filter(|e| e.get("type").and_then(Value::as_str) == Some("custom_message"))
                        .filter_map(|e| e.get("content").and_then(text_of))
                        .collect()
                })
                .unwrap_or_default();
            if let Some(text) = obj
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(text_of)
            {
                texts.push(text);
            }
            if texts.is_empty() {
                HookReply::Continue
            } else {
                HookReply::Append(texts.into_iter().map(WireMsg::user).collect())
            }
        }
        _ => HookReply::Continue,
    }
}

/// Serve one upcall from a host function with the caller's UI: tries `api_ctx::serve`, then
/// `api_ui::serve`, then `api_components::serve`; an unknown `op` answers `null`.
pub fn serve_upcall(request: &Value, port: &CallbackPort, ui: &mut dyn Ui) -> Value {
    if let Some(v) = crate::api_ctx::serve(request, ui) {
        return v;
    }
    if let Some(v) = crate::api_ui::serve(request, ui) {
        return v;
    }
    if let Some(v) = crate::api_components::serve(request, port, ui) {
        return v;
    }
    Value::Null
}

impl oxa_proto::Hooks for ExtensionHost {
    /// Run the handlers of `p` in pi's order, pass each result through [`reply_from_js`] and
    /// [`crate::guard::check`], compose as documented on `HookReply`. A throwing or timed-out
    /// handler is pushed to `ExtState::errors` and counts as `Continue`.
    ///
    /// Composition: a transforming reply (`ReplaceInput`, `ReplaceArgs`, `ReplaceResult`,
    /// `Append`) is applied to the event and the next handler sees the transformed event; the
    /// last such reply is what is returned (appends are concatenated). Any other accepted reply
    /// ends the chain and is returned as it is. `Continue` moves on. Observer points never
    /// return anything but `Continue`.
    fn fire(&self, p: HookPoint, payload: &Value, ui: &mut dyn Ui) -> HookReply {
        if matches!(p, HookPoint::SessionCompact | HookPoint::SessionCompactFailed) {
            self.run_compact_waiters(p, payload, ui);
        }
        let prefixed = p.guard() == GuardClass::PrefixChecked;
        let wire = if prefixed {
            wire_from_payload(payload)
        } else {
            Vec::new()
        };
        if prefixed {
            *locked(&self.last_wire) = wire.clone();
        }
        let mut event = event_object(p, payload);
        let mut transform: Option<HookReply> = None;
        for (h, path, declared) in self.snapshot(p) {
            let returned = match self.call_handler(h, vec![event.clone()], Some(1), true, ui) {
                Ok(v) => v,
                Err(e) => {
                    self.report(path, p.as_str().to_string(), e, ui);
                    continue;
                }
            };
            let reply = match guard::check(p, &wire, reply_from_js(p, &returned), declared) {
                GuardVerdict::Pass(r) => r,
                GuardVerdict::Ignored { reason } => {
                    self.state
                        .log(Level::Warning, &format!("{path} ({}): {reason}", p.as_str()));
                    HookReply::Continue
                }
                // W1: an undeclared rewrite never leaves the bus.
                GuardVerdict::Refused(e) => {
                    self.report(path, p.as_str().to_string(), e, ui);
                    HookReply::Continue
                }
                // The origin flag of the reply path: only a declared rebase is handed to the
                // core as `DeclaredRebase`, with the extension that declared it.
                GuardVerdict::DeclaredRebase(list) => {
                    self.state.log(
                        Level::Warning,
                        &format!(
                            "{path} ({}): declared rebase, {} -> {} messages (cache-miss cause: {})",
                            p.as_str(),
                            wire.len(),
                            list.len(),
                            oxa_proto::miss_cause::EXTENSION
                        ),
                    );
                    HookReply::DeclaredRebase {
                        messages: list,
                        origin: path,
                    }
                }
            };
            match reply {
                HookReply::Continue => {}
                HookReply::ReplaceInput(text) => {
                    set_field(&mut event, "text", Value::String(text.clone()));
                    transform = Some(HookReply::ReplaceInput(text));
                }
                HookReply::ReplaceArgs(raw) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                        set_field(&mut event, "input", v);
                    }
                    transform = Some(HookReply::ReplaceArgs(raw));
                }
                HookReply::ReplaceResult(out) => {
                    set_field(&mut event, "content", Value::String(out.text.clone()));
                    set_field(&mut event, "isError", Value::Bool(out.is_error));
                    transform = Some(HookReply::ReplaceResult(out));
                }
                HookReply::Append(msgs) => {
                    transform = Some(match transform.take() {
                        Some(HookReply::Append(mut all)) => {
                            all.extend(msgs);
                            HookReply::Append(all)
                        }
                        _ => HookReply::Append(msgs),
                    });
                }
                other => return other,
            }
        }
        transform.unwrap_or(HookReply::Continue)
    }

    /// One [`crate::ExtTool`] per registered tool, in registration order.
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let ui = locked(&self.tool_ui).clone();
        let reg = locked(&self.state.registry);
        reg.tools
            .iter()
            .map(|t| {
                Arc::new(ExtTool::new(
                    t.clone(),
                    self.state.clone(),
                    self.thread.clone(),
                    ui.clone(),
                )) as Arc<dyn Tool>
            })
            .collect()
    }

    /// `/reload` (EXT-007): every context is dropped and the extensions are loaded again.
    fn reload(&self, extension_paths: &[PathBuf]) {
        let _ = ExtensionHost::reload(self, extension_paths, &LoadOpts::default());
    }

    fn take_errors(&self) -> Vec<oxa_proto::HookError> {
        self.state
            .take_errors()
            .into_iter()
            .map(|e| oxa_proto::HookError {
                extension_path: e.extension_path,
                event: e.event,
                error: e.error,
            })
            .collect()
    }

    fn commands(&self) -> Vec<CommandSpec> {
        locked(&self.state.registry)
            .commands
            .iter()
            .map(|c| c.spec.clone())
            .collect()
    }

    /// Call the handler of the first command named `name` with `(args, ctx)`.
    fn run_command(&self, name: &str, args: &str, ui: &mut dyn Ui) -> Result<(), OxaError> {
        let found = {
            let reg = locked(&self.state.registry);
            reg.commands
                .iter()
                .find(|c| c.spec.name == name)
                .map(|c| Handler {
                    ext: c.ext,
                    slot: c.slot,
                    f: c.handler,
                })
        };
        let Some(h) = found else {
            return Err(OxaError::new(
                codes::NOT_FOUND,
                format!("no extension command named \"{name}\""),
            ));
        };
        self.call_handler(h, vec![Value::String(args.to_string())], Some(1), false, ui)
            .map(|_| ())
    }

    fn prompt_sections(&self) -> Vec<(String, String)> {
        locked(&self.state.registry).prompt_sections.clone()
    }

    fn name(&self) -> &str {
        "extensions"
    }

    fn attach(&self, host: Arc<dyn Host>) {
        *self.state.host.lock().unwrap_or_else(|p| p.into_inner()) = Some(host);
    }

    fn flags(&self) -> Vec<FlagSpec> {
        locked(&self.state.registry)
            .flags
            .iter()
            .map(|f| f.spec.clone())
            .collect()
    }

    fn set_flag(&self, name: &str, value: &Value) {
        let mut reg = locked(&self.state.registry);
        for f in reg.flags.iter_mut().filter(|f| f.spec.name == name) {
            f.value = value.clone();
        }
    }

    fn shortcuts(&self) -> Vec<ShortcutSpec> {
        locked(&self.state.registry)
            .shortcuts
            .iter()
            .map(|s| s.spec.clone())
            .collect()
    }

    /// Run the handler of the first shortcut bound to `key`. A key no extension bound is `Ok`.
    fn run_shortcut(&self, key: &KeyId, ui: &mut dyn Ui) -> Result<(), OxaError> {
        let found = {
            let reg = locked(&self.state.registry);
            reg.shortcuts
                .iter()
                .find(|s| &s.spec.key == key)
                .map(|s| Handler {
                    ext: s.ext,
                    slot: s.slot,
                    f: s.handler,
                })
        };
        match found {
            Some(h) => self.call_handler(h, Vec::new(), Some(0), false, ui).map(|_| ()),
            None => Ok(()),
        }
    }

    /// Delegates to [`crate::api_render::render`].
    fn render(&self, kind: RenderKind, payload: &Value, width: u16) -> Option<Vec<String>> {
        crate::api_render::render(&self.state, &self.thread, kind, payload, width)
    }

    /// Delegates to [`crate::api_ui::complete`].
    fn complete(&self, prefix: &str) -> Vec<String> {
        crate::api_ui::complete(&self.state, &self.thread, prefix)
    }

    /// Drop every extension context (the attached host and the thread are kept).
    fn shutdown(&self) {
        *locked(&self.last_wire) = Vec::new();
        crate::unload_all(&self.state, &self.thread);
    }

    /// A handler is registered for `p` (`oxa.on`); one registry lock, no script call.
    fn wants(&self, p: HookPoint) -> bool {
        locked(&self.state.registry)
            .handlers
            .get(&p)
            .is_some_and(|list| !list.is_empty())
    }
}

#[cfg(test)]
mod tests {
    //! Bus logic without E3's `oxa.on`: each script assigns `__h[point]` and the handlers are
    //! registered on the runtime thread directly.
    use super::*;
    use oxa_proto::{HeadlessUi, Hooks};
    use oxa_script::{FnRef, Limits, SlotId};

    /// Add an extension whose script defines `__h = {point: fn, ...}`.
    fn add(host: &ExtensionHost, ext: u32, script: &str, points: &[&str], limits: Limits) {
        let script = script.to_string();
        let points: Vec<String> = points.iter().map(|p| p.to_string()).collect();
        let (slot, fns): (SlotId, Vec<(String, FnRef)>) = host
            .thread
            .run(
                move |slots: &mut Slots| -> Result<(SlotId, Vec<(String, FnRef)>), OxaError> {
                    let id = slots.create(limits)?;
                    let rt = slots.get(id).expect("the slot was just created");
                    rt.eval_script("ext.js", &script)?;
                    let mut out = Vec::new();
                    for p in points {
                        let v = rt.eval_script("ext.js", &format!("__h[{p:?}]"))?;
                        out.push((p.clone(), FnRef::from_value(&v).expect("a function")));
                    }
                    Ok((id, out))
                },
                &mut |_, _| Value::Null,
            )
            .and_then(|r| r)
            .expect("install the test extension");
        let id = ExtId(ext);
        let mut reg = locked(&host.state.registry);
        reg.extensions.push(LoadedExtension {
            id,
            path: format!("ext{ext}.js"),
            name: format!("ext{ext}"),
            slot: Some(slot),
            declared_rebase: false,
            error: None,
        });
        for (p, f) in fns {
            let point = HookPoint::parse(&p).expect("a known point");
            reg.handlers
                .entry(point)
                .or_default()
                .push(Handler { ext: id, slot, f });
        }
    }

    fn host() -> ExtensionHost {
        ExtensionHost::new(std::env::temp_dir()).expect("runtime thread")
    }

    #[test]
    fn transforms_chain_in_load_order() {
        let h = host();
        add(
            &h,
            0,
            "var __h = {input: function(e){ return {action:'transform', text: e.text + '+a'}; }};",
            &["input"],
            Limits::EXTENSION,
        );
        add(
            &h,
            1,
            "var __h = {input: function(e){ return {action:'transform', text: e.text + '+b'}; }};",
            &["input"],
            Limits::EXTENSION,
        );
        let mut ui = HeadlessUi::default();
        let r = h.fire(HookPoint::Input, &serde_json::json!({"text": "x"}), &mut ui);
        assert_eq!(r, HookReply::ReplaceInput("x+a+b".into()));
        assert!(h.state.take_errors().is_empty());
    }

    #[test]
    fn tool_args_chain_and_block_stops() {
        let h = host();
        add(
            &h,
            0,
            "var __h = {tool_call: function(e){ return {input: {n: 1}}; }};",
            &["tool_call"],
            Limits::EXTENSION,
        );
        add(
            &h,
            1,
            "var __h = {tool_call: function(e){ return {input: {n: e.input.n + 1}}; }};",
            &["tool_call"],
            Limits::EXTENSION,
        );
        add(
            &h,
            2,
            "var __h = {tool_call: function(e){ return {block: true, reason: 'nope'}; }};",
            &["tool_call"],
            Limits::EXTENSION,
        );
        add(
            &h,
            3,
            "var __h = {tool_call: function(e){ throw new Error('reached'); }};",
            &["tool_call"],
            Limits::EXTENSION,
        );
        let mut ui = HeadlessUi::default();
        let payload = serde_json::json!({"toolName": "t", "toolCallId": "c", "input": {}});
        assert_eq!(
            h.fire(HookPoint::ToolCall, &payload, &mut ui),
            HookReply::Block("nope".into())
        );
        assert!(h.state.take_errors().is_empty(), "the block stops the chain");
    }

    #[test]
    fn non_prefix_context_is_refused_and_prefix_is_appended() {
        let h = host();
        add(
            &h,
            0,
            "var __h = {context: function(e){ return {messages: [{role:'user', content:'other'}]}; }};",
            &["context"],
            Limits::EXTENSION,
        );
        let mut ui = HeadlessUi::default();
        let payload = serde_json::json!({"messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(h.fire(HookPoint::Context, &payload, &mut ui), HookReply::Continue);
        let errors = h.state.take_errors();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].error.contains("guard_refused"));

        let h2 = host();
        add(&h2, 0, "var __h = {context: function(e){ return {messages: e.messages.concat([{role:'user', content:'extra'}])}; }};", &["context"], Limits::EXTENSION);
        assert_eq!(
            h2.fire(HookPoint::Context, &payload, &mut ui),
            HookReply::Rebase(vec![WireMsg::user("hi"), WireMsg::user("extra")])
        );
        assert!(h2.state.take_errors().is_empty());
    }

    #[test]
    fn throw_is_recorded_and_chain_goes_on() {
        let h = host();
        add(
            &h,
            0,
            "var __h = {input: function(e){ throw new Error('boom'); }};",
            &["input"],
            Limits::EXTENSION,
        );
        add(
            &h,
            1,
            "var __h = {input: function(e){ return {action:'handled'}; }};",
            &["input"],
            Limits::EXTENSION,
        );
        let mut ui = HeadlessUi::default();
        assert_eq!(
            h.fire(HookPoint::Input, &serde_json::json!({"text": "x"}), &mut ui),
            HookReply::Handled
        );
        let errors = h.state.take_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].event, "input");
        assert!(errors[0].error.contains("boom"));
    }

    #[test]
    fn runaway_handler_is_interrupted_by_deadline() {
        let h = host();
        let limits = Limits {
            deadline_ms: Some(200),
            ..Limits::EXTENSION
        };
        add(
            &h,
            0,
            "var __h = {input: function(e){ while (true) {} }};",
            &["input"],
            limits,
        );
        add(
            &h,
            1,
            "var __h = {input: function(e){ return {action:'handled'}; }};",
            &["input"],
            Limits::EXTENSION,
        );
        let mut ui = HeadlessUi::default();
        assert_eq!(
            h.fire(HookPoint::Input, &serde_json::json!({"text": "x"}), &mut ui),
            HookReply::Handled
        );
        assert_eq!(h.state.take_errors().len(), 1);
    }
}
