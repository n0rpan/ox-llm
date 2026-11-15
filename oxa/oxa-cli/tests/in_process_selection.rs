//! The opt-in selects a lazy cloud Engine without using the HTTP controller.
use oxa_cli::{InProcessEngineFactory, Stdio};
use oxa_config::{Dirs, Env};
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_proto::{Engine, OxaError};
use std::io::{Cursor, Write};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

static POLICY: Mutex<()> = Mutex::new(());

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}
struct Factory {
    engine: Arc<ScriptedEngine>,
    opened: AtomicUsize,
}
impl Factory {
    fn new() -> Self {
        Self {
            engine: Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("via-embedded")]).with_remote()),
            opened: AtomicUsize::new(0),
        }
    }
}
impl InProcessEngineFactory for Factory {
    fn open(&self, _env: &Env, _dirs: &Dirs) -> Result<Arc<dyn Engine>, OxaError> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        Ok(self.engine.clone())
    }
}
fn invoke(
    env: &TestEnv,
    url: &str,
    args: &[&str],
    factory: &dyn InProcessEngineFactory,
) -> (i32, String, String) {
    let values = env.envs(url);
    let pairs: Vec<(&str, &str)> = values
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let out = Capture::default();
    let err = Capture::default();
    let code = oxa_cli::run_with_in_process_factory(
        &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        &Env::from_pairs(&pairs),
        Stdio {
            stdin: Box::new(Cursor::new(Vec::new())),
            stdout: Box::new(out.clone()),
            stderr: Box::new(err.clone()),
            stdin_is_tty: false,
            stdout_is_tty: false,
            cwd: env.work(),
        },
        factory,
    );
    (code, out.text(), err.text())
}
fn settings(env: &TestEnv, user: Option<bool>, project: Option<bool>) {
    let mut value = serde_json::json!({"defaultProjectTrust":"always","retry":{"enabled":false}});
    if let Some(enabled) = user {
        value["oxa"] = serde_json::json!({"engine":{"inProcessCloud":enabled}});
    }
    env.write("home/.oxa/agent/settings.json", &value.to_string());
    if let Some(enabled) = project {
        env.write(
            "work/.oxa/settings.json",
            &serde_json::json!({"oxa":{"engine":{"inProcessCloud":enabled}}}).to_string(),
        );
    }
}
#[test]
fn absent_disabled_and_project_only_settings_keep_http_and_never_open_the_factory() {
    let _policy = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    for (user, project) in [
        (None, None),
        (Some(false), None),
        (None, Some(true)),
        (Some(false), Some(true)),
    ] {
        let env = TestEnv::new();
        settings(&env, user, project);
        let http = FakeServer::start(vec![ScriptTurn::text("via-http")], FakeOpts::default()).unwrap();
        let factory = Factory::new();
        let (code, out, err) = invoke(
            &env,
            &http.url(),
            &["-p", "--no-tools", "--no-session", "hello"],
            &factory,
        );
        assert_eq!(code, 0, "{err}");
        assert!(out.contains("via-http"), "{out}");
        assert_eq!(factory.opened.load(Ordering::SeqCst), 0);
        assert!(factory.engine.requests().is_empty());
        assert_eq!(http.requests().len(), 1);
    }
}
#[test]
fn user_opt_in_uses_the_in_process_engine_even_without_an_http_controller() {
    let _policy = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let env = TestEnv::new();
    settings(&env, Some(true), Some(false));
    let factory = Factory::new();
    let (code, out, err) = invoke(
        &env,
        "http://127.0.0.1:9",
        &["-p", "--no-tools", "--no-session", "hello"],
        &factory,
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("via-embedded"), "{out}");
    assert_eq!(factory.opened.load(Ordering::SeqCst), 1);
    assert_eq!(factory.engine.requests().len(), 1);
}
#[test]
fn list_models_uses_the_same_opt_in_factory() {
    let _policy = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let env = TestEnv::new();
    settings(&env, Some(true), None);
    let factory = Factory::new();
    let (code, out, err) = invoke(&env, "http://127.0.0.1:9", &["--list-models"], &factory);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("fake-model"), "{out}");
    assert_eq!(factory.opened.load(Ordering::SeqCst), 1);
    assert!(factory.engine.requests().is_empty());
}
#[test]
fn offline_refuses_before_constructing_the_gateway() {
    let _policy = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let env = TestEnv::new();
    settings(&env, Some(true), None);
    let factory = Factory::new();
    let (code, _, err) = invoke(
        &env,
        "http://127.0.0.1:9",
        &["--offline", "-p", "--no-tools", "--no-session", "hello"],
        &factory,
    );
    assert_ne!(code, 0);
    assert!(err.to_lowercase().contains("offline"), "{err}");
    assert_eq!(factory.opened.load(Ordering::SeqCst), 0);
    assert!(factory.engine.requests().is_empty());
}

#[test]
fn gateway_initialization_errors_do_not_fall_back_to_http() {
    let _policy = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    struct Unavailable;
    impl InProcessEngineFactory for Unavailable {
        fn open(&self, _env: &Env, _dirs: &Dirs) -> Result<Arc<dyn Engine>, OxaError> {
            Err(OxaError::new(
                "cloud_credentials_unavailable",
                "Configured gateway unavailable",
            ))
        }
    }
    let env = TestEnv::new();
    settings(&env, Some(true), None);
    let http = FakeServer::start(vec![ScriptTurn::text("via-http")], FakeOpts::default()).unwrap();
    let (code, out, err) = invoke(
        &env,
        &http.url(),
        &["-p", "--no-tools", "--no-session", "hello"],
        &Unavailable,
    );
    assert_ne!(code, 0);
    assert!(!out.contains("via-http"));
    assert!(err.contains("Configured gateway unavailable"), "{err}");
    assert!(http.requests().is_empty());
}

#[test]
fn invalid_setting_type_falls_back_to_the_disabled_default() {
    let _policy = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"engine":{"inProcessCloud":"true"}}}"#,
    );
    let factory = Factory::new();
    let http = FakeServer::start(vec![ScriptTurn::text("via-http")], FakeOpts::default()).unwrap();
    let (code, out, err) = invoke(
        &env,
        &http.url(),
        &["-p", "--no-tools", "--no-session", "hello"],
        &factory,
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("via-http"));
    assert_eq!(factory.opened.load(Ordering::SeqCst), 0);
}
