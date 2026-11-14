//! F1: `ctx.ui` widgets (status, widget, footer, header, title, working message, autocomplete providers). Owner: task F1. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/core/extensions/types.ts (ExtensionUIContext)
//
// The setters are JS functions added to `ctx.ui` through `globalThis.__oxa_ui_extend(ui)`, which
// `api_ctx`'s `__oxa_make_ctx()` calls for every context. Each setter sends one upcall
// `{op, ...}` (see [`crate::upcall`]) that the caller's `Ui` answers in [`serve`]. Setters
// return at once, so the upcall is timed like `notify`. Headless front ends (print, json) answer
// `null` to every setter, so they are no-ops.
//
// `addAutocompleteProvider(fn)` registers `fn` in `Registry::completers`; [`complete`] calls
// every provider with the editor text before the cursor.
use crate::{ExtError, ExtId, ExtState, RegisteredCompleter};
use oxa_proto::{codes, OxaError, Placement, Ui, Value};
use oxa_script::{CallCx, FnRef, HostFn, Runtime, RuntimeThread, Slots};
use std::sync::Arc;

/// JS glue evaluated once per runtime. It defines `globalThis.__oxa_ui_extend(ui)`, which
/// `api_ctx`'s context builder calls with the `ui` object of each context.
const GLUE: &str = r#"
(function () {
  var upcall = __oxa_ui.upcall;
  function text(v) { return v === undefined || v === null ? null : String(v); }
  function lines(v) {
    if (v === undefined || v === null) { return null; }
    if (typeof v === "string") { return [v]; }
    return Array.from(v, function (x) { return String(x); });
  }
  globalThis.__oxa_ui_extend = function (ui) {
    ui.setStatus = function (key, value) {
      upcall({ op: "set_status", key: String(key), text: text(value) });
    };
    ui.setWidget = function (key, widgetLines, options) {
      var placement = options && options.placement === "belowEditor" ? "belowEditor" : "aboveEditor";
      upcall({ op: "set_widget", key: String(key), lines: lines(widgetLines), placement: placement });
    };
    ui.setFooter = function (footerLines) {
      upcall({ op: "set_footer", lines: lines(footerLines) });
    };
    ui.setHeader = function (headerLines) {
      upcall({ op: "set_header", lines: lines(headerLines) });
    };
    ui.setTitle = function (title) {
      upcall({ op: "set_title", title: String(title) });
    };
    ui.setWorkingMessage = function (value) {
      upcall({ op: "set_working_message", text: text(value) });
    };
    ui.setEditorText = function (value) {
      upcall({ op: "set_editor_text", text: String(value) });
    };
    ui.getEditorText = function () {
      var r = upcall({ op: "get_editor_text" });
      return r === null || r === undefined ? "" : String(r);
    };
    ui.addAutocompleteProvider = function (provider) {
      __oxa_ui.add_completer(provider);
    };
  };
})();
"#;

/// Bind on `rt`: `ctx.ui.setStatus(key, text)`, `setWidget(key, lines, {placement})`,
/// `setFooter(lines)`, `setHeader(lines)`, `setTitle(title)`, `setWorkingMessage(text)`,
/// `setEditorText(text)`, `getEditorText()`, `addAutocompleteProvider(fn)`. Each is an upcall
/// ([`crate::upcall`]) except the provider registration, which goes to
/// `Registry::completers`.
pub fn install(rt: &mut Runtime, st: &Arc<ExtState>, ext: ExtId) {
    rt.register(
        "__oxa_ui.upcall",
        hf(|cx, args| cx.upcall(args.first().cloned().unwrap_or(Value::Null))),
    );
    let s = st.clone();
    rt.register(
        "__oxa_ui.add_completer",
        hf(move |_cx, args| {
            let f = args.first().and_then(FnRef::from_value).ok_or_else(|| {
                OxaError::new(codes::INVALID, "the autocomplete provider must be a function")
            })?;
            let mut reg = s.registry.lock().unwrap_or_else(|p| p.into_inner());
            let slot = reg.extension(ext).and_then(|e| e.slot).ok_or_else(|| {
                OxaError::new(codes::INTERNAL, "the extension has no runtime to register on")
            })?;
            reg.completers.push(RegisteredCompleter { ext, slot, f });
            Ok(Value::from(f.0))
        }),
    );
    if let Err(e) = rt.eval_script("oxa:ui", GLUE) {
        st.push_error(ExtError {
            extension_path: format!("extension #{}", ext.0),
            event: "load".to_string(),
            error: format!("{}: {}", e.code, e.message),
        });
    }
}

/// Serve the widget upcalls with the caller's UI. `None` = not one of mine.
pub fn serve(request: &Value, ui: &mut dyn Ui) -> Option<Value> {
    let op = request.get("op")?.as_str()?;
    let answer = match op {
        crate::upcall::SET_STATUS => {
            let key = str_field(request, "key");
            ui.set_status(&key, opt_text(request.get("text")).as_deref());
            Value::Null
        }
        crate::upcall::SET_WIDGET => {
            let key = str_field(request, "key");
            let at = match request.get("placement").and_then(Value::as_str) {
                Some("belowEditor") => Placement::BelowEditor,
                _ => Placement::AboveEditor,
            };
            ui.set_widget(&key, opt_lines(request.get("lines")).as_deref(), at);
            Value::Null
        }
        crate::upcall::SET_FOOTER => {
            ui.set_footer(opt_lines(request.get("lines")).as_deref());
            Value::Null
        }
        crate::upcall::SET_HEADER => {
            ui.set_header(opt_lines(request.get("lines")).as_deref());
            Value::Null
        }
        crate::upcall::SET_TITLE => {
            ui.set_title(&str_field(request, "title"));
            Value::Null
        }
        crate::upcall::SET_WORKING_MESSAGE => {
            ui.set_working_message(opt_text(request.get("text")).as_deref());
            Value::Null
        }
        crate::upcall::SET_EDITOR_TEXT => {
            ui.set_editor_text(&str_field(request, "text"));
            Value::Null
        }
        crate::upcall::GET_EDITOR_TEXT => Value::String(ui.editor_text()),
        _ => return None,
    };
    Some(answer)
}

/// Ask every autocomplete provider for completions of `prefix` (the editor text before the
/// cursor), in registration order, concatenated; a throwing provider contributes nothing (its
/// error is recorded as an extension error). A provider answers an array of strings (a promise
/// is awaited); other items are ignored.
pub fn complete(st: &Arc<ExtState>, thread: &Arc<RuntimeThread>, prefix: &str) -> Vec<String> {
    // Snapshot the providers with their extension paths, then release the lock before any
    // script runs.
    let providers: Vec<(RegisteredCompleter, String)> = {
        let reg = st.registry.lock().unwrap_or_else(|p| p.into_inner());
        reg.completers
            .iter()
            .map(|c| {
                let path = reg.extension(c.ext).map(|e| e.path.clone()).unwrap_or_default();
                (*c, path)
            })
            .collect()
    };
    if providers.is_empty() {
        return Vec::new();
    }
    let prefix = prefix.to_string();
    let results = thread.run(
        move |slots: &mut Slots| {
            let mut results: Vec<(u64, String, Result<Value, OxaError>)> = Vec::new();
            for (c, path) in providers {
                let Some(rt) = slots.get(c.slot) else { continue };
                results.push((c.f.0, path, rt.call(&c.f, &[Value::String(prefix.clone())])));
            }
            results
        },
        &mut |_request, _port| Value::Null,
    );
    let Ok(results) = results else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (id, path, result) in results {
        match result {
            Ok(v) => {
                st.failing_completers
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                out.extend(
                    v.as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|x| x.as_str().map(str::to_string)),
                )
            }
            Err(e) => {
                // Completion runs on every keystroke: a provider that keeps throwing is
                // reported once, until it succeeds again.
                let first = st
                    .failing_completers
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(id);
                if first {
                    st.push_error(ExtError {
                        extension_path: path,
                        event: "autocomplete".to_string(),
                        error: format!("{}: {}", e.code, e.message),
                    });
                }
            }
        }
    }
    out
}

/// `hf`: a host function with the signature `Runtime::register` takes.
fn hf(f: impl Fn(&mut CallCx, &[Value]) -> Result<Value, OxaError> + Send + 'static) -> HostFn {
    Box::new(f)
}

/// A string field, empty when absent.
fn str_field(request: &Value, key: &str) -> String {
    request
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// A nullable text field: `null` or absent is `None`.
fn opt_text(v: Option<&Value>) -> Option<String> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(other.to_string()),
    }
}

/// A nullable list of lines: `null` or absent is `None`; a string is one line; array items are
/// written as text.
fn opt_lines(v: Option<&Value>) -> Option<Vec<String>> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(vec![s.clone()]),
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|x| x.as_str().map_or_else(|| x.to_string(), str::to_string))
                .collect(),
        ),
        Some(other) => Some(vec![other.to_string()]),
    }
}
