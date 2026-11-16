//! O-182: a subagent child runs with `--yolo` while the parent's YOLO switch is on, and with the
//! argv it always had while it is off. The switch is read when each child's argv is built, so
//! toggling it between two children changes the second child only.
//!
//! The child is a shell script that dumps its argv to `dump-<hint>.txt` (the same shape as the
//! E11 tests in `e11_builtin_subagent.rs`, which own the rest of the subagent behaviour).

use oxa_config::Settings;
use oxa_core::{AgentDef, SubagentHooks};
use oxa_fake::TestEnv;
use oxa_proto::{
    CancelToken, HeadlessUi, Hooks, Host, HostAction, Mode, OxaError, Tool, ToolCtx, Value, YoloSwitch,
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

fn has_yolo(argv: &[String]) -> bool {
    argv.iter().any(|w| w == "--yolo")
}

#[test]
fn child_argv_carries_yolo_only_while_the_switch_is_on() {
    let env = TestEnv::new();
    let switch = YoloSwitch::new(false);
    let h = hooks(&env, Some(&switch));

    // Off: the argv the child always had.
    let off = child_argv(&env, &h, "sess.s1");
    assert!(!has_yolo(&off), "{off:?}");

    // On: `--yolo` right after the permission mode, before the tools and the task.
    switch.set(true);
    let on = child_argv(&env, &h, "sess.s2");
    assert!(has_yolo(&on), "{on:?}");
    let at = on.iter().position(|w| w == "--yolo").unwrap();
    assert_eq!(on[at - 2], "--permission-mode", "{on:?}");
    assert_eq!(on[at - 1], "workspace", "{on:?}");
    assert_eq!(on.last().unwrap(), "Task: look around");
    // Nothing else changed: the argv of the switch-on child is the off argv plus `--yolo`.
    let mut without = on.clone();
    without.remove(at);
    assert_eq!(without, off, "only `--yolo` is added");

    // Off again: a later child sees the new state.
    switch.set(false);
    let again = child_argv(&env, &h, "sess.s3");
    assert!(!has_yolo(&again), "{again:?}");
    assert_eq!(again, off);
}

#[test]
fn hooks_without_a_switch_never_add_yolo() {
    let env = TestEnv::new();
    let plain = child_argv(&env, &hooks(&env, None), "sess.s1");
    let switch = YoloSwitch::new(false);
    let off = child_argv(&env, &hooks(&env, Some(&switch)), "sess.s1");
    assert!(!has_yolo(&plain));
    assert_eq!(
        plain, off,
        "with the switch off the argv is byte-identical to no switch"
    );
}

#[test]
fn the_switch_is_shared_not_copied() {
    // A clone of the switch the process holds is the one the hooks read (no copy).
    let env = TestEnv::new();
    let switch = YoloSwitch::new(false);
    let h = hooks(&env, Some(&switch));
    let held = switch.clone();
    held.set(true);
    assert!(has_yolo(&child_argv(&env, &h, "sess.s1")));
}
