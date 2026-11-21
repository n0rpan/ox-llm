//! D10 acceptance tests: Dialogs and the TUI Ui.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_config::{Dirs, Keymap, Settings};
use oxa_fake::TestEnv;
use oxa_interactive::{
    access_prompt, append_always, ask_missing_cwd, ask_trust, cycle_mode, Block, DialogHost, DialogKey,
    MissingCwdChoice, TrustChoice, UiProxy, UiRequest, ViewModel, DANGER_LABELS,
};
use oxa_proto::{
    Access, Answer, ComponentAction, CustomOpts, DangerAnswer, KeyId, Level, Mode, Placement, ToolClass, Ui,
    UiComponent, Value,
};
use oxa_tui::{Key, Theme};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::channel;

/// A `Ui` that answers `select` from a script and records what it was asked.
#[derive(Default)]
struct Scripted {
    answers: VecDeque<Option<usize>>,
    title: String,
    options: Vec<String>,
}

impl Scripted {
    fn new(answers: Vec<Option<usize>>) -> Scripted {
        Scripted {
            answers: answers.into(),
            ..Default::default()
        }
    }
}

impl Ui for Scripted {
    fn select(&mut self, title: &str, options: &[String], _timeout_ms: Option<u32>) -> Option<usize> {
        self.title = title.to_string();
        self.options = options.to_vec();
        self.answers.pop_front().flatten()
    }
    fn confirm(&mut self, _: &str, _: &str, _: Option<u32>) -> Option<bool> {
        None
    }
    fn input(&mut self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn editor(&mut self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn notify(&mut self, _: &str, _: Level) {}
    fn set_status(&mut self, _: &str, _: Option<&str>) {}
    fn set_widget(&mut self, _: &str, _: Option<&[String]>, _: Placement) {}
    fn set_title(&mut self, _: &str) {}
    fn set_editor_text(&mut self, _: &str) {}
    fn ask_access(&mut self, _: &Access) -> Answer {
        Answer::Deny
    }
}

fn press(host: &mut DialogHost, keymap: &Keymap, id: &str) -> DialogKey {
    host.key(&Key::press(id), keymap)
}

/// A custom component that counts `up` presses and answers its count on `enter`.
struct Counter {
    n: u32,
}

impl UiComponent for Counter {
    fn render(&mut self, _width: u16) -> Vec<String> {
        vec![format!("count: {}", self.n)]
    }
    fn input(&mut self, key: &KeyId, _text: Option<&str>) -> ComponentAction {
        match key.as_str() {
            "up" => {
                self.n += 1;
                ComponentAction::Redraw
            }
            "enter" => ComponentAction::Done(Value::from(self.n)),
            _ => ComponentAction::Ignored,
        }
    }
}

/// Answer test: Allow once, Allow for this session, Deny, Escape = Deny, and the default focus.
/// `keys` are pressed on a fresh permission prompt; the reply is returned.
fn access_answer(keys: &[&str]) -> Answer {
    let keymap = Keymap::defaults();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let (tx, rx) = channel();
    host.handle(
        UiRequest::AskAccess {
            access: Access::Write(PathBuf::from("/w/a.rs")),
            reply: tx,
        },
        &mut vm,
        0,
    );
    for k in keys {
        press(&mut host, &keymap, k);
    }
    rx.recv().expect("the dialog answered")
}

/// Answer mapping
#[test]
fn permission_prompt_once_session_deny() {
    let keymap = Keymap::defaults();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let theme = Theme::plain();
    let (tx, rx) = channel();
    let req = UiRequest::AskAccess {
        access: Access::Write(PathBuf::from("/w/a.rs")),
        reply: tx,
    };
    assert!(host.handle(req, &mut vm, 0).is_none());
    assert!(host.is_open());
    assert!(!host.is_overlay());
    let text = host.render(80, &theme).join("\n");
    assert!(text.contains("Allow write?"), "{text}");
    assert!(text.contains("Write to /w/a.rs"), "{text}");
    assert!(text.contains("Allow once") && text.contains("Allow for this session") && text.contains("Deny"));

    // Deny has the focus by default: a stray Enter never grants access.
    assert_eq!(press(&mut host, &keymap, "enter"), DialogKey::Closed);
    assert_eq!(rx.recv().unwrap(), Answer::Deny);
    assert!(!host.is_open());

    assert_eq!(access_answer(&["up", "up", "enter"]), Answer::Once);
    assert_eq!(access_answer(&["up", "enter"]), Answer::Session);
    assert_eq!(
        access_answer(&["down", "enter"]),
        Answer::Once,
        "down from Deny wraps to Allow once"
    );
    assert_eq!(access_answer(&["escape"]), Answer::Deny);
    assert_eq!(access_answer(&["up", "up", "escape"]), Answer::Deny);
}

#[test]
fn permission_prompts_queue_and_dropping_the_host_denies() {
    let keymap = Keymap::defaults();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let theme = Theme::plain();
    let (tx1, rx1) = channel();
    let (tx2, rx2) = channel();
    host.handle(
        UiRequest::AskAccess {
            access: Access::Write(PathBuf::from("/w/a.rs")),
            reply: tx1,
        },
        &mut vm,
        0,
    );
    host.handle(
        UiRequest::AskAccess {
            access: Access::Read(PathBuf::from("/r/b.rs")),
            reply: tx2,
        },
        &mut vm,
        0,
    );
    press(&mut host, &keymap, "escape");
    assert_eq!(rx1.recv().unwrap(), Answer::Deny);
    // The second request is now open, in arrival order.
    assert!(host.render(80, &theme).join("\n").contains("Allow read?"));
    drop(host);
    assert_eq!(
        rx2.recv().unwrap(),
        Answer::Deny,
        "quit answers every open request with Deny"
    );
}

/// alt+m cycles plan/workspace/ask
#[test]
fn mode_cycle() {
    let keymap = Keymap::defaults();
    assert!(keymap.matches(&KeyId::parse("alt+m").unwrap(), "app.mode.cycle"));
    let (m1, t1) = cycle_mode(Mode::Plan);
    assert_eq!(m1, Mode::Workspace);
    assert!(t1.starts_with("Mode: workspace"), "{t1}");
    let (m2, t2) = cycle_mode(m1);
    assert_eq!(m2, Mode::Ask);
    assert!(t2.starts_with("Mode: ask"), "{t2}");
    let (m3, t3) = cycle_mode(m2);
    assert_eq!(m3, Mode::Plan);
    assert_eq!(t3, "Mode: plan (read-only)");
}

/// choices and save
#[test]
fn trust_selector() {
    let env = TestEnv::new();
    let cwd = env.work().join("proj").display().to_string();
    let parent = env.work().display().to_string();
    let gated = vec!["settings.json".to_string(), "hooks".to_string()];

    let mut ui = Scripted::new(vec![Some(0), Some(1), Some(2), Some(3), None]);
    assert_eq!(ask_trust(&mut ui, &cwd, &gated), Some(TrustChoice::Trust));
    assert_eq!(
        ui.options.len(),
        4,
        "trust, trust parent, do not trust, decide later"
    );
    assert!(ui.title.contains(&cwd), "{}", ui.title);
    assert!(ui.title.contains("settings.json, hooks"), "{}", ui.title);
    assert!(ui.options[1].contains(&parent), "{:?}", ui.options);
    assert_eq!(ask_trust(&mut ui, &cwd, &gated), Some(TrustChoice::TrustParent));
    assert_eq!(ask_trust(&mut ui, &cwd, &gated), Some(TrustChoice::NoTrust));
    assert_eq!(
        ask_trust(&mut ui, &cwd, &gated),
        Some(TrustChoice::Skip),
        "decide later is a choice"
    );
    assert_eq!(
        ask_trust(&mut ui, &cwd, &gated),
        None,
        "dismissed: the caller treats it as skip"
    );

    // A root cwd has no parent: that choice is not offered and the indices shift.
    let mut root = Scripted::new(vec![Some(1)]);
    assert_eq!(ask_trust(&mut root, "/", &[]), Some(TrustChoice::NoTrust));
    assert_eq!(root.options.len(), 3);
    assert!(root.title.contains("Gated resources: none"), "{}", root.title);
}

/// three outcomes
#[test]
fn missing_cwd_continue_fork_cancel() {
    let mut ui = Scripted::new(vec![Some(0), Some(1), Some(2), None]);
    assert_eq!(
        ask_missing_cwd(&mut ui, "/old", "/new"),
        MissingCwdChoice::Continue
    );
    assert!(ui.options[0].contains("/new"), "{:?}", ui.options);
    assert_eq!(ask_missing_cwd(&mut ui, "/old", "/new"), MissingCwdChoice::Fork);
    assert_eq!(ask_missing_cwd(&mut ui, "/old", "/new"), MissingCwdChoice::Cancel);
    assert_eq!(
        ask_missing_cwd(&mut ui, "/old", "/new"),
        MissingCwdChoice::Cancel,
        "dismissed = cancel"
    );
    assert!(ui.title.contains("/old"), "{}", ui.title);
}

/// timeout returns None with countdown
#[test]
fn timed_dialogs() {
    let keymap = Keymap::defaults();
    let theme = Theme::plain();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Select {
            title: "Pick".to_string(),
            options: vec!["a".to_string(), "b".to_string()],
            timeout_ms: Some(3000),
            reply: tx,
        },
        &mut vm,
        1000,
    );
    assert!(host.render(60, &theme).join("\n").contains("(3s)"));
    assert!(!host.tick(1500), "2.5 s left still shows 3s: no change");
    assert!(host.tick(2100));
    assert!(host.render(60, &theme).join("\n").contains("(2s)"));
    assert!(host.tick(3999), "one millisecond left shows 1s");
    assert!(host.render(60, &theme).join("\n").contains("(1s)"));
    assert!(host.is_open());
    assert!(host.tick(4000), "expiry");
    assert!(!host.is_open());
    assert_eq!(rx.recv().unwrap(), None);

    // A timed confirm expires to None too, and keys after expiry go nowhere.
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Confirm {
            title: "Sure?".to_string(),
            body: "Delete it".to_string(),
            timeout_ms: Some(1000),
            reply: tx,
        },
        &mut vm,
        0,
    );
    assert!(host.tick(1000));
    assert_eq!(rx.recv().unwrap(), None);
    assert_eq!(press(&mut host, &keymap, "enter"), DialogKey::NotOpen);
}

/// Ui methods
#[test]
fn select_confirm_input_editor() {
    let keymap = Keymap::defaults();
    let theme = Theme::plain();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();

    // select: down, enter -> the second option
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Select {
            title: "Pick".to_string(),
            options: vec!["x".to_string(), "y".to_string(), "z".to_string()],
            timeout_ms: None,
            reply: tx,
        },
        &mut vm,
        0,
    );
    let shown = host.render(60, &theme).join("\n");
    assert!(
        shown.contains("Pick") && shown.contains("x") && shown.contains("z"),
        "{shown}"
    );
    press(&mut host, &keymap, "down");
    assert_eq!(press(&mut host, &keymap, "enter"), DialogKey::Closed);
    assert_eq!(rx.recv().unwrap(), Some(1));

    // select dismissed with escape
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Select {
            title: "Pick".to_string(),
            options: vec!["x".to_string()],
            timeout_ms: None,
            reply: tx,
        },
        &mut vm,
        0,
    );
    press(&mut host, &keymap, "escape");
    assert_eq!(rx.recv().unwrap(), None);

    // confirm: Yes is first; down reaches No; escape dismisses
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Confirm {
            title: "Sure?".to_string(),
            body: "Delete it".to_string(),
            timeout_ms: None,
            reply: tx,
        },
        &mut vm,
        0,
    );
    assert!(host.render(60, &theme).join("\n").contains("Delete it"));
    press(&mut host, &keymap, "enter");
    assert_eq!(rx.recv().unwrap(), Some(true));
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Confirm {
            title: "Sure?".to_string(),
            body: String::new(),
            timeout_ms: None,
            reply: tx,
        },
        &mut vm,
        0,
    );
    press(&mut host, &keymap, "down");
    press(&mut host, &keymap, "enter");
    assert_eq!(rx.recv().unwrap(), Some(false));
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Confirm {
            title: "Sure?".to_string(),
            body: String::new(),
            timeout_ms: None,
            reply: tx,
        },
        &mut vm,
        0,
    );
    press(&mut host, &keymap, "escape");
    assert_eq!(rx.recv().unwrap(), None);

    // input: type, enter; and escape dismisses
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Input {
            title: "Name".to_string(),
            placeholder: "your name".to_string(),
            reply: tx,
        },
        &mut vm,
        0,
    );
    assert!(host.render(60, &theme).join("\n").contains("your name"));
    host.key(&Key::char('a'), &keymap);
    host.key(&Key::char('b'), &keymap);
    host.key(&Key::press("enter"), &keymap);
    assert_eq!(rx.recv().unwrap(), Some("ab".to_string()));
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Input {
            title: "Name".to_string(),
            placeholder: String::new(),
            reply: tx,
        },
        &mut vm,
        0,
    );
    press(&mut host, &keymap, "escape");
    assert_eq!(rx.recv().unwrap(), None);

    // editor: prefilled, typed into, submitted; escape dismisses
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Editor {
            title: "Notes".to_string(),
            prefill: "one".to_string(),
            reply: tx,
        },
        &mut vm,
        0,
    );
    host.key(&Key::char('x'), &keymap);
    host.key(&Key::press("enter"), &keymap);
    assert_eq!(rx.recv().unwrap(), Some("onex".to_string()));
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Editor {
            title: "Notes".to_string(),
            prefill: String::new(),
            reply: tx,
        },
        &mut vm,
        0,
    );
    press(&mut host, &keymap, "escape");
    assert_eq!(rx.recv().unwrap(), None);

    // Non-blocking requests change the view model and never open a dialog.
    assert!(host
        .handle(
            UiRequest::Notify {
                text: "saved".to_string(),
                level: Level::Warning
            },
            &mut vm,
            0
        )
        .is_none());
    assert!(matches!(
        vm.blocks.last(),
        Some(Block::Notice { level: Level::Warning, text }) if text == "saved"
    ));
    host.handle(
        UiRequest::SetStatus {
            key: "k".to_string(),
            text: Some("v".to_string()),
        },
        &mut vm,
        0,
    );
    assert_eq!(vm.status.ext_status.get("k").map(String::as_str), Some("v"));
    host.handle(
        UiRequest::SetStatus {
            key: "k".to_string(),
            text: None,
        },
        &mut vm,
        0,
    );
    assert!(vm.status.ext_status.is_empty());
    host.handle(
        UiRequest::SetWidget {
            key: "w".to_string(),
            lines: Some(vec!["line".to_string()]),
            at: Placement::BelowEditor,
        },
        &mut vm,
        0,
    );
    assert_eq!(vm.widgets_below.get("w"), Some(&vec!["line".to_string()]));
    assert!(vm.widgets_above.is_empty());
    host.handle(UiRequest::SetTitle("t".to_string()), &mut vm, 0);
    assert_eq!(vm.title_override.as_deref(), Some("t"));
    assert!(!host.is_open());

    // Editor text round trips are the caller's to answer.
    let (tx, _rx) = channel();
    assert!(matches!(
        host.handle(UiRequest::EditorText { reply: tx }, &mut vm, 0),
        Some(UiRequest::EditorText { .. })
    ));
    assert!(matches!(
        host.handle(UiRequest::SetEditorText("hi".to_string()), &mut vm, 0),
        Some(UiRequest::SetEditorText(t)) if t == "hi"
    ));
}

/// Ui::custom until Done
#[test]
fn custom_component_hosting() {
    let keymap = Keymap::defaults();
    let theme = Theme::plain();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();

    let (tx, rx) = channel();
    host.handle(
        UiRequest::Custom {
            component: Box::new(Counter { n: 0 }),
            opts: CustomOpts {
                overlay: true,
                as_editor: false,
            },
            reply: tx,
        },
        &mut vm,
        0,
    );
    assert!(host.is_open());
    assert!(host.is_overlay());
    assert!(host.render(40, &theme).join("\n").contains("count: 0"));
    assert_eq!(press(&mut host, &keymap, "up"), DialogKey::Consumed);
    assert!(host.render(40, &theme).join("\n").contains("count: 1"));
    assert_eq!(press(&mut host, &keymap, "enter"), DialogKey::Closed);
    assert_eq!(rx.recv().unwrap(), Some(Value::from(1)));
    assert!(!host.is_open());

    // Escape the component does not use answers None.
    let (tx, rx) = channel();
    host.handle(
        UiRequest::Custom {
            component: Box::new(Counter { n: 0 }),
            opts: CustomOpts::default(),
            reply: tx,
        },
        &mut vm,
        0,
    );
    assert!(!host.is_overlay(), "not an overlay unless asked");
    assert_eq!(press(&mut host, &keymap, "escape"), DialogKey::Closed);
    assert_eq!(rx.recv().unwrap(), None);
}

/// The permission text is deterministic and the same for every access class.
#[test]
fn access_prompt_text() {
    assert_eq!(
        access_prompt(&Access::Write(PathBuf::from("/w/a.rs"))),
        ("Allow write?".to_string(), "Write to /w/a.rs".to_string())
    );
    assert_eq!(
        access_prompt(&Access::Read(PathBuf::from("/r/b.rs"))).1,
        "Read /r/b.rs"
    );
    assert_eq!(
        access_prompt(&Access::Exec {
            command: "ls -la".to_string(),
            net: false,
            background: false,
        }),
        (
            "Allow command?".to_string(),
            "Run ls -la (no network)".to_string()
        )
    );
    assert_eq!(
        access_prompt(&Access::Net {
            host: "example.test".to_string()
        })
        .1,
        "Connect to example.test"
    );
    assert_eq!(
        access_prompt(&Access::Mcp {
            server: "docs".to_string(),
            tool: "search".to_string(),
            class: ToolClass::Net,
        }),
        (
            "Allow MCP tool?".to_string(),
            "Call docs:search (network class)".to_string()
        )
    );
}

/// The UI proxy blocks the worker until the UI answers, and a missing UI never grants consent.
#[test]
fn ui_proxy_round_trip_and_no_consent_without_ui() {
    let (tx, rx) = channel();
    let mut proxy = UiProxy::new(tx);
    let worker = std::thread::spawn(move || {
        let picked = proxy.select("Pick", &["a".to_string(), "b".to_string()], None);
        let answer = proxy.ask_access(&Access::Net {
            host: "example.test".to_string(),
        });
        (picked, answer)
    });
    match rx.recv().unwrap() {
        UiRequest::Select { options, reply, .. } => {
            assert_eq!(options.len(), 2);
            reply.send(Some(1)).unwrap();
        }
        _ => panic!("expected a select"),
    }
    match rx.recv().unwrap() {
        UiRequest::AskAccess { reply, .. } => reply.send(Answer::Session).unwrap(),
        _ => panic!("expected an access prompt"),
    }
    assert_eq!(worker.join().unwrap(), (Some(1), Answer::Session));

    // A UI that drops the reply without answering counts as no answer.
    let (tx, rx) = channel();
    let mut proxy = UiProxy::new(tx);
    let worker = std::thread::spawn(move || proxy.ask_access(&Access::Write(PathBuf::from("/w/a.rs"))));
    match rx.recv().unwrap() {
        UiRequest::AskAccess { reply, .. } => drop(reply),
        _ => panic!("expected an access prompt"),
    }
    assert_eq!(worker.join().unwrap(), Answer::Deny);

    // No UI thread at all: every dialog is dismissed.
    let (tx, rx) = channel();
    drop(rx);
    let mut proxy = UiProxy::new(tx);
    assert_eq!(
        proxy.ask_access(&Access::Write(PathBuf::from("/w/a.rs"))),
        Answer::Deny
    );
    assert_eq!(proxy.confirm("t", "b", None), None);
    assert_eq!(proxy.select("t", &["a".to_string()], None), None);
}

// ---------------------------------------------------------------- #1512 S2: dangerous commands

const DANGER_KEY: &str = "git-push-force git push --force origin main";

/// A dangerous-command request for `git push --force origin main`, with its reply channel.
fn danger_request(reply: std::sync::mpsc::Sender<DangerAnswer>) -> UiRequest {
    UiRequest::AskDangerous {
        access: Access::Exec {
            command: "git push --force origin main".into(),
            net: false,
            background: false,
        },
        reasons: vec!["git-push-force: git push with a force flag".into()],
        keys: vec![DANGER_KEY.into()],
        reply,
    }
}

/// The dialog shows the four choices, the command and the reason; Deny has the focus, so a
/// stray Enter never runs a dangerous command.
#[test]
fn dangerous_dialog_has_four_choices_deny_focused() {
    let keymap = Keymap::defaults();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let theme = Theme::plain();
    let (tx, rx) = channel();
    assert!(host.handle(danger_request(tx), &mut vm, 0).is_none());
    let text = host.render(100, &theme).join("\n");
    assert!(text.contains("Dangerous command"), "{text}");
    assert!(text.contains("git push --force origin main"), "{text}");
    assert!(
        text.contains("git-push-force: git push with a force flag"),
        "{text}"
    );
    assert_eq!(DANGER_LABELS.len(), 4, "{DANGER_LABELS:?}");
    for label in DANGER_LABELS {
        assert!(text.contains(label), "{label}\n{text}");
    }
    assert_eq!(press(&mut host, &keymap, "enter"), DialogKey::Closed);
    assert_eq!(rx.recv().unwrap(), DangerAnswer::Deny);
}

/// "Always allow" is the third choice. Its keys are handed to the driver first: nothing is
/// answered until they are saved to the user file, and the project file is never copied.
#[test]
fn dangerous_dialog_always_writes_user_settings_never_project() {
    let keymap = Keymap::defaults();
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    env.write(
        "work/.oxa/settings.json",
        r#"{"oxa":{"approval":{"always":["project-only"]}}}"#,
    );
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let (tx, rx) = channel();
    host.handle(danger_request(tx), &mut vm, 0);
    // Deny is focused: one step up is "Always allow".
    press(&mut host, &keymap, "up");
    assert_eq!(press(&mut host, &keymap, "enter"), DialogKey::Closed);
    assert!(rx.try_recv().is_err(), "the worker waits for the save");
    let (keys, reply) = host.take_danger_always().expect("always is pending");
    assert_eq!(keys, vec![DANGER_KEY.to_string()]);
    let mut settings = Settings::load(&dirs, &cwd, false);
    append_always(&mut settings, &dirs, &cwd, &keys).expect("the user settings are writable");
    reply.send(DangerAnswer::Always).unwrap();
    assert_eq!(rx.recv().unwrap(), DangerAnswer::Always);
    let user = std::fs::read_to_string(dirs.settings_file()).unwrap();
    assert!(user.contains(DANGER_KEY), "{user}");
    assert!(!user.contains("project-only"), "{user}");
    let project = std::fs::read_to_string(dirs.project_settings_file(&cwd)).unwrap();
    assert!(
        project.contains("project-only") && !project.contains(DANGER_KEY),
        "{project}"
    );
}

/// A failed save is an error for the driver, which then answers `Session`: the command still
/// runs this session, and the user file is not written.
#[test]
fn dangerous_dialog_always_write_failure_still_grants_session() {
    let keymap = Keymap::defaults();
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let cwd = env.work();
    // The user settings path is a directory, so the write cannot succeed.
    std::fs::create_dir_all(dirs.settings_file()).unwrap();
    let mut host = DialogHost::new();
    let mut vm = ViewModel::default();
    let (tx, rx) = channel();
    host.handle(danger_request(tx), &mut vm, 0);
    press(&mut host, &keymap, "up");
    press(&mut host, &keymap, "enter");
    let (keys, reply) = host.take_danger_always().expect("always is pending");
    let mut settings = Settings::load(&dirs, &cwd, false);
    assert!(append_always(&mut settings, &dirs, &cwd, &keys).is_err());
    reply.send(DangerAnswer::Session).unwrap();
    assert_eq!(rx.recv().unwrap(), DangerAnswer::Session);
}
