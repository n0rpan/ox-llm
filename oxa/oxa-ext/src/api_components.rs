//! F2: custom UI components, overlays, the custom editor, and the pi-tui class shim. Owner: task F2. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/core/extensions/types.ts (ctx.ui.custom) and
// packages/tui/src/tui.ts (Component: render(width) -> string[], handleInput(data))
//
// Script side (JS glue evaluated by `install`, extends `ctx.ui` through `__oxa_ui_extend`):
//
// * `ctx.ui.custom(factory, {overlay?})`: `factory(tui, theme, keybindings, done)` (pi's order)
//   returns the component object
//   (`render(width)`, optional `handleInput(key, text)`), or a promise of it. `done(value)` closes
//   the component: the next key returns `{done: value}`. The glue sends the upcall
//   `{op: "custom", component: {render, handleInput}, overlay, asEditor}` and resolves with the
//   value the component finished with (`null` when dismissed or unavailable).
// * `ctx.ui.setEditorComponent(factory)`: `factory(tui, theme, keybindings)`, the same with
//   `asEditor: true`; it returns when the component is dismissed.
//
// A component that throws in `handleInput` is closed (the reply is an `ExtensionError` marker);
// the glue rethrows it from `ctx.ui.custom`, so the handler that opened the component fails with
// that error and the bus reports it like any other extension error. A throw in `render` shows
// one line with the error text and keeps the component open.
use crate::{ExtError, ExtId, ExtState};
use oxa_proto::{ComponentAction, CustomOpts, KeyId, Ui, UiComponent, Value};
use oxa_script::{CallbackPort, FnRef, Runtime};
use std::sync::Arc;

/// The virtual module `oxa:tui`: JavaScript classes with pi-tui's shape (`Text`, `Box`,
/// `Container`, `SelectList`, `Input`, `Spacer`, key matching helpers) written against
/// `render(width) -> string[]` / `handleInput(data)`. F2 writes `js/tui.js`.
pub const TUI_JS: &str = include_str!("../js/tui.js");

/// The JS glue: extends the `ctx.ui` object of every handler context with `custom` and
/// `setEditorComponent`. Chains to an earlier `__oxa_ui_extend` (another `ui` extension).
const GLUE: &str = r#"
(function () {
  var extendPrev = typeof __oxa_ui_extend === "function" ? __oxa_ui_extend : null;
  function runCustom(factory, opts, asEditor) {
    var st = { done: false, value: null };
    var done = function (v) { st.done = true; st.value = v === undefined ? null : v; };
    function show(comp) {
      if (!comp || typeof comp.render !== "function") {
        throw new Error("ui.custom: the factory must return an object with render(width)");
      }
      var component = {
        render: function (width) { return comp.render(width); },
        handleInput: function (key, text) {
          renderRequested = false;
          var r = typeof comp.handleInput === "function"
            ? comp.handleInput(key, text === undefined ? null : text)
            : undefined;
          if (st.done) { return { done: st.value }; }
          // pi's components ask for a redraw with tui.requestRender() instead of a result.
          if ((r === undefined || r === null) && renderRequested) { return true; }
          return r;
        }
      };
      var out = __oxa_ctx.upcall({
        op: "custom",
        component: component,
        overlay: !!(opts && opts.overlay),
        asEditor: !!asEditor
      });
      if (out instanceof Error) { throw out; }
      return out === undefined ? null : out;
    }
    // pi's argument order: factory(tui, theme, keybindings, done).
    var made = asEditor ? factory(tui, theme, keybindings) : factory(tui, theme, keybindings, done);
    if (made && typeof made.then === "function") { return made.then(show); }
    return Promise.resolve(show(made));
  }
  // The `tui`, `theme` and `keybindings` arguments of pi's factories. `tui.requestRender()`
  // during handleInput makes that key a redraw. Themes are the terminal's own: the theme
  // functions return the text unchanged.
  var renderRequested = false;
  var tui = { requestRender: function () { renderRequested = true; }, terminal: { columns: 80, rows: 24 } };
  function plain(a, b) { return b === undefined ? String(a) : String(b); }
  var theme = {
    fg: plain, bg: plain,
    bold: plain, italic: plain, underline: plain, dim: plain, strikethrough: plain, inverse: plain,
    getFgAnsi: function () { return ""; }, getBgAnsi: function () { return ""; }
  };
  var KEYS = {
    "tui.select.up": ["up"], "tui.select.down": ["down"],
    "tui.select.pageUp": ["pageup"], "tui.select.pageDown": ["pagedown"],
    "tui.select.confirm": ["enter"], "tui.select.cancel": ["escape", "ctrl+c"],
    "tui.input.submit": ["enter"], "tui.input.newLine": ["shift+enter"],
    "app.interrupt": ["escape"], "app.exit": ["ctrl+d"]
  };
  function norm(k) { return String(k === undefined || k === null ? "" : k).trim().toLowerCase(); }
  var keybindings = {
    getKeys: function (id) { return (KEYS[id] || []).slice(); },
    matches: function (data, id) {
      var d = norm(data);
      return (KEYS[id] || []).some(function (k) { return k === d; });
    }
  };
  globalThis.__oxa_ui_extend = function (ui) {
    if (extendPrev) { extendPrev(ui); }
    ui.theme = theme;
    ui.custom = function (factory, opts) { return runCustom(factory, opts, false); };
    ui.setEditorComponent = function (factory) { return runCustom(factory, {}, true); };
  };
})();
"#;

/// Bind on `rt`: `ctx.ui.custom(factory, {overlay?})` and `ctx.ui.setEditorComponent(factory)`.
/// Both resolve the factory to a component object and send the `custom` upcall with its
/// `render` / `handleInput` functions.
///
/// W0: a no-op so wave E can load extensions before F2 lands.
pub fn install(rt: &mut Runtime, st: &Arc<ExtState>, ext: ExtId) {
    if let Err(e) = rt.eval_script("oxa:components", GLUE) {
        st.push_error(ExtError {
            extension_path: format!("extension #{}", ext.0),
            event: "load".to_string(),
            error: e.to_string(),
        });
    }
}

/// Serve the `custom` upcall: wrap the JS component in a [`JsComponent`] and show it with
/// `ui.custom`. `None` = not one of mine.
///
/// W0: answers `None` so wave E works before F2 lands.
pub fn serve(request: &Value, port: &CallbackPort, ui: &mut dyn Ui) -> Option<Value> {
    let op = request.get("op")?.as_str()?;
    if op != crate::upcall::CUSTOM {
        return None;
    }
    let component = request.get("component");
    let Some(render) = component
        .and_then(|c| c.get("render"))
        .and_then(FnRef::from_value)
    else {
        return Some(error_marker("ui.custom: the component needs render(width)"));
    };
    let handle_input = component
        .and_then(|c| c.get("handleInput"))
        .and_then(FnRef::from_value);
    let opts = CustomOpts {
        overlay: request.get("overlay").and_then(Value::as_bool).unwrap_or(false),
        as_editor: request.get("asEditor").and_then(Value::as_bool).unwrap_or(false),
    };
    let comp = JsComponent::new(port.clone(), render, handle_input);
    Some(ui.custom(Box::new(comp), opts).unwrap_or(Value::Null))
}

/// The reply that closes a component with an error. The script sees it as an `Error` (name
/// `ExtensionError`) thrown from `ctx.ui.custom`.
fn error_marker(message: &str) -> Value {
    serde_json::json!({ "$error": { "name": "ExtensionError", "message": message } })
}

/// A script component as a `UiComponent`. It calls the script's `render` / `handleInput`
/// through the [`CallbackPort`] of the upcall that is being served, so it works exactly as long
/// as `ui.custom` has not returned.
pub struct JsComponent {
    pub(crate) port: CallbackPort,
    pub(crate) render: FnRef,
    pub(crate) handle_input: Option<FnRef>,
}

impl JsComponent {
    pub fn new(port: CallbackPort, render: FnRef, handle_input: Option<FnRef>) -> JsComponent {
        JsComponent {
            port,
            render,
            handle_input,
        }
    }
}

impl UiComponent for JsComponent {
    /// `render(width)` -> lines. A throwing component renders one line with the error text.
    /// Every line is cut to `width` columns (SGR sequences are kept).
    fn render(&mut self, width: u16) -> Vec<String> {
        match self.port.call(&self.render, &[Value::from(u64::from(width))]) {
            Ok(v) => lines_of(&v, width),
            Err(e) => vec![fit_width(&format!("[extension error] {}", e.message), width)],
        }
    }

    /// `handleInput(keyId, text)`; the script returns `undefined` (ignored), `true` (redraw)
    /// or `{done: value}`. Anything else is ignored. A throw closes the component with an
    /// `ExtensionError` (see the module docs).
    fn input(&mut self, key: &KeyId, text: Option<&str>) -> ComponentAction {
        let Some(handle) = self.handle_input else {
            return ComponentAction::Ignored;
        };
        let args = [
            Value::String(key.as_str().to_string()),
            text.map_or(Value::Null, |t| Value::String(t.to_string())),
        ];
        match self.port.call(&handle, &args) {
            Ok(v) => action_of(&v),
            Err(e) => ComponentAction::Done(error_marker(&format!(
                "component handleInput failed: {}",
                e.message
            ))),
        }
    }
}

/// What a `handleInput` result means (see [`JsComponent::input`]).
fn action_of(v: &Value) -> ComponentAction {
    match v {
        Value::Bool(true) => ComponentAction::Redraw,
        Value::Object(o) => match o.get("done") {
            Some(done) => ComponentAction::Done(done.clone()),
            None => ComponentAction::Ignored,
        },
        _ => ComponentAction::Ignored,
    }
}

/// The lines a `render` result stands for, each cut to `width`. An array of strings is taken
/// line by line; a string is split on newlines; `null` is no lines.
fn lines_of(v: &Value, width: u16) -> Vec<String> {
    let raw: Vec<String> = match v {
        Value::Null => Vec::new(),
        Value::String(s) => s.split('\n').map(str::to_string).collect(),
        Value::Array(items) => items
            .iter()
            .map(|i| match i {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        other => vec![other.to_string()],
    };
    raw.iter()
        .map(|line| fit_width(&line.replace('\t', "    "), width))
        .collect()
}

/// Cut `line` to at most `width` columns. CSI escape sequences cost nothing; a cut line keeps a
/// reset after it when it held any escape, so colours do not leak.
pub(crate) fn fit_width(line: &str, width: u16) -> String {
    let max = usize::from(width);
    let mut out = String::with_capacity(line.len());
    let mut cols = 0usize;
    let mut saw_escape = false;
    let mut cut = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            saw_escape = true;
            out.push(c);
            if chars.peek() == Some(&'[') {
                // CSI: parameters, then one final byte in 0x40..=0x7E.
                while let Some(&n) = chars.peek() {
                    chars.next();
                    out.push(n);
                    if n != '[' && ('\x40'..='\x7e').contains(&n) {
                        break;
                    }
                }
            }
            continue;
        }
        let w = char_width(c);
        if cols + w > max {
            cut = true;
            break;
        }
        out.push(c);
        cols += w;
    }
    if cut && saw_escape {
        out.push_str("\x1b[0m");
    }
    out
}

/// Columns of one character: 0 for controls and combining marks, 2 for East Asian wide and
/// emoji ranges, 1 otherwise. An approximation of wcwidth; the TUI shim uses the same table.
pub(crate) fn char_width(c: char) -> usize {
    let cp = c as u32;
    if cp < 0x20 || (0x7f..0xa0).contains(&cp) {
        return 0;
    }
    let zero = (0x300..=0x36f).contains(&cp)
        || (0x200b..=0x200f).contains(&cp)
        || (0xfe00..=0xfe0f).contains(&cp)
        || (0x20d0..=0x20ff).contains(&cp);
    if zero {
        return 0;
    }
    let wide = (0x1100..=0x115f).contains(&cp)
        || (0x2e80..=0xa4cf).contains(&cp)
        || (0xac00..=0xd7a3).contains(&cp)
        || (0xf900..=0xfaff).contains(&cp)
        || (0xfe30..=0xfe4f).contains(&cp)
        || (0xff00..=0xff60).contains(&cp)
        || (0xffe0..=0xffe6).contains(&cp)
        || (0x1f300..=0x1f64f).contains(&cp)
        || (0x1f900..=0x1f9ff).contains(&cp)
        || (0x20000..=0x3fffd).contains(&cp);
    if wide {
        2
    } else {
        1
    }
}
