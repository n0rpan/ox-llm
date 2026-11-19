//! F8: the extension-UI sub-protocol.
// ported from pi packages/coding-agent/docs/rpc-extension-ui.md and src/modes/rpc/rpc-mode.ts
use crate::server::write_out;
use crate::{Out, RpcCommand};
use oxa_proto::{Access, Answer, DangerAnswer, Level, Placement, Ui, Value};
use serde_json::{json, Map};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// Request ids of the extension-UI records. Process-wide and unique, so a late answer can never
/// match a dialog of another router.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn new_id() -> String {
    format!("ui-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Matches `extension_ui_response` commands (by `id`) to dialogs waiting on the session
/// thread. Shared between the reader thread and [`RpcUi`].
#[derive(Default)]
pub struct UiRouter {
    waiting: Mutex<Vec<(String, Sender<Value>)>>,
    /// Set at stdin EOF ([`UiRouter::close`]): no answer can arrive any more.
    closed: std::sync::atomic::AtomicBool,
}

impl UiRouter {
    pub fn new() -> UiRouter {
        UiRouter::default()
    }
    /// Deliver a response command. Returns false when `cmd` is not an `extension_ui_response`
    /// or no dialog waits for its id.
    pub fn deliver(&self, cmd: &RpcCommand) -> bool {
        if cmd.kind != "extension_ui_response" {
            return false;
        }
        let id = match cmd.body.get("id").and_then(Value::as_str) {
            Some(id) => id,
            None => return false,
        };
        let tx = {
            let mut waiting = lock(&self.waiting);
            match waiting.iter().position(|(w, _)| w == id) {
                Some(i) => waiting.remove(i).1,
                None => return false,
            }
        };
        // The whole response object goes to the dialog; it reads `value`, `confirmed` or `cancelled`.
        tx.send(cmd.body.clone()).is_ok()
    }
    /// Register a waiting dialog; returns its fresh request id and the receiver for the answer.
    /// After [`UiRouter::close`] the receiver is already disconnected: the dialog resolves as
    /// cancelled at once.
    pub fn register(&self) -> (String, Receiver<Value>) {
        let (tx, rx) = mpsc::channel();
        let id = new_id();
        let mut waiting = lock(&self.waiting);
        if !self.closed.load(Ordering::SeqCst) {
            waiting.push((id.clone(), tx));
        }
        (id, rx)
    }
    /// The input is gone (stdin EOF): every waiting dialog resolves as cancelled now, and every
    /// later one at once, so no run can block on an answer that will never come.
    pub fn close(&self) {
        let mut waiting = lock(&self.waiting);
        self.closed.store(true, Ordering::SeqCst);
        // Dropping the senders disconnects the receivers the dialogs wait on.
        waiting.clear();
    }
    /// Dismiss dialogs waiting now without closing the router to future dialogs.
    pub fn cancel_waiting(&self) {
        lock(&self.waiting).clear();
    }
    /// Drop a dialog that gave up (timeout): a late answer is then not delivered to anyone.
    pub(crate) fn forget(&self, id: &str) {
        lock(&self.waiting).retain(|(w, _)| w != id);
    }
}

/// `Ui` over RPC: every method emits an `extension_ui_request` record with pi's `method`
/// (`select`, `confirm`, `input`, `editor`, `notify`, `setStatus`, `setWidget`, `setTitle`,
/// `set_editor_text`); dialog methods block for the matching response (timeouts as pi:
/// `None`). `ask_access` is relayed as a `confirm` whose title names the access; "yes" =
/// `Answer::Once`, anything else = `Deny`.
pub struct RpcUi {
    pub out: Out,
    pub router: Arc<UiRouter>,
}

/// `{"type":"extension_ui_request","id":..,"method":..}`; the caller adds the fields.
fn request_base(id: String, method: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("type".to_string(), Value::from("extension_ui_request"));
    m.insert("id".to_string(), Value::from(id));
    m.insert("method".to_string(), Value::from(method));
    m
}

fn is_cancelled(v: &Value) -> bool {
    v.get("cancelled").and_then(Value::as_bool) == Some(true)
}

fn placement_name(at: Placement) -> &'static str {
    match at {
        Placement::AboveEditor => "aboveEditor",
        Placement::BelowEditor => "belowEditor",
    }
}

/// The access as the person reads it in the permission prompt.
fn describe(a: &Access) -> String {
    match a {
        Access::Read(p) => format!("read {}", p.display()),
        Access::Write(p) => format!("write {}", p.display()),
        Access::Exec { command, net: true } => format!("run `{command}` with network access"),
        Access::Exec { command, net: false } => format!("run `{command}`"),
        Access::Net { host } => format!("connect to {host}"),
        Access::Mcp { server, tool, .. } => format!("call the MCP tool {server}/{tool}"),
    }
}

impl RpcUi {
    /// A dialog: registers the id, writes the request (with `timeout` when given) and waits for
    /// the answer. `None` on timeout, when the router is gone, or when no answer comes.
    fn dialog(&mut self, method: &str, fields: Map<String, Value>, timeout_ms: Option<u32>) -> Option<Value> {
        let (id, rx) = self.router.register();
        let mut m = request_base(id.clone(), method);
        m.extend(fields);
        if let Some(ms) = timeout_ms {
            m.insert("timeout".to_string(), Value::from(ms));
        }
        write_out(&self.out, &Value::Object(m));
        let answer = match timeout_ms {
            Some(ms) => rx.recv_timeout(Duration::from_millis(u64::from(ms))),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match answer {
            Ok(v) => Some(v),
            Err(_) => {
                self.router.forget(&id);
                None
            }
        }
    }

    /// A fire-and-forget record: no id is awaited.
    fn fire(&self, method: &str, fields: Map<String, Value>) {
        let mut m = request_base(new_id(), method);
        m.extend(fields);
        write_out(&self.out, &Value::Object(m));
    }
}

impl Ui for RpcUi {
    fn select(&mut self, title: &str, options: &[String], timeout_ms: Option<u32>) -> Option<usize> {
        let mut f = Map::new();
        f.insert("title".to_string(), Value::from(title));
        f.insert("options".to_string(), json!(options));
        let v = self.dialog("select", f, timeout_ms)?;
        if is_cancelled(&v) {
            return None;
        }
        let picked = v.get("value")?.as_str()?;
        options.iter().position(|o| o == picked)
    }

    fn confirm(&mut self, title: &str, body: &str, timeout_ms: Option<u32>) -> Option<bool> {
        let mut f = Map::new();
        f.insert("title".to_string(), Value::from(title));
        f.insert("message".to_string(), Value::from(body));
        let v = self.dialog("confirm", f, timeout_ms)?;
        if is_cancelled(&v) {
            return None;
        }
        v.get("confirmed")?.as_bool()
    }

    fn dialog_canceller(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let router = self.router.clone();
        Some(Arc::new(move || router.cancel_waiting()))
    }

    fn input(&mut self, title: &str, placeholder: &str) -> Option<String> {
        let mut f = Map::new();
        f.insert("title".to_string(), Value::from(title));
        f.insert("placeholder".to_string(), Value::from(placeholder));
        let v = self.dialog("input", f, None)?;
        if is_cancelled(&v) {
            return None;
        }
        v.get("value")?.as_str().map(str::to_string)
    }

    fn editor(&mut self, title: &str, prefill: &str) -> Option<String> {
        let mut f = Map::new();
        f.insert("title".to_string(), Value::from(title));
        f.insert("prefill".to_string(), Value::from(prefill));
        let v = self.dialog("editor", f, None)?;
        if is_cancelled(&v) {
            return None;
        }
        v.get("value")?.as_str().map(str::to_string)
    }

    fn notify(&mut self, text: &str, level: Level) {
        let mut f = Map::new();
        f.insert("message".to_string(), Value::from(text));
        f.insert("notifyType".to_string(), Value::from(level.as_str()));
        self.fire("notify", f);
    }

    fn set_status(&mut self, key: &str, text: Option<&str>) {
        // Without `statusText` the key is cleared (pi sends `undefined`, which JSON omits).
        let mut f = Map::new();
        f.insert("statusKey".to_string(), Value::from(key));
        if let Some(t) = text {
            f.insert("statusText".to_string(), Value::from(t));
        }
        self.fire("setStatus", f);
    }

    fn set_widget(&mut self, key: &str, lines: Option<&[String]>, at: Placement) {
        let mut f = Map::new();
        f.insert("widgetKey".to_string(), Value::from(key));
        f.insert("widgetPlacement".to_string(), Value::from(placement_name(at)));
        if let Some(lines) = lines {
            f.insert("widgetLines".to_string(), json!(lines));
        }
        self.fire("setWidget", f);
    }

    fn set_title(&mut self, t: &str) {
        let mut f = Map::new();
        f.insert("title".to_string(), Value::from(t));
        self.fire("setTitle", f);
    }

    fn set_editor_text(&mut self, t: &str) {
        let mut f = Map::new();
        f.insert("text".to_string(), Value::from(t));
        self.fire("set_editor_text", f);
    }

    fn ask_access(&mut self, a: &Access) -> Answer {
        let what = describe(a);
        match self.confirm(
            &format!("Allow: {what}?"),
            "The agent asks permission for this action.",
            None,
        ) {
            Some(true) => Answer::Once,
            _ => Answer::Deny,
        }
    }

    /// A dangerous command is relayed like an access request: the client has a person behind it,
    /// and a yes runs the command this once. Session and Always grants stay with the surfaces that
    /// can write the owner's approval file.
    fn ask_dangerous(&mut self, a: &Access, reasons: &[String], _keys: &[String]) -> DangerAnswer {
        let what = describe(a);
        let detail = if reasons.is_empty() {
            "The command matches a dangerous rule.".to_string()
        } else {
            reasons.join("; ")
        };
        match self.confirm(&format!("Run dangerous command: {what}?"), &detail, None) {
            Some(true) => DangerAnswer::Once,
            _ => DangerAnswer::Deny,
        }
    }

    /// The RPC host has a person behind it who answers dialogs (pi: `ctx.hasUI` is true).
    fn interactive(&self) -> bool {
        true
    }
}
