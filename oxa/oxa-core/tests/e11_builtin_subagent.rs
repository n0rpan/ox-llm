//! E11 acceptance tests: builtin:subagent.
//! One test per acceptance bullet of WAVES.toml (plus the OXA-ENGINE-SUPPORT sec. 4 amendments).
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.
//!
//! The child binary here is a small shell script written into the test directory: it prints
//! JSON-mode records (what `oxa --mode json` writes), dumps its argv, cwd and `OXA_*`
//! environment to `dump-<hint>.txt` and logs `start <hint>` / `end <hint>` to `calls.log`. The
//! `Host` is a recording mock that also logs `park` / `clear <hint>` to the same file, so the
//! order of parks, children and clears across processes is one file. The real binary against
//! the fake engine (headers, cache) is tested in `oxa-cli/tests/e11_subagent_processes.rs`.

use oxa_config::{Dirs, Settings};
use oxa_core::{discover_agents, AgentDef, SubagentHooks};
use oxa_fake::TestEnv;
use oxa_proto::{
    codes, CancelToken, HeadlessUi, HookPoint, Hooks, Host, HostAction, Level, Mode, OxaError, Tool, ToolCtx,
    ToolOutput, Value,
};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- helpers

/// A recording `Host`: answers `EngineLimits`, `ParkSession` and `ClearEngineHint`.
struct MockHost {
    log: PathBuf,
    calls: Mutex<Vec<HostAction>>,
    limits: Mutex<Value>,
    park: Value,
    clear_fails: bool,
    /// Model ids `HostAction::ModelInfo` reports as remote (cloud through oxc).
    remote: Mutex<Vec<String>>,
}

impl MockHost {
    fn new(env: &TestEnv) -> Arc<MockHost> {
        Arc::new(MockHost {
            log: env.root().join("calls.log"),
            calls: Mutex::new(Vec::new()),
            limits: Mutex::new(limits("sess", 131072, None)),
            park: json!({"parked": true, "promptTokens": 100, "snapPos": 100}),
            clear_fails: false,
            remote: Mutex::new(Vec::new()),
        })
    }
    fn with(env: &TestEnv, limits: Value, park: Value, clear_fails: bool) -> Arc<MockHost> {
        Arc::new(MockHost {
            log: env.root().join("calls.log"),
            calls: Mutex::new(Vec::new()),
            limits: Mutex::new(limits),
            park,
            clear_fails,
            remote: Mutex::new(Vec::new()),
        })
    }
    fn calls(&self) -> Vec<HostAction> {
        self.calls.lock().unwrap().clone()
    }
    fn clears(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|a| match a {
                HostAction::ClearEngineHint(h) => Some(h),
                _ => None,
            })
            .collect()
    }
    fn parks(&self) -> usize {
        self.calls()
            .iter()
            .filter(|a| matches!(a, HostAction::ParkSession))
            .count()
    }
}

fn append(path: &Path, line: &str) {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(f, "{line}").unwrap();
}

impl Host for MockHost {
    fn call(&self, action: HostAction) -> Result<Value, OxaError> {
        self.calls.lock().unwrap().push(action.clone());
        match action {
            HostAction::EngineLimits => Ok(self.limits.lock().unwrap().clone()),
            HostAction::AccountRunTokens(tokens) => {
                let mut limits = self.limits.lock().unwrap();
                if let Some(left) = limits["remainingTokens"].as_u64() {
                    limits["remainingTokens"] = json!(left.saturating_sub(tokens));
                }
                Ok(Value::Null)
            }
            HostAction::ModelInfo(m) => {
                let remote = self.remote.lock().unwrap().contains(&m);
                Ok(json!({"model": m, "remote": remote}))
            }
            HostAction::ParkSession => {
                append(&self.log, "park");
                Ok(self.park.clone())
            }
            HostAction::ClearEngineHint(h) => {
                append(&self.log, &format!("clear {h}"));
                if self.clear_fails {
                    Err(OxaError::new(codes::BUSY, "the engine is busy"))
                } else {
                    Ok(json!({"cleared": true}))
                }
            }
            _ => Err(OxaError::new(codes::INTERNAL, "not in the mock")),
        }
    }
}

fn limits(session: &str, window: u32, compact_at: Option<u32>) -> Value {
    json!({
        "sessionId": session,
        "model": "fake-model",
        "contextWindow": window,
        "maxOutput": 4096,
        "compactAt": compact_at,
        "softTokens": 1000,
        "hintClear": true,
        "prefillOnly": true,
    })
}

/// The JSON-mode records of a successful child: a tool call, then the final text
/// `done: <last argv word>`.
const OK_BODY: &str = r#"printf '%s\n' '{"type":"session","version":3,"id":"child"}'
printf '%s\n' '{"type":"tool_execution_start","toolCallId":"c1","toolName":"read","args":{"path":"notes.txt"}}'
printf '{"type":"message_end","message":{"role":"assistant","content":[{"type":"toolCall","id":"c1","name":"read","arguments":{}}],"stopReason":"toolUse"}}\n'
printf '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"done: %s"}],"stopReason":"stop"}}\n' "$last"
printf '%s\n' '{"type":"agent_end","messages":[]}'
"#;

/// A child that runs until it is killed: its own pid and a grandchild (`sleep`) in its group.
const HANG_BODY: &str = r#"echo $$ > "$ROOT/pid-$hint"
sleep 30 &
echo $! > "$ROOT/sleep-$hint"
wait
"#;

/// Write the child script (`body` runs between the dump and the `end` line).
fn child_script(env: &TestEnv, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let root = env.root().display().to_string();
    let text = format!(
        r#"#!/bin/sh
ROOT='{root}'
last=""
for a; do last="$a"; done
hint="$OXA_SESSION_HINT"
{{ for a; do printf 'arg=%s\n' "$a"; done; printf 'cwd=%s\n' "$(pwd)"; env | grep '^OXA_' | sort; }} > "$ROOT/dump-$hint.txt"
echo "start $hint" >> "$ROOT/calls.log"
{body}
echo "end $hint" >> "$ROOT/calls.log"
"#
    );
    let path = env.root().join("bin/oxa-child");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn agent(name: &str, model: Option<&str>) -> AgentDef {
    AgentDef {
        name: name.to_string(),
        description: format!("the {name} agent"),
        tools: Some(vec!["read".to_string(), "grep".to_string()]),
        model: model.map(str::to_string),
        system_prompt: format!("You are {name}."),
        path: PathBuf::from(format!("/agents/{name}.md")),
        project: false,
    }
}

fn hooks_with(bin: PathBuf, agents: Vec<AgentDef>, settings: Value, host: &Arc<MockHost>) -> SubagentHooks {
    let h = SubagentHooks::new(bin, agents).with_settings(&Settings::from_values(settings, Value::Null));
    h.attach(host.clone());
    h
}

fn hooks(env: &TestEnv, body: &str, host: &Arc<MockHost>) -> SubagentHooks {
    hooks_with(
        child_script(env, body),
        vec![agent("scout", None), agent("worker", None)],
        Value::Null,
        host,
    )
}

fn tool(h: &SubagentHooks) -> Arc<dyn Tool> {
    let mut tools = h.tools();
    assert_eq!(tools.len(), 1, "one tool");
    tools.remove(0)
}

struct Outcome {
    out: ToolOutput,
    live: Vec<String>,
    notes: Vec<(Level, String)>,
}

fn run_with(env: &TestEnv, h: &SubagentHooks, args: Value, cancel: &CancelToken) -> Outcome {
    let t = tool(h);
    let sandbox = oxa_sandbox::NoSandbox;
    let spill = env.tmp().join("spill");
    let work = env.work();
    let cx = ToolCtx {
        checked: &[],
        cwd: &work,
        spill_dir: &spill,
        sandbox: &sandbox,
        mode: Mode::Workspace,
        cancel,
        env: &[],
        tune: Default::default(),
        hidden: None,
    };
    let mut live = Vec::new();
    let mut ui = HeadlessUi::default();
    assert!(t.needs_ui(), "serial with the UI, never in a parallel batch");
    let out = t.run_with_ui(&args, &cx, &mut |s| live.push(s.to_string()), &mut ui);
    Outcome {
        out,
        live,
        notes: ui.notes,
    }
}

fn run(env: &TestEnv, h: &SubagentHooks, args: Value) -> Outcome {
    run_with(env, h, args, &CancelToken::new())
}

fn log(env: &TestEnv) -> Vec<String> {
    std::fs::read_to_string(env.root().join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// The argv words of the child with `hint`.
fn argv(env: &TestEnv, hint: &str) -> Vec<String> {
    dump(env, hint)
        .lines()
        .filter_map(|l| l.strip_prefix("arg=").map(str::to_string))
        .collect()
}

fn dump(env: &TestEnv, hint: &str) -> String {
    std::fs::read_to_string(env.root().join(format!("dump-{hint}.txt")))
        .unwrap_or_else(|_| panic!("child {hint} did not run"))
}

fn env_of(env: &TestEnv, hint: &str, name: &str) -> Option<String> {
    dump(env, hint)
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name}=")).map(str::to_string))
}

fn wait_for(path: &Path) {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if text.ends_with('\n') && text.trim().parse::<u32>().is_ok_and(|pid| pid > 0) {
                return;
            }
        }
        assert!(
            Instant::now() < end,
            "{} never contained a complete PID line",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn pid_in(path: &Path) -> u32 {
    std::fs::read_to_string(path).unwrap().trim().parse().unwrap()
}

/// File creation and even a PID prefix are not a completed readiness record.
#[test]
fn readiness_waits_for_the_complete_pid_line() {
    let env = TestEnv::new();
    let path = env.root().join("partial-pid");
    std::fs::write(&path, "12").unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            // Deliberately slow publication, not a delay used to infer readiness.
            std::thread::sleep(Duration::from_millis(100));
            append(&path, "345");
        });
        wait_for(&path);
        assert_eq!(pid_in(&path), 12345, "wait for the newline, not file creation");
    });
}

/// `true` while `pid` exists and is not a zombie.
fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        Err(_) => false,
    }
}

fn gone_soon(pid: u32) -> bool {
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

// ---------------------------------------------------------------- acceptance

/// user and trusted project agents, override rule
#[test]
fn agent_discovery() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    env.write(
        "home/.oxa/agent/agents/scout.md",
        "---\nname: scout\ndescription: user scout\ntools: read, grep\nmodel: m1\n---\nYou scout.\n",
    );
    env.write(
        "home/.oxa/agent/agents/worker.md",
        "---\nname: worker\ndescription: \"user worker\"\ntools: [read, bash]\n---\nYou work.\n",
    );
    env.write(
        "home/.oxa/agent/agents/broken.md",
        "---\nname: broken\n---\nno description\n",
    );
    env.write(
        "home/.oxa/agent/agents/notes.txt",
        "---\nname: x\ndescription: y\n---\n",
    );
    env.write(
        "work/.oxa/agents/scout.md",
        "---\nname: scout\ndescription: project scout\n---\nProject scout.\n",
    );
    env.write(
        "work/.oxa/agents/planner.md",
        "---\nname: planner\ndescription: project planner\n---\nPlan.\n",
    );

    let untrusted = discover_agents(&dirs, &env.work(), false);
    let names: Vec<&str> = untrusted.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["scout", "worker"], "untrusted: user agents only, sorted");
    let scout = &untrusted[0];
    assert_eq!(scout.description, "user scout");
    assert_eq!(scout.tools, Some(vec!["read".to_string(), "grep".to_string()]));
    assert_eq!(scout.model.as_deref(), Some("m1"));
    assert_eq!(scout.system_prompt, "You scout.\n");
    assert!(!scout.project);
    assert_eq!(
        untrusted[1].tools,
        Some(vec!["read".to_string(), "bash".to_string()]),
        "the [a, b] form"
    );
    assert_eq!(untrusted[1].description, "user worker");

    let trusted = discover_agents(&dirs, &env.work(), true);
    let names: Vec<&str> = trusted.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["planner", "scout", "worker"]);
    let scout = trusted.iter().find(|a| a.name == "scout").unwrap();
    assert!(
        scout.project,
        "the project agent overrides the user agent of the same name"
    );
    assert_eq!(scout.description, "project scout");
    assert_eq!(scout.model, None);

    // The list is part of the tool description (frozen prefix): sorted, configuration only.
    let h = SubagentHooks::new(PathBuf::from("/bin/false"), trusted);
    let d = tool(&h).spec().description.clone();
    assert!(
        d.ends_with(
            "Available agents: planner (project): project planner; scout (project): project scout; worker (user): user worker."
        ),
        "{d}"
    );
    assert_eq!(tool(&h).spec().name, "subagent");
}

/// one child, final text returned
#[test]
fn single() {
    let env = TestEnv::new();
    let host = MockHost::new(&env);
    let h = hooks(&env, OK_BODY, &host);
    let o = run(&env, &h, json!({"agent": "scout", "task": "look around"}));
    assert!(!o.out.is_error, "{}", o.out.text);
    assert_eq!(o.out.text, "done: Task: look around", "the final text only (W4)");

    let words = argv(&env, "sess.s1");
    assert_eq!(
        &words[..6],
        &["--mode", "json", "-p", "--no-session", "--model", "fake-model"]
    );
    let joined = words.join(" ");
    assert!(joined.contains("--permission-mode workspace"), "{joined}");
    assert!(joined.contains("--tools read,grep"), "{joined}");
    assert!(
        joined.contains("--append-system-prompt You are scout."),
        "{joined}"
    );
    assert_eq!(words.last().unwrap(), "Task: look around");
    assert_eq!(
        env_of(&env, "sess.s1", "cwd").as_deref(),
        Some(env.work().to_str().unwrap())
    );

    // Unknown agent: pi's text, no child, no park.
    let o = run(&env, &h, json!({"agent": "nobody", "task": "x"}));
    assert!(o.out.is_error);
    assert_eq!(
        o.out.text,
        "Agent failed: Error: Unknown agent: \"nobody\". Available agents: \"scout\", \"worker\"."
    );
    // Not exactly one mode.
    let o = run(&env, &h, json!({"agent": "scout"}));
    assert!(o.out.is_error);
    assert_eq!(
        o.out.text,
        "Invalid parameters. Provide exactly one mode.\nAvailable agents: scout (user), worker (user)"
    );
    assert_eq!(host.parks(), 1, "only the call that started a child parked");
}

/// {previous} substitution
#[test]
fn chain() {
    let env = TestEnv::new();
    let host = MockHost::new(&env);
    let h = hooks(&env, OK_BODY, &host);
    let o = run(
        &env,
        &h,
        json!({"chain": [
            {"agent": "scout", "task": "find alpha"},
            {"agent": "worker", "task": "use {previous} now"},
        ]}),
    );
    assert!(!o.out.is_error, "{}", o.out.text);
    assert_eq!(
        argv(&env, "sess.s2").last().unwrap(),
        "Task: use done: Task: find alpha now"
    );
    assert_eq!(o.out.text, "done: Task: use done: Task: find alpha now");

    // A failing step stops the chain (pi's text).
    let o = run(
        &env,
        &h,
        json!({"chain": [
            {"agent": "scout", "task": "a"},
            {"agent": "ghost", "task": "{previous}"},
            {"agent": "scout", "task": "never"},
        ]}),
    );
    assert!(o.out.is_error);
    assert!(
        o.out
            .text
            .starts_with("Chain stopped at step 2 (ghost): Error: Unknown agent: \"ghost\""),
        "{}",
        o.out.text
    );
    assert!(
        !env.root().join("dump-sess.s4.txt").exists(),
        "the third step never ran"
    );
}

/// concurrency 1: at most one child process alive (tasks run in call order, shown as a
/// queue); more than 8 tasks refused with pi's error text
#[test]
fn parallel_is_queued() {
    let env = TestEnv::new();
    let host = MockHost::new(&env);
    let h = hooks(&env, OK_BODY, &host);
    let o = run(
        &env,
        &h,
        json!({"tasks": [
            {"agent": "scout", "task": "one"},
            {"agent": "worker", "task": "two"},
            {"agent": "scout", "task": "three"},
        ]}),
    );
    assert!(!o.out.is_error, "{}", o.out.text);
    assert_eq!(
        o.out.text,
        "Parallel: 3/3 succeeded\n\n### [scout] completed\n\ndone: Task: one\n\n---\n\n### [worker] completed\n\ndone: Task: two\n\n---\n\n### [scout] completed\n\ndone: Task: three"
    );
    let starts_ends: Vec<String> = log(&env)
        .into_iter()
        .filter(|l| l.starts_with("start") || l.starts_with("end"))
        .collect();
    assert_eq!(
        starts_ends,
        [
            "start sess.s1",
            "end sess.s1",
            "start sess.s2",
            "end sess.s2",
            "start sess.s3",
            "end sess.s3"
        ],
        "one child at a time, in call order"
    );
    assert!(
        o.live
            .iter()
            .any(|l| l.contains("subagent 2/3, queued") && l.contains("subagent 3/3, queued")),
        "{:?}",
        o.live
    );
    assert!(o
        .live
        .iter()
        .any(|l| l.starts_with("subagent 1/3 scout: done\nsubagent 2/3 worker: ")));

    // Two calls at once on one hook set (codemode's allSettled): still one child at a time.
    let h = Arc::new(h);
    std::fs::remove_file(env.root().join("calls.log")).unwrap();
    let threads: Vec<_> = (0..2)
        .map(|i| {
            let h = h.clone();
            let root = env.root().to_path_buf();
            std::thread::spawn(move || {
                let t = tool(&h);
                let sandbox = oxa_sandbox::NoSandbox;
                let spill = root.join("tmp");
                let work = root.join("work");
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
                t.run(
                    &json!({"agent": "scout", "task": format!("c{i}")}),
                    &cx,
                    &mut |_| {},
                )
            })
        })
        .collect();
    for t in threads {
        assert!(!t.join().unwrap().is_error);
    }
    let lines: Vec<String> = log(&env)
        .into_iter()
        .filter(|l| l.starts_with("start") || l.starts_with("end"))
        .collect();
    assert_eq!(lines.len(), 4);
    for pair in lines.chunks(2) {
        assert_eq!(pair[0].replace("start", "end"), pair[1], "{lines:?}");
    }

    // More than 8: refused before anything starts.
    let parks = host.parks();
    let nine: Vec<Value> = (0..9)
        .map(|i| json!({"agent": "scout", "task": format!("t{i}")}))
        .collect();
    let o = run(&env, &h, json!({ "tasks": nine }));
    assert!(o.out.is_error);
    assert_eq!(o.out.text, "Too many parallel tasks (9). Max is 8.");
    assert_eq!(host.parks(), parks, "no park for a refused call");
    assert_eq!(SubagentHooks::MAX_PARALLEL_TASKS, 8);
}

/// live updates
#[test]
fn child_events_streamed() {
    let env = TestEnv::new();
    let host = MockHost::new(&env);
    let h = hooks(&env, OK_BODY, &host);
    let o = run(&env, &h, json!({"agent": "scout", "task": "x"}));
    assert!(!o.out.is_error);
    assert!(
        o.live.contains(&"subagent 1/1 scout: running".to_string()),
        "{:?}",
        o.live
    );
    assert!(
        o.live.contains(&"subagent 1/1 scout: read notes.txt".to_string()),
        "{:?}",
        o.live
    );
    assert!(
        o.live.contains(&"subagent 1/1 scout: done: Task: x".to_string()),
        "{:?}",
        o.live
    );
    assert_eq!(o.live.last().unwrap(), "subagent 1/1 scout: done");
    // Progress is display only: the tool result holds the final text and nothing else.
    assert!(!o.out.text.contains("notes.txt"));
    // Deterministic: the same child gives the same lines.
    let again = run(&env, &h, json!({"agent": "scout", "task": "x"}));
    assert_eq!(again.live, o.live);
}

/// process groups: the cancel token kills the child and its grandchildren; `shutdown` too
#[test]
fn abort_kills_children() {
    let env = TestEnv::new();
    let host = MockHost::new(&env);
    let h = Arc::new(hooks(&env, HANG_BODY, &host));

    // The tool's cancel token.
    let cancel = CancelToken::new();
    let started = {
        let (h, root, cancel) = (h.clone(), env.root().to_path_buf(), cancel.clone());
        std::thread::spawn(move || {
            let env2 = TestEnvRef(root);
            run_in(&env2, &h, json!({"agent": "scout", "task": "hang"}), &cancel)
        })
    };
    wait_for(&env.root().join("sleep-sess.s1"));
    let (child, grandchild) = (
        pid_in(&env.root().join("pid-sess.s1")),
        pid_in(&env.root().join("sleep-sess.s1")),
    );
    assert!(alive(child) && alive(grandchild));
    let t0 = Instant::now();
    cancel.cancel();
    let out = started.join().unwrap();
    assert!(t0.elapsed() < Duration::from_secs(5), "returns promptly");
    assert!(out.is_error);
    assert!(
        out.text.starts_with("Agent aborted: Error: Subagent was aborted"),
        "{}",
        out.text
    );
    assert!(gone_soon(child), "the child is gone");
    assert!(gone_soon(grandchild), "its process group is gone (no orphan)");

    // `Hooks::shutdown` (exit, `/reload`).
    let started = {
        let (h, root) = (h.clone(), env.root().to_path_buf());
        std::thread::spawn(move || {
            let env2 = TestEnvRef(root);
            run_in(
                &env2,
                &h,
                json!({"agent": "scout", "task": "hang"}),
                &CancelToken::new(),
            )
        })
    };
    wait_for(&env.root().join("sleep-sess.s2"));
    let grandchild = pid_in(&env.root().join("sleep-sess.s2"));
    h.shutdown();
    let out = started.join().unwrap();
    assert!(out.is_error, "{}", out.text);
    assert!(gone_soon(grandchild));
}

/// A `TestEnv` path for a helper thread (the env itself stays with the test).
struct TestEnvRef(PathBuf);

fn run_in(env: &TestEnvRef, h: &SubagentHooks, args: Value, cancel: &CancelToken) -> ToolOutput {
    let t = tool(h);
    let sandbox = oxa_sandbox::NoSandbox;
    let spill = env.0.join("tmp");
    let work = env.0.join("work");
    let cx = ToolCtx {
        checked: &[],
        cwd: &work,
        spill_dir: &spill,
        sandbox: &sandbox,
        mode: Mode::Workspace,
        cancel,
        env: &[],
        tune: Default::default(),
        hidden: None,
    };
    t.run(&args, &cx, &mut |_| {})
}

/// one H6+H4 request per batch, before the first child (skipped by the host without
/// `prefill_only`); the parent's next request has `cached` = the park's `prompt_tokens` on the
/// fake (`oxa-cli/tests/e11_subagent_processes.rs`)
#[test]
fn park_request_precedes_first_child() {
    let env = TestEnv::new();
    let host = MockHost::new(&env);
    let h = hooks(&env, OK_BODY, &host);
    let o = run(
        &env,
        &h,
        json!({"tasks": [{"agent": "scout", "task": "a"}, {"agent": "worker", "task": "b"}]}),
    );
    assert!(!o.out.is_error);
    assert_eq!(
        log(&env),
        [
            "park",
            "start sess.s1",
            "end sess.s1",
            "clear sess.s1",
            "start sess.s2",
            "end sess.s2",
            "clear sess.s2"
        ],
        "one park per batch, before the first child; nothing else on the parent meanwhile"
    );
    assert_eq!(host.parks(), 1);

    // An engine without H6: the host answers `{parked: false}`; the children still run.
    let env = TestEnv::new();
    let host = MockHost::with(
        &env,
        limits("sess", 131072, None),
        json!({"parked": false}),
        false,
    );
    let h = hooks(&env, OK_BODY, &host);
    let o = run(&env, &h, json!({"agent": "scout", "task": "a"}));
    assert!(!o.out.is_error);
    assert!(o.notes.is_empty(), "{:?}", o.notes);
    assert_eq!(log(&env)[..2], ["park", "start sess.s1"]);
}

/// X-Oxe-Priority on child requests: the child is told its hint and depth through the
/// environment; `oxa-cli/src/child.rs` turns them into `X-Session-Id` / `X-Oxe-Session`,
/// `X-Oxe-Priority: background` and `X-Oxe-Turn-Kind: subagent` (asserted on the fake engine's
/// recorded headers in `oxa-cli/tests/e11_subagent_processes.rs`)
#[test]
fn children_are_background() {
    let env = TestEnv::new();
    let host = MockHost::new(&env);
    let h = hooks(&env, OK_BODY, &host);
    run(&env, &h, json!({"agent": "scout", "task": "a"}));
    assert_eq!(
        env_of(&env, "sess.s1", "OXA_SESSION_HINT").as_deref(),
        Some("sess.s1")
    );
    assert_eq!(
        env_of(&env, "sess.s1", "OXA_SUBAGENT_DEPTH").as_deref(),
        Some("1")
    );
    assert_eq!(SubagentHooks::ENV_SESSION_HINT, "OXA_SESSION_HINT");
}

// ---------------------------------------------------------------- amendments (O-144)

/// `X-Session-Id` per child: `<parent>.s<n>`, counting up per parent session
#[test]
fn child_session_ids_distinct() {
    let env = TestEnv::new();
    let host = MockHost::new(&env);
    let h = hooks(&env, OK_BODY, &host);
    run(
        &env,
        &h,
        json!({"tasks": [{"agent": "scout", "task": "a"}, {"agent": "scout", "task": "b"}]}),
    );
    run(&env, &h, json!({"agent": "worker", "task": "c"}));
    let starts: Vec<String> = log(&env).into_iter().filter(|l| l.starts_with("start")).collect();
    assert_eq!(starts, ["start sess.s1", "start sess.s2", "start sess.s3"]);

    // Another parent session (a switch re-attaches; the id comes from the host) starts at s1.
    *host.limits.lock().unwrap() = limits("other", 131072, None);
    run(&env, &h, json!({"agent": "worker", "task": "d"}));
    assert_eq!(
        env_of(&env, "other.s1", "OXA_SESSION_HINT").as_deref(),
        Some("other.s1")
    );
}

/// depth 1: every child is told it is one, so its binary does not build `builtin:subagent`
/// (the binary side: `oxa-cli/tests/e11_subagent_processes.rs`, the child's tool list)
#[test]
fn no_nested_subagents() {
    let env = TestEnv::new();
    let host = MockHost::new(&env);
    let h = hooks(&env, OK_BODY, &host);
    run(
        &env,
        &h,
        json!({"chain": [{"agent": "scout", "task": "a"}, {"agent": "scout", "task": "b"}]}),
    );
    for hint in ["sess.s1", "sess.s2"] {
        assert_eq!(env_of(&env, hint, "OXA_SUBAGENT_DEPTH").as_deref(), Some("1"));
    }
    assert_eq!(SubagentHooks::ENV_SUBAGENT_DEPTH, "OXA_SUBAGENT_DEPTH");
}

/// an agent's `model:` other than the session's is ignored with a warning; the child gets the
/// session's model
#[test]
fn agent_model_other_than_session_ignored() {
    let env = TestEnv::new();
    let host = MockHost::new(&env);
    let h = hooks_with(
        child_script(&env, OK_BODY),
        vec![
            agent("scout", Some("big-other-model")),
            agent("worker", Some("fake-model")),
        ],
        Value::Null,
        &host,
    );
    let o = run(
        &env,
        &h,
        json!({"tasks": [{"agent": "scout", "task": "a"}, {"agent": "scout", "task": "b"}, {"agent": "worker", "task": "c"}]}),
    );
    assert!(!o.out.is_error);
    for hint in ["sess.s1", "sess.s2", "sess.s3"] {
        let words = argv(&env, hint).join(" ");
        assert!(words.contains("--model fake-model"), "{words}");
        assert!(!words.contains("big-other-model"), "{words}");
    }
    assert_eq!(
        o.notes,
        [(
            Level::Warning,
            "subagent: agent scout asks for model big-other-model; ignored, the session's model fake-model is used (one model is loaded)"
                .to_string()
        )],
        "one warning per agent and call; none for the session's own model"
    );
}

/// #458: a cloud lead runs remote children concurrently, each on its agent's own `model:`,
/// and the results come back in call order.
#[test]
fn remote_children_run_concurrently_on_their_own_model() {
    let env = TestEnv::new();
    let mut l = limits("sess", 200_000, None);
    l["model"] = json!("cloud-lead");
    l["remote"] = json!(true);
    let host = MockHost::with(&env, l, json!({"parked": false}), false);
    host.remote.lock().unwrap().push("cloud-child".to_string());
    // Every child waits until all three have started: serial children would time out.
    let body = format!(
        r#"i=0
while [ "$(grep -c '^start' "$ROOT/calls.log")" -lt 3 ]; do
  i=$((i+1)); [ $i -gt 250 ] && {{ echo "serial" >&2; exit 3; }}; sleep 0.02
done
case "$last" in *one*) sleep 0.3;; esac
{OK_BODY}"#
    );
    let h = hooks_with(
        child_script(&env, &body),
        vec![agent("scout", Some("cloud-child")), agent("worker", None)],
        Value::Null,
        &host,
    );
    let o = run(
        &env,
        &h,
        json!({"tasks": [
            {"agent": "scout", "task": "one"},
            {"agent": "worker", "task": "two"},
            {"agent": "scout", "task": "three"},
        ]}),
    );
    assert!(!o.out.is_error, "{}", o.out.text);
    assert_eq!(
        o.out.text,
        "Parallel: 3/3 succeeded\n\n### [scout] completed\n\ndone: Task: one\n\n---\n\n### [worker] completed\n\ndone: Task: two\n\n---\n\n### [scout] completed\n\ndone: Task: three"
    );
    let words = |hint: &str| argv(&env, hint).join(" ");
    assert!(
        words("sess.s1").contains("--model cloud-child"),
        "{}",
        words("sess.s1")
    );
    assert!(
        words("sess.s2").contains("--model cloud-lead"),
        "{}",
        words("sess.s2")
    );
    assert!(
        words("sess.s3").contains("--model cloud-child"),
        "{}",
        words("sess.s3")
    );
    assert!(o.notes.is_empty(), "{:?}", o.notes);
    // Remote children hold no engine state: nothing to clear.
    assert_eq!(host.clears().len(), 0);
}

/// A local child under a remote lead still gets its engine state cleared.
#[test]
fn local_child_of_remote_lead_is_cleared() {
    let env = TestEnv::new();
    let mut l = limits("sess", 200_000, None);
    l["model"] = json!("cloud-lead");
    l["remote"] = json!(true);
    let host = MockHost::with(&env, l, json!({"parked": false}), false);
    let h = hooks_with(
        child_script(&env, OK_BODY),
        vec![agent("scout", Some("local-child"))],
        Value::Null,
        &host,
    );
    let o = run(&env, &h, json!({"agent": "scout", "task": "one"}));
    assert!(!o.out.is_error, "{}", o.out.text);
    assert_eq!(host.clears(), vec!["sess.s1".to_string()]);
}

/// child soft limit = min(`oxa.subagent.softTokens`, the parent's `compactAt`), below the cap
#[test]
fn child_limits_from_model_limits() {
    let cases: [(Value, Option<Value>, &str); 4] = [
        (Value::Null, None, "24000"),
        (Value::Null, Some(limits("sess", 131072, Some(20000))), "20000"),
        (
            json!({"oxa": {"subagent": {"softTokens": 9000, "smallContext": true}}}),
            Some(limits("sess", 16384, Some(12000))),
            "9000",
        ),
        (
            json!({"oxa": {"subagent": {"smallContext": true}}}),
            Some(limits("sess", 16384, None)),
            "16383",
        ),
    ];
    for (settings, lim, want) in cases {
        let env = TestEnv::new();
        let host = match lim {
            Some(l) => MockHost::with(&env, l, json!({"parked": true}), false),
            None => MockHost::new(&env),
        };
        let h = hooks_with(
            child_script(&env, OK_BODY),
            vec![agent("scout", None)],
            settings,
            &host,
        );
        run(&env, &h, json!({"agent": "scout", "task": "a"}));
        assert_eq!(
            env_of(&env, "sess.s1", "OXA_CHILD_SOFT_TOKENS").as_deref(),
            Some(want)
        );
    }
}

/// the result is capped at `oxa.subagent.resultCap` on a UTF-8 boundary; the full text is in
/// the spill file the note names
#[test]
fn result_capped_with_spill() {
    let env = TestEnv::new();
    let host = MockHost::new(&env);
    let long = "é".repeat(500); // 1000 bytes, two per character
    let body = format!(
        "printf '{{\"type\":\"message_end\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{long}\"}}],\"stopReason\":\"stop\"}}}}\\n'\n"
    );
    let h = hooks_with(
        child_script(&env, &body),
        vec![agent("scout", None)],
        json!({"oxa": {"subagent": {"resultCap": 301}}}),
        &host,
    );
    let o = run(&env, &h, json!({"agent": "scout", "task": "a"}));
    assert!(!o.out.is_error);
    let text = &o.out.text;
    assert!(text.len() <= 301, "{} bytes", text.len());
    let (head, note) = text.split_once("\n\n").unwrap();
    assert!(head.chars().all(|c| c == 'é') && !head.is_empty());
    let prefix = "[subagent output truncated: 1000 bytes, full text in ";
    assert!(note.starts_with(prefix) && note.ends_with(".txt]"), "{note}");
    let spill = PathBuf::from(&note[prefix.len()..note.len() - 1]);
    assert!(spill.starts_with(env.tmp().join("spill")));
    assert_eq!(std::fs::read_to_string(&spill).unwrap(), long);
    // Deterministic: the same output gives the same bytes.
    assert_eq!(
        &run(&env, &h, json!({"agent": "scout", "task": "a"})).out.text,
        text
    );
}

/// not declared when the session model's context is below 32K, unless
/// `oxa.subagent.smallContext`; a model switch (new epoch) evaluates again
#[test]
fn not_declared_below_min_context() {
    let env = TestEnv::new();
    let bin = child_script(&env, OK_BODY);
    let flash = MockHost::with(&env, limits("sess", 16384, None), json!({"parked": true}), false);
    let h = hooks_with(bin.clone(), vec![agent("scout", None)], Value::Null, &flash);
    assert!(h.tools().is_empty(), "Flash (16K): not declared");

    // A switch to a big model: `ModelSelect` re-reads the limits for the next epoch.
    *flash.limits.lock().unwrap() = limits("sess", 131072, None);
    let mut ui = HeadlessUi::default();
    h.fire(HookPoint::ModelSelect, &json!({"model": "big"}), &mut ui);
    assert_eq!(h.tools().len(), 1, "declared again on the big model");
    *flash.limits.lock().unwrap() = limits("sess", 16384, None);
    h.fire(HookPoint::SessionStart, &Value::Null, &mut ui);
    assert!(h.tools().is_empty());

    let h = hooks_with(
        bin.clone(),
        vec![agent("scout", None)],
        json!({"oxa": {"subagent": {"smallContext": true}}}),
        &flash,
    );
    assert_eq!(h.tools().len(), 1, "smallContext declares it on Flash");

    let big = MockHost::with(&env, limits("sess", 32000, None), json!({"parked": true}), false);
    let h = hooks_with(bin, vec![agent("scout", None)], Value::Null, &big);
    assert_eq!(h.tools().len(), 1, "32000 is enough");
    assert_eq!(SubagentHooks::MIN_CONTEXT, 32000);
}

/// EG5: each child's hint is cleared after it ends, also after an abort; a failed clear is a
/// warning, never a tool failure
#[test]
fn cleanup_clears_child_hint_also_on_abort() {
    let env = TestEnv::new();
    let host = MockHost::new(&env);
    let h = Arc::new(hooks(&env, HANG_BODY, &host));
    let cancel = CancelToken::new();
    let started = {
        let (h, root, cancel) = (h.clone(), env.root().to_path_buf(), cancel.clone());
        std::thread::spawn(move || {
            run_in(
                &TestEnvRef(root),
                &h,
                json!({"agent": "scout", "task": "x"}),
                &cancel,
            )
        })
    };
    wait_for(&env.root().join("sleep-sess.s1"));
    cancel.cancel();
    assert!(started.join().unwrap().is_error);
    assert_eq!(host.clears(), ["sess.s1"]);
    assert_eq!(
        log(&env).last().unwrap(),
        "clear sess.s1",
        "after the child ended"
    );

    // Success, and a clear that fails.
    let env = TestEnv::new();
    let host = MockHost::with(&env, limits("sess", 131072, None), json!({"parked": true}), true);
    let h = hooks(&env, OK_BODY, &host);
    let o = run(&env, &h, json!({"agent": "scout", "task": "x"}));
    assert!(!o.out.is_error, "a failed clear is not a tool failure");
    assert_eq!(o.out.text, "done: Task: x");
    assert_eq!(host.clears(), ["sess.s1"]);
    assert_eq!(
        o.notes,
        [(
            Level::Warning,
            "subagent: clearing the engine state of sess.s1 failed (the engine is busy)".to_string()
        )]
    );
}

/// abort: queued tasks never start
#[test]
fn cancel_stops_queued_tasks() {
    let env = TestEnv::new();
    let host = MockHost::new(&env);
    let h = Arc::new(hooks(&env, HANG_BODY, &host));
    let cancel = CancelToken::new();
    let started = {
        let (h, root, cancel) = (h.clone(), env.root().to_path_buf(), cancel.clone());
        std::thread::spawn(move || {
            run_in(
                &TestEnvRef(root),
                &h,
                json!({"tasks": [{"agent": "scout", "task": "1"}, {"agent": "scout", "task": "2"}, {"agent": "worker", "task": "3"}]}),
                &cancel,
            )
        })
    };
    wait_for(&env.root().join("sleep-sess.s1"));
    cancel.cancel();
    let out = started.join().unwrap();
    assert!(out.is_error);
    assert!(out.text.starts_with("Parallel: 0/3 succeeded"), "{}", out.text);
    assert!(
        out.text.contains("Subagent was aborted before starting"),
        "{}",
        out.text
    );
    let starts: Vec<String> = log(&env).into_iter().filter(|l| l.starts_with("start")).collect();
    assert_eq!(starts, ["start sess.s1"], "the queued tasks never started");
    assert_eq!(host.clears(), ["sess.s1"]);
}

// ---- O-146: the eight-children cap over the whole tree of a parent session ----

fn items(n: usize) -> Vec<Value> {
    (1..=n)
        .map(|i| json!({"agent": "scout", "task": format!("t{i}")}))
        .collect()
}

fn starts(env: &TestEnv) -> usize {
    log(env).into_iter().filter(|l| l.starts_with("start")).count()
}

/// Join a tool call running on another thread, failing instead of hanging.
fn join_within(rx: &std::sync::mpsc::Receiver<ToolOutput>, what: &str) -> ToolOutput {
    rx.recv_timeout(std::time::Duration::from_secs(20))
        .unwrap_or_else(|_| panic!("{what} did not finish within 20 s"))
}

/// direct spawning: eight tasks in one call run, the ninth is refused with pi's text and
/// nothing starts
#[test]
fn cap_eight_tasks_ok_ninth_refused() {
    let env = TestEnv::new();
    let host = MockHost::new(&env);
    let h = hooks(&env, OK_BODY, &host);
    let o = run(&env, &h, json!({ "tasks": items(8) }));
    assert!(!o.out.is_error, "{}", o.out.text);
    assert!(
        o.out.text.starts_with("Parallel: 8/8 succeeded"),
        "{}",
        o.out.text
    );
    assert_eq!(starts(&env), 8);

    let o = run(&env, &h, json!({ "tasks": items(9) }));
    assert!(o.out.is_error);
    assert_eq!(o.out.text, "Too many parallel tasks (9). Max is 8.");
    assert_eq!(starts(&env), 8, "a refused call starts no child");
    assert_eq!(host.parks(), 1, "a refused call does not park the session");
}

/// chains: eight steps run, a chain of nine is refused (the cap is not only for `tasks`)
#[test]
fn cap_chain_of_eight_ok_nine_refused() {
    let env = TestEnv::new();
    let host = MockHost::new(&env);
    let h = hooks(&env, OK_BODY, &host);
    let o = run(&env, &h, json!({ "chain": items(8) }));
    assert!(!o.out.is_error, "{}", o.out.text);
    assert_eq!(starts(&env), 8, "eight steps, one after another");

    let o = run(&env, &h, json!({ "chain": items(9) }));
    assert!(o.out.is_error);
    assert_eq!(o.out.text, "Too many chain steps (9). Max is 8.");
    assert_eq!(starts(&env), 8, "a refused chain starts no child");
}

/// the cap is per parent session over every call in flight: with five children running or
/// queued, a call (tasks or chain) asking for four more is refused, three more fit, and the
/// budget is free again when the calls have ended
#[test]
fn cap_counts_running_and_queued_children_across_calls() {
    let env = TestEnv::new();
    let host = MockHost::new(&env);
    let h = Arc::new(hooks(&env, HANG_BODY, &host));
    let cancel = CancelToken::new();
    let (tx, rx) = std::sync::mpsc::channel();
    {
        let (h, root, cancel) = (h.clone(), env.root().to_path_buf(), cancel.clone());
        std::thread::spawn(move || {
            let out = run_in(&TestEnvRef(root), &h, json!({ "tasks": items(5) }), &cancel);
            let _ = tx.send(out);
        });
    }
    // One child is live, four are queued.
    wait_for(&env.root().join("sleep-sess.s1"));

    for shape in ["tasks", "chain"] {
        let o = run(&env, &h, json!({ shape: items(4) }));
        assert!(o.out.is_error, "{shape}");
        assert_eq!(
            o.out.text,
            "Too many subagent tasks for this session (5 running or queued, 4 more requested). Max is 8. Wait for the running subagent call to finish, or ask for fewer tasks.",
            "{shape}"
        );
    }
    assert_eq!(starts(&env), 1, "the refused calls started nothing");

    // Three more fit (5 + 3 = 8): the call is accepted and waits for its turn; a fourth
    // child on top of those is refused while it waits.
    let cancel2 = CancelToken::new();
    let (tx2, rx2) = std::sync::mpsc::channel();
    {
        let (h, root, cancel2) = (h.clone(), env.root().to_path_buf(), cancel2.clone());
        std::thread::spawn(move || {
            let out = run_in(&TestEnvRef(root), &h, json!({ "chain": items(3) }), &cancel2);
            let _ = tx2.send(out);
        });
    }
    // Wait until the queued chain holds its reservation. The probe asks for four, which is
    // refused before (5 + 4) and after (8 + 4) the reservation, so it can never be accepted
    // and block behind the hanging call.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let o = run(&env, &h, json!({ "tasks": items(4) }));
        assert!(o.out.is_error, "{}", o.out.text);
        if o.out.text.contains("(8 running or queued, 4 more requested)") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the queued chain never reserved its three children: {}",
            o.out.text
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // Eight are running or queued: the ninth child is refused, also as a single task.
    let o = run(&env, &h, json!({"agent": "scout", "task": "one more"}));
    assert!(o.out.is_error);
    assert!(
        o.out.text.starts_with(
            "Too many subagent tasks for this session (8 running or queued, 1 more requested). Max is 8."
        ),
        "{}",
        o.out.text
    );
    assert_eq!(starts(&env), 1, "still only the first child");

    cancel2.cancel();
    cancel.cancel();
    let first = join_within(&rx, "the five-task call");
    assert!(
        first.text.starts_with("Parallel: 0/5 succeeded"),
        "{}",
        first.text
    );
    let second = join_within(&rx2, "the queued chain");
    assert!(second.is_error, "{}", second.text);
    assert_eq!(
        starts(&env),
        1,
        "only the first child of the first call ever started"
    );

    // Everything was given back: eight fit again (cancelled, so nothing hangs).
    let done = CancelToken::new();
    done.cancel();
    let o = run_with(&env, &h, json!({ "tasks": items(8) }), &done);
    assert!(
        o.out.text.starts_with("Parallel: 0/8 succeeded"),
        "the budget is released when calls end: {}",
        o.out.text
    );
}

#[test]
fn run_token_budget_is_passed_at_spawn_and_child_usage_is_charged() {
    let env = TestEnv::new();
    let host = MockHost::new(&env);
    host.limits.lock().unwrap()["remainingTokens"] = json!(100);
    let body = r#"printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"done"}],"stopReason":"stop","usage":{"input":10,"cacheRead":70,"output":5}}}'
"#;
    let h = hooks(&env, body, &host);
    let outcome = run(
        &env,
        &h,
        json!({"chain":[
            {"agent":"scout","task":"first"}, {"agent":"worker","task":"second"}
        ]}),
    );
    assert!(!outcome.out.is_error, "{}", outcome.out.text);
    for (hint, remaining) in [("sess.s1", "100"), ("sess.s2", "85")] {
        let args = argv(&env, hint);
        let at = args
            .iter()
            .position(|a| a == "--max-tokens")
            .expect("child run budget");
        assert_eq!(args[at + 1], remaining);
    }
    assert_eq!(host.limits.lock().unwrap()["remainingTokens"], json!(70));
}

#[test]
fn child_run_usage_includes_non_chat_requests_without_double_charging() {
    let env = TestEnv::new();
    let host = MockHost::new(&env);
    host.limits.lock().unwrap()["remainingTokens"] = json!(100);
    // 15 chat tokens plus 45 from compaction / tool prefill. The cumulative
    // record includes the chat already reported and may exceed the child budget.
    let body = r#"printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"done"}],"stopReason":"stop","usage":{"input":10,"cacheRead":70,"output":5}}}' '{"type":"run_usage","tokens":60}'
"#;
    let h = hooks(&env, body, &host);
    let _ = run(
        &env,
        &h,
        json!({"tasks":[
            {"agent":"scout","task":"first"},
            {"agent":"worker","task":"second"},
            {"agent":"scout","task":"third"}
        ]}),
    );
    let args = argv(&env, "sess.s2");
    let at = args.iter().position(|a| a == "--max-tokens").unwrap();
    assert_eq!(args[at + 1], "40", "charge total usage exactly once");
    assert_eq!(host.limits.lock().unwrap()["remainingTokens"], json!(0));
    assert_eq!(log(&env).iter().filter(|s| s.starts_with("start ")).count(), 2);
}

#[test]
fn depleted_run_token_budget_starts_no_child() {
    let env = TestEnv::new();
    let host = MockHost::new(&env);
    host.limits.lock().unwrap()["remainingTokens"] = json!(0);
    let h = hooks(&env, OK_BODY, &host);
    let outcome = run(&env, &h, json!({"agent":"scout","task":"go"}));
    assert!(outcome.out.is_error);
    assert!(!log(&env).iter().any(|s| s.starts_with("start ")));
}
