use crate::{Access, Answer, KeyId, Value};

/// Severity of a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Level {
    Info,
    Warning,
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Warning => "warning",
            Level::Error => "error",
        }
    }
}

/// Where an extension widget is placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Placement {
    AboveEditor,
    BelowEditor,
}

/// What a custom component asks for after handling a key.
#[derive(Debug, Clone, PartialEq)]
pub enum ComponentAction {
    /// The key was not used.
    Ignored,
    /// The key was used; re-render.
    Redraw,
    /// Close the component; [`Ui::custom`] returns this value.
    Done(Value),
}

/// A component supplied by an extension (pi's `ctx.ui.custom`). Same model as
/// `oxa_tui::Component`: render lines for a width; the renderer diffs lines. Lines may contain
/// SGR colour sequences; each line must fit `width` columns.
pub trait UiComponent: Send {
    fn render(&mut self, width: u16) -> Vec<String>;
    fn input(&mut self, key: &KeyId, text: Option<&str>) -> ComponentAction;
}

/// Options of [`Ui::custom`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CustomOpts {
    /// Draw as an overlay above the transcript instead of replacing the editor.
    pub overlay: bool,
    /// Replace the prompt editor until dismissed (pi's custom editor).
    pub as_editor: bool,
}

/// The user-interaction surface seen by the core, hooks and extensions.
///
/// Implemented by the TUI (`oxa-interactive`, D10), by RPC (extension-UI records, F8) and by
/// print/json mode ([`HeadlessUi`]). Every dialog returns `None` when dismissed, timed out or
/// unavailable; callers must handle `None` as "no answer", never as consent.
pub trait Ui: Send {
    fn select(&mut self, title: &str, options: &[String], timeout_ms: Option<u32>) -> Option<usize>;
    fn confirm(&mut self, title: &str, body: &str, timeout_ms: Option<u32>) -> Option<bool>;
    fn input(&mut self, title: &str, placeholder: &str) -> Option<String>;
    fn editor(&mut self, title: &str, prefill: &str) -> Option<String>;
    fn notify(&mut self, text: &str, level: Level);
    /// `None` removes the status entry `key`.
    fn set_status(&mut self, key: &str, text: Option<&str>);
    /// `None` removes the widget `key`.
    fn set_widget(&mut self, key: &str, lines: Option<&[String]>, at: Placement);
    fn set_title(&mut self, t: &str);
    fn set_editor_text(&mut self, t: &str);
    /// The permission prompt. Non-interactive implementations return [`Answer::Deny`]
    /// (print/json: Ask -> Deny); RPC relays it as an extension-UI `confirm`.
    fn ask_access(&mut self, a: &Access) -> Answer;
    /// The dangerous-command prompt (#1512 S2): `reasons` are one `rule: reason` line per hit,
    /// `keys` the grant keys of the same hits ([`crate::Policy::danger_ask`]). The default is
    /// [`DangerAnswer::Deny`]: a `Ui` that cannot ask never consents.
    fn ask_dangerous(&mut self, _a: &Access, _reasons: &[String], _keys: &[String]) -> DangerAnswer {
        DangerAnswer::Deny
    }

    // ---- optional surface (defaults = not available) ----
    /// Current prompt editor text.
    fn editor_text(&mut self) -> String {
        String::new()
    }
    /// Replace the footer (`None` restores the built-in one).
    fn set_footer(&mut self, _lines: Option<&[String]>) {}
    /// Replace the header (`None` restores the built-in one).
    fn set_header(&mut self, _lines: Option<&[String]>) {}
    /// Text next to the working spinner (`None` restores the default).
    fn set_working_message(&mut self, _text: Option<&str>) {}
    /// Show a custom component until it returns [`ComponentAction::Done`]. `None` when the UI
    /// cannot show components (print, json, RPC) or the user dismissed it.
    fn custom(&mut self, _c: Box<dyn UiComponent>, _opts: CustomOpts) -> Option<Value> {
        None
    }
    /// `true` when a person can answer dialogs.
    fn interactive(&self) -> bool {
        false
    }
    /// The publication approval dialog: shows `body` exactly, with Approve and Deny (Deny
    /// focused), and returns `Some(true)` only when the person pressed Approve. Only a UI that
    /// a person answers by key implements it; the default (print, json, RPC, hooks, tests)
    /// answers `None`, which callers treat as "not approved". Never relay it to a model, an
    /// extension or a timeout, and never skip it for yolo.
    fn approve_publication(&mut self, _title: &str, _body: &str) -> Option<bool> {
        None
    }
    /// A masked input for a password a process of this session asks for (sudo's askpass helper):
    /// shows `body` as the requesting process's own text and returns what the owner typed, or
    /// `None` when dismissed or when this UI cannot ask. The value goes straight to that
    /// process; callers must never log, record or display it. Never answered by a model,
    /// a typed chat message or yolo.
    fn secret(&mut self, _title: &str, _body: &str) -> Option<String> {
        None
    }
    /// [`Ui::confirm`] that `cancel` can dismiss: only this dialog is dismissed, and a cancel
    /// that came before the dialog was registered is never lost (the answer is `None`).
    /// The default cannot dismiss an open dialog; it only honours a cancel that came first.
    fn confirm_cancellable(&mut self, title: &str, body: &str, cancel: &DialogCancel) -> Option<bool> {
        if cancel.is_cancelled() {
            return None;
        }
        self.confirm(title, body, None)
    }
}

/// A cloneable, sticky cancel handle for one dialog. `cancel` marks it and runs the hook the
/// dialog armed; `arm` on a handle that is already cancelled runs the hook at once. Both share
/// one mutex, so whichever comes first, the dialog is released exactly once.
#[derive(Clone, Default)]
pub struct DialogCancel(std::sync::Arc<std::sync::Mutex<CancelState>>);

#[derive(Default)]
struct CancelState {
    cancelled: bool,
    hook: Option<Box<dyn FnOnce() + Send>>,
}

impl DialogCancel {
    pub fn new() -> DialogCancel {
        DialogCancel::default()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, CancelState> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn cancel(&self) {
        let hook = {
            let mut st = self.state();
            st.cancelled = true;
            st.hook.take()
        };
        if let Some(hook) = hook {
            hook();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.state().cancelled
    }

    pub fn arm(&self, hook: impl FnOnce() + Send + 'static) {
        let now = {
            let mut st = self.state();
            if st.cancelled {
                Some(hook)
            } else {
                st.hook = Some(Box::new(hook));
                None
            }
        };
        if let Some(hook) = now {
            hook();
        }
    }
}

impl std::fmt::Debug for DialogCancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DialogCancel")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

/// The answer to the dangerous-command prompt (#1512 S2). `Once` runs the command this time;
/// `Session` and `Always` also remember its rule keys (`Always` also saves them to the user's
/// settings); `Deny` refuses it. Deny is the answer of every `Ui` that cannot ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DangerAnswer {
    Once,
    Session,
    Always,
    Deny,
}

/// The non-interactive `Ui`: dialogs answer `None`, permission asks answer `Deny`, notifications
/// are collected for the caller to print to stderr.
#[derive(Debug, Default)]
pub struct HeadlessUi {
    pub notes: Vec<(Level, String)>,
}

impl Ui for HeadlessUi {
    fn select(&mut self, _title: &str, _options: &[String], _timeout_ms: Option<u32>) -> Option<usize> {
        None
    }
    fn confirm(&mut self, _title: &str, _body: &str, _timeout_ms: Option<u32>) -> Option<bool> {
        None
    }
    fn input(&mut self, _title: &str, _placeholder: &str) -> Option<String> {
        None
    }
    fn editor(&mut self, _title: &str, _prefill: &str) -> Option<String> {
        None
    }
    fn notify(&mut self, text: &str, level: Level) {
        self.notes.push((level, text.to_string()));
    }
    fn set_status(&mut self, _key: &str, _text: Option<&str>) {}
    fn set_widget(&mut self, _key: &str, _lines: Option<&[String]>, _at: Placement) {}
    fn set_title(&mut self, _t: &str) {}
    fn set_editor_text(&mut self, _t: &str) {}
    fn ask_access(&mut self, _a: &Access) -> Answer {
        Answer::Deny
    }
}
