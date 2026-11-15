//! E4 acceptance tests: ctx API and the core Host.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.
//!
//! These tests drive the `ctx` glue on a real `RuntimeThread` with `api_ctx::serve` as the
//! dialog answerer. The session side is a `Host` double: the policy, sandbox and session behaviour
//! behind it are tested where they live, in `oxa-core/src/host.rs` (`CoreHost` + `serve_pending`).

use oxa_ext::{api_ctx, ExtId, ExtState};
use oxa_proto::{codes, Access, Answer, ExecSpec, Host, HostAction, Level, OxaError, Placement, Ui, Value};
use oxa_script::{Limits, RuntimeThread};
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A UI with scripted answers. `None` = dismissed.
#[derive(Default)]
struct TestUi {
    select: Option<usize>,
    confirm: Option<bool>,
    input: Option<String>,
    editor: Option<String>,
    notes: Vec<(Level, String)>,
    asked: Vec<Access>,
}

impl Ui for TestUi {
    fn select(&mut self, _title: &str, _options: &[String], _timeout_ms: Option<u32>) -> Option<usize> {
        self.select
    }
    fn confirm(&mut self, _title: &str, _body: &str, _timeout_ms: Option<u32>) -> Option<bool> {
        self.confirm
    }
    fn input(&mut self, _title: &str, _placeholder: &str) -> Option<String> {
        self.input.clone()
    }
    fn editor(&mut self, _title: &str, _prefill: &str) -> Option<String> {
        self.editor.clone()
    }
    fn notify(&mut self, text: &str, level: Level) {
        self.notes.push((level, text.to_string()));
    }
    fn set_status(&mut self, _key: &str, _text: Option<&str>) {}
    fn set_widget(&mut self, _key: &str, _lines: Option<&[String]>, _at: Placement) {}
    fn set_title(&mut self, _t: &str) {}
    fn set_editor_text(&mut self, _t: &str) {}
    fn ask_access(&mut self, a: &Access) -> Answer {
        self.asked.push(a.clone());
        Answer::Deny
    }
    fn interactive(&self) -> bool {
        true
    }
}

type AnswerFn = Box<dyn Fn(&HostAction) -> Result<Value, OxaError> + Send + Sync>;

/// A session double: records every action and answers with `answer`.
struct DoubleHost {
    seen: Mutex<Vec<HostAction>>,
    answer: AnswerFn,
}

impl Host for DoubleHost {
    fn call(&self, action: HostAction) -> Result<Value, OxaError> {
        let out = (self.answer)(&action);
        self.seen.lock().unwrap().push(action);
        out
    }
}

/// A state with `host` attached and `cwd` as the session directory.
fn state(
    cwd: PathBuf,
    answer: impl Fn(&HostAction) -> Result<Value, OxaError> + Send + Sync + 'static,
) -> (Arc<ExtState>, Arc<DoubleHost>) {
    let st = Arc::new(ExtState::new(cwd));
    let host = Arc::new(DoubleHost {
        seen: Mutex::new(Vec::new()),
        answer: Box::new(answer),
    });
    *st.host.lock().unwrap() = Some(host.clone() as Arc<dyn Host>);
    (st, host)
}

/// Run `src` on a fresh runtime with the E4 API installed. Upcalls go to `ui`.
fn js(st: &Arc<ExtState>, ui: &mut dyn Ui, src: &str) -> Result<Value, OxaError> {
    let thread = RuntimeThread::spawn("e4-test").expect("spawn");
    let st2 = st.clone();
    let src = src.to_string();
    thread
        .run(
            move |slots| {
                let id = slots.create(Limits::EXTENSION)?;
                let rt = slots.get(id).expect("runtime");
                api_ctx::install(rt, &st2, ExtId(0));
                rt.eval_script("test.js", &src)
            },
            &mut |request, _port| api_ctx::serve(&request, ui).unwrap_or(Value::Null),
        )
        .and_then(|r| r)
}

fn ok(st: &Arc<ExtState>, ui: &mut dyn Ui, src: &str) -> Value {
    js(st, ui, src).unwrap_or_else(|e| panic!("script failed: {e}"))
}

/// select / confirm / input / editor / notify
#[test]
fn ui_dialogs() {
    let (st, _host) = state(PathBuf::from("/work"), |_| Ok(Value::Null));
    let mut ui = TestUi {
        select: Some(1),
        confirm: Some(true),
        input: Some("typed".into()),
        editor: None,
        ..TestUi::default()
    };
    let got = ok(
        &st,
        &mut ui,
        r#"
        var c = __oxa_make_ctx();
        [
          c.ui.select("pick", ["a", "b"]),
          c.ui.confirm("sure?", "body"),
          c.ui.input("name", "who"),
          c.ui.editor("text", "pre"),
          (c.ui.notify("careful", "warning"), "notified")
        ]
        "#,
    );
    // pi: select answers the chosen option string; a dismissed editor is undefined.
    assert_eq!(got, json!(["b", true, "typed", null, "notified"]));
    assert_eq!(ui.notes, vec![(Level::Warning, "careful".to_string())]);
    assert_eq!(ok(&st, &mut ui, "__oxa_make_ctx().hasUI"), json!(true));
}

/// entries, branch, leaf
#[test]
fn session_read_api() {
    let (st, host) = state(PathBuf::from("/work"), |a| match a {
        HostAction::GetEntries => Ok(json!([{"id": "aaaa0001"}, {"id": "aaaa0002"}])),
        HostAction::GetBranch => Ok(json!([{"id": "aaaa0002"}])),
        _ => Ok(Value::Null),
    });
    let mut ui = TestUi::default();
    let got = ok(
        &st,
        &mut ui,
        r#"
        var sm = __oxa_make_ctx().sessionManager;
        [sm.getEntries().length, sm.getBranch()[0].id, sm.getLeafId()]
        "#,
    );
    assert_eq!(got, json!([2, "aaaa0002", "aaaa0002"]));
    let seen = host.seen.lock().unwrap();
    assert!(matches!(seen[0], HostAction::GetEntries));
    assert!(matches!(seen[1], HostAction::GetBranch));
}

/// values
#[test]
fn compact_and_context_usage() {
    let (st, host) = state(PathBuf::from("/work"), |a| match a {
        HostAction::ContextUsage => Ok(json!({"tokens": 10, "contextWindow": 100, "percent": 10.0})),
        _ => Ok(Value::Null),
    });
    let mut ui = TestUi::default();
    let got = ok(
        &st,
        &mut ui,
        r#"
        var c = __oxa_make_ctx();
        var usage = c.getContextUsage();
        c.compact({ customInstructions: "keep the todo list" });
        [usage.tokens, usage.contextWindow, usage.percent]
        "#,
    );
    assert_eq!(got, json!([10, 100, 10]));
    let seen = host.seen.lock().unwrap();
    assert!(matches!(
        &seen[1],
        HostAction::Compact { instructions: Some(s) } if s == "keep the todo list"
    ));
}

/// newSession / fork / navigateTree / switchSession / reload
#[test]
fn session_shape_actions_deferred_to_settle() {
    let (st, host) = state(PathBuf::from("/work"), |_| Ok(Value::Null));
    let mut ui = TestUi::default();
    ok(
        &st,
        &mut ui,
        r#"
        var c = __oxa_make_ctx();
        c.newSession();
        c.fork("aaaa0001");
        c.navigateTree("aaaa0002", { summarize: true });
        c.switchSession("/s/other.jsonl");
        c.reload();
        "#,
    );
    let seen = host.seen.lock().unwrap();
    assert!(matches!(seen[0], HostAction::NewSession));
    assert!(matches!(&seen[1], HostAction::Fork { entry } if entry == "aaaa0001"));
    assert!(matches!(
        &seen[2],
        HostAction::NavigateTree { target, summarize: true } if target == "aaaa0002"
    ));
    assert!(matches!(&seen[3], HostAction::SwitchSession { path } if path == "/s/other.jsonl"));
    assert!(matches!(seen[4], HostAction::Reload));
}

/// resolves at settle
#[test]
fn wait_for_idle() {
    let (st, host) = state(PathBuf::from("/work"), |_| Ok(Value::Null));
    let mut ui = TestUi::default();
    assert_eq!(ok(&st, &mut ui, "__oxa_make_ctx().waitForIdle()"), Value::Null);
    assert!(matches!(host.seen.lock().unwrap()[0], HostAction::WaitForIdle));
}

/// Ask path and Deny path
#[test]
fn exec_through_policy_and_sandbox() {
    // Deny path: the session refuses; the script sees `denied`.
    let (st, host) = state(PathBuf::from("/work"), |_| {
        Err(OxaError::new(codes::DENIED, "plan mode refuses writes"))
    });
    let mut ui = TestUi::default();
    let got = ok(
        &st,
        &mut ui,
        r#"
        try { oxa.exec("rm", ["-rf", "build dir"], { timeout: 1500 }); "ran" }
        catch (e) { e.code }
        "#,
    );
    assert_eq!(got, json!("denied"));
    let first = host.seen.lock().unwrap()[0].clone();
    match &first {
        HostAction::Exec(ExecSpec {
            command,
            cwd,
            timeout_s,
            ..
        }) => {
            assert_eq!(command, "rm '-rf' 'build dir'");
            assert_eq!(cwd, &PathBuf::from("/work"));
            assert_eq!(*timeout_s, Some(2), "1500 ms rounds up to 2 s");
        }
        other => panic!("expected Exec, got {other:?}"),
    }

    // Ask path: the session answered after the user allowed it; the output comes back.
    let (st, _host) = state(PathBuf::from("/work"), |_| {
        Ok(json!({"output": "hi\n", "exit": 0, "timedOut": false}))
    });
    let got = ok(&st, &mut ui, r#"oxa.exec("echo", ["hi"]).output"#);
    assert_eq!(got, json!("hi\n"));
}

/// read and write
#[test]
fn fs_through_policy() {
    let (st, host) = state(PathBuf::from("/work"), |a| match a {
        HostAction::ReadFile(_) => Ok(json!("contents")),
        _ => Ok(Value::Null),
    });
    let mut ui = TestUi::default();
    let got = ok(
        &st,
        &mut ui,
        r#"
        var fs = __oxa_make_ctx().fs;
        fs.writeFile("notes.txt", "draft");
        fs.readFile("/etc/hosts")
        "#,
    );
    assert_eq!(got, json!("contents"));
    let seen = host.seen.lock().unwrap();
    assert!(matches!(
        &seen[0],
        HostAction::WriteFile { path, data } if path == &PathBuf::from("/work/notes.txt") && data == b"draft"
    ));
    assert!(matches!(&seen[1], HostAction::ReadFile(p) if p == &PathBuf::from("/etc/hosts")));

    // A refused write reaches the script as `denied`.
    let (st, _host) = state(PathBuf::from("/work"), |_| {
        Err(OxaError::new(
            codes::DENIED,
            "write outside the working directory",
        ))
    });
    let got = ok(
        &st,
        &mut ui,
        r#"
        try { __oxa_make_ctx().fs.writeFile("/etc/x", "no"); "wrote" } catch (e) { e.code }
        "#,
    );
    assert_eq!(got, json!("denied"));
}

/// Deny in plan mode
#[test]
fn fetch_needs_net_grant() {
    let (st, host) = state(PathBuf::from("/work"), |_| {
        Err(OxaError::new(codes::DENIED, "plan mode has no network"))
    });
    let mut ui = TestUi::default();
    let got = ok(
        &st,
        &mut ui,
        r#"
        try { __oxa_make_ctx().fetch("http://example.test/x", { method: "post", body: "{}" }); "fetched" }
        catch (e) { e.code }
        "#,
    );
    assert_eq!(got, json!("denied"));
    let first = host.seen.lock().unwrap()[0].clone();
    match &first {
        HostAction::Fetch {
            url, method, body, ..
        } => {
            assert_eq!(url, "http://example.test/x");
            assert_eq!(method, "POST");
            assert_eq!(body.as_deref(), Some(b"{}".as_slice()));
        }
        other => panic!("expected Fetch, got {other:?}"),
    }
}

/// GetState during a run
#[test]
fn host_queries_answer_mid_run() {
    let (st, _host) = state(PathBuf::from("/work"), |a| match a {
        HostAction::GetState => Ok(json!({"isStreaming": true, "mode": "plan"})),
        _ => Ok(Value::Null),
    });
    let mut ui = TestUi::default();
    let got = ok(
        &st,
        &mut ui,
        r#"
        var state = __oxa_ctx.query("GetState", {});
        [state.isStreaming, state.mode, __oxa_make_ctx().isIdle()]
        "#,
    );
    assert_eq!(got, json!([true, "plan", false]));
}

/// no hang
#[test]
fn dropped_session_answers_internal() {
    // A session that has ended answers `internal`; the script sees that code and carries on.
    let (st, _host) = state(PathBuf::from("/work"), |_| {
        Err(OxaError::new(
            codes::INTERNAL,
            "the session that serves this host has ended",
        ))
    });
    let mut ui = TestUi::default();
    let got = ok(
        &st,
        &mut ui,
        r#"
        try { __oxa_make_ctx().sessionManager.getEntries(); "answered" } catch (e) { e.code }
        "#,
    );
    assert_eq!(got, json!("internal"));
}

/// Without an attached session the host functions fail `busy` (the state before `attach`).
#[test]
fn no_session_attached_is_busy() {
    let st = Arc::new(ExtState::new(PathBuf::from("/work")));
    let mut ui = TestUi::default();
    let got = ok(
        &st,
        &mut ui,
        r#"
        try { __oxa_make_ctx().getContextUsage(); "answered" } catch (e) { e.code }
        "#,
    );
    assert_eq!(got, json!("busy"));
}

/// The `ui` serve function leaves other upcalls alone.
#[test]
fn serve_ignores_foreign_ops() {
    let mut ui = TestUi::default();
    assert_eq!(api_ctx::serve(&json!({"op": "set_status"}), &mut ui), None);
    assert_eq!(
        api_ctx::serve(&json!({"op": "confirm", "title": "t", "body": "b"}), &mut ui),
        Some(Value::Null)
    );
}
