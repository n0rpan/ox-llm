//! Post-wave-E fixes (lane L4) on the extension side: the handler deadline excludes the time a
//! dialog stays open (item 2); `ctx.waitForIdle()` from a handler the run waits for is refused
//! (item 9). The session is a `Host` double.

use oxa_ext::{ExtensionHost, LoadOpts};
use oxa_fake::TestEnv;
use oxa_proto::{
    codes, Access, Answer, HookPoint, HookReply, Hooks, Host, HostAction, Level, OxaError, Placement, Ui,
    Value,
};
use oxa_script::Limits;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn ext(body: &str) -> String {
    format!("export default function (oxa) {{ {body} }}")
}

fn short_deadline() -> LoadOpts {
    LoadOpts {
        limits: Limits {
            deadline_ms: Some(300),
            ..Limits::EXTENSION
        },
        ..LoadOpts::default()
    }
}

/// Every dialog stays open for `open` and is then confirmed.
struct SlowUi {
    open: Duration,
    notes: Vec<String>,
}

impl Ui for SlowUi {
    fn select(&mut self, _: &str, _: &[String], _: Option<u32>) -> Option<usize> {
        std::thread::sleep(self.open);
        Some(0)
    }
    fn confirm(&mut self, _: &str, _: &str, _: Option<u32>) -> Option<bool> {
        std::thread::sleep(self.open);
        Some(true)
    }
    fn input(&mut self, _: &str, _: &str) -> Option<String> {
        std::thread::sleep(self.open);
        Some("typed".into())
    }
    fn editor(&mut self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn notify(&mut self, text: &str, _: Level) {
        self.notes.push(text.to_string());
    }
    fn set_status(&mut self, _: &str, _: Option<&str>) {}
    fn set_widget(&mut self, _: &str, _: Option<&[String]>, _: Placement) {}
    fn set_title(&mut self, _: &str) {}
    fn set_editor_text(&mut self, _: &str) {}
    fn ask_access(&mut self, _: &Access) -> Answer {
        Answer::Deny
    }
    fn interactive(&self) -> bool {
        true
    }
}

/// Item 2: dialogs that stay open longer than the handler deadline (three of them, 3 x 400 ms
/// against 300 ms) do not time the handler out.
#[test]
fn open_dialogs_do_not_count_against_the_deadline() {
    let env = TestEnv::new();
    let host = ExtensionHost::new(env.work()).expect("runtime");
    let loaded = host.load_source(
        "dialogs",
        &ext(r#"oxa.on("input", (e, ctx) => {
            const ok = ctx.ui.confirm("Go?", "body");
            const pick = ctx.ui.select("Pick", ["a", "b"]);
            const text = ctx.ui.input("Name", "");
            return {action: "transform", text: e.text + ":" + ok + ":" + pick + ":" + text};
        });"#),
        &short_deadline(),
    );
    assert!(loaded.error.is_none(), "{:?}", loaded.error);
    let mut ui = SlowUi {
        open: Duration::from_millis(400),
        notes: Vec::new(),
    };
    let r = host.fire(HookPoint::Input, &json!({"text": "x"}), &mut ui);
    assert_eq!(r, HookReply::ReplaceInput("x:true:a:typed".into()));
    assert!(host.state().take_errors().is_empty());
}

/// Item 2: the clock runs again after the dialog: a handler that loops after its dialog is still
/// interrupted, after about one deadline of script time (not counting the dialog).
#[test]
fn script_time_after_a_dialog_still_counts() {
    let env = TestEnv::new();
    let host = ExtensionHost::new(env.work()).expect("runtime");
    let loaded = host.load_source(
        "looper",
        &ext(r#"oxa.on("input", (e, ctx) => { ctx.ui.confirm("Go?", "body"); while (true) {} });"#),
        &short_deadline(),
    );
    assert!(loaded.error.is_none(), "{:?}", loaded.error);
    let mut ui = SlowUi {
        open: Duration::from_millis(400),
        notes: Vec::new(),
    };
    let start = Instant::now();
    let r = host.fire(HookPoint::Input, &json!({"text": "x"}), &mut ui);
    let took = start.elapsed();
    assert_eq!(r, HookReply::Continue);
    let errors = host.state().take_errors();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].error.contains("deadline"), "{}", errors[0].error);
    assert!(
        took >= Duration::from_millis(650),
        "dialog + full budget: {took:?}"
    );
    assert!(took < Duration::from_secs(5), "{took:?}");
}

/// A session double that is busy (a run in progress) and records what reaches it.
struct BusyHost {
    seen: Mutex<Vec<HostAction>>,
}

impl Host for BusyHost {
    fn call(&self, action: HostAction) -> Result<Value, OxaError> {
        let answer = match &action {
            HostAction::IsIdle => Value::Bool(false),
            _ => Value::Null,
        };
        self.seen.lock().unwrap().push(action);
        Ok(answer)
    }
}

/// Item 9: an event handler (the run waits for it) gets `busy` from `ctx.waitForIdle()` at once;
/// a command handler's wait reaches the session.
#[test]
fn wait_for_idle_is_refused_inside_the_run_only() {
    let env = TestEnv::new();
    let host = ExtensionHost::new(env.work()).expect("runtime");
    let loaded = host.load_source(
        "waiter",
        &ext(r#"
            oxa.on("input", (e, ctx) => {
                try { ctx.waitForIdle(); return {action: "transform", text: "resolved"}; }
                catch (err) { return {action: "transform", text: "refused:" + err.code}; }
            });
            oxa.registerCommand("w", {description: "wait", handler: (args, ctx) => { ctx.waitForIdle(); }});
        "#),
        &LoadOpts::default(),
    );
    assert!(loaded.error.is_none(), "{:?}", loaded.error);
    let session = Arc::new(BusyHost {
        seen: Mutex::new(Vec::new()),
    });
    host.attach(session.clone());
    let mut ui = SlowUi {
        open: Duration::ZERO,
        notes: Vec::new(),
    };
    let r = host.fire(HookPoint::Input, &json!({"text": "x"}), &mut ui);
    assert_eq!(r, HookReply::ReplaceInput(format!("refused:{}", codes::BUSY)));
    assert!(!session.seen.lock().unwrap().contains(&HostAction::WaitForIdle));

    host.run_command("w", "", &mut ui).expect("command");
    assert!(session.seen.lock().unwrap().contains(&HostAction::WaitForIdle));
}
