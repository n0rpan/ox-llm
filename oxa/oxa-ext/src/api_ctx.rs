//! E4: the `ctx.*` object handed to handlers, commands and tools.
// ported from pi packages/coding-agent/src/core/extensions/types.ts (ExtensionContext, ExtensionCommandContext)
//
// Host functions bound on each runtime (all synchronous from the script's side):
//
// * `oxa.exec(command, args?, opts?)` -> `HostAction::Exec` (`command` and `args` are joined into
//   one shell line, each argument quoted; `opts` = `{cwd?, timeout? (ms), net?}`). Returns
//   `{output, exit, timedOut}`.
// * `__oxa_ctx.query(op, payload)`: the session operations of `ctx` by name (see
//   [`action_from_js`]); reads answer at once, session-shape operations are applied at settle.
// * `__oxa_ctx.upcall(request)`: a dialog or a `ui` query, answered by the caller's UI through
//   [`serve`] (`CallCx::upcall`).
// * `__oxa_ctx.fs(op, path, data?)` (`read` / `write`) -> `ReadFile` / `WriteFile`;
//   `__oxa_ctx.fetch(url, init?)` -> `Fetch`; both pass the policy on the core side.
//
// `install` also evaluates the glue that defines `globalThis.__oxa_make_ctx()`: it returns a
// fresh `ctx` object for one handler call (the loader and the bus call it on the runtime thread).
use crate::{ExtError, ExtId, ExtState};
use oxa_proto::{codes, ExecSpec, Host, HostAction, Level, Mode, OxaError, Ui, Value};
use oxa_script::{CallCx, HostFn, Runtime};
use std::path::PathBuf;
use std::sync::Arc;

/// Upcall ops this module answers (in addition to the names in [`crate::upcall`]).
const OP_INTERACTIVE: &str = "interactive";

/// JS glue evaluated once per runtime; defines `__oxa_make_ctx`.
const GLUE: &str = r#"
(function () {
  var query = __oxa_ctx.query, upcall = __oxa_ctx.upcall, fsop = __oxa_ctx.fs, fetchop = __oxa_ctx.fetch;
  function answer(op, fields) {
    var r = upcall(Object.assign({ op: op }, fields));
    return r === null ? undefined : r;
  }
  function timeoutOf(opts) {
    if (!opts) { return undefined; }
    return opts.timeout !== undefined ? opts.timeout : opts.timeoutMs;
  }
  function branch() { return query("GetBranch", {}); }
  globalThis.__oxa_make_ctx = function () {
    var cwd = __oxa_ctx.cwd();
    // pi's dialogs answer through a promise; oxa's answer at once (the handler waits while the
    // dialog is open), so the value is returned directly and `await` works on it.
    var ui = {
      select: function (title, options, opts) {
        return answer("select", { title: title, options: options, timeoutMs: timeoutOf(opts) });
      },
      confirm: function (title, body, opts) {
        return answer("confirm", { title: title, body: body, timeoutMs: timeoutOf(opts) }) === true;
      },
      input: function (title, placeholder, opts) {
        return answer("input", { title: title, placeholder: placeholder || "", timeoutMs: timeoutOf(opts) });
      },
      editor: function (title, prefill) {
        return answer("editor", { title: title, prefill: prefill || "" });
      },
      notify: function (text, level) {
        upcall({ op: "notify", text: text, level: level || "info" });
      }
    };
    var sessionManager = {
      getCwd: function () { return cwd; },
      getSessionId: function () { return query("GetState", {}).sessionId; },
      getSessionName: function () {
        var n = query("GetSessionName", {});
        return n === null ? undefined : n;
      },
      getEntries: function () { return query("GetEntries", {}); },
      getBranch: function () { return branch(); },
      getEntry: function (id) {
        var all = query("GetEntries", {});
        for (var i = 0; i < all.length; i++) { if (all[i].id === id) { return all[i]; } }
        return undefined;
      },
      getLeafId: function () {
        var b = branch();
        return b.length ? b[b.length - 1].id : null;
      },
      getLeafEntry: function () {
        var b = branch();
        return b.length ? b[b.length - 1] : undefined;
      }
    };
    var session = {
      getState: function () { return query("GetState", {}); },
      getEntries: sessionManager.getEntries,
      getBranch: sessionManager.getBranch,
      getLeafId: sessionManager.getLeafId,
      getContextUsage: function () { return query("ContextUsage", {}); },
      isIdle: function () { return query("IsIdle", {}) === true; },
      getActiveTools: function () { return query("GetActiveTools", {}); },
      getAllTools: function () { return query("GetAllTools", {}); }
    };
    if (typeof __oxa_ui_extend === "function") { __oxa_ui_extend(ui); }
    return {
      ui: ui,
      cwd: cwd,
      sessionManager: sessionManager,
      session: session,
      signal: undefined,
      exec: function (command, args, opts) { return oxa.exec(command, args, opts); },
      get hasUI() { return upcall({ op: "interactive" }) === true; },
      get canPrompt() { return upcall({ op: "interactive" }) === true; },
      get mode() { return __oxa_ctx.mode(); },
      get thinkingLevel() { return query("GetThinkingLevel", {}); },
      get model() {
        var s = query("GetState", {});
        return s && s.model ? { id: s.model, provider: "oxe", contextWindow: s.contextWindow } : undefined;
      },
      executeTool: function (name, args) {
        // pi: never rejects for tool failures; they come back with isError.
        try {
          var r = query("CallTool", { name: name, args: args === undefined ? {} : args });
          return { content: [{ type: "text", text: r.text }], isError: !!r.isError };
        } catch (e) {
          return { content: [{ type: "text", text: String(e && e.message || e) }], isError: true };
        }
      },
      isIdle: function () { return query("IsIdle", {}) === true; },
      hasPendingMessages: function () { return query("GetState", {}).pendingMessageCount > 0; },
      abort: function () { query("Abort", {}); },
      shutdown: function () { query("Shutdown", {}); },
      getContextUsage: function () { return query("ContextUsage", {}); },
      getSystemPrompt: function () { return query("GetSystemPrompt", {}); },
      compact: function (opts) {
        if (opts && (typeof opts.onComplete === "function" || typeof opts.onError === "function")) {
          __oxa_ctx.compact_waiter(opts.onComplete || null, opts.onError || null);
        }
        query("Compact", { instructions: (opts && (opts.customInstructions || opts.instructions)) || null });
      },
      waitForIdle: function () { return query("WaitForIdle", {}); },
      newSession: function () { return query("NewSession", {}); },
      fork: function (entryId) { return query("Fork", { entry: entryId }); },
      navigateTree: function (target, opts) {
        return query("NavigateTree", { target: target, summarize: !!(opts && opts.summarize) });
      },
      switchSession: function (path) { return query("SwitchSession", { path: path }); },
      reload: function () { return query("Reload", {}); },
      fs: {
        readFile: function (path) { return fsop("read", path, null); },
        writeFile: function (path, data) { fsop("write", path, String(data)); }
      },
      fetch: function (url, init) { return fetchop(url, init || {}); }
    };
  };
})();
"#;

std::thread_local! {
    /// `true` while the runtime thread runs an event handler or a tool the core's run waits for
    /// (set by the job, see [`in_run_scope`]).
    static RUN_SCOPE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` on the runtime thread marked as an event handler or tool the core's run waits for
/// (`on = true`) or not (commands, shortcuts). Inside such a call `ctx.waitForIdle()` could
/// never be answered (the run settles only after the call returned), so it is refused with
/// `busy` while the session is not idle (pi has `waitForIdle` on the command context only).
pub(crate) fn in_run_scope<R>(on: bool, f: impl FnOnce() -> R) -> R {
    let prev = RUN_SCOPE.with(|r| r.replace(on));
    let out = f();
    RUN_SCOPE.with(|r| r.set(prev));
    out
}

/// The error of a refused `waitForIdle` (same text as the core's).
fn wait_for_idle_refused() -> OxaError {
    OxaError::new(
        codes::BUSY,
        "waitForIdle called from a handler or tool that the current run is waiting for; \
         it can only be used from a command or shortcut handler or outside a run",
    )
}

/// Upcall ops that wait for the user (their open time is excluded from the handler deadline).
fn waits_for_user(op: &str) -> bool {
    matches!(
        op,
        crate::upcall::SELECT
            | crate::upcall::CONFIRM
            | crate::upcall::INPUT
            | crate::upcall::EDITOR
            | crate::upcall::CUSTOM
    )
}

fn hf(f: impl Fn(&mut CallCx, &[Value]) -> Result<Value, OxaError> + Send + 'static) -> HostFn {
    Box::new(f)
}

/// Bind on `rt` the host functions behind `ctx`, and the JS glue that builds a `ctx` object for
/// each handler call:
///
/// * `ctx.ui.{select, confirm, input, editor, notify}` -> upcalls ([`crate::upcall`]);
/// * `ctx.hasUI`, `ctx.cwd`, `ctx.sessionManager.{getEntries, getBranch, getLeafId, ..}` (read
///   API over `HostAction::{GetEntries, GetBranch, GetState}`), `ctx.getContextUsage()`,
///   `ctx.isIdle()`, `ctx.abort()`, `ctx.compact(opts)`, `ctx.shutdown()`;
/// * command contexts add `waitForIdle()`, `newSession()`, `fork(entryId)`,
///   `navigateTree(target, opts)`, `switchSession(path)`, `reload()` (the core host refuses
///   `newSession` / `switchSession` with `unsupported_in_oxa`);
/// * `oxa.exec(command, args, opts)` -> `HostAction::Exec`, `ctx.fs.{readFile, writeFile}` ->
///   `ReadFile` / `WriteFile`, `ctx.fetch(url, init)` -> `Fetch`. All pass `Policy`; a denial
///   rejects with an `Error` whose `code` is `denied`.
///
/// Session-shape actions requested during a run are applied when the run settles (the timing
/// rule on `HostAction`); the returned promise resolves then.
pub fn install(rt: &mut Runtime, st: &Arc<ExtState>, ext: ExtId) {
    // CT-13 / A2 (wave H lane `harden`): a host function that panics, for the binary-level
    // test of the release build (`oxa-cli/tests/h_panic_binary.rs`). Registered only when the
    // process runs under the test isolation marker AND names this site, so it is inert for
    // users: `OXA_TEST_ISOLATION=1 OXA_TEST_PANIC=host_function`.
    if test_panic_site() {
        rt.register(
            "__oxaTestPanic",
            hf(|_cx, _args| panic!("OXA_TEST_PANIC: injected host function panic")),
        );
    }
    let s = st.clone();
    rt.register(
        "__oxa_ctx.query",
        hf(move |_cx, args| {
            let op = str_arg(args, 0, "op")?;
            let payload = args.get(1).cloned().unwrap_or(Value::Null);
            let action = action_from_js(&op, &payload)?;
            let host = host_of(&s)?;
            if matches!(action, HostAction::WaitForIdle)
                && RUN_SCOPE.with(|r| r.get())
                && host.call(HostAction::IsIdle)? != Value::Bool(true)
            {
                return Err(wait_for_idle_refused());
            }
            host.call(action)
        }),
    );
    rt.register(
        "__oxa_ctx.upcall",
        hf(|cx, args| {
            let request = args.first().cloned().unwrap_or(Value::Null);
            // A dialog waits for the user: that time does not count against the handler's
            // deadline. `notify` and the setters return at once and are timed as usual.
            let op = request.get("op").and_then(Value::as_str).unwrap_or_default();
            let out = if waits_for_user(op) {
                cx.upcall_paused(request.clone())
            } else {
                cx.upcall(request.clone())
            };
            // Functions in the request (a custom component's `render` / `handleInput`) were
            // captured for this call only: release them now that it is over.
            for f in fn_refs(&request) {
                cx.release(&f);
            }
            out
        }),
    );
    let s = st.clone();
    rt.register(
        "__oxa_ctx.mode",
        hf(move |cx, _args| {
            let set = s.mode.lock().unwrap_or_else(|p| p.into_inner()).clone();
            if let Some(mode) = set {
                return Ok(Value::String(mode));
            }
            let interactive = cx.upcall(serde_json::json!({ "op": OP_INTERACTIVE }))? == Value::Bool(true);
            Ok(Value::String(
                if interactive { "tui" } else { "print" }.to_string(),
            ))
        }),
    );
    let s = st.clone();
    rt.register(
        "__oxa_ctx.compact_waiter",
        hf(move |_cx, args| {
            let on_complete = args.first().and_then(oxa_script::FnRef::from_value);
            let on_error = args.get(1).and_then(oxa_script::FnRef::from_value);
            let slot = {
                let reg = s.registry.lock().unwrap_or_else(|p| p.into_inner());
                reg.extension(ext).and_then(|e| e.slot)
            };
            let Some(slot) = slot else {
                return Err(OxaError::new(codes::INTERNAL, "the extension has no runtime"));
            };
            s.compact_waiters
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(crate::CompactWaiter {
                    ext,
                    slot,
                    on_complete,
                    on_error,
                });
            Ok(Value::Null)
        }),
    );
    let s = st.clone();
    rt.register(
        "__oxa_ctx.cwd",
        hf(move |_cx, _args| {
            let cwd = s.cwd.lock().unwrap_or_else(|p| p.into_inner()).clone();
            Ok(Value::String(cwd.display().to_string()))
        }),
    );
    let s = st.clone();
    rt.register(
        "__oxa_ctx.fs",
        hf(move |_cx, args| {
            let op = str_arg(args, 0, "op")?;
            let path = resolve_path(&s, &str_arg(args, 1, "path")?);
            let host = host_of(&s)?;
            match op.as_str() {
                "read" => host.call(HostAction::ReadFile(path)),
                "write" => {
                    let data = args
                        .get(2)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .as_bytes()
                        .to_vec();
                    host.call(HostAction::WriteFile { path, data })
                        .map(|_| Value::Null)
                }
                other => Err(OxaError::new(
                    codes::INVALID,
                    format!("unknown fs operation {other}"),
                )),
            }
        }),
    );
    let s = st.clone();
    rt.register(
        "__oxa_ctx.fetch",
        hf(move |_cx, args| {
            let url = str_arg(args, 0, "url")?;
            let init = args.get(1).cloned().unwrap_or(Value::Null);
            let method = init
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("GET")
                .to_ascii_uppercase();
            let headers = init
                .get("headers")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| {
                            (
                                k.clone(),
                                v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let body = init
                .get("body")
                .and_then(Value::as_str)
                .map(|b| b.as_bytes().to_vec());
            host_of(&s)?.call(HostAction::Fetch {
                url,
                method,
                headers,
                body,
            })
        }),
    );
    let s = st.clone();
    rt.register(
        "oxa.exec",
        hf(move |_cx, args| {
            let command = str_arg(args, 0, "command")?;
            let argv: Vec<String> = match args.get(1) {
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
                    .collect(),
                _ => Vec::new(),
            };
            let opts = args.get(2).cloned().unwrap_or(Value::Null);
            let mut line = command;
            for a in &argv {
                line.push(' ');
                line.push_str(&shell_word(a));
            }
            let cwd = match opts.get("cwd").and_then(Value::as_str) {
                Some(c) => resolve_path(&s, c),
                None => s.cwd.lock().unwrap_or_else(|p| p.into_inner()).clone(),
            };
            let spec = ExecSpec {
                command: line,
                cwd,
                writable: true,
                net: opts.get("net").and_then(Value::as_bool).unwrap_or(false),
                timeout_s: opts
                    .get("timeout")
                    .and_then(Value::as_u64)
                    .map(|ms| u32::try_from(ms.div_ceil(1000)).unwrap_or(u32::MAX)),
                env: Vec::new(),
            };
            let mut out = host_of(&s)?.call(HostAction::Exec(spec))?;
            // pi's `ExecResult` beside the core's shape: `stdout` (the output; oxa interleaves
            // stderr into it, so `stderr` is empty), `code` (the exit status) and `killed`.
            if let Some(m) = out.as_object_mut() {
                let output = m.get("output").cloned().unwrap_or(Value::String(String::new()));
                let exit = m.get("exit").cloned().unwrap_or(Value::Null);
                let timed_out = m.get("timedOut").cloned().unwrap_or(Value::Bool(false));
                m.insert("stdout".to_string(), output);
                m.insert("stderr".to_string(), Value::String(String::new()));
                m.insert("code".to_string(), exit);
                m.insert("killed".to_string(), timed_out);
            }
            Ok(out)
        }),
    );

    // The glue that builds `ctx`. A failure is an extension error, not a panic.
    if let Err(e) = rt.eval_script("oxa:ctx", GLUE) {
        st.push_error(ExtError {
            extension_path: format!("extension #{}", ext.0),
            event: "load".to_string(),
            error: e.to_string(),
        });
    }
}

/// Serve the dialog upcalls (`select`, `confirm`, `input`, `editor`, `notify`) with the
/// caller's UI. `None` = not one of mine. A dismissed or unavailable dialog answers
/// `Some(Value::Null)`.
pub fn serve(request: &Value, ui: &mut dyn Ui) -> Option<Value> {
    let op = request.get("op")?.as_str()?;
    let title = request
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let timeout = request
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .and_then(|t| u32::try_from(t).ok());
    let answer = match op {
        crate::upcall::SELECT => {
            let options = strings(request.get("options"));
            // pi: the chosen option string; `null` (undefined in the script) when dismissed.
            match ui.select(&title, &options, timeout) {
                Some(i) => options.get(i).cloned().map_or(Value::Null, Value::String),
                None => Value::Null,
            }
        }
        crate::upcall::CONFIRM => {
            let body = request.get("body").and_then(Value::as_str).unwrap_or("");
            match ui.confirm(&title, body, timeout) {
                Some(b) => Value::Bool(b),
                None => Value::Null,
            }
        }
        crate::upcall::INPUT => {
            let placeholder = request.get("placeholder").and_then(Value::as_str).unwrap_or("");
            ui.input(&title, placeholder).map_or(Value::Null, Value::String)
        }
        crate::upcall::EDITOR => {
            let prefill = request.get("prefill").and_then(Value::as_str).unwrap_or("");
            ui.editor(&title, prefill).map_or(Value::Null, Value::String)
        }
        crate::upcall::NOTIFY => {
            let text = request.get("text").and_then(Value::as_str).unwrap_or("");
            let level = match request.get("level").and_then(Value::as_str) {
                Some("warning") => Level::Warning,
                Some("error") => Level::Error,
                _ => Level::Info,
            };
            ui.notify(text, level);
            Value::Null
        }
        OP_INTERACTIVE => Value::Bool(ui.interactive()),
        _ => return None,
    };
    Some(answer)
}

/// Every function reference (`{"$fn": id}`) inside `v`.
fn fn_refs(v: &Value) -> Vec<oxa_script::FnRef> {
    let mut out = Vec::new();
    let mut stack = vec![v];
    while let Some(v) = stack.pop() {
        if let Some(f) = oxa_script::FnRef::from_value(v) {
            out.push(f);
            continue;
        }
        match v {
            Value::Array(items) => stack.extend(items.iter()),
            Value::Object(m) => stack.extend(m.values()),
            _ => {}
        }
    }
    out
}

/// The `HostAction` of one `ctx` session operation (`op` is the `HostAction` variant name).
/// Unknown names are `invalid`.
pub fn action_from_js(op: &str, p: &Value) -> Result<HostAction, OxaError> {
    let text = |key: &str| p.get(key).and_then(Value::as_str).map(str::to_string);
    let required = |key: &str| {
        text(key).ok_or_else(|| OxaError::new(codes::INVALID, format!("{op} needs `{key}` as a string")))
    };
    Ok(match op {
        "IsIdle" => HostAction::IsIdle,
        "Abort" => HostAction::Abort,
        "Shutdown" => HostAction::Shutdown,
        "WaitForIdle" => HostAction::WaitForIdle,
        "GetEntries" => HostAction::GetEntries,
        "GetBranch" => HostAction::GetBranch,
        "GetState" => HostAction::GetState,
        "GetActiveTools" => HostAction::GetActiveTools,
        "GetAllTools" => HostAction::GetAllTools,
        "ContextUsage" => HostAction::ContextUsage,
        "Compact" => HostAction::Compact {
            instructions: text("instructions"),
        },
        "NewSession" => HostAction::NewSession,
        "Fork" => HostAction::Fork {
            entry: required("entry")?,
        },
        "NavigateTree" => HostAction::NavigateTree {
            target: required("target")?,
            summarize: p.get("summarize").and_then(Value::as_bool).unwrap_or(false),
        },
        "SwitchSession" => HostAction::SwitchSession {
            path: required("path")?,
        },
        "Reload" => HostAction::Reload,
        "SetMode" => HostAction::SetMode(parse_mode(&required("mode")?)?),
        "GetSessionName" => HostAction::GetSessionName,
        "GetThinkingLevel" => HostAction::GetThinkingLevel,
        "GetSettings" => HostAction::GetSettings,
        "GetSystemPrompt" => HostAction::GetSystemPrompt,
        "GetCommands" => HostAction::GetCommands,
        "CallTool" => HostAction::CallTool {
            name: required("name")?,
            args: p.get("args").cloned().unwrap_or(Value::Null),
        },
        other => {
            return Err(OxaError::new(
                codes::INVALID,
                format!("unknown host operation {other}"),
            ))
        }
    })
}

fn parse_mode(s: &str) -> Result<Mode, OxaError> {
    Mode::parse(s).ok_or_else(|| OxaError::new(codes::INVALID, format!("unknown mode {s}")))
}

fn host_of(st: &ExtState) -> Result<Arc<dyn Host>, OxaError> {
    st.host()
        .ok_or_else(|| OxaError::new(codes::BUSY, "no session is attached to the extensions yet"))
}

fn str_arg(args: &[Value], i: usize, name: &str) -> Result<String, OxaError> {
    args.get(i)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| OxaError::new(codes::INVALID, format!("`{name}` must be a string")))
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// A path as the script wrote it, made absolute against the session directory.
fn resolve_path(st: &ExtState, path: &str) -> PathBuf {
    let cwd = st.cwd.lock().unwrap_or_else(|p| p.into_inner()).clone();
    cwd.join(path)
}

/// POSIX single-quoted word.
fn shell_word(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The test marker of the process (see `install`): both variables, exactly.
fn test_panic_site() -> bool {
    std::env::var(oxa_proto::names::ENV_TEST_ISOLATION).is_ok_and(|v| v == "1")
        && std::env::var(TEST_PANIC_ENV).is_ok_and(|v| v == "host_function")
}

/// CT-13: the site a test asks to panic (only with `OXA_TEST_ISOLATION=1`).
pub const TEST_PANIC_ENV: &str = "OXA_TEST_PANIC";
