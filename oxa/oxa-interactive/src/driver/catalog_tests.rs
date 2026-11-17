//! Deliver metadata explicitly: no scheduler timing or sleeps can hide an early picker.
use super::*;
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_engine::HttpEngine;
use oxa_fake::{FakeModel, FakeOpts, FakeServer, TestEnv};
use oxa_proto::{Access, CancelToken, Decision, Engine, ExecResult, ExecSpec, Mode, Policy, Sandbox};
use oxa_resources::Resources;
use oxa_session::MemStore;
use oxa_tui::TestTerminal;
use serde_json::json;
use std::sync::{mpsc, Arc};

struct NoActions;
impl Policy for NoActions {
    fn decide(&self, _: Mode, _: &Access, _: &Path) -> Decision {
        panic!("catalog updates must not ask permission or execute tools")
    }
    fn remember(&mut self, _: &Access) {}
}
impl Sandbox for NoActions {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _: &ExecSpec, _: &CancelToken, _: &mut dyn FnMut(&[u8])) -> ExecResult {
        panic!("catalog updates must not execute tools")
    }
}

pub(super) fn with_loop(
    test: impl FnOnce(&mut Loop<'_>, &mut TestTerminal, SessionInfo, &mpsc::Receiver<SessionCmd>),
) {
    let env = TestEnv::new();
    let server = FakeServer::start(
        Vec::new(),
        FakeOpts {
            model: "model-a".into(),
            models: ["model-a", "model-b", "model-c"]
                .into_iter()
                .map(|id| FakeModel::moe(id, 1 << 20))
                .collect(),
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let dirs = oxa_config::Dirs::under(env.root());
    let settings =
        oxa_config::Settings::from_values(json!({"enabledModels":["model-a","model-c"]}), json!({}));
    let store = Arc::new(MemStore::new());
    let engine: Arc<dyn Engine> = Arc::new(HttpEngine::new(&server.url(), None).unwrap());
    let session = Session::open(
        Deps {
            engine,
            store: store.clone(),
            tools: Vec::new(),
            hooks: Vec::new(),
            policy: Box::new(NoActions),
            sandbox: Arc::new(NoActions),
            settings: settings.clone(),
            dirs: dirs.clone(),
            cwd: env.work(),
            selection: ToolSelection::default(),
            resources: Resources::default(),
            model: None,
            thinking: None,
            mode: None,
            session_id: None,
            jobs: oxa_core::fresh_jobs(),
        },
        Open::Ephemeral,
    )
    .unwrap();
    let info = SessionInfo {
        id: session.state().session_id,
        engine_url: session.inner().info.base_url.clone(),
        models: session.inner().info.models.clone(),
        ..SessionInfo::default()
    };
    assert_eq!(info.models, ["model-a", "model-b", "model-c"]);
    let mut opts = InteractiveOpts {
        fullscreen: true,
        dirs,
        settings,
        keymap: Keymap::defaults(),
        store,
        hooks: Vec::new(),
        mcp: None,
        initial_prompt: None,
        initial_parts: Vec::new(),
        initial_follow_ups: Vec::new(),
        theme: None,
        notices: Vec::new(),
        env: Vec::new(),
        caps: Some(TermCaps::BASIC),
        reopen: Box::new(|_, _| panic!("must not reopen")),
        open_picker: false,
        save_trust: None,
        gated: Vec::new(),
        yolo: None,
    };
    let mut vm = ViewModel::default();
    vm.status.model = "model-a".into();
    vm.status.cwd = env.work().display().to_string();
    vm.status.n_ctx = 1 << 20;
    let mut term = TestTerminal::new(80, 24);
    let (tx, rx) = mpsc::channel();
    let mut lp = Loop::new(&mut term, &mut opts, session.handle(), tx, vm);
    test(&mut lp, &mut term, info, &rx);
    assert!(
        server.requests().is_empty(),
        "picker refresh must not run inference"
    );
}

fn rendered(lp: &mut Loop<'_>) -> String {
    match lp.overlay.as_mut().expect("picker remains open") {
        Overlay::Model(s) => s.render(100, &Theme::plain()).join("\n"),
        Overlay::Scoped(s) => s.render(100, &Theme::plain()).join("\n"),
        _ => panic!("wrong picker"),
    }
}
fn key(lp: &mut Loop<'_>, term: &mut TestTerminal, id: &str) {
    lp.input(term, InputEvent::Key(Key::press(id)));
}

#[test]
fn initial_model_catalog_arrives_before_or_after_ctrl_l() {
    for metadata_first in [false, true] {
        with_loop(|lp, term, info, rx| {
            if metadata_first {
                lp.worker_msg(WorkerMsg::Session(info.clone()));
            }
            key(lp, term, "ctrl+l");
            if !metadata_first {
                assert!(!rendered(lp).contains("model-b"));
                lp.worker_msg(WorkerMsg::Session(info));
            }
            let screen = rendered(lp);
            assert!(
                screen.contains("model-b") && screen.contains("model-c"),
                "late catalog must refresh the open picker: {screen}"
            );
            assert!(rx.try_recv().is_err(), "metadata must not switch models");
            assert_eq!(lp.vm.status.model, "model-a");
            key(lp, term, "down");
            key(lp, term, "enter");
            assert!(matches!(rx.try_recv(), Ok(SessionCmd::SetModel(id)) if id == "model-b"));
        });
    }
}

#[test]
fn catalog_refresh_preserves_search_scope_and_selected_identity() {
    with_loop(|lp, term, mut info, rx| {
        lp.worker_msg(WorkerMsg::Session(info.clone()));
        key(lp, term, "ctrl+l");
        key(lp, term, "tab");
        if let Some(Overlay::Model(s)) = lp.overlay.as_mut() {
            s.paste("model");
        }
        key(lp, term, "down"); // selected model-c, not the currently running model-a
        info.models = vec![
            "model-c".into(),
            "model-b".into(),
            "model-a".into(),
            "other".into(),
        ];
        lp.worker_msg(WorkerMsg::Session(info));
        let screen = rendered(lp);
        assert!(
            screen.contains("Select model (scoped)") && screen.contains("search: model"),
            "{screen}"
        );
        assert!(
            !screen.contains("model-b") && !screen.contains("other"),
            "scope/search lost: {screen}"
        );
        assert!(rx.try_recv().is_err());
        key(lp, term, "enter");
        assert!(matches!(rx.try_recv(), Ok(SessionCmd::SetModel(id)) if id == "model-c"));
    });
}

#[test]
fn late_catalog_refreshes_scoped_editor_without_losing_unsaved_choices() {
    with_loop(|lp, term, info, rx| {
        lp.open_selector(SelectorKind::ScopedModels);
        key(lp, term, "space"); // disable model-a in the session-only draft
        key(lp, term, "alt+down"); // retain the user's order and selected identity
        lp.worker_msg(WorkerMsg::Session(info));
        let screen = rendered(lp);
        assert!(screen.contains("model-b"), "new catalog row missing: {screen}");
        assert!(
            screen.contains("[ ] model-a") && screen.contains("[x] model-c") && screen.contains("(unsaved)"),
            "draft lost: {screen}"
        );
        assert!(rx.try_recv().is_err());
        key(lp, term, "space"); // cursor must still name model-a
        key(lp, term, "escape");
        assert_eq!(lp.scoped, ["model-c", "model-a"]);
    });
}
