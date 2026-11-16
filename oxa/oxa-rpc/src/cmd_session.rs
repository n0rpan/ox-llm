//! C11: RPC session commands.
// ported from pi packages/coding-agent/src/modes/rpc/rpc-mode.ts (docs: rpc-commands.md)
//
// Every handler runs on the session thread (`serve`), so it may use the `Session` directly.
// Records that a response is built from (the compaction result, the bash entry) are read back
// from the events the session emits: the sink below writes each event as a record and keeps
// the few it needs.
use crate::events::{entry_to_json, event_to_json};
use crate::server::write_out;
use crate::{Handled, Out, RpcCommand, RpcCx, RpcUi};
use oxa_core::{Open, SessionState};
use oxa_export::{default_output_name, export_html, write_export, ExportOpts, ExportTheme};
use oxa_proto::{codes, AgentEvent, Entry, EntryBody, EventSink, OxaError, Role, Value};
use serde_json::{json, Map};
use std::collections::HashMap;
use std::path::PathBuf;

/// The text a `bash` response reports as truncated (pi's notice, appended by `user_bash`).
const TRUNCATED_MARK: &str = "\n\n[Output truncated";

/// Commands: `compact`, `bash`, `abort_bash`, `get_session_stats`, `export_html`,
/// `switch_session`, `fork`, `clone`, `get_fork_messages`, `get_entries`, `get_tree`,
/// `get_last_assistant_text`, `set_session_name`, `get_commands`. Request, response and
/// error shapes are the examples of rpc-commands.md. Returns `None` for any other command.
pub fn handle(cx: &mut RpcCx, cmd: &RpcCommand) -> Handled {
    let r = match cmd.kind.as_str() {
        "compact" => compact(cx, cmd),
        "bash" => bash(cx, cmd),
        "abort_bash" => {
            // Cancels a running `!cmd` through the shared cancel token; a no-op when idle.
            cx.handle.abort();
            Ok(Value::Null)
        }
        "get_session_stats" => Ok(session_stats(cx)),
        "export_html" => export(cx, cmd),
        "switch_session" => switch(cx, cmd),
        "fork" => fork(cx, cmd),
        "clone" => clone(cx),
        "get_fork_messages" => Ok(json!({"messages": fork_messages(cx.session.inner())})),
        "get_entries" => get_entries(cx, cmd),
        "get_tree" => {
            let st = cx.session.inner();
            Ok(json!({"tree": tree(st), "leafId": leaf_json(st)}))
        }
        "get_last_assistant_text" => Ok(json!({"text": last_assistant_text(cx.session.inner())})),
        "set_session_name" => set_name(cx, cmd),
        "get_commands" => Ok(json!({"commands": commands(cx)})),
        _ => return None,
    };
    Some(r)
}

/// Writes the session's events as records and keeps the ones a response is built from.
struct Tap {
    out: Out,
    /// The request id of a `bash` command: stamped on its `bash_execution_update` records.
    bash_id: Option<String>,
    /// The last `compaction_end` event.
    compaction: Option<AgentEvent>,
    /// Every entry appended while the command ran, in order.
    appended: Vec<Entry>,
}

impl EventSink for Tap {
    fn emit(&mut self, e: &AgentEvent) {
        match e {
            AgentEvent::CompactionEnd { .. } => self.compaction = Some(e.clone()),
            AgentEvent::EntryAppended { entry } => self.appended.push(entry.clone()),
            _ => {}
        }
        let record = match e {
            AgentEvent::BashExecutionUpdate { id: None, delta } if self.bash_id.is_some() => {
                event_to_json(&AgentEvent::BashExecutionUpdate {
                    id: self.bash_id.clone(),
                    delta: delta.clone(),
                })
            }
            _ if !crate::events::on_wire(e) => return,
            _ => event_to_json(e),
        };
        write_out(&self.out, &record);
    }
}

fn tap(cx: &RpcCx, bash_id: Option<String>) -> Tap {
    Tap {
        out: cx.out.clone(),
        bash_id,
        compaction: None,
        appended: Vec::new(),
    }
}

fn rpc_ui(cx: &RpcCx) -> RpcUi {
    RpcUi {
        out: cx.out.clone(),
        router: cx.ui.clone(),
    }
}

fn str_arg<'a>(cmd: &'a RpcCommand, key: &str) -> Result<&'a str, OxaError> {
    cmd.body
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| OxaError::new(codes::INVALID, format!("{} needs a string \"{key}\"", cmd.kind)))
}

fn opt_str<'a>(cmd: &'a RpcCommand, key: &str) -> Option<&'a str> {
    cmd.body.get(key).and_then(Value::as_str)
}

/// `compact`: a manual compaction. The response is the compaction's result object (pi's
/// `CompactionResult`, as the `compaction_end` record carries it).
fn compact(cx: &mut RpcCx, cmd: &RpcCommand) -> Result<Value, OxaError> {
    let instructions = opt_str(cmd, "customInstructions").map(str::to_string);
    let mut sink = tap(cx, None);
    let mut ui = rpc_ui(cx);
    cx.session.compact(instructions.as_deref(), &mut sink, &mut ui)?;
    // ENG-002: input sent during the compaction's idle prefill cancelled it and waits in the
    // queue; it runs now (`Session::run_queued`, as the interactive mode does after `/compact`).
    let _ = cx.session.run_queued(&mut sink, &mut ui);
    match &sink.compaction {
        Some(AgentEvent::CompactionEnd { aborted: true, .. }) => {
            Err(OxaError::new(codes::ABORTED, "compaction was cancelled"))
        }
        Some(AgentEvent::CompactionEnd {
            error_message: Some(msg),
            ..
        }) => Err(OxaError::new(codes::INTERNAL, msg.clone())),
        Some(end @ AgentEvent::CompactionEnd { .. }) => {
            Ok(event_to_json(end).get("result").cloned().unwrap_or(Value::Null))
        }
        _ => Err(OxaError::new(codes::INVALID, "nothing to compact")),
    }
}

/// `bash`: run a `!cmd` (`excludeFromContext` = `!!cmd`). `bash_execution_update` records carry
/// the request id. The response is pi's `BashResult`; the output is the entry's output without
/// pi's truncation notice, which is reported as `truncated` and `fullOutputPath`.
fn bash(cx: &mut RpcCx, cmd: &RpcCommand) -> Result<Value, OxaError> {
    let command = str_arg(cmd, "command")?.to_string();
    let excluded = cmd
        .body
        .get("excludeFromContext")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut sink = tap(cx, cmd.id.clone());
    let mut ui = rpc_ui(cx);
    cx.session.user_bash(&command, excluded, &mut sink, &mut ui)?;
    let entry_body = sink.appended.iter().rev().find_map(|e| match &e.body {
        EntryBody::BashExecution { output, exit, .. } => Some((output.clone(), *exit)),
        _ => None,
    });
    let Some((stored, exit)) = entry_body else {
        return Err(OxaError::new(codes::INTERNAL, "the bash result was not recorded"));
    };
    let (output, truncated, full_path) = split_truncation(&stored);
    let mut data = json!({
        "output": output,
        "exitCode": exit,
        "cancelled": exit.is_none(),
        "truncated": truncated,
    });
    if let (Some(path), Value::Object(m)) = (full_path, &mut data) {
        m.insert("fullOutputPath".to_string(), Value::from(path));
    }
    Ok(data)
}

/// Splits the truncation notice that `user_bash` appends: `(output, truncated, full path)`.
fn split_truncation(stored: &str) -> (String, bool, Option<String>) {
    let Some(at) = stored.rfind(TRUNCATED_MARK) else {
        return (stored.to_string(), false, None);
    };
    let notice = &stored[at + 2..];
    if !notice.ends_with(']') {
        return (stored.to_string(), false, None);
    }
    let path = notice
        .strip_prefix("[Output truncated. Full output: ")
        .and_then(|p| p.strip_suffix(']'))
        .map(str::to_string);
    (stored[..at].to_string(), true, path)
}

/// `get_session_stats`: pi's field names. Cost is zero (local engine). `contextUsage` is
/// omitted when the engine reports no context window.
fn session_stats(cx: &RpcCx) -> Value {
    let stats = cx.session.stats();
    let state = cx.session.state();
    let input = stats.prompt_tokens.saturating_sub(stats.cached_tokens);
    let total = input + stats.completion_tokens + stats.cached_tokens;
    let mut m = Map::new();
    m.insert(
        "sessionFile".to_string(),
        state
            .session_file
            .as_ref()
            .map_or(Value::Null, |p| Value::from(p.to_string_lossy().into_owned())),
    );
    m.insert("sessionId".to_string(), Value::from(state.session_id.as_str()));
    m.insert("userMessages".to_string(), Value::from(stats.user_messages));
    m.insert(
        "assistantMessages".to_string(),
        Value::from(stats.assistant_messages),
    );
    m.insert("toolCalls".to_string(), Value::from(stats.tool_calls));
    m.insert("toolResults".to_string(), Value::from(stats.tool_results));
    m.insert("toolRepairs".to_string(), Value::from(stats.tool_repairs));
    m.insert(
        "totalMessages".to_string(),
        Value::from(stats.user_messages + stats.assistant_messages + stats.tool_results),
    );
    m.insert(
        "tokens".to_string(),
        json!({
            "input": input,
            "output": stats.completion_tokens,
            "cacheRead": stats.cached_tokens,
            "cacheWrite": 0,
            "total": total,
        }),
    );
    m.insert("cost".to_string(), Value::from(0));
    let n_ctx = u64::from(state.n_ctx);
    if n_ctx > 0 {
        let tokens = u64::from(stats.context_tokens);
        m.insert(
            "contextUsage".to_string(),
            json!({
                "tokens": tokens,
                "contextWindow": n_ctx,
                "percent": tokens * 100 / n_ctx,
            }),
        );
    }
    Value::Object(m)
}

/// `export_html`: write the session as one HTML file. `outputPath` is relative to the session
/// cwd when given as a relative path; the default name is pi's (`default_output_name`).
fn export(cx: &mut RpcCx, cmd: &RpcCommand) -> Result<Value, OxaError> {
    let requested = opt_str(cmd, "outputPath").map(PathBuf::from);
    let (header, entries, opts, default_path) = {
        let st = cx.session.inner();
        let header = st.store.header(&st.sref)?;
        let entries = st.store.load(&st.sref)?;
        let state = cx.session.state();
        let session_file = state.session_file.clone();
        let default_path = default_output_name(session_file.as_deref(), &st.sref.id, &st.cwd);
        let opts = ExportOpts {
            theme: ExportTheme::dark(),
            session_name: state.session_name.clone(),
            system_prompt: Some(st.prefix.system_text()),
            tools: serde_json::from_str::<Value>(&st.prefix.tools_json).unwrap_or(Value::Null),
            rendered_tools: Vec::new(),
            leaf: (!st.tree.leaf().is_empty()).then(|| st.tree.leaf().to_string()),
        };
        (header, entries, opts, default_path)
    };
    let html = export_html(&header, &entries, &opts)?;
    let path = match requested {
        Some(p) if p.is_absolute() => p,
        Some(p) => cx.session.inner().cwd.join(p),
        None => default_path,
    };
    let written = write_export(&path, &html)?;
    Ok(json!({"path": written.to_string_lossy()}))
}

/// `switch_session`: resume another session file (`sessionPath`, a path or an id).
fn switch(cx: &mut RpcCx, cmd: &RpcCommand) -> Result<Value, OxaError> {
    let path = str_arg(cmd, "sessionPath")?;
    let target = cx.session.inner().store.resolve(path)?;
    let mut sink = tap(cx, None);
    let mut ui = rpc_ui(cx);
    match cx.session.switch(Open::Resume(target), &mut sink, &mut ui) {
        Ok(()) => Ok(json!({"cancelled": false})),
        Err(e) if e.code == codes::ABORTED => Ok(json!({"cancelled": true})),
        Err(e) => Err(e),
    }
}

/// `fork`: a new session from the branch that ends at a user message (`entryId`). The response
/// carries that message's text, so the client can refill its editor.
fn fork(cx: &mut RpcCx, cmd: &RpcCommand) -> Result<Value, OxaError> {
    let entry = str_arg(cmd, "entryId")?;
    let text = user_text(cx.session.inner(), entry).ok_or_else(|| {
        OxaError::new(codes::NOT_FOUND, "Invalid entry ID for forking")
            .with_hint("use an id from get_fork_messages")
    })?;
    match cx.session.fork(entry) {
        Ok(_) => Ok(json!({"text": text, "cancelled": false})),
        Err(e) if e.code == codes::ABORTED => Ok(json!({"cancelled": true})),
        Err(e) => Err(e),
    }
}

/// `clone`: a new session with the active branch, at the current position.
fn clone(cx: &mut RpcCx) -> Result<Value, OxaError> {
    if cx.session.inner().tree.leaf().is_empty() {
        return Err(OxaError::new(
            codes::INVALID,
            "Cannot clone session: no current entry selected",
        ));
    }
    match cx.session.clone_session() {
        Ok(_) => Ok(json!({"cancelled": false})),
        Err(e) if e.code == codes::ABORTED => Ok(json!({"cancelled": true})),
        Err(e) => Err(e),
    }
}

/// The text of a user message entry, `None` for any other entry.
fn user_text(st: &SessionState, id: &str) -> Option<String> {
    match &st.tree.get(id)?.body {
        EntryBody::Msg { wire, .. } if wire.role == Role::User => {
            Some(wire.content.clone().unwrap_or_default())
        }
        _ => None,
    }
}

/// `get_fork_messages`: the user messages of the active branch, oldest first.
fn fork_messages(st: &SessionState) -> Vec<Value> {
    st.tree
        .branch(st.tree.leaf())
        .into_iter()
        .filter_map(|e| match &e.body {
            EntryBody::Msg { wire, .. } if wire.role == Role::User => Some(json!({
                "entryId": e.id,
                "text": wire.content.clone().unwrap_or_default(),
            })),
            _ => None,
        })
        .collect()
}

/// `get_entries`: every entry in append order (header excluded). With `since`, only the entries
/// after that id; an unknown id is an error.
fn get_entries(cx: &mut RpcCx, cmd: &RpcCommand) -> Result<Value, OxaError> {
    let st = cx.session.inner();
    let all = st.tree.entries();
    let skip = match cmd.body.get("since") {
        None | Some(Value::Null) => 0,
        Some(v) => {
            let since = v
                .as_str()
                .ok_or_else(|| OxaError::new(codes::INVALID, "get_entries: \"since\" must be a string"))?;
            all.iter()
                .position(|e| e.id == since)
                .map(|i| i + 1)
                .ok_or_else(|| OxaError::new(codes::NOT_FOUND, format!("Entry not found: {since}")))?
        }
    };
    let entries: Vec<Value> = all[skip..].iter().map(entry_to_json).collect();
    Ok(json!({"entries": entries, "leafId": leaf_json(st)}))
}

fn leaf_json(st: &SessionState) -> Value {
    if st.tree.leaf().is_empty() {
        Value::Null
    } else {
        Value::from(st.tree.leaf())
    }
}

/// `get_tree`: the roots of the session tree; each node is `{entry, children}` plus `label` and
/// `labelTimestamp` when the entry has a label.
fn tree(st: &SessionState) -> Value {
    let mut children: HashMap<&str, Vec<&Entry>> = HashMap::new();
    for e in st.tree.entries() {
        if let Some(p) = e.parent.as_deref() {
            children.entry(p).or_default().push(e);
        }
    }
    let known: std::collections::HashSet<&str> = st.tree.entries().iter().map(|e| e.id.as_str()).collect();
    let roots: Vec<Value> = st
        .tree
        .entries()
        .iter()
        .filter(|e| e.parent.as_deref().is_none_or(|p| !known.contains(p)))
        .map(|e| node(st, e, &children))
        .collect();
    Value::Array(roots)
}

fn node(st: &SessionState, e: &Entry, children: &HashMap<&str, Vec<&Entry>>) -> Value {
    let kids: Vec<Value> = children
        .get(e.id.as_str())
        .map(|v| v.iter().map(|c| node(st, c, children)).collect())
        .unwrap_or_default();
    let mut m = Map::new();
    m.insert("entry".to_string(), entry_to_json(e));
    m.insert("children".to_string(), Value::Array(kids));
    if let Some(label) = st.tree.label(&e.id) {
        m.insert("label".to_string(), Value::from(label));
        let stamp = st
            .tree
            .entries()
            .iter()
            .rev()
            .find(|x| matches!(&x.body, EntryBody::Label { target, .. } if *target == e.id))
            .map(|x| entry_to_json(x)["timestamp"].clone())
            .unwrap_or(Value::Null);
        m.insert("labelTimestamp".to_string(), stamp);
    }
    Value::Object(m)
}

/// `get_last_assistant_text`: the text of the last assistant message of the active branch that
/// has any text; `null` when there is none.
fn last_assistant_text(st: &SessionState) -> Value {
    st.tree
        .branch(st.tree.leaf())
        .into_iter()
        .rev()
        .find_map(|e| match &e.body {
            EntryBody::Msg { wire, .. } if wire.role == Role::Assistant => {
                wire.content.clone().filter(|t| !t.is_empty())
            }
            _ => None,
        })
        .map_or(Value::Null, Value::from)
}

/// `set_session_name`: a trimmed, non-empty name. The name is an entry (`session_info`).
fn set_name(cx: &mut RpcCx, cmd: &RpcCommand) -> Result<Value, OxaError> {
    let name = str_arg(cmd, "name")?.trim().to_string();
    if name.is_empty() {
        return Err(OxaError::new(codes::INVALID, "Session name cannot be empty"));
    }
    let mut sink = tap(cx, None);
    cx.session.set_name(&name, &mut sink)?;
    Ok(Value::Null)
}

/// `get_commands`: the slash commands invocable through `prompt` (built-in TUI commands are not
/// listed, as in pi). `source` is `extension` (hook command), `prompt` (template or MCP prompt)
/// or `skill` (`skill:` prefix).
fn commands(cx: &RpcCx) -> Vec<Value> {
    use oxa_proto::Source;
    cx.session
        .commands()
        .into_iter()
        .filter(|c| c.source != Source::Builtin)
        .map(|c| {
            let (source, path) = match &c.source {
                Source::Extension(p) if c.name.starts_with("skill:") => ("skill", Some(p.clone())),
                Source::Extension(p) if p.ends_with(".md") => ("prompt", Some(p.clone())),
                Source::Extension(p) => ("extension", Some(p.clone())),
                Source::Mcp(_) => ("prompt", None),
                Source::Builtin => ("extension", None),
            };
            let mut m = Map::new();
            m.insert("name".to_string(), Value::from(c.name.as_str()));
            m.insert("description".to_string(), Value::from(c.description.as_str()));
            m.insert("source".to_string(), Value::from(source));
            if let Some(hint) = &c.argument_hint {
                m.insert("argumentHint".to_string(), Value::from(hint.as_str()));
            }
            if let Some(p) = path {
                m.insert(
                    "sourceInfo".to_string(),
                    json!({"path": p, "source": "local", "origin": "top-level"}),
                );
            }
            Value::Object(m)
        })
        .collect()
}
