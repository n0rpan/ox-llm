//! P4 (wave H): the offline switch from the front end: `--offline`, `OXA_OFFLINE`
//! (pi's truthy values) and `oxa.offline`; `gh` for report-issue is refused before anything is
//! asked or run. The tests share the process network policy, so they hold one lock each.

use oxa_cli::{run, submit, ScrubCtx, Stdio};
use oxa_config::{Env, Settings};
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_proto::codes;
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

const OFFLINE_REMOTE_ENGINE: &str = "Error: Refused: the engine (10.255.0.2) needs the network: offline mode is on (OXA_OFFLINE=1, --offline or oxa.offline) (Fix: turn offline mode off: unset OXA_OFFLINE, drop --offline, set \"oxa.offline\": false)\n";

#[test]
fn every_way_to_turn_offline_on_refuses_a_remote_engine_with_one_shape() {
    let _g = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let env = TestEnv::new();
    // allowRemote is on, so only offline can refuse.
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"engine":{"allowRemote":true}}}"#,
    );
    let remote = "http://10.255.0.2:8092";
    type Case<'a> = (Vec<(&'a str, &'a str)>, Vec<&'a str>);
    let cases: Vec<Case> = vec![
        (vec![], vec!["--offline", "-p", "--no-session", "x"]),
        (vec![("OXA_OFFLINE", "1")], vec!["-p", "--no-session", "x"]),
        (vec![("OXA_OFFLINE", "TRUE")], vec!["-p", "--no-session", "x"]),
        (vec![("OXA_OFFLINE", "yes")], vec!["-p", "--no-session", "x"]),
    ];
    for (extra, words) in cases {
        let (code, _, err) = go(&env, remote, &extra, &words);
        assert_ne!(code, 0);
        assert_eq!(code, 7);
        assert_eq!(
            err.lines().next().unwrap(),
            OFFLINE_REMOTE_ENGINE.trim_end(),
            "{extra:?} {words:?}"
        );
        let record: serde_json::Value = serde_json::from_str(err.lines().last().unwrap()).unwrap();
        assert_eq!(record["outcome"], "blocked");
        assert_eq!(record["error_class"], "offline");
    }
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"offline":true,"engine":{"allowRemote":true}}}"#,
    );
    let (code, _, err) = go(&env, remote, &[], &["-p", "--no-session", "x"]);
    assert_ne!(code, 0);
    assert_eq!(code, 7);
    assert_eq!(
        err.lines().next().unwrap(),
        OFFLINE_REMOTE_ENGINE.trim_end(),
        "oxa.offline"
    );
}

#[test]
fn other_values_leave_offline_off_and_the_loopback_engine_always_works() {
    let _g = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"engine":{"allowRemote":true}}}"#,
    );
    // `OXA_OFFLINE=0` is not offline: the startup check passes (the remote engine is allowed),
    // which shows as the remote-engine notice path, not as the offline refusal.
    for v in ["0", "no", "false"] {
        let _ = go(
            &env,
            "http://127.0.0.1:9",
            &[("OXA_OFFLINE", v)],
            &["-p", "--no-session", "x"],
        );
        assert!(!oxa_proto::offline(), "OXA_OFFLINE={v}");
    }
    let server = FakeServer::start(vec![ScriptTurn::text("a")], FakeOpts::default()).unwrap();
    let (code, out, err) = go(
        &env,
        &server.url(),
        &[],
        &["--offline", "-p", "--no-session", "x"],
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(out.trim(), "a");
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn report_issue_is_refused_before_anything_is_asked_or_run() {
    let _g = POLICY.lock().unwrap_or_else(|p| p.into_inner());
    let env = TestEnv::new();
    let draft = env.write("draft.md", "# T\n\nB\n");
    let settings = Settings::from_values(
        serde_json::json!({"oxa": {"reportIssue": {"repo": "owner/name"}}}),
        serde_json::json!({}),
    );
    oxa_proto::set_net_policy(true, false);
    let (mut asked, mut ran) = (false, false);
    let mut out = Vec::new();
    let r = submit(
        &draft,
        &settings,
        &ScrubCtx::default(),
        &mut |_: &str| {
            asked = true;
            Some(true)
        },
        &mut |_: &str, _: &[String]| {
            ran = true;
            Some((0, String::new()))
        },
        &mut out,
    );
    oxa_proto::set_net_policy(false, false);
    let e = r.unwrap_err();
    assert_eq!(e.code, codes::OFFLINE);
    assert!(
        e.message
            .starts_with("Refused: posting the issue with `gh` needs the network"),
        "{e}"
    );
    assert!(!asked && !ran);
}
