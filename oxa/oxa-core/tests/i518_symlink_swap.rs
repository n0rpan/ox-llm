//! #518: a file tool opens the file the permission check judged, not whatever a symlink points
//! at by the time the tool runs. A sandboxed background job can swap a link between the check
//! and the open; here the swap happens while the permission prompt is open (the widest window),
//! so the race is deterministic. Isolation: every file lives under `oxa_fake::TestEnv`.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{Access, Answer, Level, Mode, Placement, Role, Ui, VecSink};
use oxa_resources::{discover, DiscoverOpts};
use oxa_sandbox::NoSandbox;
use oxa_session::MemStore;
use std::path::PathBuf;
use std::sync::Arc;

/// Answers every permission prompt with `Once`, after pointing `link` at `to` (the swap a
/// background job would make between the check and the open).
struct SwapUi {
    link: PathBuf,
    to: PathBuf,
    asked: usize,
}
impl Ui for SwapUi {
    fn select(&mut self, _: &str, _: &[String], _: Option<u32>) -> Option<usize> {
        None
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
        self.asked += 1;
        std::fs::remove_file(&self.link).expect("remove link");
        std::os::unix::fs::symlink(&self.to, &self.link).expect("swap link");
        Answer::Once
    }
}

/// Runs one tool call in ask mode (every write asks; a read outside the cwd asks), with the link
/// `work/link` swapped to `to` while the prompt is open. Returns the tool result text.
fn run_swapped(env: &TestEnv, call: &str, args: serde_json::Value, to: PathBuf) -> String {
    let dirs = Dirs::under(env.root());
    let settings = Settings::load(&dirs, &env.work(), true);
    let opts = DiscoverOpts {
        trusted: true,
        ..Default::default()
    };
    let resources = discover(&dirs, &env.work(), &settings, &opts);
    let engine = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::tool("c1", call, &args.to_string()),
        ScriptTurn::text("done"),
    ]));
    let mut s = Session::open(
        Deps {
            engine: engine.clone(),
            store: Arc::new(MemStore::new()),
            tools: oxa_tools::builtin_tools(),
            hooks: Vec::new(),
            policy: Box::new(ModePolicy::new()),
            sandbox: Arc::new(NoSandbox),
            settings,
            dirs,
            cwd: env.work(),
            selection: ToolSelection::default(),
            resources,
            model: None,
            thinking: None,
            mode: Some(Mode::Ask),
            session_id: Some("s1".to_string()),
            jobs: oxa_core::fresh_jobs(),
        },
        Open::New,
    )
    .expect("open");
    let mut ui = SwapUi {
        link: env.work().join("link"),
        to,
        asked: 0,
    };
    let mut sink = VecSink::default();
    s.prompt("go".to_string(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt");
    assert_eq!(ui.asked, 1, "the call asked once");
    let reqs = engine.requests();
    reqs[1]
        .messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .and_then(|m| m.content.clone())
        .expect("a tool result")
}

#[test]
fn write_through_a_link_swapped_after_the_check_does_not_reach_the_new_target() {
    let env = TestEnv::new();
    let real = env.write("work/real.txt", "old\n");
    let outside = env.write("outside/hook", "original\n");
    std::os::unix::fs::symlink(&real, env.work().join("link")).unwrap();
    let text = run_swapped(
        &env,
        "write",
        serde_json::json!({"path": "link", "content": "pwned\n"}),
        outside.clone(),
    );
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "original\n", "{text}");
    // The write lands where the check said it would: the link's target at the time of the check.
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "pwned\n", "{text}");
}

#[test]
fn edit_through_a_link_swapped_after_the_check_is_refused() {
    let env = TestEnv::new();
    let real = env.write("work/real.txt", "a\n");
    let outside = env.write("outside/hook", "a\n");
    std::os::unix::fs::symlink(&real, env.work().join("link")).unwrap();
    let text = run_swapped(
        &env,
        "edit",
        serde_json::json!({"path": "link", "oldText": "a", "newText": "b"}),
        outside.clone(),
    );
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "a\n", "{text}");
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "a\n", "{text}");
    assert!(text.contains("changed after the permission check"), "{text}");
}

#[test]
fn read_through_a_link_swapped_after_the_check_is_refused() {
    let env = TestEnv::new();
    let approved = env.write("outside/notes.txt", "fine\n");
    let secret = env.write("outside/secret", "SECRET-518\n");
    std::os::unix::fs::symlink(&approved, env.work().join("link")).unwrap();
    let text = run_swapped(&env, "read", serde_json::json!({"path": "link"}), secret);
    assert!(!text.contains("SECRET-518"), "{text}");
    assert!(text.contains("changed after the permission check"), "{text}");
}

#[test]
fn a_link_left_alone_still_works_through_the_check() {
    let env = TestEnv::new();
    let real = env.write("work/real.txt", "a\n");
    std::os::unix::fs::symlink(&real, env.work().join("link")).unwrap();
    // The "swap" re-points the link at the same target: nothing changed.
    let text = run_swapped(
        &env,
        "edit",
        serde_json::json!({"path": "link", "oldText": "a", "newText": "b"}),
        real.clone(),
    );
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "b\n", "{text}");
    assert!(env.work().join("link").is_symlink());
}
