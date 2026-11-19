//! Wave H (lane hooks): the fire sites of the hook points that are not a plain chain at one
//! place, and the payloads of pi's event objects (`packages/coding-agent/src/core/extensions/
//! types.ts`, oxa names). Each call site in the loop, the session and the host is one call into
//! this module, so the payload shapes live in one file.
//!
//! **Cost.** Every builder runs only when some hook set [`wants`] the point
//! (`Hooks::wants`); the per-delta points (`MessageUpdate`, `ProviderStreamEvent`,
//! `ToolExecutionUpdate`) cost one bool per delta when nothing subscribed.
//!
//! **Re-entrancy.** Hooks fire through [`crate::host::fire_hook`] (a scoped helper thread while
//! the session thread keeps serving host requests), also from inside a running hook or tool
//! (nested [`crate::host::pumped`]): the session thread never blocks on a hook set, so a handler
//! that queries the host while another call into the same hook set waits cannot deadlock.
//! Partial tool output arrives in the tool's live callback, where the state is borrowed: it is
//! queued with [`defer`] and fired by [`drain`] from the polling loop of `pumped` (and after
//! the call).
//!
//! **W-guard.** Every point here except `AgentBeforeSettle` is an observer: replies are ignored
//! ([`crate::host::observe_hooks`]). Nothing here reaches the prefix (W3) or the wire (W1/W2);
//! `AgentBeforeSettle` may only append user messages (custom message entries), as pi's
//! boundary drafts do.
use crate::host::observe_hooks;
use crate::SessionState;
use oxa_proto::{
    AgentEvent, Answer, EventMessage, EventSink, HookPoint, Level, MessageDelta, OxaError, StreamEvent,
    ThinkingLevel, ToolCall, ToolOutput, Ui, UiComponent, Value, WireMsg,
};
use serde_json::json;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

/// Some hook set has something to run at `p`.
pub(crate) fn wants(st: &SessionState, p: HookPoint) -> bool {
    st.hooks.iter().any(|h| h.wants(p))
}

/// Fire observer point `p` with the payload `build` makes, only when some hook set wants it.
pub(crate) fn observe(
    st: &mut SessionState,
    p: HookPoint,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
    build: impl FnOnce(&SessionState) -> Value,
) {
    if !wants(st, p) {
        return;
    }
    let payload = build(st);
    observe_hooks(st, p, &payload, sink, ui);
}

// ---------------------------------------------------------------------------------------------
// Message JSON

/// One wire message in the shape every hook payload uses (`context`, `agent_end`, ...):
/// `{role, content, reasoning?, toolCallId?, toolCalls?: [{id, name, args}]}` with `args` the
/// argument string as received (W2). A `context` handler can return such a list unchanged.
pub(crate) fn wire_json(m: &WireMsg) -> Value {
    let mut o = serde_json::Map::new();
    o.insert("role".into(), Value::String(m.role.as_str().to_string()));
    o.insert(
        "content".into(),
        m.content.clone().map(Value::String).unwrap_or(Value::Null),
    );
    if let Some(r) = &m.reasoning {
        o.insert("reasoning".into(), Value::String(r.clone()));
    }
    if let Some(id) = &m.tool_call_id {
        o.insert("toolCallId".into(), Value::String(id.clone()));
    }
    if !m.tool_calls.is_empty() {
        let calls: Vec<Value> = m
            .tool_calls
            .iter()
            .map(|c| json!({"id": c.id, "name": c.name, "args": c.args_raw}))
            .collect();
        o.insert("toolCalls".into(), Value::Array(calls));
    }
    Value::Object(o)
}

/// [`wire_json`] plus the record fields of pi's `AgentMessage`: `timestamp`, and for an
/// assistant message `stopReason`, `model`, `usage` (`{prompt, completion, cached}`),
/// `errorMessage`; for a tool result `toolName` and `isError`.
pub(crate) fn message_json(m: &EventMessage) -> Value {
    let mut v = wire_json(&m.msg);
    let o = v.as_object_mut().expect("wire_json builds an object");
    o.insert("timestamp".into(), json!(m.ts_ms));
    if let Some(stop) = m.stop {
        o.insert("stopReason".into(), json!(stop.as_str()));
    }
    if let Some(model) = &m.model {
        o.insert("model".into(), json!(model));
    }
    if let Some(u) = m.usage {
        o.insert(
            "usage".into(),
            json!({"prompt": u.prompt, "completion": u.completion, "cached": u.cached}),
        );
    }
    if let Some(name) = &m.tool_name {
        o.insert("toolName".into(), json!(name));
        o.insert("isError".into(), json!(m.is_error));
    }
    if let Some(e) = &m.error_message {
        o.insert("errorMessage".into(), json!(e));
    }
    v
}

/// pi's tool result object: `{content: [{type: "text", text}], details}`.
fn text_result(text: &str, details: &Value) -> Value {
    let details = if details.is_null() {
        json!({})
    } else {
        details.clone()
    };
    json!({"content": [{"type": "text", "text": text}], "details": details})
}

fn tool_call_json(t: &ToolCall) -> Value {
    json!({"type": "toolCall", "id": t.id, "name": t.name, "args": t.args_raw})
}

/// pi's `assistantMessageEvent` (the same shape as the JSON-mode record).
fn delta_json(d: &MessageDelta) -> Value {
    match d {
        MessageDelta::TextStart { index } => json!({"type": "text_start", "contentIndex": index}),
        MessageDelta::TextDelta { index, delta } => {
            json!({"type": "text_delta", "contentIndex": index, "delta": delta})
        }
        MessageDelta::TextEnd { index, content } => {
            json!({"type": "text_end", "contentIndex": index, "content": content})
        }
        MessageDelta::ThinkingStart { index } => json!({"type": "thinking_start", "contentIndex": index}),
        MessageDelta::ThinkingDelta { index, delta } => {
            json!({"type": "thinking_delta", "contentIndex": index, "delta": delta})
        }
        MessageDelta::ThinkingEnd { index, content } => {
            json!({"type": "thinking_end", "contentIndex": index, "content": content})
        }
        MessageDelta::ToolcallStart { index, id, name } => {
            json!({"type": "toolcall_start", "contentIndex": index, "id": id, "toolName": name})
        }
        MessageDelta::ToolcallDelta { index, delta } => {
            json!({"type": "toolcall_delta", "contentIndex": index, "delta": delta})
        }
        MessageDelta::ToolcallEnd { index, call } => {
            json!({"type": "toolcall_end", "contentIndex": index, "toolCall": tool_call_json(call)})
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The loop: agent, turn and message points

/// `agent_end` (pi `AgentEndEvent`): `{messages}` = the messages this run added.
pub(crate) fn agent_end(
    st: &mut SessionState,
    msgs: &[EventMessage],
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) {
    observe(
        st,
        HookPoint::AgentEnd,
        sink,
        ui,
        |_| json!({ "messages": msgs.iter().map(message_json).collect::<Vec<_>>() }),
    );
}

/// The prompt messages of a run that is about to start: the user messages at the end of the
/// wire (after the last reply or tool result), oldest first. Empty when nobody wants the
/// message or agent_end points.
pub(crate) fn opening_messages(st: &SessionState) -> Vec<EventMessage> {
    if ![
        HookPoint::MessageStart,
        HookPoint::MessageEnd,
        HookPoint::AgentEnd,
    ]
    .into_iter()
    .any(|p| wants(st, p))
    {
        return Vec::new();
    }
    let wire = st.transcript.wire();
    let first = wire
        .iter()
        .rposition(|m| m.role != oxa_proto::Role::User)
        .map_or(0, |i| i + 1);
    wire[first..]
        .iter()
        .map(|m| EventMessage {
            msg: m.clone(),
            ts_ms: now_ms(),
            stop: None,
            usage: None,
            model: None,
            tool_name: None,
            is_error: false,
            error_message: None,
        })
        .collect()
}

/// `turn_start` (pi `TurnStartEvent`): `{turnIndex, timestamp}`.
pub(crate) fn turn_start(st: &mut SessionState, index: u32, sink: &mut dyn EventSink, ui: &mut dyn Ui) {
    observe(
        st,
        HookPoint::TurnStart,
        sink,
        ui,
        |_| json!({ "turnIndex": index, "timestamp": now_ms() }),
    );
}

/// `turn_end` (pi `TurnEndEvent`): `{turnIndex, message, toolResults, messageEntryId,
/// toolResultEntryIds, entries, continue, outcome}`. `entries` is always empty and `continue`
/// tells whether the loop goes on (tool results to answer); the point is an observer in oxa
/// (W1: a turn boundary cannot add drafts), so pi's `context` preview is not built.
pub(crate) fn turn_end(
    st: &mut SessionState,
    index: u32,
    message: &EventMessage,
    tool_results: &[EventMessage],
    entry_ids: (&str, &[String]),
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) {
    observe(st, HookPoint::TurnEnd, sink, ui, |st| {
        let aborted = st.shared.cancel.is_cancelled() || message.stop == Some(oxa_proto::StopReason::Aborted);
        json!({
            "turnIndex": index,
            "message": message_json(message),
            "toolResults": tool_results.iter().map(message_json).collect::<Vec<_>>(),
            "messageEntryId": entry_ids.0,
            "toolResultEntryIds": entry_ids.1,
            "entries": [],
            "continue": !tool_results.is_empty() && !aborted,
            "outcome": if aborted { "aborted" } else { "completed" },
        })
    });
}

/// `message_start` / `message_end` (pi `MessageStartEvent` / `MessageEndEvent`): `{message}`.
pub(crate) fn message(
    st: &mut SessionState,
    p: HookPoint,
    m: &EventMessage,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) {
    observe(st, p, sink, ui, |_| json!({ "message": message_json(m) }));
}

// ---------------------------------------------------------------------------------------------
// The request: provider and stream points

/// The provider points of one request (`after_provider_response`, `provider_stream_event`,
/// `message_update`). [`StreamHooks::new`] reads once per request which of them are wanted;
/// when none is, every call is one branch.
pub(crate) struct StreamHooks {
    response: bool,
    raw: bool,
    update: bool,
    responded: bool,
    model: String,
    /// The assistant message so far (for `message_update`), built from the deltas.
    partial: WireMsg,
    /// `contentIndex` -> position in `partial.tool_calls`.
    calls: Vec<(usize, usize)>,
}

impl StreamHooks {
    /// `events`: the request streams a chat reply (summaries and prefill fire nothing, as pi's
    /// provider events belong to the agent's own model calls).
    pub(crate) fn new(st: &SessionState, events: bool) -> StreamHooks {
        StreamHooks {
            response: events && wants(st, HookPoint::AfterProviderResponse),
            raw: events && wants(st, HookPoint::ProviderStreamEvent),
            update: events && wants(st, HookPoint::MessageUpdate),
            responded: false,
            model: st.model.clone(),
            partial: WireMsg::assistant(""),
            calls: Vec::new(),
        }
    }

    /// Before the engine's increment `ev` is translated: `after_provider_response` once (the
    /// first increment means the response arrived and its stream is being read), then
    /// `provider_stream_event` with the increment.
    pub(crate) fn raw(
        &mut self,
        st: &mut SessionState,
        ev: &StreamEvent,
        sink: &mut dyn EventSink,
        ui: &mut dyn Ui,
    ) {
        self.respond(st, sink, ui);
        if !self.raw {
            return;
        }
        let data = match ev {
            StreamEvent::Prefill { done, total, secs } => {
                json!({"type": "prefill", "done": done, "total": total, "secs": secs})
            }
            StreamEvent::Queued { pos, wait_s } => json!({"type": "queued", "pos": pos, "waitS": wait_s}),
            StreamEvent::Reasoning(d) => json!({"type": "reasoning", "delta": d}),
            StreamEvent::Text(d) => json!({"type": "text", "delta": d}),
            StreamEvent::ToolArgs {
                index,
                id,
                name,
                delta,
            } => json!({"type": "tool_args", "index": index, "id": id, "name": name, "delta": delta}),
        };
        let payload = json!({
            "provider": "oxe",
            "api": "openai-completions",
            "model": self.model,
            "data": data,
        });
        observe_hooks(st, HookPoint::ProviderStreamEvent, &payload, sink, ui);
    }

    /// `after_provider_response` when the request returned without a single increment.
    pub(crate) fn done(&mut self, st: &mut SessionState, sink: &mut dyn EventSink, ui: &mut dyn Ui) {
        self.respond(st, sink, ui);
    }

    /// pi `AfterProviderResponseEvent`: `{status, headers}`. The engine client reads the
    /// status before streaming and does not hand the headers up, so `headers` is empty.
    fn respond(&mut self, st: &mut SessionState, sink: &mut dyn EventSink, ui: &mut dyn Ui) {
        if !self.response || self.responded {
            return;
        }
        self.responded = true;
        let payload = json!({ "status": 200, "headers": {} });
        observe_hooks(st, HookPoint::AfterProviderResponse, &payload, sink, ui);
    }

    /// A sink that forwards to `inner` and fires `message_update` for each `MessageUpdate`.
    pub(crate) fn sink<'a>(
        &'a mut self,
        st: &'a mut SessionState,
        inner: &'a mut dyn EventSink,
        ui: &'a mut dyn Ui,
    ) -> UpdateSink<'a> {
        UpdateSink {
            hooks: self,
            st,
            inner,
            ui,
        }
    }

    fn fold(&mut self, d: &MessageDelta) {
        match d {
            MessageDelta::TextDelta { delta, .. } => {
                self.partial
                    .content
                    .get_or_insert_with(String::new)
                    .push_str(delta);
            }
            MessageDelta::ThinkingDelta { delta, .. } => {
                self.partial
                    .reasoning
                    .get_or_insert_with(String::new)
                    .push_str(delta);
            }
            MessageDelta::ToolcallStart { index, id, name } => {
                self.calls.push((*index, self.partial.tool_calls.len()));
                self.partial.tool_calls.push(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    args_raw: String::new(),
                });
            }
            MessageDelta::ToolcallDelta { index, delta } => {
                if let Some(&(_, at)) = self.calls.iter().find(|(i, _)| i == index) {
                    self.partial.tool_calls[at].args_raw.push_str(delta);
                }
            }
            _ => {}
        }
    }
}

/// See [`StreamHooks::sink`].
pub(crate) struct UpdateSink<'a> {
    hooks: &'a mut StreamHooks,
    st: &'a mut SessionState,
    inner: &'a mut dyn EventSink,
    ui: &'a mut dyn Ui,
}

impl EventSink for UpdateSink<'_> {
    fn emit(&mut self, e: &AgentEvent) {
        self.inner.emit(e);
        if !self.hooks.update {
            return;
        }
        if let AgentEvent::MessageUpdate { delta, .. } = e {
            self.hooks.fold(delta);
            let payload = json!({
                "message": wire_json(&self.hooks.partial),
                "assistantMessageEvent": delta_json(delta),
            });
            observe_hooks(self.st, HookPoint::MessageUpdate, &payload, self.inner, self.ui);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tool execution

/// Emit `ToolExecutionStart` and fire `tool_execution_start` (pi: `{toolCallId, toolName,
/// args}`).
pub(crate) fn tool_start(
    st: &mut SessionState,
    id: &str,
    name: &str,
    args: &Value,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) {
    crate::progress::notify(st, crate::Progress::ToolStart(name));
    sink.emit(&AgentEvent::ToolExecutionStart {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        args: args.clone(),
    });
    observe(
        st,
        HookPoint::ToolExecutionStart,
        sink,
        ui,
        |_| json!({ "toolCallId": id, "toolName": name, "args": args }),
    );
}

/// Emit `ToolExecutionEnd` and fire `tool_execution_end` (pi: `{toolCallId, toolName, result,
/// isError, durationMs?}`, `result` = `{content: [{type: "text", text}], details}`).
pub(crate) fn tool_end(
    st: &mut SessionState,
    id: &str,
    name: &str,
    output: &ToolOutput,
    duration_ms: Option<u64>,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) {
    crate::progress::notify(st, crate::Progress::ToolEnd);
    sink.emit(&AgentEvent::ToolExecutionEnd {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        result: output.clone(),
        duration_ms,
    });
    observe(st, HookPoint::ToolExecutionEnd, sink, ui, |_| {
        let mut v = json!({
            "toolCallId": id,
            "toolName": name,
            "result": text_result(&output.text, &output.ui),
            "isError": output.is_error,
        });
        if let Some(ms) = duration_ms {
            v["durationMs"] = json!(ms);
        }
        v
    });
}

/// The payload of `tool_execution_update` (pi: `{toolCallId, toolName, args, partialResult}`).
pub(crate) fn tool_update_payload(id: &str, name: &str, args: &Value, partial: &str) -> Value {
    json!({
        "toolCallId": id,
        "toolName": name,
        "args": args,
        "partialResult": text_result(partial, &Value::Null),
    })
}

thread_local! {
    /// Observer fires queued on the session thread where the state is borrowed (a tool's live
    /// callback); [`drain`] fires them in order.
    static DEFERRED: RefCell<Vec<(HookPoint, Value)>> = const { RefCell::new(Vec::new()) };
    /// While > 0, dialogs are not reported as `ui_prompt_*` (the handlers of those points
    /// themselves; pi coalesces nested prompts into one span).
    static QUIET_PROMPTS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Queue an observer fire for [`drain`] (session thread only).
pub(crate) fn defer(p: HookPoint, payload: Value) {
    DEFERRED.with(|d| d.borrow_mut().push((p, payload)));
}

/// Fire what [`defer`] queued, in order.
pub(crate) fn drain(st: &mut SessionState, sink: &mut dyn EventSink, ui: &mut dyn Ui) {
    loop {
        let next = DEFERRED.with(|d| {
            let mut d = d.borrow_mut();
            (!d.is_empty()).then(|| d.remove(0))
        });
        let Some((p, payload)) = next else {
            return;
        };
        observe_hooks(st, p, &payload, sink, ui);
    }
}

/// Drop what [`defer`] queued (a session is entered: nothing of the previous one fires).
pub(crate) fn clear_deferred() {
    DEFERRED.with(|d| d.borrow_mut().clear());
}

// ---------------------------------------------------------------------------------------------
// UI prompts

/// One `ui_prompt_start` / `ui_prompt_end` waiting to be fired.
#[derive(Debug, Clone)]
pub(crate) struct PromptMark {
    start: bool,
    kind: &'static str,
    title: Option<String>,
}

/// Where a [`PromptTap`] records the dialogs it saw; the session thread drains it.
pub(crate) type PromptLog = Arc<Mutex<Vec<PromptMark>>>;

/// A `Ui` that reports every blocking dialog (select, confirm, input, editor, custom, the
/// permission prompt) as a start and an end mark (pi's `withUIPrompt`; nested dialogs are one
/// span). Used by [`crate::host::pumped`] around every call into a hook set or a tool.
pub(crate) struct PromptTap<'u> {
    inner: &'u mut dyn Ui,
    log: PromptLog,
    depth: u32,
}

impl<'u> PromptTap<'u> {
    pub(crate) fn new(inner: &'u mut dyn Ui, log: PromptLog) -> PromptTap<'u> {
        PromptTap { inner, log, depth: 0 }
    }

    /// `true` while the `ui_prompt_*` handlers themselves run: their dialogs are not reported.
    pub(crate) fn quiet() -> bool {
        QUIET_PROMPTS.with(|q| q.get() > 0)
    }

    /// The wrapped UI, for a caller that must not report its dialogs.
    pub(crate) fn inner_mut(&mut self) -> &mut dyn Ui {
        &mut *self.inner
    }

    fn span<R>(&mut self, kind: &'static str, title: Option<&str>, f: impl FnOnce(&mut dyn Ui) -> R) -> R {
        let outer = self.depth == 0;
        if outer {
            self.mark(true, kind, title);
        }
        self.depth += 1;
        let out = f(&mut *self.inner);
        self.depth -= 1;
        if outer {
            self.mark(false, kind, title);
        }
        out
    }

    fn mark(&self, start: bool, kind: &'static str, title: Option<&str>) {
        let mut log = self.log.lock().unwrap_or_else(|p| p.into_inner());
        log.push(PromptMark {
            start,
            kind,
            title: title.map(str::to_string),
        });
    }
}

impl Ui for PromptTap<'_> {
    fn select(&mut self, title: &str, options: &[String], timeout_ms: Option<u32>) -> Option<usize> {
        self.span("select", Some(title), |u| u.select(title, options, timeout_ms))
    }
    fn confirm(&mut self, title: &str, body: &str, timeout_ms: Option<u32>) -> Option<bool> {
        self.span("confirm", Some(title), |u| u.confirm(title, body, timeout_ms))
    }
    fn dialog_canceller(&self) -> Option<std::sync::Arc<dyn Fn() + Send + Sync>> {
        self.inner.dialog_canceller()
    }
    fn input(&mut self, title: &str, placeholder: &str) -> Option<String> {
        self.span("input", Some(title), |u| u.input(title, placeholder))
    }
    fn editor(&mut self, title: &str, prefill: &str) -> Option<String> {
        self.span("editor", Some(title), |u| u.editor(title, prefill))
    }
    fn notify(&mut self, text: &str, level: Level) {
        self.inner.notify(text, level)
    }
    fn set_status(&mut self, key: &str, text: Option<&str>) {
        self.inner.set_status(key, text)
    }
    fn set_widget(&mut self, key: &str, lines: Option<&[String]>, at: oxa_proto::Placement) {
        self.inner.set_widget(key, lines, at)
    }
    fn set_title(&mut self, t: &str) {
        self.inner.set_title(t)
    }
    fn set_editor_text(&mut self, t: &str) {
        self.inner.set_editor_text(t)
    }
    fn ask_access(&mut self, a: &oxa_proto::Access) -> Answer {
        let title = access_title(a);
        self.span("confirm", Some(&title), |u| u.ask_access(a))
    }
    fn editor_text(&mut self) -> String {
        self.inner.editor_text()
    }
    fn set_footer(&mut self, lines: Option<&[String]>) {
        self.inner.set_footer(lines)
    }
    fn set_header(&mut self, lines: Option<&[String]>) {
        self.inner.set_header(lines)
    }
    fn set_working_message(&mut self, text: Option<&str>) {
        self.inner.set_working_message(text)
    }
    fn custom(&mut self, c: Box<dyn UiComponent>, opts: oxa_proto::CustomOpts) -> Option<Value> {
        self.span("custom", None, |u| u.custom(c, opts))
    }
    fn interactive(&self) -> bool {
        self.inner.interactive()
    }
}

/// The title a permission prompt is reported with.
pub(crate) fn access_title(a: &oxa_proto::Access) -> String {
    use oxa_proto::Access;
    match a {
        Access::Read(p) => format!("read {}", p.display()),
        Access::Write(p) => format!("write {}", p.display()),
        Access::Exec { command, .. } => format!("run {command}"),
        Access::Net { host } => format!("connect to {host}"),
        Access::Mcp { server, tool, .. } => format!("call {server}/{tool}"),
    }
}

/// Fire the marks a [`PromptTap`] recorded (pi `UIPromptStartEvent` / `UIPromptEndEvent`:
/// `{reason: "ui_prompt", kind, title?}`). Dialogs the handlers open are not reported.
pub(crate) fn fire_prompts(
    st: &mut SessionState,
    log: &PromptLog,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) {
    loop {
        let next = {
            let mut l = log.lock().unwrap_or_else(|p| p.into_inner());
            (!l.is_empty()).then(|| l.remove(0))
        };
        let Some(m) = next else {
            return;
        };
        let p = if m.start {
            HookPoint::UiPromptStart
        } else {
            HookPoint::UiPromptEnd
        };
        QUIET_PROMPTS.with(|q| q.set(q.get() + 1));
        observe(st, p, sink, ui, |_| prompt_payload(&m));
        QUIET_PROMPTS.with(|q| q.set(q.get().saturating_sub(1)));
    }
}

fn prompt_payload(m: &PromptMark) -> Value {
    let mut v = json!({ "reason": "ui_prompt", "kind": m.kind });
    if let Some(t) = &m.title {
        v["title"] = json!(t);
    }
    v
}

/// The permission prompt of a tool call, asked on the session thread: `ui_prompt_start`, the
/// question, `ui_prompt_end`.
pub(crate) fn ask_access(
    st: &mut SessionState,
    a: &oxa_proto::Access,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Answer {
    if PromptTap::quiet() {
        return ui.ask_access(a);
    }
    let title = Some(access_title(a));
    let log: PromptLog = Arc::default();
    let push = |start: bool| {
        log.lock().unwrap_or_else(|p| p.into_inner()).push(PromptMark {
            start,
            kind: "confirm",
            title: title.clone(),
        });
    };
    push(true);
    fire_prompts(st, &log, sink, ui);
    let answer = ui.ask_access(a);
    push(false);
    fire_prompts(st, &log, sink, ui);
    answer
}

// ---------------------------------------------------------------------------------------------
// Session points

/// The images of a prompt as pi's `ImageContent` references (`{type: "image", mimeType,
/// path, width, height}`; oxa keeps the file, the engine never sees the pixels).
pub(crate) fn images_json(parts: &[oxa_proto::Part]) -> Vec<Value> {
    parts
        .iter()
        .filter_map(|p| match p {
            oxa_proto::Part::Image { path, mime, w, h, .. } => Some(json!({
                "type": "image",
                "mimeType": mime,
                "path": path.display().to_string(),
                "width": w,
                "height": h,
            })),
            oxa_proto::Part::Text(_) | oxa_proto::Part::EnvNote(_) => None,
        })
        .collect()
}

/// pi `InputEvent`: `{text, images?, source}`. `source` is pi's default for
/// `session.prompt`, `"interactive"` (print mode included); the session API has no caller
/// field, so RPC prompts report it too (pi: `"rpc"`).
pub(crate) fn input_payload(text: &str, parts: &[oxa_proto::Part]) -> Value {
    let mut v = json!({
        "text": text,
        "source": "interactive",
    });
    let images = images_json(parts);
    if !images.is_empty() {
        v["images"] = Value::Array(images);
    }
    v
}

/// pi `BeforeAgentStartUserEvent`: `{messageType: "user", prompt, images?, systemPrompt}`.
/// `systemPrompt` is the system text of the frozen prefix in force (W3: read-only here; pi's
/// mutable `systemPromptOptions` are not offered, the prefix changes only at an epoch).
pub(crate) fn before_agent_start_payload(st: &SessionState, text: &str, images: Vec<Value>) -> Value {
    let mut v = json!({
        "messageType": "user",
        "prompt": text,
        "systemPrompt": st.prefix.system_text(),
    });
    if !images.is_empty() {
        v["images"] = Value::Array(images);
    }
    v
}

/// `before_agent_start` for a run a hook set triggered (pi `BeforeAgentStartCustomEvent`:
/// `{messageType: "custom", message: {customType, content, display, details}, systemPrompt}`).
/// The triggering message is the custom message entry at the leaf. Accepted reply: `Append`
/// (hidden custom messages after it), as for a prompt.
pub(crate) fn triggered_start(
    st: &mut SessionState,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<(), OxaError> {
    if !wants(st, HookPoint::BeforeAgentStart) {
        return Ok(());
    }
    let message = match st.tree.get(st.tree.leaf()).map(|e| &e.body) {
        Some(oxa_proto::EntryBody::CustomMessage {
            custom_type,
            content,
            display,
            details,
        }) => json!({
            "role": "custom",
            "customType": custom_type,
            "content": content,
            "display": display,
            "details": details,
        }),
        _ => return Ok(()),
    };
    let payload = json!({
        "messageType": "custom",
        "message": message,
        "systemPrompt": st.prefix.system_text(),
    });
    if let oxa_proto::HookReply::Append(msgs) =
        crate::host::first_reply(st, HookPoint::BeforeAgentStart, &payload, sink, ui)
    {
        for m in msgs {
            if let Some(content) = m.content {
                crate::agent_loop::persist(
                    st,
                    oxa_proto::EntryBody::CustomMessage {
                        custom_type: HookPoint::BeforeAgentStart.as_str().to_string(),
                        content,
                        display: false,
                        details: Value::Null,
                    },
                    sink,
                )?;
            }
        }
    }
    Ok(())
}

/// pi `SessionBeforeSwitchEvent`: `{reason: "new" | "resume", targetSessionFile?}`.
pub(crate) fn before_switch_payload(how: &crate::Open) -> Value {
    match how {
        crate::Open::Resume(sref) => json!({
            "reason": "resume",
            "targetSessionFile": sref.path.display().to_string(),
        }),
        crate::Open::Continue | crate::Open::Fork(_) => json!({ "reason": "resume" }),
        crate::Open::New | crate::Open::Ephemeral => json!({ "reason": "new" }),
    }
}

/// `session_shutdown` (pi: `{reason, targetSessionFile?}`) when the session is left for another
/// one inside the process (`reason`: `"new" | "resume" | "fork"`).
pub(crate) fn session_shutdown(
    st: &mut SessionState,
    reason: &str,
    target: Option<&str>,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) {
    observe(st, HookPoint::SessionShutdown, sink, ui, |_| {
        let mut v = json!({ "reason": reason });
        if let Some(t) = target {
            v["targetSessionFile"] = json!(t);
        }
        v
    });
}

/// pi's fork (and `/clone`, a fork at the leaf): `session_shutdown` with reason `fork` before
/// the switch, then (`after`) `session_start` with reason `fork` and the previous file.
pub(crate) fn fork_switch<T>(
    st: &mut SessionState,
    target: &oxa_session::SessionRef,
    ui: &mut dyn Ui,
    switch: impl FnOnce(&mut SessionState) -> Result<T, OxaError>,
) -> Result<T, OxaError> {
    // The fork entry points have no event sink: what the hooks emit is dropped.
    let mut quiet = oxa_proto::VecSink::default();
    let previous = st.sref.path.display().to_string();
    session_shutdown(
        st,
        "fork",
        Some(&target.path.display().to_string()),
        &mut quiet,
        ui,
    );
    let out = switch(st)?;
    let start = SessionStart {
        reason: "fork",
        previous: Some(previous),
    };
    session_start(st, &start, &mut quiet, ui);
    Ok(out)
}

/// Why a session was entered (pi `SessionStartEvent`).
pub(crate) struct SessionStart {
    /// `"startup" | "new" | "resume" | "fork"`.
    pub reason: &'static str,
    /// The session file left behind (a switch inside the process).
    pub previous: Option<String>,
}

/// `session_start` (pi: `{reason, previousSessionFile?}`), then `resources_discover` (pi:
/// `{cwd, reason: "startup"}`; `/reload` fires it with `"reload"`).
pub(crate) fn session_start(
    st: &mut SessionState,
    start: &SessionStart,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) {
    crate::progress::session(st);
    observe(st, HookPoint::SessionStart, sink, ui, |_| {
        let mut v = json!({ "reason": start.reason });
        if let Some(p) = &start.previous {
            v["previousSessionFile"] = json!(p);
        }
        v
    });
    observe(
        st,
        HookPoint::ResourcesDiscover,
        sink,
        ui,
        |st| json!({ "cwd": st.cwd.display().to_string(), "reason": "startup" }),
    );
}

/// `session_info_changed` (pi: `{name}`; `undefined` = cleared, here absent).
pub(crate) fn session_info_changed(
    st: &mut SessionState,
    name: Option<&str>,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) {
    observe(st, HookPoint::SessionInfoChanged, sink, ui, |_| match name {
        Some(n) => json!({ "name": n }),
        None => json!({}),
    });
}

/// `thinking_level_select` (pi: `{level, previousLevel}`), on every change of the level.
pub(crate) fn thinking_level_select(
    st: &mut SessionState,
    level: ThinkingLevel,
    previous: ThinkingLevel,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) {
    if level == previous {
        return;
    }
    observe(
        st,
        HookPoint::ThinkingLevelSelect,
        sink,
        ui,
        |_| json!({ "level": level.as_str(), "previousLevel": previous.as_str() }),
    );
}

/// Fire the `agent_settled` hook (pi: `{aborted}`): the run has settled and nothing more runs
/// for it. The hook runs while the session is still claimed; the caller writes the
/// `AgentSettled` record afterwards, once the claim is released (the busy-race rule: the record
/// is the last thing an operation does).
pub(crate) fn agent_settled(st: &mut SessionState, aborted: bool, sink: &mut dyn EventSink, ui: &mut dyn Ui) {
    observe(
        st,
        HookPoint::AgentSettled,
        sink,
        ui,
        |_| json!({ "aborted": aborted }),
    );
    crate::progress::notify(st, crate::Progress::Settled(st.stats.outcome));
}

/// `agent_before_settle` (pi `_runBeforeSettleBoundary`): fired after a run that ended without
/// an abort, before the session settles. A handler may append user messages (pi's
/// `custom_message` drafts, W1: appends only) and the loop runs again from the wire; work it
/// queued meanwhile (steering, follow-ups, triggered turns) runs too. Repeats until a pass adds
/// nothing. `last` is the end of the run so far; returns the end of the last run.
pub(crate) fn before_settle(
    st: &mut SessionState,
    mut last: Option<crate::RunEnd>,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Result<Option<crate::RunEnd>, OxaError> {
    use crate::RunEnd;
    loop {
        if crate::goal::terminal(st) {
            return Ok(last);
        }
        if !matches!(last, Some(RunEnd::Stopped)) || st.shared.cancel.is_cancelled() {
            return Ok(last);
        }
        if st.shared.gate.wrap_up.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return crate::session::run_from_wire(st, sink, ui).map(Some);
        }
        if !wants(st, HookPoint::AgentBeforeSettle) {
            return Ok(last);
        }
        let payload = json!({
            "entries": [],
            "continue": false,
            "outcome": "completed",
        });
        let appended = match crate::host::first_reply(st, HookPoint::AgentBeforeSettle, &payload, sink, ui) {
            oxa_proto::HookReply::Append(msgs) => {
                let mut n = 0;
                for m in msgs {
                    if let Some(content) = m.content {
                        crate::agent_loop::persist(
                            st,
                            oxa_proto::EntryBody::CustomMessage {
                                custom_type: HookPoint::AgentBeforeSettle.as_str().to_string(),
                                content,
                                display: false,
                                details: Value::Null,
                            },
                            sink,
                        )?;
                        n += 1;
                    }
                }
                n > 0
            }
            _ => false,
        };
        // A limit may have arrived inside the callback even when it queued no work.
        // Finish the promised final turn before publishing the settled outcome.
        let end = if appended || st.shared.gate.wrap_up.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            Some(crate::session::run_from_wire(st, sink, ui)?)
        } else {
            crate::session::run_pending(st, sink, ui)?
        };
        match end {
            Some(e) => last = Some(e),
            None => return Ok(last),
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
