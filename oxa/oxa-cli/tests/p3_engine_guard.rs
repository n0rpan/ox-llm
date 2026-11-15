//! P3 (wave H): the engine URL guard at startup, in process. The tests share the
//! process network policy, so they hold one lock each.

use oxa_cli::{run, Stdio};
use oxa_config::Env;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};

static POLICY: Mutex<()> = Mutex::new(());

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn go(env: &TestEnv, engine_url: &str, extra_env: &[(&str, &str)], words: &[&str]) -> (i32, String, String) {
    let mut owned = env.envs(engine_url);
    owned.extend(extra_env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let pairs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let (out, err) = (Sink::default(), Sink::default());
    let io = Stdio {
        stdin: Box::new(Cursor::new(Vec::new())),
        stdout: Box::new(out.clone()),
        stderr: Box::new(err.clone()),
        stdin_is_tty: false,
        stdout_is_tty: false,
        cwd: env.work(),
    };
    let argv: Vec<String> = words.iter().map(|s| s.to_string()).collect();
    let code = run(&argv, &Env::from_pairs(&pairs), io);
    let text = |s: &Sink| String::from_utf8(s.0.lock().unwrap().clone()).unwrap();
    (code, text(&out), text(&err))
}

const REFUSAL: &str = "Error: Refused to use the engine at 10.255.0.2: it is not a loopback address, so prompts and files would leave this machine over plain HTTP (Fix: set \"oxa.engine.allowRemote\": true in the user settings.json, or use an engine on 127.0.0.1)\n";

#[test]
fn a_remote_engine_url_is_refused_at_startup_with_the_fix() {
    let _g = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let env = TestEnv::new();
    let (code, out, err) = go(&env, "http://10.255.0.2:8092", &[], &["-p", "--no-session", "x"]);
    assert_ne!(code, 0);
    assert_eq!(out, "");
    assert_eq!(err, REFUSAL);
    // The same for a models.json endpoint (the engine URL of the environment is loopback).
    env.write(
        "home/.oxa/agent/models.json",
        r#"{"default":"lan","providers":{"lan":{"baseUrl":"http://10.255.0.2:8092","api":"openai-completions"}}}"#,
    );
    let (code, _, err) = go(&env, "http://127.0.0.1:9", &[], &["-p", "--no-session", "x"]);
    assert_ne!(code, 0);
    assert!(err.ends_with(REFUSAL), "{err}");
}

#[test]
fn a_project_cannot_allow_a_remote_engine() {
    let _g = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let env = TestEnv::new();
    env.write(
        "work/.oxa/settings.json",
        r#"{"oxa":{"engine":{"allowRemote":true}}}"#,
    );
    let (code, _, err) = go(
        &env,
        "http://10.255.0.2:8092",
        &[],
        &["-p", "--no-session", "--approve", "x"],
    );
    assert_ne!(code, 0);
    assert_eq!(err, REFUSAL);
}

#[test]
fn loopback_engines_need_nothing() {
    let _g = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let server = FakeServer::start(vec![ScriptTurn::text("a")], FakeOpts::default()).unwrap();
    let env = TestEnv::new();
    let url = server.url().replace("127.0.0.1", "localhost");
    let (code, out, err) = go(&env, &url, &[], &["-p", "--no-session", "x"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out.trim(), "a");
    assert!(!err.contains("Remote engine"), "{err}");
}

#[test]
fn allowed_remote_engine_gives_one_notice_and_the_connect_site_agrees() {
    let _g = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    // The policy as `install` sets it from user settings with allowRemote.
    oxa_proto::set_net_policy(false, true);
    assert!(oxa_proto::check_engine_host("10.255.0.2").is_ok());
    assert_eq!(
        oxa_proto::remote_engine_notice("10.255.0.2"),
        "Remote engine 10.255.0.2: prompts and files leave this machine unencrypted (plain HTTP)\nFix: use an engine on 127.0.0.1 or an encrypted tunnel (ssh -L) to keep them private"
    );
    oxa_proto::set_net_policy(false, false);
    let e = oxa_proto::check_engine_host("10.255.0.2").unwrap_err();
    assert_eq!(e.code, oxa_proto::codes::REMOTE_ENGINE);
    assert!(oxa_proto::check_engine_host("127.0.0.1").is_ok());
}
