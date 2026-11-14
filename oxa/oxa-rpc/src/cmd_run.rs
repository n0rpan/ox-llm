//! C10: RPC run-control commands. Owner: task C10. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/modes/rpc/rpc-mode.ts (docs: rpc-commands.md)
use crate::events::{event_to_json, message_to_json, state_to_json};
use crate::{Handled, RpcCommand, RpcCx, RpcUi};
use oxa_config::QueueMode;
use oxa_core::{Handle, Open};
use oxa_proto::{
    codes, AgentEvent, EngineInfo, EntryBody, EventMessage, EventSink, Mode, OxaError, Role, StopReason,
    ThinkingLevel, Value,
};
use serde_json::json;
use std::collections::HashMap;

/// Commands: `prompt` (with `images`, `streamingBehavior`), `steer`, `follow_up`, `abort`,
/// `abort_retry`, `clear_queue`, `new_session`, `get_state`, `get_messages`, `set_model`,
/// `cycle_model`, `get_available_models`, `set_thinking_level`, `cycle_thinking_level`,
/// `get_available_thinking_levels`, `set_steering_mode`, `set_follow_up_mode`,
/// `set_auto_compaction`, `set_auto_retry`, and oxa's `set_mode`. Request and response
/// shapes are the examples of rpc-commands.md (used as fixtures). Returns `None` for any
/// other command.
pub fn handle(cx: &mut RpcCx, cmd: &RpcCommand) -> Handled {
    let r = match cmd.kind.as_str() {
        "prompt" => run_prompt(cx, cmd),
        "steer" | "follow_up" | "abort" | "abort_retry" | "clear_queue" => {
            return queue_command(&cx.handle, cmd)
        }
        "new_session" => new_session(cx),
        "get_state" => Ok(state_to_json(&cx.session.state())),
        "get_messages" => Ok(json!({"messages": messages(cx)})),
        "set_model" => set_model(cx, cmd),
        "cycle_model" => cycle_model(cx),
        "get_available_models" => Ok(json!({"models": models(cx)})),
        "set_thinking_level" => set_thinking_level(cx, cmd),
        "cycle_thinking_level" => cycle_thinking_level(cx),
        "get_available_thinking_levels" => {
            Ok(json!({"levels": SUPPORTED_THINKING.map(ThinkingLevel::as_str)}))
        }
        "set_steering_mode" | "set_follow_up_mode" => set_queue_mode(cx, cmd),
        "set_auto_compaction" => bool_field(cmd, "enabled").map(|on| {
            cx.session.set_auto_compaction(on);
            Value::Null
        }),
        "set_auto_retry" => bool_field(cmd, "enabled").map(|on| {
            cx.session.set_auto_retry(on);
            Value::Null
        }),
        "set_mode" => set_mode(cx, cmd),
        _ => return None,
    };
    Some(r)
}

/// The levels this engine offers: `Off` is `enable_thinking: false`, the others are on (W6).
const SUPPORTED_THINKING: [ThinkingLevel; 5] = [
    ThinkingLevel::Off,
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
];

/// The commands the front answers without the session thread: queue operations and abort.
/// `abort` only cancels here; the caller waits for the run to settle before it responds.
/// Returns `None` for any other command.
pub(crate) fn queue_command(handle: &Handle, cmd: &RpcCommand) -> Handled {
    let r = match cmd.kind.as_str() {
        "steer" => str_field(cmd, "message").map(|text| {
            handle.steer(text.to_string());
            json!({"disposition": "queued"})
        }),
        "follow_up" => str_field(cmd, "message").map(|text| {
            handle.follow_up(text.to_string());
            json!({"disposition": "queued"})
        }),
        "abort" => {
            handle.abort();
            Ok(Value::Null)
        }
        "abort_retry" => {
            handle.abort_retry();
            Ok(Value::Null)
        }
        "clear_queue" => {
            let (steering, follow_up) = handle.queued();
            handle.dequeue();
            Ok(json!({"steering": steering, "followUp": follow_up}))
        }
        _ => return None,
    };
    Some(r)
}

/// Writes events of the session thread to the shared record writer.
pub(crate) struct OutSink(pub crate::Out);

impl EventSink for OutSink {
    fn emit(&mut self, e: &AgentEvent) {
        if crate::events::on_wire(e) {
            crate::server::write_out(&self.0, &event_to_json(e));
        }
    }
}

fn rpc_ui(cx: &RpcCx) -> RpcUi {
    RpcUi {
        out: cx.out.clone(),
        router: cx.ui.clone(),
    }
}

fn str_field<'a>(cmd: &'a RpcCommand, key: &str) -> Result<&'a str, OxaError> {
    cmd.body
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| OxaError::new(codes::INVALID, format!("{} needs a string \"{key}\"", cmd.kind)))
}

fn bool_field(cmd: &RpcCommand, key: &str) -> Result<bool, OxaError> {
    cmd.body
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| OxaError::new(codes::INVALID, format!("{} needs a boolean \"{key}\"", cmd.kind)))
}

/// `prompt` on the session thread. A prompt that starts a run was answered by the reader (a
/// failure here is reported by the caller as a notice); an extension command is answered here,
/// after its handler returned, with pi's `handled` disposition.
fn run_prompt(cx: &mut RpcCx, cmd: &RpcCommand) -> Result<Value, OxaError> {
    let text = str_field(cmd, "message")?.to_string();
    let mut sink = OutSink(cx.out.clone());
    let mut ui = rpc_ui(cx);
    let command = match oxa_core::expand_input(cx.session.inner(), &text) {
        oxa_core::Expanded::Hook { .. } => true,
        // oxa: a built-in runs headless; its lines are `notice` records.
        oxa_core::Expanded::Builtin { name, args } => {
            for text in oxa_core::run_builtin(&mut cx.session, &name, &args, &mut sink, &mut ui)? {
                sink.emit(&AgentEvent::Notice {
                    level: oxa_proto::Level::Info,
                    text,
                });
            }
            return Ok(json!({"disposition": "handled"}));
        }
        _ => false,
    };
    cx.session.prompt(text, Vec::new(), &mut sink, &mut ui)?;
    Ok(if command {
        json!({"disposition": "handled"})
    } else {
        Value::Null
    })
}

fn new_session(cx: &mut RpcCx) -> Result<Value, OxaError> {
    let mut sink = OutSink(cx.out.clone());
    let mut ui = rpc_ui(cx);
    match cx.session.switch(Open::New, &mut sink, &mut ui) {
        Ok(()) => Ok(json!({"cancelled": false})),
        Err(e) if e.code == codes::ABORTED => Ok(json!({"cancelled": true})),
        Err(e) => Err(e),
    }
}

/// The active branch as pi's message objects. Only message entries (user, assistant, tool
/// result) are listed; shown notices and bash entries are not.
fn messages(cx: &RpcCx) -> Vec<Value> {
    let st = cx.session.inner();
    let branch = st.tree.branch(st.tree.leaf());
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    for e in branch {
        let EntryBody::Msg { wire, .. } = &e.body else {
            continue;
        };
        for c in &wire.tool_calls {
            tool_names.insert(c.id.clone(), c.name.clone());
        }
        let tool_name = wire
            .tool_call_id
            .as_ref()
            .and_then(|id| tool_names.get(id))
            .cloned();
        let assistant = wire.role == Role::Assistant;
        let m = EventMessage {
            msg: wire.clone(),
            ts_ms: e.ts_ms,
            stop: assistant.then_some({
                if wire.tool_calls.is_empty() {
                    StopReason::Stop
                } else {
                    StopReason::ToolUse
                }
            }),
            usage: None,
            model: assistant.then(|| st.model.clone()),
            tool_name,
            is_error: false,
            error_message: None,
        };
        out.push(message_to_json(&m));
    }
    out
}

/// pi's model object for one model id of this engine.
fn model_json(info: &EngineInfo, id: &str) -> Value {
    json!({
        "id": id,
        "name": id,
        "api": "openai-completions",
        "provider": "oxe",
        "baseUrl": info.base_url,
        "reasoning": true,
        "input": ["text"],
        "contextWindow": info.n_ctx,
        "maxTokens": info.max_output,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
    })
}

fn models(cx: &RpcCx) -> Vec<Value> {
    let info = &cx.session.inner().info;
    info.models.iter().map(|m| model_json(info, m)).collect()
}

fn set_model(cx: &mut RpcCx, cmd: &RpcCommand) -> Result<Value, OxaError> {
    let id = str_field(cmd, "modelId")?.to_string();
    if !cx.session.inner().info.models.contains(&id) {
        return Err(OxaError::new(codes::NOT_FOUND, format!("Model not found: {id}")));
    }
    let mut sink = OutSink(cx.out.clone());
    cx.session.set_model(&id, &mut sink)?;
    Ok(model_json(&cx.session.inner().info, &id))
}

/// Next model of the engine's list (wrapping). `Null` when there is only one model.
fn cycle_model(cx: &mut RpcCx) -> Result<Value, OxaError> {
    let (ids, current) = {
        let st = cx.session.inner();
        (st.info.models.clone(), st.model.clone())
    };
    if ids.len() < 2 {
        return Ok(Value::Null);
    }
    let next = ids
        .iter()
        .position(|m| *m == current)
        .map_or(0, |i| (i + 1) % ids.len());
    let id = ids[next].clone();
    let mut sink = OutSink(cx.out.clone());
    cx.session.set_model(&id, &mut sink)?;
    let st = cx.session.inner();
    Ok(json!({
        "model": model_json(&st.info, &id),
        "thinkingLevel": st.thinking.as_str(),
        "isScoped": false,
    }))
}

fn set_thinking_level(cx: &mut RpcCx, cmd: &RpcCommand) -> Result<Value, OxaError> {
    let name = str_field(cmd, "level")?;
    let level = ThinkingLevel::parse(name)
        .filter(|l| SUPPORTED_THINKING.contains(l))
        .ok_or_else(|| OxaError::new(codes::INVALID, format!("Invalid thinking level: {name}")))?;
    let mut sink = OutSink(cx.out.clone());
    cx.session.set_thinking(level, &mut sink);
    Ok(Value::Null)
}

fn cycle_thinking_level(cx: &mut RpcCx) -> Result<Value, OxaError> {
    let current = cx.session.inner().thinking;
    let next = SUPPORTED_THINKING
        .iter()
        .position(|l| *l == current)
        .map_or(0, |i| (i + 1) % SUPPORTED_THINKING.len());
    let level = SUPPORTED_THINKING[next];
    let mut sink = OutSink(cx.out.clone());
    cx.session.set_thinking(level, &mut sink);
    Ok(json!({"level": level.as_str()}))
}

fn queue_mode(name: &str) -> Result<QueueMode, OxaError> {
    match name {
        "all" => Ok(QueueMode::All),
        "one-at-a-time" => Ok(QueueMode::OneAtATime),
        _ => Err(OxaError::new(
            codes::INVALID,
            format!("Invalid queue mode: {name}"),
        )),
    }
}

fn set_queue_mode(cx: &mut RpcCx, cmd: &RpcCommand) -> Result<Value, OxaError> {
    let mode = queue_mode(str_field(cmd, "mode")?)?;
    let st = cx.session.state();
    if cmd.kind == "set_steering_mode" {
        cx.handle.set_modes(mode, st.follow_up_mode);
    } else {
        cx.handle.set_modes(st.steering_mode, mode);
    }
    Ok(Value::Null)
}

/// oxa addition: `{"type":"set_mode","mode":"plan"|"workspace"|"ask"}`. Appends a `ModeChange`.
fn set_mode(cx: &mut RpcCx, cmd: &RpcCommand) -> Result<Value, OxaError> {
    let name = str_field(cmd, "mode")?;
    let mode =
        Mode::parse(name).ok_or_else(|| OxaError::new(codes::INVALID, format!("Invalid mode: {name}")))?;
    let mut sink = OutSink(cx.out.clone());
    cx.session.set_mode(mode, &mut sink);
    Ok(Value::Null)
}
