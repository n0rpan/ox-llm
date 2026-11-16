use oxa_cli::{run, Stdio};
use oxa_config::Env;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// Run `oxa <args>` in-process against a fake engine with `turns`; returns (code, stdout, stderr).
fn go(env: &TestEnv, turns: Vec<ScriptTurn>, args: &[&str]) -> (i32, String, String) {
    go_env(env, turns, args, &[])
}

fn go_env(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    args: &[&str],
    extra: &[(&str, &str)],
) -> (i32, String, String) {
    let server = FakeServer::start(turns, FakeOpts::default()).expect("fake engine");
    let mut owned = env.envs(&server.url());
    owned.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let cwd = env.work();
    let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let pairs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let env = Env::from_pairs(&pairs);
        let (out, err) = (Sink::default(), Sink::default());
        let io = Stdio {
            stdin: Box::new(Cursor::new(Vec::new())),
            stdout: Box::new(out.clone()),
            stderr: Box::new(err.clone()),
            stdin_is_tty: false,
            stdout_is_tty: false,
            cwd,
        };
        let code = run(&argv, &env, io);
        let text = |s: &Sink| String::from_utf8_lossy(&s.0.lock().unwrap()).into_owned();
        let _ = tx.send((code, text(&out), text(&err)));
    });
    rx.recv_timeout(Duration::from_secs(120)).expect("oxa finished")
}

#[test]
fn output_schema_repairs_once_and_reports_mismatch() {
    let env = TestEnv::new();
    env.write(
        "work/schema.json",
        r#"{"type":"object","properties":{"answer":{"type":"number"}},"required":["answer"]}"#,
    );
    let args = [
        "--mode",
        "json",
        "--no-session",
        "--output-schema",
        "schema.json",
        "answer",
    ];
    let (code, out, err) = go(
        &env,
        vec![ScriptTurn::text("wrong"), ScriptTurn::text(r#"{"answer":42}"#)],
        &args,
    );
    assert_eq!(code, 0, "{out} {err}");
    assert!(err.contains(r#""outcome":"done""#), "{out} {err}");
    let (code, out, err) = go(
        &env,
        vec![
            ScriptTurn::text("wrong"),
            ScriptTurn::text(r#"{"answer":"wrong"}"#),
        ],
        &args,
    );
    assert_eq!(code, 1, "{out} {err}");
    assert!(err.contains(r#""error_class":"schema_mismatch""#), "{out} {err}");
}

#[test]
fn output_schema_prints_only_validated_answer_after_local_length_recovery() {
    let env = TestEnv::new();
    env.write(
        "work/schema.json",
        r#"{"type":"object","properties":{"answer":{"type":"number"}},"required":["answer"]}"#,
    );
    let args = ["-p", "--no-session", "--output-schema", "schema.json", "answer"];
    let good = r#"{"answer":42}"#;
    for needs_repair in [false, true] {
        let mut cut = ScriptTurn::text(r#"{"outer":"#);
        cut.finish = oxa_proto::Finish::Length;
        let mut turns = vec![cut];
        if needs_repair {
            turns.push(ScriptTurn::text("invalid continuation"));
        }
        turns.push(ScriptTurn::text(good));
        let (code, out, err) = go(&env, turns, &args);
        assert_eq!(code, 0, "{out} {err}");
        assert_eq!(
            out.trim(),
            good,
            "only the validated final message belongs on stdout: {err}"
        );
        let value: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(value["answer"], 42);
    }
}

#[test]
fn output_schema_valid_first_answer_and_invalid_contract() {
    let env = TestEnv::new();
    env.write("work/schema.json", r#"{"type":"string","enum":["ok"]}"#);
    let args = ["-p", "--no-session", "--output-schema", "schema.json", "answer"];
    let (code, out, err) = go(&env, vec![ScriptTurn::text(r#""ok""#)], &args);
    assert_eq!(code, 0, "{out} {err}");
    assert!(out.contains(r#""ok""#));
    for schema in [
        "not json",
        r#"{"$ref":"https://example.invalid/schema"}"#,
        r#"{"type":"boolean"}"#,
    ] {
        env.write("work/schema.json", schema);
        let (code, out, err) = go(&env, vec![], &args);
        assert_eq!(code, 2, "{out} {err}");
        assert!(err.contains("output schema"), "{err}");
    }
}
