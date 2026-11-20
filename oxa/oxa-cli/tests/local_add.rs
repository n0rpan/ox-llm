//! `oxa local add`: user settings only, from the engine's admission report, with a backup; a
//! missing value is an explained error and nothing is written. A stand-in `oxe` (a shell script
//! that prints a fixed report) is placed in an explicit `--engine-dir`; the real report is
//! produced by `oxe admission` and tested in the engine crate.
use oxa_fake::TestEnv;
use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;

/// The tests write and run scripts; one at a time avoids "text file busy" races on exec.
static LOCK: Mutex<()> = Mutex::new(());

fn report(ctx: u64) -> Value {
    json!({
        "schema": "oxe-admission/1",
        "limits": {
            "context_window": ctx,
            "max_output": ctx,
            "allocation_budget": {
                "kv_bytes_per_position": 22528,
                "scratch_bytes": 1234567,
                "mtp_state_bytes": 7654321,
                "margin_bytes": 1073741824u64
            }
        },
        "arguments": ["--engine", "oxe", "--ctx-size", ctx.to_string(), "--oxe-cpus", "0-3", "--spec-type", "none"]
    })
}

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// An engine directory with a stand-in `oxe` that prints `report` and an `oxe-server`.
fn engine_dir(env: &TestEnv, report: &Value) -> PathBuf {
    let dir = env.root().join("engine");
    fs::create_dir_all(&dir).unwrap();
    script(
        &dir.join("oxe"),
        &format!("d=${{0%/*}}\nprintf '%s\\n' \"$@\" > \"$d/oxe-args.txt\"\nprintf '%s' '{report}'\n"),
    );
    script(&dir.join("oxe-server"), "exit 0\n");
    fs::canonicalize(dir).unwrap()
}

fn command(env: &TestEnv, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
    cmd.args(args).current_dir(env.work()).env_clear();
    for (key, value) in env.envs("http://127.0.0.1:9") {
        cmd.env(key, value);
    }
    cmd
}

fn run(env: &TestEnv, args: &[&str]) -> Output {
    command(env, args).output().unwrap()
}

fn add(env: &TestEnv, dir: &Path, extra: &[&str]) -> Output {
    let model = env.write("models/m.gguf", "synthetic");
    let (model, dir) = (
        model.to_str().unwrap().to_owned(),
        dir.to_str().unwrap().to_owned(),
    );
    let mut args = vec!["local", "add", &model, "--engine-dir", &dir];
    args.extend_from_slice(extra);
    run(env, &args)
}

fn user_settings(env: &TestEnv) -> PathBuf {
    env.home().join(".oxa/agent/settings.json")
}

fn read(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn local_add_requires_owner_before_admission() {
    let env = TestEnv::new();
    let original = r#"{"defaultProjectTrust":"never"}"#;
    let settings = env.write("home/.oxa/agent/settings.json", original);
    for marker in ["AI_AGENT", "OXA_CODING_AGENT", "OXA_SUBAGENT_DEPTH"] {
        let out = command(&env, &["local", "add", "missing.gguf"])
            .env(marker, "")
            .output()
            .unwrap();
        assert!(!out.status.success(), "{marker}");
        assert!(
            stderr(&out).contains("Local model setup requires the owner outside an agent tool"),
            "{marker}: {}",
            stderr(&out)
        );
        assert!(out.stdout.is_empty(), "{marker}");
        assert_eq!(fs::read_to_string(&settings).unwrap(), original, "{marker}");
    }
}

#[test]
fn local_add_writes_user_settings_from_admission_report() {
    let _guard = LOCK.lock().unwrap();
    let env = TestEnv::new();
    let original = json!({"defaultProjectTrust": "always", "retry": {"enabled": false}}).to_string();
    env.write("home/.oxa/agent/settings.json", &original);
    let dir = engine_dir(&env, &report(4096));
    let out = add(
        &env,
        &dir,
        &[
            "--name",
            "synthetic-local",
            "--ctx",
            "4096",
            "--enable",
            "--",
            "--oxe-cpus",
            "0-3",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    // the engine was asked exactly what the owner asked
    let model = fs::canonicalize(env.root().join("models/m.gguf")).unwrap();
    let args = fs::read_to_string(dir.join("oxe-args.txt")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(
        args,
        [
            "admission",
            "--model",
            model.to_str().unwrap(),
            "--ctx",
            "4096",
            "--oxe-cpus",
            "0-3",
            "--json"
        ]
    );

    let settings = read(&user_settings(&env));
    let r = report(4096);
    let local = &settings["oxa"]["local"];
    assert_eq!(local["binary"], dir.join("oxe-server").to_str().unwrap());
    assert_eq!(local["environment"]["HOME"], env.home().to_str().unwrap());
    assert_eq!(
        local["models"]["synthetic-local"]["model"],
        model.to_str().unwrap()
    );
    assert_eq!(local["models"]["synthetic-local"]["arguments"], r["arguments"]);
    assert_eq!(local["limits"]["synthetic-local"], r["limits"]);
    assert_eq!(settings["oxa"]["engine"]["inProcessLocal"], true);
    // unrelated keys survive, and the previous file is kept next to it
    assert_eq!(settings["retry"]["enabled"], false);
    let backups: Vec<_> = fs::read_dir(env.home().join(".oxa/agent"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("settings.json.bak-")
        })
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(fs::read_to_string(&backups[0]).unwrap(), original);
    assert_eq!(
        fs::metadata(&backups[0]).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // the real loader accepts it: the alias is listed
    let listed = run(&env, &["--list-models"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert!(String::from_utf8_lossy(&listed.stdout).contains("synthetic-local"));

    // a second add of the same alias needs --replace
    let again = add(&env, &dir, &["--name", "synthetic-local", "--ctx", "4096"]);
    assert!(!again.status.success());
    assert!(stderr(&again).contains("--replace"), "{}", stderr(&again));
}

#[test]
fn local_add_without_enable_leaves_the_engine_switch_alone() {
    let _guard = LOCK.lock().unwrap();
    let env = TestEnv::new();
    let dir = engine_dir(&env, &report(2048));
    let out = add(&env, &dir, &["--name", "quiet", "--ctx", "2048"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let settings = read(&user_settings(&env));
    assert!(settings["oxa"]["engine"].get("inProcessLocal").is_none());
    assert_eq!(
        settings["oxa"]["local"]["limits"]["quiet"]["context_window"],
        2048
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("not enabled yet"));
}

#[test]
fn local_add_preserves_an_explicit_engine_home() {
    let _guard = LOCK.lock().unwrap();
    let env = TestEnv::new();
    let configured_home = env.root().join("engine-home");
    fs::create_dir_all(&configured_home).unwrap();
    env.write(
        "home/.oxa/agent/settings.json",
        &json!({
            "oxa": {"local": {"environment": {"HOME": configured_home}}}
        })
        .to_string(),
    );
    let dir = engine_dir(&env, &report(2048));

    let out = add(&env, &dir, &["--name", "preserved", "--ctx", "2048"]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        read(&user_settings(&env))["oxa"]["local"]["environment"]["HOME"],
        configured_home.to_str().unwrap()
    );
}

#[test]
fn local_add_never_writes_project_settings() {
    let _guard = LOCK.lock().unwrap();
    let env = TestEnv::new();
    let project =
        json!({"defaultProjectTrust": "always", "oxa": {"engine": {"inProcessLocal": false}}}).to_string();
    let project_file = env.write("work/.oxa/settings.json", &project);
    let dir = engine_dir(&env, &report(4096));
    let out = add(&env, &dir, &["--name", "p", "--ctx", "4096", "--enable"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(fs::read_to_string(&project_file).unwrap(), project);
    let entries: Vec<_> = fs::read_dir(env.work().join(".oxa"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(entries, ["settings.json"]);
    assert_eq!(
        read(&user_settings(&env))["oxa"]["engine"]["inProcessLocal"],
        true
    );
    assert!(read(&user_settings(&env))["oxa"]["local"]["models"]["p"].is_object());
}

#[test]
fn local_add_missing_value_is_an_error() {
    let _guard = LOCK.lock().unwrap();
    // each removal names the missing field and writes nothing (no settings file, no backup)
    let cases: [(&str, Box<dyn Fn(&mut Value)>); 9] = [
        (
            "scratch_bytes",
            Box::new(|r| {
                r["limits"]["allocation_budget"]
                    .as_object_mut()
                    .unwrap()
                    .remove("scratch_bytes");
            }),
        ),
        (
            "mtp_state_bytes",
            Box::new(|r| {
                r["limits"]["allocation_budget"]
                    .as_object_mut()
                    .unwrap()
                    .remove("mtp_state_bytes");
            }),
        ),
        (
            "kv_bytes_per_position",
            Box::new(|r| {
                r["limits"]["allocation_budget"]["kv_bytes_per_position"] = json!(0);
            }),
        ),
        (
            "margin_bytes",
            Box::new(|r| {
                r["limits"]["allocation_budget"]["margin_bytes"] = json!(0);
            }),
        ),
        (
            "context_window",
            Box::new(|r| {
                r["limits"].as_object_mut().unwrap().remove("context_window");
            }),
        ),
        (
            "max_output",
            Box::new(|r| {
                r["limits"]["max_output"] = Value::Null;
            }),
        ),
        (
            "/arguments",
            Box::new(|r| {
                r.as_object_mut().unwrap().remove("arguments");
            }),
        ),
        (
            "--ctx-size",
            Box::new(|r| {
                r["arguments"] = json!(["--engine", "oxe"]);
            }),
        ),
        (
            "oxe-admission/1",
            Box::new(|r| {
                r["schema"] = json!("other/9");
            }),
        ),
    ];
    for (field, edit) in cases {
        let env = TestEnv::new();
        let mut r = report(4096);
        edit(&mut r);
        let dir = engine_dir(&env, &r);
        let out = add(&env, &dir, &["--name", "m", "--ctx", "4096", "--enable"]);
        assert!(!out.status.success(), "{field}");
        assert!(stderr(&out).contains(field), "{field}: {}", stderr(&out));
        assert!(!user_settings(&env).exists(), "{field}: settings were written");
        assert!(
            !env.home().join(".oxa/agent").exists()
                || fs::read_dir(env.home().join(".oxa/agent"))
                    .unwrap()
                    .all(|e| { !e.unwrap().file_name().to_str().unwrap().contains("settings") }),
            "{field}: backup or settings written"
        );
    }

    // an engine that refuses (unknown architecture, ...) passes its reason on
    let env = TestEnv::new();
    let dir = env.root().join("engine");
    fs::create_dir_all(&dir).unwrap();
    script(
        &dir.join("oxe"),
        "echo 'oxe admission: architecture llama is not supported' >&2\nexit 2\n",
    );
    script(&dir.join("oxe-server"), "exit 0\n");
    let out = add(&env, &fs::canonicalize(&dir).unwrap(), &["--name", "m"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("architecture llama is not supported"),
        "{}",
        stderr(&out)
    );
    assert!(!user_settings(&env).exists());

    // no sibling oxe: an explained error naming the missing binary and the no-PATH rule
    let env = TestEnv::new();
    let empty = env.root().join("empty");
    fs::create_dir_all(&empty).unwrap();
    let out = add(&env, &fs::canonicalize(&empty).unwrap(), &[]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("`oxe`") && err.contains("no PATH search"), "{err}");

    // a narrower output limit may not exceed the context
    let env = TestEnv::new();
    let dir = engine_dir(&env, &report(4096));
    let out = add(&env, &dir, &["--ctx", "4096", "--max-output", "8192"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("exceeds the context window"),
        "{}",
        stderr(&out)
    );
    assert!(!user_settings(&env).exists());
}
