//! O-182, #202: a subagent child inherits the parent's autonomy level (`--standard`, `--auto`
//! or `--yolo`). The switch is read when each child's argv is built, so switching it between
//! two children changes the second child only.
//!
//! The child is a shell script that dumps its argv to `dump-<hint>.txt` (the same shape as the
//! E11 tests in `e11_builtin_subagent.rs`, which own the rest of the subagent behaviour).

use oxa_config::Settings;
use oxa_core::{AgentDef, SubagentHooks};
use oxa_fake::TestEnv;
use oxa_proto::{
    Autonomy, CancelToken, HeadlessUi, Hooks, Host, HostAction, Mode, OxaError, Tool, ToolCtx, Value,
    YoloSwitch,
};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

/// The parent's engine limits: a large model, session `sess`.
struct Parent;

impl Host for Parent {
    fn call(&self, action: HostAction) -> Result<Value, OxaError> {
        match action {
            HostAction::EngineLimits => Ok(json!({
                "sessionId": "sess",
                "model": "fake-model",
                "contextWindow": 131072,
                "maxOutput": 4096,
                "compactAt": null,
                "softTokens": 1000,
                "hintClear": true,
                "prefillOnly": true,
            })),
            _ => Ok(json!({})),
        }
    }
}

/// A child that only dumps its argv (one word per line, `arg=<word>`).
fn child_script(env: &TestEnv) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let root = env.root().display().to_string();
    let text = format!(
        r#"#!/bin/sh
for a; do printf 'arg=%s\n' "$a"; done > "{root}/dump-$OXA_SESSION_HINT.txt"
"#
    );
    let path = env.root().join("bin/oxa-child");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn scout() -> AgentDef {
    AgentDef {
        name: "scout".to_string(),
        description: "the scout agent".to_string(),
        tools: Some(vec!["read".to_string()]),
        model: None,
        system_prompt: "You are scout.".to_string(),
        path: PathBuf::from("/agents/scout.md"),
        project: false,
    }
}

fn hooks(env: &TestEnv, yolo: Option<&YoloSwitch>) -> SubagentHooks {
    let mut h = SubagentHooks::new(child_script(env), vec![scout()])
        .with_settings(&Settings::from_values(Value::Null, Value::Null));
    if let Some(y) = yolo {
        h = h.with_yolo(y.clone());
    }
    h.attach(Arc::new(Parent));
    h
}

/// One subagent call of `h` (a single child); the child's argv words.
fn child_argv(env: &TestEnv, h: &SubagentHooks, hint: &str) -> Vec<String> {
    let mut tools = h.tools();
    assert_eq!(tools.len(), 1, "the tool is declared");
    let tool: Arc<dyn Tool> = tools.remove(0);
    let sandbox = oxa_sandbox::NoSandbox;
    let spill = env.tmp().join("spill");
    let work = env.work();
    let cancel = CancelToken::new();
    let cx = ToolCtx {
        checked: &[],
        cwd: &work,
        spill_dir: &spill,
        sandbox: &sandbox,
        mode: Mode::Workspace,
        cancel: &cancel,
        env: &[],
        tune: Default::default(),
        hidden: None,
    };
    let mut ui = HeadlessUi::default();
    let args = json!({"agent": "scout", "task": "look around"});
    let _ = tool.run_with_ui(&args, &cx, &mut |_| {}, &mut ui);
    let dump = std::fs::read_to_string(env.root().join(format!("dump-{hint}.txt")))
        .unwrap_or_else(|_| panic!("child {hint} did not run"));
    dump.lines()
        .filter_map(|l| l.strip_prefix("arg=").map(str::to_string))
        .collect()
}

/// The level flags of a child's argv.
fn levels(argv: &[String]) -> Vec<&str> {
    argv.iter()
        .map(String::as_str)
        .filter(|w| ["--standard", "--auto", "--yolo"].contains(w))
        .collect()
}

#[test]
fn child_argv_carries_the_parents_level_at_start() {
    let env = TestEnv::new();
    let switch = YoloSwitch::new(Autonomy::Standard);
    let h = hooks(&env, Some(&switch));

    // Standard is explicit too: the child must not take a higher default from its settings.
    let std_argv = child_argv(&env, &h, "sess.s1");
    assert_eq!(levels(&std_argv), ["--standard"], "{std_argv:?}");
    let at = std_argv.iter().position(|w| w == "--standard").unwrap();
    assert_eq!(std_argv[at - 2], "--permission-mode", "{std_argv:?}");
    assert_eq!(std_argv[at - 1], "workspace", "{std_argv:?}");
    assert_eq!(std_argv.last().unwrap(), "Task: look around");

    switch.set_level(Autonomy::Yolo);
    let yolo = child_argv(&env, &h, "sess.s2");
    assert_eq!(levels(&yolo), ["--yolo"], "{yolo:?}");
    // Nothing else changed.
    let strip = |v: &[String]| -> Vec<String> {
        v.iter()
            .filter(|w| levels(std::slice::from_ref(*w)).is_empty())
            .cloned()
            .collect()
    };
    assert_eq!(strip(&yolo), strip(&std_argv));

    // A later child sees the new state.
    switch.set_level(Autonomy::Auto);
    assert_eq!(levels(&child_argv(&env, &h, "sess.s3")), ["--auto"]);
}

#[test]
fn hooks_without_a_switch_add_no_level() {
    let env = TestEnv::new();
    let plain = child_argv(&env, &hooks(&env, None), "sess.s1");
    assert!(levels(&plain).is_empty(), "{plain:?}");
}

#[test]
fn the_switch_is_shared_not_copied() {
    // A clone of the switch the process holds is the one the hooks read (no copy).
    let env = TestEnv::new();
    let switch = YoloSwitch::new(Autonomy::Standard);
    let h = hooks(&env, Some(&switch));
    let held = switch.clone();
    held.set_level(Autonomy::Yolo);
    assert_eq!(levels(&child_argv(&env, &h, "sess.s1")), ["--yolo"]);
}
