//! Architect-owned (W0): the registry shared by the loader, the bus and every `api_*` module.
use oxa_proto::{CommandSpec, FlagSpec, HookPoint, Host, RenderKind, ShortcutSpec, ToolSpec, Value};
use oxa_script::{FnRef, SlotId};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Identifies one loaded extension (its index in load order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExtId(pub u32);

/// One loaded (or failed) extension.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedExtension {
    pub id: ExtId,
    /// Entry file, or `builtin:<name>`.
    pub path: String,
    /// Display name: the manifest name or the file stem.
    pub name: String,
    /// Its runtime; `None` when loading failed.
    pub slot: Option<SlotId>,
    /// The extension declared `rebase: true` (W-guard).
    pub declared_rebase: bool,
    /// The load error, when loading failed.
    pub error: Option<String>,
}

/// An event handler registered with `oxa.on(event, handler)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handler {
    pub ext: ExtId,
    pub slot: SlotId,
    pub f: FnRef,
}

/// A tool registered with `oxa.registerTool(def)`.
#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredTool {
    pub ext: ExtId,
    pub slot: SlotId,
    pub spec: ToolSpec,
    /// `def.execute(toolCallId, params, signal, onUpdate, ctx)`.
    pub execute: FnRef,
    /// `def.renderCall`, `def.renderResult` (F3).
    pub render_call: Option<FnRef>,
    pub render_result: Option<FnRef>,
}

/// A command registered with `oxa.registerCommand(name, def)`.
#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredCommand {
    pub ext: ExtId,
    pub slot: SlotId,
    pub spec: CommandSpec,
    /// `def.handler(args, ctx)`.
    pub handler: FnRef,
    /// `def.getArgumentCompletions(prefix)`.
    pub complete: Option<FnRef>,
}

/// A shortcut registered with `oxa.registerShortcut(key, def)`.
#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredShortcut {
    pub ext: ExtId,
    pub slot: SlotId,
    pub spec: ShortcutSpec,
    pub handler: FnRef,
}

/// A flag registered with `oxa.registerFlag(name, def)` and its current value.
#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredFlag {
    pub ext: ExtId,
    pub spec: FlagSpec,
    pub value: Value,
}

/// A renderer registered with `registerMessageRenderer`, `registerEntryRenderer`, a tool
/// renderer override, or a Markdown transformer.
#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredRenderer {
    pub ext: ExtId,
    pub slot: SlotId,
    pub kind: RenderKind,
    /// What it applies to: the `customType`, the tool name, or (Markdown) a comma-separated
    /// `messageTypes` list (`*` = all).
    pub key: String,
    pub f: FnRef,
}

/// An autocomplete provider (`ctx.ui.addAutocompleteProvider`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisteredCompleter {
    pub ext: ExtId,
    pub slot: SlotId,
    pub f: FnRef,
}

/// The callbacks of one `ctx.compact({onComplete, onError})` call, run at the next
/// `session_compact` (`onComplete(result)`) or `session_compact_failed` (`onError(error)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactWaiter {
    pub ext: ExtId,
    pub slot: SlotId,
    pub on_complete: Option<FnRef>,
    pub on_error: Option<FnRef>,
}

/// An error raised by extension code, to be emitted as `AgentEvent::ExtensionError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtError {
    pub extension_path: String,
    pub event: String,
    pub error: String,
}

/// Everything extensions have registered. Vectors are in registration order.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    pub extensions: Vec<LoadedExtension>,
    pub handlers: HashMap<HookPoint, Vec<Handler>>,
    pub tools: Vec<RegisteredTool>,
    pub commands: Vec<RegisteredCommand>,
    pub shortcuts: Vec<RegisteredShortcut>,
    pub flags: Vec<RegisteredFlag>,
    pub renderers: Vec<RegisteredRenderer>,
    pub completers: Vec<RegisteredCompleter>,
    /// Extra prompt sections `(name, text)` (`oxa.addPromptSection`), read once per epoch.
    pub prompt_sections: Vec<(String, String)>,
    /// The inter-extension event bus (`oxa.events.on(name, f)`).
    pub bus_listeners: Vec<(String, Handler)>,
}

impl Registry {
    /// Remove everything registered by `ext` (failed load, unload).
    pub fn remove_extension(&mut self, ext: ExtId) {
        for hs in self.handlers.values_mut() {
            hs.retain(|h| h.ext != ext);
        }
        self.tools.retain(|t| t.ext != ext);
        self.commands.retain(|c| c.ext != ext);
        self.shortcuts.retain(|s| s.ext != ext);
        self.flags.retain(|f| f.ext != ext);
        self.renderers.retain(|r| r.ext != ext);
        self.completers.retain(|c| c.ext != ext);
        self.bus_listeners.retain(|(_, h)| h.ext != ext);
    }

    pub fn extension(&self, ext: ExtId) -> Option<&LoadedExtension> {
        self.extensions.iter().find(|e| e.id == ext)
    }
}

/// Where the extension host writes its log lines (ignored replies, refused and declared
/// rebases): `(level, line)`. Set by the embedder (`oxa-cli` appends to `extensions.log`).
pub type ExtLog = Arc<dyn Fn(oxa_proto::Level, &str) + Send + Sync>;

/// State shared between the session thread and the runtime thread. Lock one mutex at a time
/// and never hold a lock across a call into a script.
#[derive(Default)]
pub struct ExtState {
    pub registry: Mutex<Registry>,
    /// Set by `Hooks::attach`; `None` before the session exists (host calls then fail with
    /// `busy`).
    pub host: Mutex<Option<Arc<dyn Host>>>,
    /// Errors waiting to be reported.
    pub errors: Mutex<Vec<ExtError>>,
    /// Session working directory, for `ctx.cwd`.
    pub cwd: Mutex<PathBuf>,
    /// The log sink (E-INT 3). `None` = log lines are dropped.
    pub log: Mutex<Option<ExtLog>>,
    /// Events emitted with `oxa.events.emit(name, data)` and not yet delivered. The bus
    /// delivers them from the session side right after the emitting handler returned.
    pub bus_queue: Mutex<Vec<(String, Value)>>,
    /// The run mode as pi names it (`"tui"`, `"rpc"`, `"json"`, `"print"`), for `ctx.mode`. Set
    /// by the embedder ([`crate::ExtensionHost::set_mode`]); `None` = derived from the UI
    /// (`"tui"` when it can prompt, else `"print"`).
    pub mode: Mutex<Option<String>>,
    /// Pending `ctx.compact` callbacks, in request order.
    pub compact_waiters: Mutex<Vec<CompactWaiter>>,
    /// Autocomplete providers (by function id) whose last call failed. A provider that keeps
    /// throwing is reported once, not on every keystroke; a success clears its entry.
    pub failing_completers: Mutex<std::collections::HashSet<u64>>,
}

impl ExtState {
    pub fn new(cwd: PathBuf) -> ExtState {
        ExtState {
            cwd: Mutex::new(cwd),
            ..ExtState::default()
        }
    }

    /// The attached host, if any.
    pub fn host(&self) -> Option<Arc<dyn Host>> {
        self.host.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Keep at most this many unreported errors (the oldest are dropped).
    pub const MAX_ERRORS: usize = 256;

    pub fn push_error(&self, e: ExtError) {
        self.log(
            oxa_proto::Level::Error,
            &format!("{} ({}): {}", e.extension_path, e.event, e.error),
        );
        let mut errors = self.errors.lock().unwrap_or_else(|p| p.into_inner());
        if errors.len() >= Self::MAX_ERRORS {
            errors.remove(0);
        }
        errors.push(e);
    }

    /// Set (or remove) the log sink.
    pub fn set_log(&self, sink: Option<ExtLog>) {
        *self.log.lock().unwrap_or_else(|p| p.into_inner()) = sink;
    }

    /// Write one line to the log sink, if there is one.
    pub fn log(&self, level: oxa_proto::Level, line: &str) {
        let sink = self.log.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(sink) = sink {
            sink(level, line);
        }
    }

    /// Drain the pending errors (the core emits them as `ExtensionError` events).
    pub fn take_errors(&self) -> Vec<ExtError> {
        std::mem::take(&mut *self.errors.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

/// Upcall operations: the `op` field of the JSON request a host function sends to the session
/// thread with `CallCx::upcall` when it needs the caller's `&mut dyn Ui`. The request is
/// `{"op": <one of these>, ...fields}`; the reply is the dialog's result (`null` = dismissed).
pub mod upcall {
    /// `{title, options: [string], timeoutMs?}` -> index | null. Served by `api_ctx::serve`.
    pub const SELECT: &str = "select";
    /// `{title, body, timeoutMs?}` -> bool | null.
    pub const CONFIRM: &str = "confirm";
    /// `{title, placeholder}` -> string | null.
    pub const INPUT: &str = "input";
    /// `{title, prefill}` -> string | null.
    pub const EDITOR: &str = "editor";
    /// `{text, level}` -> null.
    pub const NOTIFY: &str = "notify";
    /// `{key, text|null}` -> null. Served by `api_ui::serve` (this and the following six).
    pub const SET_STATUS: &str = "set_status";
    /// `{key, lines|null, placement: "aboveEditor"|"belowEditor"}` -> null.
    pub const SET_WIDGET: &str = "set_widget";
    /// `{lines|null}` -> null.
    pub const SET_FOOTER: &str = "set_footer";
    /// `{lines|null}` -> null.
    pub const SET_HEADER: &str = "set_header";
    /// `{title}` -> null.
    pub const SET_TITLE: &str = "set_title";
    /// `{text|null}` -> null.
    pub const SET_WORKING_MESSAGE: &str = "set_working_message";
    /// `{text}` -> null; `get_editor_text` `{}` -> string.
    pub const SET_EDITOR_TEXT: &str = "set_editor_text";
    pub const GET_EDITOR_TEXT: &str = "get_editor_text";
    /// `{component: {render: $fn, handleInput: $fn, invalidate?: $fn}, overlay, asEditor}` ->
    /// the component's result | null. Served by `api_components::serve`.
    pub const CUSTOM: &str = "custom";
}
