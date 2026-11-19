//! Real native CLI, live discovery and chat tools through the shared loopback fixture.
#![cfg(target_os = "linux")]
#[allow(dead_code, unexpected_cfgs)] // Shared gateway mock; CLI exercises JSON, not feature-gated form auth.
#[path = "../../oxc-gateway/tests/common/openai.rs"]
mod upstream;
#[path = "common/work_repo.rs"]
mod work_repo;
use oxa_fake::TestEnv;
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    process::{Command, Output, Stdio},
};
use upstream::{MockOpenAi, Reply};
const MODEL: &str = "opencode/synthetic-flash";
const CREDENTIAL: &str = "synthetic-provider-credential";

fn discovery(base: &str) -> Value {
    json!({"data":[{"id":"synthetic-flash","canonical_id":"synthetic/canonical","limit":{"context":32768,"output":512},
        "api":{"chat-completions":{"base_url":format!("{base}/inference/go/openai/v1")}},"cost":{"input":0.1},"capabilities":{"tools":true},"modalities":{"input":["text"],"output":["text"]}},
        {"id":"synthetic-messages","canonical_id":"synthetic/messages","limit":{"context":32768,"output":512},"api":{"messages":{"base_url":format!("{base}/inference/go/anthropic/v1")}}}]})
}
fn usage() -> Value {
    json!({"prompt_tokens":100,"completion_tokens":5,"total_tokens":105,"prompt_tokens_details":{"cached_tokens":80},"cost":0,
        "opencode":{"billing_source":"go","list_cost":0.001,"allowance":0.0008,
            "tokens":{"input":20,"output":5,"reasoning":1,"cache_read":80,"cache_write_5m":0,"cache_write_1h":0}}})
}
fn stream(delta: Value, reason: &str) -> Reply {
    let chunks = [
        json!({"id":"synthetic-chat","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"id":"synthetic-chat","choices":[{"index":0,"delta":{},"finish_reason":reason}]}),
        json!({"choices":[],"usage":usage()}),
    ];
    let mut bytes = Vec::new();
    for chunk in chunks {
        bytes.extend_from_slice(format!("data: {chunk}\r\n\r\n").as_bytes());
    }
    bytes.extend_from_slice(b"data: [DONE]\r\n\r\n");
    Reply::raw(200, "text/event-stream", bytes, vec![])
}
fn setup(env: &TestEnv, base: &str) {
    env.write("home/.oxa/agent/settings.json",&json!({"defaultProjectTrust":"always","defaultModel":MODEL,"defaultThinkingLevel":"off",
        "retry":{"enabled":false},"permissions":{"defaultMode":"auto"},"oxa":{"engine":{"inProcessCloud":true},
        "cloud":{"opencode":{"enabled":true,"base_url":format!("{base}/inference/go")},"subscription_limits":{"daily_requests":20,"daily_tokens":2000000}}}}).to_string());
    let path = env.home().join(".config/oxc/llm/cloud-secrets.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    write!(file, "{}", json!({"opencode":CREDENTIAL})).unwrap();
}
fn run(env: &TestEnv, args: &[&str]) -> Output {
    run_with(env, args, &[])
}
fn run_with(env: &TestEnv, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
    cmd.current_dir(env.work())
        .env_clear()
        .envs(env.envs("http://127.0.0.1:9"));
    for (name, value) in extra {
        cmd.env(name, value);
    }
    cmd.args(args).stdin(Stdio::null()).output().unwrap()
}
fn terminal(output: &Output) -> Value {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(|s| serde_json::from_str::<Value>(s).ok())
        .find(|v| v["type"] == "oxa_result")
        .expect("typed terminal result")
}
fn rows(env: &TestEnv) -> Vec<Value> {
    fs::read_dir(env.home().join(".oxa/agent/cloud-ledger"))
        .unwrap()
        .filter_map(|e| {
            let path = e.unwrap().path();
            path.extension()
                .is_some_and(|e| e == "jsonl")
                .then(|| fs::read_to_string(path).unwrap())
        })
        .flat_map(|s| {
            s.lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}
#[test]
fn a_key_enables_go_catalog_and_plan_chat_without_legacy_enable_or_limits() {
    let env = TestEnv::new();
    let patterns = [
        ("--model", MODEL),
        ("--model", "synthetic-flash"),
        ("--model", "oxe/opencode/synthetic-flash"),
        ("--models", MODEL),
        ("--models", "*-flash"),
    ];
    let mock = MockOpenAi::start_with(|base| {
        let mut replies = vec![Reply::json(discovery(base))];
        for _ in patterns {
            replies.extend([
                Reply::json(discovery(base)),
                stream(json!({"content":"synthetic plan reply"}), "stop"),
            ]);
        }
        replies
    });
    setup(&env, &format!("http://{}", mock.address()));
    let path = env.home().join(".oxa/agent/settings.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    settings["oxa"].as_object_mut().unwrap().remove("engine");
    let cloud = settings["oxa"]["cloud"].as_object_mut().unwrap();
    cloud.remove("subscription_limits");
    cloud["opencode"].as_object_mut().unwrap().remove("enabled");
    fs::write(&path, settings.to_string()).unwrap();

    let list = run(&env, &["--list-models"]);
    let status = String::from_utf8_lossy(&list.stderr);
    assert!(list.status.success(), "{status}");
    assert!(String::from_utf8_lossy(&list.stdout).contains(MODEL));
    assert!(status.contains("OpenCode Go: on"), "{status}");
    assert!(status.contains("oxa.cloud.opencode.enabled"), "{status}");
    assert!(!status.contains(CREDENTIAL));

    for (flag, pattern) in patterns {
        let output = run(&env, &[flag, pattern, "--no-tools", "-p", "Synthetic task"]);
        assert!(
            output.status.success(),
            "{pattern}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("synthetic plan reply"));
    }
    let ledger = rows(&env);
    assert_eq!(ledger.len(), patterns.len());
    for row in ledger {
        assert_eq!(row["billing"], "subscription");
        assert_eq!(row["cost_usd"].as_f64(), Some(0.0));
        assert_eq!(row["settled"], "usage");
    }
    assert_eq!(mock.requests().len(), 1 + 2 * patterns.len());
    mock.assert_healthy();
}

#[test]
fn taskfile_model_does_not_authorize_keyed_go() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start(vec![]);
    let (output, record) = run_go_work_taskfile(&env, &mock, false);
    assert!(record["worker"].is_object(), "{record}");
    assert!(!output.status.success());
    assert!(mock.requests().is_empty());
    assert!(!env.agent_dir().join("cloud-ledger").exists());
    mock.assert_healthy();
}

#[test]
fn taskfile_can_use_go_explicitly_enabled_by_the_owner() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            stream(json!({"content":"Synthetic work answer"}), "stop"),
        ]
    });
    let (_, record) = run_go_work_taskfile(&env, &mock, true);
    assert!(
        matches!(record["worker"]["exit_code"].as_i64(), Some(0 | 4)),
        "{record}"
    );
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        1
    );
    assert_eq!(rows(&env)[0]["remote_provider"], "opencode");
    mock.assert_healthy();
}

fn run_go_work_taskfile(env: &TestEnv, mock: &MockOpenAi, explicit: bool) -> (Output, Value) {
    setup(env, &format!("http://{}", mock.address()));
    work_repo::setup(env, "[]");
    let path = env.agent_dir().join("settings.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    settings.as_object_mut().unwrap().remove("defaultModel");
    settings["oxa"].as_object_mut().unwrap().remove("engine");
    if !explicit {
        settings["oxa"]["cloud"]["opencode"]
            .as_object_mut()
            .unwrap()
            .remove("enabled");
    }
    fs::write(path, settings.to_string()).unwrap();
    env.write(
        "work/tasks.json",
        &json!([{"id":"test","branch":"agent/test","model":MODEL,"prompt":"Synthetic work task"}])
            .to_string(),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:9"))
        .args(["work", "../tasks.json"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let record: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("missing work record: {}", String::from_utf8_lossy(&output.stderr)));
    (output, record)
}

#[test]
fn supervised_child_arguments_do_not_authorize_keyed_go() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start(vec![]);
    setup(&env, &format!("http://{}", mock.address()));
    let path = env.agent_dir().join("settings.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    settings.as_object_mut().unwrap().remove("defaultModel");
    settings["oxa"].as_object_mut().unwrap().remove("engine");
    settings["oxa"]["cloud"]["opencode"]
        .as_object_mut()
        .unwrap()
        .remove("enabled");
    fs::write(path, settings.to_string()).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs("http://127.0.0.1:9"))
        .env("OXA_SUBAGENT_DEPTH", "1")
        .args(["--model", MODEL, "--no-tools", "-p", "Synthetic child task"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(mock.requests().is_empty());
    assert!(!env.agent_dir().join("cloud-ledger").exists());
    mock.assert_healthy();
}

#[test]
fn native_go_continue_of_a_keyed_session_needs_the_owner_switch() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            stream(json!({"content":"Synthetic first reply"}), "stop"),
            Reply::json(discovery(base)),
            stream(json!({"content":"Synthetic resumed reply"}), "stop"),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    let path = env.agent_dir().join("settings.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    settings.as_object_mut().unwrap().remove("defaultModel");
    settings["oxa"].as_object_mut().unwrap().remove("engine");
    settings["oxa"]["cloud"]["opencode"]
        .as_object_mut()
        .unwrap()
        .remove("enabled");
    fs::write(&path, settings.to_string()).unwrap();

    let first = run(
        &env,
        &["--model", MODEL, "--no-tools", "-p", "Synthetic first task"],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stdout).contains("Synthetic first reply"));
    let posts = || {
        mock.requests()
            .iter()
            .filter(|request| request.method == "POST")
            .count()
    };
    assert_eq!(posts(), 1);

    // The restored model is provenance-less: the session records the ID, not the owner's
    // transport choice, so continuing it alone does not open the Go transport.
    let resumed = run(
        &env,
        &["--continue", "--no-tools", "-p", "Synthetic resumed task"],
    );
    assert!(!resumed.status.success());
    assert_eq!(posts(), 1, "no upstream request without the owner switch");
    assert_eq!(rows(&env).len(), 1);

    // The owner's own switch does continue it.
    let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    settings["oxa"]["cloud"]["opencode"]["enabled"] = json!(true);
    fs::write(&path, settings.to_string()).unwrap();
    let resumed = run(
        &env,
        &["--continue", "--no-tools", "-p", "Synthetic resumed task"],
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert!(String::from_utf8_lossy(&resumed.stdout).contains("Synthetic resumed reply"));
    assert_eq!(terminal(&resumed)["session_id"], terminal(&first)["session_id"]);
    assert_eq!(posts(), 2);
    assert_eq!(rows(&env).len(), 2);
    mock.assert_healthy();
}

#[test]
fn explicit_go_opt_in_still_selects_its_catalog_without_a_default_model() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            stream(json!({"content":"synthetic plan reply"}), "stop"),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    let path = env.agent_dir().join("settings.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    settings.as_object_mut().unwrap().remove("defaultModel");
    settings["oxa"].as_object_mut().unwrap().remove("engine");
    fs::write(path, settings.to_string()).unwrap();

    let output = run(&env, &["--no-tools", "-p", "Synthetic task"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("synthetic plan reply"));
    assert_eq!(mock.requests().len(), 2);
    mock.assert_healthy();
}

#[test]
fn configured_go_subscription_limits_warn_at_thresholds_and_keep_accounting() {
    for (limit, threshold) in [(41_600, "80%"), (33_279, "100%")] {
        let env = TestEnv::new();
        let mock = MockOpenAi::start_with(|base| {
            vec![
                Reply::json(discovery(base)),
                stream(json!({"content":"Synthetic plan reply"}), "stop"),
            ]
        });
        setup(&env, &format!("http://{}", mock.address()));
        let path = env.agent_dir().join("settings.json");
        let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        settings["oxa"]["cloud"]["subscription_limits"]["daily_tokens"] = json!(limit);
        fs::write(path, settings.to_string()).unwrap();

        let output = run(
            &env,
            &["--no-tools", "-p", "Synthetic request with advisory limits"],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(
            stderr.contains(&format!("projected usage reached {threshold}")),
            "{stderr}"
        );
        assert!(!stderr.contains(CREDENTIAL));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("Synthetic plan reply"));
        assert!(!stdout.contains("projected usage"));
        let ledger = rows(&env);
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0]["subscription_requests"], 1);
        assert_eq!(ledger[0]["subscription_tokens"], 105);
        mock.assert_healthy();
    }
}

#[test]
fn unavailable_go_reports_the_owner_fix_before_any_discovery() {
    for (disabled, reason) in [(true, "disabled"), (false, "no key")] {
        let env = TestEnv::new();
        let mock = MockOpenAi::start(vec![]);
        setup(&env, &format!("http://{}", mock.address()));
        let path = env.home().join(".oxa/agent/settings.json");
        let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        settings["oxa"]["cloud"]["opencode"]
            .as_object_mut()
            .unwrap()
            .remove("enabled");
        if disabled {
            settings["oxa"]["cloud"]["opencode"]["enabled"] = json!(false);
        } else {
            fs::remove_file(env.home().join(".config/oxc/llm/cloud-secrets.json")).unwrap();
        }
        fs::write(&path, settings.to_string()).unwrap();

        let output = run(&env, &["--model", MODEL, "--no-tools", "-p", "Synthetic task"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert_ne!(terminal(&output)["error_class"], "not_found", "{stderr}");
        assert!(stderr.contains("OpenCode Go"), "{stderr}");
        assert!(stderr.contains(reason), "{stderr}");
        assert!(stderr.contains("oxa.cloud.opencode.enabled"), "{stderr}");
        assert!(!stderr.contains(CREDENTIAL));
        assert!(mock.requests().is_empty());
        mock.assert_healthy();
    }
}

#[test]
fn project_default_cannot_select_a_credential_enabled_go_route() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start(vec![]);
    setup(&env, &format!("http://{}", mock.address()));
    let path = env.agent_dir().join("settings.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    settings.as_object_mut().unwrap().remove("defaultModel");
    settings["oxa"].as_object_mut().unwrap().remove("engine");
    settings["oxa"]["cloud"]["opencode"]
        .as_object_mut()
        .unwrap()
        .remove("enabled");
    fs::write(path, settings.to_string()).unwrap();
    env.write(
        "work/.oxa/settings.json",
        &json!({"defaultModel":MODEL}).to_string(),
    );

    let output = run(&env, &["--no-tools", "-p", "Synthetic task"]);
    assert!(!output.status.success());
    assert!(mock.requests().is_empty());
    assert!(!env.agent_dir().join("cloud-ledger").exists());
    mock.assert_healthy();
}

#[test]
fn an_agent_marker_does_not_make_a_model_argument_the_owners_choice() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            stream(json!({"content":"synthetic plan reply"}), "stop"),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    let path = env.agent_dir().join("settings.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    settings.as_object_mut().unwrap().remove("defaultModel");
    settings["oxa"].as_object_mut().unwrap().remove("engine");
    settings["oxa"]["cloud"]["opencode"]
        .as_object_mut()
        .unwrap()
        .remove("enabled");
    fs::write(path, settings.to_string()).unwrap();

    // Tool subprocesses carry these markers; their `--model` is not the owner's selection.
    for marker in ["AI_AGENT", "OXA_CODING_AGENT"] {
        let output = run_with(
            &env,
            &["--model", MODEL, "--no-tools", "-p", "Synthetic task"],
            &[(marker, "1")],
        );
        assert!(!output.status.success(), "{marker}");
        assert!(mock.requests().is_empty(), "{marker}");
    }
    assert!(!env.agent_dir().join("cloud-ledger").exists());

    // The same run from the owner's own shell does select the key-only Go model.
    let owner = run(&env, &["--model", MODEL, "--no-tools", "-p", "Synthetic task"]);
    assert!(
        owner.status.success(),
        "{}",
        String::from_utf8_lossy(&owner.stderr)
    );
    assert_eq!(mock.requests().len(), 2);
    mock.assert_healthy();
}

#[test]
fn a_fuzzy_local_alias_does_not_hide_the_exact_go_model_in_a_scope() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            stream(json!({"content":"synthetic plan reply"}), "stop"),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    let path = env.agent_dir().join("settings.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    settings.as_object_mut().unwrap().remove("defaultModel");
    // Native local with a neighbouring alias, and no explicit Go switch: the key plus the
    // exact scope is the admission.
    settings["oxa"]["engine"] = json!({"inProcessLocal": true});
    settings["oxa"]["local"] = json!({"binary":"/synthetic/not-launched-oxe",
        "models":{"opencode/synthetic-flash-local":{"model":"/synthetic/model.gguf"}},
        "limits":{"opencode/synthetic-flash-local":{"context_window":4096,"max_output":64,
            "allocation_budget":{"kv_bytes_per_position":1,"scratch_bytes":0,"mtp_state_bytes":0,"margin_bytes":1044480}}}});
    settings["oxa"]["cloud"]["opencode"]
        .as_object_mut()
        .unwrap()
        .remove("enabled");
    fs::write(path, settings.to_string()).unwrap();

    let output = run(&env, &["--models", MODEL, "--no-tools", "-p", "Synthetic task"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("synthetic plan reply"));
    assert_eq!(mock.requests().len(), 2);
    assert_eq!(rows(&env).len(), 1);
    mock.assert_healthy();
}

#[test]
fn a_go_scope_admission_serves_only_that_scope() {
    let env = TestEnv::new();
    let other = MockOpenAi::start(vec![]);
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            stream(json!({"content":"synthetic plan reply"}), "stop"),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    let path = env.agent_dir().join("settings.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    settings.as_object_mut().unwrap().remove("defaultModel");
    settings["oxa"].as_object_mut().unwrap().remove("engine");
    settings["oxa"]["cloud"]["opencode"]
        .as_object_mut()
        .unwrap()
        .remove("enabled");
    // The owner's Go scope is the admission, and another provider's alias is configured.
    settings["enabledModels"] = json!([MODEL]);
    settings["oxa"]["cloud"]["models"]["synthetic-other"] = json!({
        "provider":"openai", "upstream_model":"synthetic-other",
        "context_window":32768, "max_output":64,
        "base_url":format!("http://{}", other.address())
    });
    fs::write(path, settings.to_string()).unwrap();
    // The project may not move the run onto the other provider: neither through its default
    // nor through the effective scope it replaces the owner's with.
    env.write(
        "work/.oxa/settings.json",
        &json!({"defaultModel":"synthetic-other","enabledModels":["synthetic-other"]}).to_string(),
    );

    // The run stays on the model the owner's scope named, not on the project's choice.
    let output = run(&env, &["--no-tools", "-p", "Synthetic task"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("synthetic plan reply"));
    assert!(
        other.requests().is_empty(),
        "a Go admission must not spend another provider: {:?}",
        other.requests()
    );
    assert_eq!(rows(&env).len(), 1);
    mock.assert_healthy();
    other.assert_healthy();
}

#[test]
fn a_local_model_pattern_can_use_a_go_prefixed_alias_when_cloud_is_off() {
    let env = TestEnv::new();
    let alias = "opencode/synthetic-local";
    setup(&env, "http://127.0.0.1:9");
    let path = env.agent_dir().join("settings.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    settings["oxa"]["engine"] = json!({"inProcessLocal":true, "inProcessCloud":false});
    settings["oxa"]["local"] = json!({
        "binary":"/synthetic/not-launched-oxe",
        "models":{(alias):{"model":"/synthetic/missing.gguf"}},
        "limits":{(alias):{"context_window":4096,"max_output":64,"thinking":true,
            "allocation_budget":{"kv_bytes_per_position":1,"scratch_bytes":0,
                                "mtp_state_bytes":0,"margin_bytes":1044480}}}
    });
    fs::write(path, settings.to_string()).unwrap();

    for pattern in ["opencode/SYNTHETIC-LOCAL", "opencode/synthetic-local:high"] {
        let output = run(&env, &["--model", pattern, "--no-tools", "-p", "Synthetic task"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(
            stderr.contains("Local weights are missing"),
            "{pattern}: {stderr}"
        );
        assert!(!stderr.contains("OpenCode Go"), "{pattern}: {stderr}");
    }
}

#[test]
fn go_inventory_shows_disabled_or_broken_alongside_other_models() {
    for disabled in [false, true] {
        let env = TestEnv::new();
        let replies = if disabled {
            vec![]
        } else {
            vec![Reply::error(401, "invalid_request_error", CREDENTIAL)]
        };
        let mock = MockOpenAi::start(replies);
        setup(&env, &format!("http://{}", mock.address()));
        let path = env.home().join(".oxa/agent/settings.json");
        let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        settings["oxa"]["cloud"]["opencode"]["enabled"] = json!(!disabled);
        settings["oxa"]["cloud"]["models"]["synthetic-other"] = json!({
            "provider":"openai", "upstream_model":"synthetic-other",
            "context_window":32768, "max_output":64
        });
        fs::write(&path, settings.to_string()).unwrap();

        let list = run(&env, &["--list-models"]);
        let status = String::from_utf8_lossy(&list.stderr);
        assert!(list.status.success(), "{status}");
        let models = String::from_utf8_lossy(&list.stdout);
        assert!(models.contains("synthetic-other"), "{models}");
        assert!(!models.contains(MODEL), "{models}");
        let state = if disabled { "off" } else { "broken" };
        assert!(status.contains(&format!("OpenCode Go: {state}")), "{status}");
        assert!(status.contains("oxa.cloud.opencode.enabled"), "{status}");
        assert!(!status.contains(CREDENTIAL));
        assert_eq!(mock.requests().len(), usize::from(!disabled));
        mock.assert_healthy();
    }
}

#[test]
fn native_go_review_skill_schema_and_tool_loop_settle_subscription_rows() {
    let env = TestEnv::new();
    env.write("work/note.txt", "Synthetic review evidence.");
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            stream(
                json!({"role":"assistant","reasoning_content":"Check evidence.","tool_calls":[{"index":0,"id":"read-note","type":"function","function":{"name":"read","arguments":"{\"path\":\"note.txt\"}"}}]}),
                "tool_calls",
            ),
            stream(
                json!({"role":"assistant","content":"{\"findings\":[],\"overall_verdict\":\"correct\"}"}),
                "stop",
            ),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    let skill = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../oxa-resources/builtin-skills/review/SKILL.md"
    );
    let schema = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../oxa-resources/builtin-skills/review/findings.schema.json"
    );
    let output = run(
        &env,
        &[
            "-p",
            "--tools",
            "read",
            "--no-skills",
            "--skill",
            skill,
            "--output-schema",
            schema,
            "/skill:review Review the synthetic evidence in note.txt.",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(terminal(&output)["outcome"], "done");
    assert!(String::from_utf8_lossy(&output.stdout).contains("overall_verdict"));
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].target, "/inference/go/v1/models");
    assert_eq!(requests[1].target, "/inference/go/openai/v1/chat/completions");
    assert_eq!(requests[1].body["model"], "synthetic-flash");
    assert_eq!(
        requests[1].header("authorization"),
        Some(format!("Bearer {CREDENTIAL}").as_str())
    );
    assert!(requests[1].header("user-agent").unwrap().starts_with("oxa/"));
    let hint = requests[1].header("x-opencode-session").unwrap();
    assert_eq!(hint.len(), 32);
    assert_eq!(requests[2].header("x-opencode-session"), Some(hint));
    assert!(requests[2].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "tool"
            && m["content"]
                .as_str()
                .is_some_and(|s| s.contains("Synthetic review evidence"))));
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 2);
    for row in ledger {
        assert_eq!(row["billing"], "subscription");
        assert_eq!(row["cost_usd"], 0.0);
        assert_eq!(row["remote_provider"], "opencode");
        assert_eq!(row["opencode"]["allowance"], 0.0008);
    }
    assert!(!String::from_utf8_lossy(&output.stdout).contains(CREDENTIAL));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(CREDENTIAL));
    mock.assert_healthy();
}
#[test]
fn native_go_plus_usage_on_finish_settles_subscription() {
    let env = TestEnv::new();
    let mut final_usage = usage();
    final_usage["opencode"]["billing_source"] = json!("go-plus");
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            terminal_frames(
                &[
                    json!({"choices":[{"index":0,"delta":{"content":"Synthetic Go Plus complete"},"finish_reason":null}],"usage":null}),
                    json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":final_usage}),
                ],
                true,
            ),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    let output = run(&env, &["--no-session", "--no-tools", "-p", "Synthetic request"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Synthetic Go Plus complete"));
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].body["stream_options"]["include_usage"], true);
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0]["billing"], "subscription");
    assert_eq!(ledger[0]["cost_usd"], 0.0);
    assert_eq!(ledger[0]["settled"], "usage");
    assert_eq!(ledger[0]["subscription_tokens"], 105);
    assert!(ledger[0].get("usage_missing").is_none());
    mock.assert_healthy();
}
#[test]
fn native_go_records_bounded_plan_labels_in_usage_and_status() {
    for (source, expected) in [
        (json!("go"), "go"),
        (json!("go-plus"), "go-plus"),
        (json!("future-plan"), "future-plan"),
        (json!("bad plan"), "unknown"),
        (json!(CREDENTIAL), "unknown"),
        (Value::Null, "unknown"),
        (json!("x".repeat(33)), "unknown"),
    ] {
        check_go_billing(source, expected, 0.0, true);
    }
}
#[test]
fn native_go_paid_balance_settles_cost_and_admission_continues() {
    check_go_billing(json!("balance"), "balance", 0.01, true);
}
#[test]
fn native_go_subcent_paid_cost_is_recorded_and_admission_continues() {
    check_go_billing(json!("balance"), "balance", 4e-9, true);
}
#[test]
fn native_go_paid_byok_without_usd_cap_keeps_existing_admission() {
    check_go_billing(json!("byok"), "byok", 0.01, false);
}

fn capped_go(env: &TestEnv, base: &str) {
    setup(env, base);
    let path = env.home().join(".oxa/agent/settings.json");
    let mut settings: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    settings["oxa"]["cloud"]["daily_budget_usd"] = json!(0.01);
    fs::write(path, settings.to_string()).unwrap();
}

#[test]
fn native_go_processes_forward_concurrently_without_local_usd_holds() {
    let env = TestEnv::new();
    let (held, release) = stream(json!({"content":"Synthetic held reply"}), "stop").gated();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            held,
            Reply::json(discovery(base)),
            stream(json!({"content":"Synthetic concurrent reply"}), "stop"),
        ]
    });
    capped_go(&env, &format!("http://{}", mock.address()));
    let first = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs("http://127.0.0.1:9"))
        .args(["--no-session", "--no-tools", "-p", "Synthetic first request"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    mock.wait_for_requests(2);
    // The second process forwards and settles while the first still holds its
    // reply open. A tiny or fully held cap must never refuse either process.
    let second = run(
        &env,
        &["--no-session", "--no-tools", "-p", "Synthetic second request"],
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    release.send(()).unwrap();
    let first = first.wait_with_output().unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 2);
    for row in &ledger {
        assert_eq!(row["settled"], "usage");
        assert_eq!(row["billing"], "subscription");
        assert_eq!(row["cost_usd"], 0.0);
        assert!(row.get("go_reserved_usd").is_none());
    }
    assert_eq!(mock.requests().len(), 4);
    for output in [&first, &second] {
        assert!(!String::from_utf8_lossy(&output.stdout).contains(CREDENTIAL));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(CREDENTIAL));
    }
    mock.assert_healthy();
}

#[test]
fn native_go_unknown_cost_stays_unknown_and_admission_continues() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start_with(|base| {
        let finish = json!({"choices":[{"index":0,"delta":{"content":"Synthetic next process reply"},"finish_reason":"stop"}]});
        let mut paid = usage();
        paid["opencode"]["billing_source"] = json!("balance");
        paid["cost"] = json!(0.02);
        let mut estimated = paid.clone();
        estimated["opencode"]["estimated"] = json!(true);
        vec![
            Reply::json(discovery(base)),
            terminal_frames(
                &[json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})],
                true,
            ),
            Reply::json(discovery(base)),
            terminal_frames(&[finish.clone(), json!({"choices":[],"usage":estimated})], true),
            Reply::json(discovery(base)),
            terminal_frames(&[finish, json!({"choices":[],"usage":paid})], true),
        ]
    });
    capped_go(&env, &format!("http://{}", mock.address()));
    let first = run(
        &env,
        &["--no-session", "--no-tools", "-p", "Synthetic unknown usage"],
    );
    assert!(!first.status.success());
    // Each new process must read earlier unknown rows and still forward.
    for prompt in ["Synthetic estimated usage", "Synthetic paid usage"] {
        let output = run(&env, &["--no-session", "--no-tools", "-p", prompt]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(mock.requests().len(), 6);
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 3);
    for row in &ledger[..2] {
        assert_eq!(row["settled"], "reserved");
        assert_eq!(row["usage_missing"], true);
        assert_eq!(row["billing"], "subscription");
        assert!(row.get("cost_usd").is_none(), "{row}");
        assert!(row.get("reported_cost_usd").is_none(), "{row}");
        assert!(row.get("estimated_cost_usd").is_none(), "{row}");
        assert!(row.get("go_reserved_usd").is_none(), "{row}");
    }
    assert_eq!(ledger[2]["settled"], "usage");
    assert_eq!(ledger[2]["cost_usd"], 0.02);
    assert_eq!(ledger[2]["reported_cost_usd"], 0.02);
    mock.assert_healthy();
}

#[test]
fn native_go_settled_paid_and_free_usage_release_unused_usd_across_processes() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start_with(|base| {
        [0.002, 0.0, 0.002]
            .into_iter()
            .flat_map(|cost| {
                let mut reported = usage();
                reported["cost"] = json!(cost);
                reported["opencode"]["billing_source"] = json!("balance");
                [
                    Reply::json(discovery(base)),
                    terminal_frames(
                        &[json!({"choices":[{"index":0,"delta":{"content":"Synthetic settlement"},"finish_reason":"stop"}],"usage":reported})],
                        true,
                    ),
                ]
            })
            .collect()
    });
    capped_go(&env, &format!("http://{}", mock.address()));
    for _ in 0..3 {
        let output = run(
            &env,
            &["--no-session", "--no-tools", "-p", "Synthetic settlement"],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 3);
    assert!(ledger.iter().all(|row| row["settled"] == "usage"));
    let spent: f64 = ledger.iter().map(|row| row["cost_usd"].as_f64().unwrap()).sum();
    assert_eq!(spent, 0.004);
    assert_eq!(mock.requests().len(), 6);
    mock.assert_healthy();
}
fn check_go_billing(source: Value, expected: &str, cost: f64, cap: bool) {
    let env = TestEnv::new();
    let mut final_usage = usage();
    final_usage["opencode"]["billing_source"] = source;
    final_usage["cost"] = json!(cost);
    let mock = MockOpenAi::start_with(|base| {
        let reply = || {
            terminal_frames(
                &[
                    json!({"choices":[{"index":0,"delta":{"content":"Synthetic billing result"},"finish_reason":"stop"}],"usage":final_usage}),
                ],
                true,
            )
        };
        vec![
            Reply::json(discovery(base)),
            reply(),
            Reply::json(discovery(base)),
            reply(),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    let path = env.home().join(".oxa/agent/settings.json");
    let mut settings: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    if cap {
        settings["oxa"]["cloud"]["daily_budget_usd"] = json!(if cost > 0.0 { cost } else { 0.01 });
    }
    fs::write(path, settings.to_string()).unwrap();
    let output = run(&env, &["--no-tools", "-p", "Synthetic request"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0]["opencode"]["billing_source"], expected);
    assert_eq!(ledger[0]["cost_usd"], cost);
    assert_eq!(ledger[0]["settled"], "usage");
    let last = &terminal(&output)["last_billing"];
    assert_eq!(last["plan"], expected);
    assert_eq!(last["cost_usd"], cost);
    assert_eq!(
        last["display"],
        if cost > 0.0 {
            format!("billed USD {cost:.2}")
        } else {
            format!("plan {expected}")
        }
    );
    let session = terminal(&output)["session_id"].as_str().unwrap().to_string();
    let status = run(&env, &["status", &session]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["last_billing"], *last);

    assert_eq!(
        ledger[0]["billing"],
        if cost > 0.0 { "metered" } else { "subscription" }
    );
    if cost > 0.0 {
        // The recorded charge consumes or exceeds the configured day cap, yet
        // a new process keeps admitting Go: the charge is recorded, never a
        // local stop. The `cap` flag only makes the day cap exact.
        let next = run(
            &env,
            &["--no-session", "--no-tools", "-p", "Synthetic next request"],
        );
        assert!(next.status.success(), "{}", String::from_utf8_lossy(&next.stderr));
        assert_eq!(mock.requests().len(), 4);
        let ledger = rows(&env);
        assert_eq!(ledger.len(), 2);
        assert_eq!(ledger[0]["cost_usd"], cost);
        assert_eq!(ledger[1]["cost_usd"], cost);
    } else {
        assert_eq!(mock.requests().len(), 2);
    }
    mock.assert_healthy();
}
#[test]
fn region_and_rate_refusals_stay_typed_without_paid_fallback() {
    for (status,message,expected) in [(400,"Model is hosted outside Europe and the United States; https://example.invalid/workspaces/synthetic","region_blocked"),
        (429,"Synthetic subscription allowance exhausted","cloud_rate_limited")] {
        let env = TestEnv::new();
        let mock = MockOpenAi::start_with(|base|vec![Reply::json(discovery(base)),Reply::error(status,"invalid_request_error",message)]);
        setup(&env,&format!("http://{}",mock.address()));
        let output = run(&env,&["-p","--no-tools","Synthetic request"]);
        assert!(!output.status.success());
        assert_eq!(terminal(&output)["error_class"],expected);
        assert_eq!(mock.requests().len(),2);
        assert!(!String::from_utf8_lossy(&output.stderr).contains("workspaces"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(CREDENTIAL));
        let ledger = rows(&env); assert_eq!(ledger.len(),1); assert_eq!(ledger[0]["cost_usd"],0.0);
        mock.assert_healthy();
    }
}

#[test]
fn native_go_401_keeps_public_credentials_error_and_a_valid_refusal_receipt() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            Reply::error(401, "authentication_error", CREDENTIAL),
            Reply::json(discovery(base)),
            stream(json!({"content":"Synthetic recovery"}), "stop"),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    let first = run(&env, &["--no-session", "--no-tools", "-p", "Synthetic request"]);
    assert_eq!(
        first.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(terminal(&first)["error_class"], "cloud_credentials_unavailable");
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0]["status"], 401);
    assert_eq!(ledger[0]["error"], "cloud_credentials_unavailable");
    assert_eq!(ledger[0]["settled"], "refused");
    assert_eq!(ledger[0]["subscription_requests"], 1);
    assert_eq!(ledger[0]["subscription_tokens"], 0);
    let second = run(
        &env,
        &["--no-session", "--no-tools", "-p", "Synthetic recovery request"],
    );
    assert_eq!(
        second.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(rows(&env).len(), 2);
    assert_eq!(mock.requests().len(), 4);
    for output in [first, second] {
        assert!(!String::from_utf8_lossy(&output.stdout).contains(CREDENTIAL));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(CREDENTIAL));
    }
    mock.assert_healthy();
}

#[test]
fn native_go_all_429_bodies_are_retryable_and_numeric_retry_after_is_bounded() {
    for (kind, headers, delay) in [
        ("RateLimit.Exceeded", vec![("Retry-After", "7".into())], Some(7)),
        (
            "inference_failed",
            vec![("Retry-After", "4294967296".into())],
            None,
        ),
        ("unknown", vec![("Retry-After", CREDENTIAL.into())], None),
        (
            "unknown",
            vec![("Retry-After", "7".into()), ("Retry-After", "8".into())],
            None,
        ),
    ] {
        let env = TestEnv::new();
        let mock = MockOpenAi::start_with(|base| {
            vec![
                Reply::json(discovery(base)),
                Reply::raw(
                    429,
                    "application/json",
                    json!({"error":{"type":kind,"message":CREDENTIAL}})
                        .to_string()
                        .into_bytes(),
                    headers,
                ),
            ]
        });
        setup(&env, &format!("http://{}", mock.address()));
        let output = run(&env, &["--no-session", "--no-tools", "-p", "Synthetic request"]);
        assert_eq!(terminal(&output)["error_class"], "cloud_rate_limited");
        let stderr = String::from_utf8_lossy(&output.stderr);
        if let Some(delay) = delay {
            assert!(stderr.contains(&format!("retry after {delay} s")), "{stderr}");
        } else {
            assert!(!stderr.contains("retry after "), "{stderr}");
        }
        assert!(!stderr.contains("subscription_limit_reached"));
        assert!(!stderr.contains(CREDENTIAL));
        let ledger = rows(&env);
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0]["status"], 429);
        assert_eq!(ledger[0]["error"], "cloud_rate_limited");
        assert_eq!(ledger[0]["subscription_tokens"], 0);
        assert_eq!(ledger[0]["settled"], "refused");
        assert_eq!(mock.requests().len(), 2);
        mock.assert_healthy();
    }
}

fn enable_one_fast_retry(env: &TestEnv) {
    let path = env.home().join(".oxa/agent/settings.json");
    let mut settings: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    settings["retry"] = json!({"enabled":true,"maxRetries":1,"baseDelayMs":1,"maxAgentDelayMs":1});
    fs::write(path, settings.to_string()).unwrap();
}

#[test]
fn native_go_rate_limit_uses_existing_bounded_retry_then_completes() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            Reply::raw(
                429,
                "application/json",
                b"not JSON".to_vec(),
                vec![("Retry-After", "0".into())],
            ),
            stream(json!({"content":"Synthetic after rate wait"}), "stop"),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    enable_one_fast_retry(&env);
    let output = run(
        &env,
        &[
            "--no-session",
            "--no-tools",
            "--mode",
            "json",
            "-p",
            "Synthetic request",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<Value> = output
        .stdout
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect();
    let retries: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "auto_retry_start")
        .collect();
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0]["delayMs"], 0);
    assert!(retries[0]["errorMessage"]
        .as_str()
        .unwrap()
        .contains("retry after 0 s"));
    assert_eq!(mock.requests().len(), 3);
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 2);
    assert!(ledger
        .iter()
        .any(|row| row["status"] == 429 && row["settled"] == "refused"));
    assert!(ledger
        .iter()
        .any(|row| row["status"] == 200 && row["settled"] == "usage"));
    mock.assert_healthy();
}

#[test]
fn native_go_rate_limit_defers_instead_of_retrying_before_provider_wait() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            Reply::raw(
                429,
                "application/json",
                b"{}".to_vec(),
                vec![("Retry-After", "7".into())],
            ),
            stream(json!({"content":"Retry attempted too early"}), "stop"),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    enable_one_fast_retry(&env);
    let output = run(&env, &["--no-session", "--no-tools", "-p", "Synthetic request"]);
    assert_eq!(terminal(&output)["error_class"], "cloud_rate_limited");
    assert!(String::from_utf8_lossy(&output.stderr).contains("retry after 7 s"));
    assert_eq!(mock.requests().len(), 2, "only discovery and the refused request");
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0]["settled"], "refused");
    mock.assert_healthy();
}

#[test]
fn native_go_inference_failed_is_a_request_error_without_a_retry_or_allowance_claim() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            Reply::error(400, "inference_failed", CREDENTIAL),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    enable_one_fast_retry(&env);
    let output = run(&env, &["--no-session", "--no-tools", "-p", "Synthetic request"]);
    assert_eq!(terminal(&output)["error_class"], "engine_http");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("HTTP 400"), "{stderr}");
    assert!(!stderr.contains("subscription_limit"));
    assert!(!stderr.contains(CREDENTIAL));
    assert_eq!(mock.requests().len(), 2);
    assert_eq!(rows(&env)[0]["status"], 400);
    mock.assert_healthy();
}
fn set_one_request_token_cap(env: &TestEnv) {
    let path = env.home().join(".oxa/agent/settings.json");
    let mut settings: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    // Exactly one discovered context+output window can be reserved at a time.
    settings["oxa"]["cloud"]["subscription_limits"]["daily_tokens"] = json!(32768 + 512);
    fs::write(path, settings.to_string()).unwrap();
}

#[test]
fn native_go_rejected_requests_release_tokens_before_the_next_valid_request() {
    for status in [400, 404, 413] {
        let env = TestEnv::new();
        let mock = MockOpenAi::start_with(|base| {
            vec![
                Reply::json(discovery(base)),
                Reply::error(status, "inference_failed", CREDENTIAL),
                Reply::json(discovery(base)),
                stream(json!({"content":"Synthetic valid request"}), "stop"),
            ]
        });
        setup(&env, &format!("http://{}", mock.address()));
        set_one_request_token_cap(&env);
        let rejected = run(
            &env,
            &["--no-session", "--no-tools", "-p", "Synthetic rejected request"],
        );
        assert_eq!(terminal(&rejected)["error_class"], "engine_http");
        let ledger = rows(&env);
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0]["status"], status);
        assert_eq!(ledger[0]["settled"], "refused", "HTTP{status}");
        assert_eq!(ledger[0]["subscription_requests"], 1);
        assert_eq!(ledger[0]["subscription_tokens"], 0);
        let valid = run(
            &env,
            &["--no-session", "--no-tools", "-p", "Synthetic valid request"],
        );
        assert_eq!(
            valid.status.code(),
            Some(0),
            "HTTP{status}: {}",
            String::from_utf8_lossy(&valid.stderr)
        );
        assert_eq!(rows(&env).len(), 2);
        assert_eq!(mock.requests().len(), 4);
        assert!(!String::from_utf8_lossy(&rejected.stderr).contains(CREDENTIAL));
        mock.assert_healthy();
    }
}

#[test]
fn native_go_5xx_usage_stays_reserved_and_the_token_limit_warns() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            Reply::error(500, "inference_failed", CREDENTIAL),
            Reply::json(discovery(base)),
            stream(json!({"content":"Synthetic next reply"}), "stop"),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    set_one_request_token_cap(&env);
    let failed = run(
        &env,
        &["--no-session", "--no-tools", "-p", "Synthetic failed request"],
    );
    assert_eq!(terminal(&failed)["error_class"], "engine_http");
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0]["settled"], "reserved");
    assert_eq!(ledger[0]["subscription_tokens"], 32768 + 512);
    assert_eq!(ledger[0]["usage_missing"], true);
    let next = run(
        &env,
        &["--no-session", "--no-tools", "-p", "Synthetic next request"],
    );
    let stderr = String::from_utf8_lossy(&next.stderr);
    assert!(next.status.success(), "{stderr}");
    assert!(stderr.contains("projected usage reached 100%"), "{stderr}");
    assert!(String::from_utf8_lossy(&next.stdout).contains("Synthetic next reply"));
    assert_eq!(mock.requests().len(), 4);
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 2);
    assert_eq!(ledger[0]["subscription_tokens"], 32768 + 512);
    assert_eq!(ledger[1]["subscription_tokens"], 105);
    mock.assert_healthy();
}

#[test]
fn native_go_text_deltas_allow_null_or_empty_tool_calls_and_keep_final_usage() {
    for calls in [Value::Null, json!([])] {
        let env = TestEnv::new();
        let mock = MockOpenAi::start_with(|base| {
            vec![
                Reply::json(discovery(base)),
                stream(
                    json!({"content":"Synthetic nullable text", "tool_calls":calls}),
                    "stop",
                ),
            ]
        });
        setup(&env, &format!("http://{}", mock.address()));
        let output = run(&env, &["--no-session", "--no-tools", "-p", "Synthetic request"]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("Synthetic nullable text"));
        let ledger = rows(&env);
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0]["settled"], "usage");
        assert_eq!(ledger[0]["subscription_tokens"], 105);
        assert!(ledger[0].get("usage_missing").is_none());
        assert_eq!(mock.requests().len(), 2);
        // The client returned at [DONE]; draining peers before the verdict keeps
        // the late transfer-end write out of the health check (#514 lane).
        mock.finish();
    }
}

#[test]
fn native_go_wrong_tool_call_shapes_still_fail_closed_with_unknown_usage() {
    for (case, calls) in [
        ("object", json!({})),
        ("string", json!(CREDENTIAL)),
        ("number", json!(0)),
        ("boolean", json!(false)),
    ] {
        let env = TestEnv::new();
        let mock = MockOpenAi::start_with(|base| {
            vec![
                Reply::json(discovery(base)),
                stream(
                    json!({"content":"Synthetic invalid shape", "tool_calls":calls}),
                    "stop",
                ),
            ]
        });
        setup(&env, &format!("http://{}", mock.address()));
        let output = run(&env, &["--no-session", "--no-tools", "-p", "Synthetic request"]);
        assert_eq!(terminal(&output)["error_class"], "stream_broken");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("cause=tool_shape"),
            "malformed tool shape must be rejected before a framing/EOF error"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("Synthetic invalid shape"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(CREDENTIAL));
        let ledger = rows(&env);
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0]["settled"], "reserved");
        assert_eq!(ledger[0]["subscription_tokens"], 32768 + 512);
        assert_eq!(ledger[0]["usage_missing"], true);
        assert_eq!(mock.requests().len(), 2);
        eprintln!("synthetic_tool_shape_case={case}");
        mock.finish();
    }
}

#[test]
fn live_discovery_lists_chat_models_only_and_offline_precedes_credentials() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start_with(|base| vec![Reply::json(discovery(base))]);
    setup(&env, &format!("http://{}", mock.address()));
    // Live Go discovery must extend, rather than replace, the same factory's
    // static routes. No other provider credential is installed by setup().
    let path = env.home().join(".oxa/agent/settings.json");
    let mut settings: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    settings["oxa"]["engine"]["inProcessLocal"] = json!(false);
    for provider in ["anthropic", "openrouter", "openai", "chatgpt"] {
        settings["oxa"]["cloud"]["models"][format!("synthetic-{provider}")] = json!({"provider":provider,"upstream_model":"synthetic-model",
                "context_window":32768,"max_output":64,"pricing":{
                    "input_per_million":1,"output_per_million":2}});
    }
    fs::write(&path, settings.to_string()).unwrap();
    let output = run(&env, &["--list-models"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(MODEL));
    for provider in ["anthropic", "openrouter", "openai", "chatgpt"] {
        assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("synthetic-{provider}")));
    }
    assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-messages"));
    assert!(!env.home().join(".config/oxc/llm/chatgpt-auth.json").exists());
    assert!(!env.home().join(".oxa/agent/cloud-ledger").exists());
    fs::remove_file(env.home().join(".config/oxc/llm/cloud-secrets.json")).unwrap();
    let output = run(&env, &["--offline", "-p", "Synthetic request"]);
    assert_eq!(terminal(&output)["error_class"], "offline");
    assert_eq!(mock.requests().len(), 1);
    mock.assert_healthy();
}

#[test]
fn non_json_discovery_refusals_keep_http_classification_without_exposing_body() {
    for (status, expected) in [
        (401, "cloud_credentials_unavailable"),
        (403, "cloud_forbidden"),
        (429, "cloud_rate_limited"),
    ] {
        let env = TestEnv::new();
        let mock = MockOpenAi::start(vec![Reply::raw(
            status,
            "text/html",
            format!("<html>{CREDENTIAL}</html>").into_bytes(),
            vec![],
        )]);
        setup(&env, &format!("http://{}", mock.address()));
        let output = run(&env, &["-p", "--no-tools", "Synthetic task"]);
        assert_eq!(terminal(&output)["error_class"], expected);
        assert!(!output.status.success());
        assert_eq!(mock.requests().len(), 1);
        assert_eq!(mock.requests()[0].method, "GET");
        assert!(!env.home().join(".oxa/agent/cloud-ledger").exists());
        assert!(!String::from_utf8_lossy(&output.stderr).contains(CREDENTIAL));
        assert!(!String::from_utf8_lossy(&output.stdout).contains(CREDENTIAL));
        mock.assert_healthy();
    }
}

#[test]
fn invalid_discovery_reports_only_fixed_stage_categories_before_any_inference() {
    for (case, message) in [
        ("json", "Go model discovery returned invalid JSON."),
        ("envelope", "Go model discovery has an invalid data envelope."),
        ("id", "Go model discovery has an invalid model id."),
        ("canonical", "Go model discovery has an invalid canonical id."),
        ("collision", "Go model discovery has colliding canonical ids."),
        ("partial", "Go model discovery has an invalid context limit."),
        ("context", "Go model discovery has an invalid context limit."),
        ("output", "Go model discovery has an invalid output limit."),
        ("exceeds", "Go model discovery output limit exceeds context."),
        ("api", "Go model discovery has invalid API metadata."),
        ("endpoint", "Go model discovery has an unsafe API endpoint."),
        ("duplicate", "Go model discovery repeats a model id."),
        (
            "echo",
            "Go model discovery contained a credential; metadata was refused.",
        ),
    ] {
        let env = TestEnv::new();
        let mock = MockOpenAi::start_with(|base| {
            if case == "json" {
                return vec![Reply::raw(
                    200,
                    "application/json",
                    b"<html>synthetic</html>".to_vec(),
                    vec![],
                )];
            }
            let mut value = discovery(base);
            match case {
                "envelope" => value["data"] = json!({}),
                "id" => value["data"][0]["id"] = "invalid id".into(),
                "canonical" => value["data"][0]["id"] = "---".into(),
                "collision" => {
                    value["data"][0]["id"] = "Synthetic.Flash".into();
                    value["data"][1]["id"] = "synthetic-flash".into();
                }
                "partial" => value["data"].as_array_mut().unwrap().push(json!({})),
                "context" => value["data"][0]["limit"]["context"] = 0.into(),
                "output" => value["data"][0]["limit"]["output"] = 0.into(),
                "exceeds" => value["data"][0]["limit"]["output"] = 32769.into(),
                "api" => value["data"][0]["api"] = json!([]),
                "endpoint" => {
                    value["data"][0]["api"]["chat-completions"]["base_url"] =
                        format!("{base}/outside/v1").into()
                }
                "duplicate" => {
                    let item = value["data"][0].clone();
                    value["data"].as_array_mut().unwrap().push(item);
                }
                "echo" => value["unexpected"] = CREDENTIAL.into(),
                _ => unreachable!(),
            }
            vec![Reply::json(value)]
        });
        setup(&env, &format!("http://{}", mock.address()));
        let output = run(&env, &["-p", "--no-tools", "Synthetic task"]);
        assert!(!output.status.success());
        assert_eq!(terminal(&output)["error_class"], "cloud_discovery_invalid");
        assert!(String::from_utf8_lossy(&output.stderr).contains(message));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(CREDENTIAL));
        assert!(!String::from_utf8_lossy(&output.stdout).contains(CREDENTIAL));
        assert!(output.stdout.is_empty());
        assert_eq!(mock.requests().len(), 1);
        assert!(!env.home().join(".oxa/agent/cloud-ledger").exists());
        mock.assert_healthy();
    }
}

#[test]
fn unused_canonical_metadata_does_not_control_routing_or_admission() {
    for metadata in [
        None,
        Some(Value::Null),
        Some(json!("synthetic@display name")),
        Some(json!({"unused":true})),
    ] {
        let env = TestEnv::new();
        let mock = MockOpenAi::start_with(|base| {
            let mut value = discovery(base);
            for model in value["data"].as_array_mut().unwrap() {
                if let Some(metadata) = &metadata {
                    model["canonical_id"] = metadata.clone();
                } else {
                    model.as_object_mut().unwrap().remove("canonical_id");
                }
            }
            vec![
                Reply::json(value),
                stream(json!({"content":"Synthetic answer"}), "stop"),
            ]
        });
        setup(&env, &format!("http://{}", mock.address()));
        let output = run(&env, &["-p", "--no-tools", "Synthetic task"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].target, "/inference/go/openai/v1/chat/completions");
        assert_eq!(requests[1].body["model"], "synthetic-flash");
        let ledger = rows(&env);
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0]["cost_usd"], 0.0);
        assert_eq!(ledger[0]["billing"], "subscription");
        mock.assert_healthy();
    }
}

#[test]
fn authenticated_discovery_never_publishes_an_echoed_credential() {
    for field in [
        "/data/0/id",
        "/data/0/canonical_id",
        "/data/0/api/chat-completions/base_url",
        "/data/0/api/chat-completions",
        "/data/0/cost",
        "/data/0/capabilities",
        "/data/0/modalities",
    ] {
        let env = TestEnv::new();
        let mock = MockOpenAi::start_with(|base| {
            let mut value = discovery(base);
            if field.ends_with("/chat-completions") {
                value
                    .pointer_mut(field)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .insert(CREDENTIAL.into(), json!(true));
            } else {
                *value.pointer_mut(field).unwrap() = if field.ends_with("/cost") {
                    json!({"nested":[{"description":CREDENTIAL}]})
                } else if field.ends_with("base_url") {
                    json!(format!("{base}/inference/go/{CREDENTIAL}"))
                } else {
                    json!(format!("prefix-{CREDENTIAL}-suffix"))
                };
            }
            vec![Reply::json(value)]
        });
        setup(&env, &format!("http://{}", mock.address()));
        let output = run(&env, &["--list-models"]);
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains(CREDENTIAL),
            "{field}"
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains(CREDENTIAL),
            "{field}"
        );
        assert!(
            !output.status.success(),
            "credential-bearing discovery accepted: {field}"
        );
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains(MODEL),
            "partial catalog"
        );
        assert!(!env.home().join(".oxa/agent/cloud-ledger").exists());
        assert_eq!(mock.requests().len(), 1);
        mock.assert_healthy();
    }
}

#[test]
fn credentials_split_across_chat_fields_never_reach_output_or_receipts() {
    let env = TestEnv::new();
    let half = CREDENTIAL.len() / 2;
    let mock = MockOpenAi::start_with(|base| {
        let chunks = [
            json!({"choices":[{"index":0,"delta":{"content":&CREDENTIAL[..half],"reasoning_content":&CREDENTIAL[..half]},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{"content":&CREDENTIAL[half..],"reasoning_content":&CREDENTIAL[half..]},"finish_reason":"stop"}]}),
            json!({"choices":[],"usage":usage()}),
        ];
        let mut bytes = Vec::new();
        for chunk in chunks {
            bytes.extend_from_slice(format!("data: {chunk}\n\n").as_bytes());
        }
        bytes.extend_from_slice(b"data: [DONE]\n\n");
        vec![
            Reply::json(discovery(base)),
            Reply::raw(200, "text/event-stream", bytes, vec![]),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    let output = run(&env, &["-p", "--no-tools", "Synthetic request"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("[redacted]"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(CREDENTIAL));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(CREDENTIAL));
    assert!(!serde_json::to_string(&rows(&env)).unwrap().contains(CREDENTIAL));
    mock.assert_healthy();
}

#[test]
fn encoded_credentials_in_streamed_tool_arguments_never_execute() {
    for secret in [true, false] {
        let env = TestEnv::new();
        let text = if secret {
            CREDENTIAL
        } else {
            "synthetic safe content"
        };
        let escaped: String = text.chars().map(|c| format!("\\u{:04x}", u32::from(c))).collect();
        let args = format!(r#"{{"path":"encoded.txt","content":"{escaped}"}}"#);
        let split = args.len() / 2;
        let mock = MockOpenAi::start_with(|base| {
            let frames = [
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"encoded-write","type":"function","function":{"name":"write","arguments":&args[..split]}}]},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":&args[split..]}}]},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
                json!({"choices":[],"usage":usage()}),
            ];
            let mut bytes = Vec::new();
            for frame in frames {
                bytes.extend_from_slice(format!("data: {frame}\n\n").as_bytes());
            }
            bytes.extend_from_slice(b"data: [DONE]\n\n");
            vec![
                Reply::json(discovery(base)),
                Reply::raw(200, "text/event-stream", bytes, vec![]),
                stream(json!({"content":"Complete"}), "stop"),
            ]
        });
        setup(&env, &format!("http://{}", mock.address()));
        let output = run(&env, &["-p", "--tools", "write", "Write the synthetic fixture"]);
        let path = env.work().join("encoded.txt");
        if secret {
            if path.exists() {
                assert_ne!(
                    fs::read_to_string(&path).unwrap(),
                    CREDENTIAL,
                    "the actual decoded synthetic credential was written to disk"
                );
            }
            assert!(!path.exists(), "decoded credential reached the real write tool");
            assert!(!output.status.success());
            assert_eq!(
                mock.requests().len(),
                2,
                "unsafe tool arguments must stop before tool execution/follow-up"
            );
        } else {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(fs::read_to_string(path).unwrap(), text);
            assert_eq!(mock.requests().len(), 3);
            assert!(mock.requests()[2].body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "assistant"
                    && message["tool_calls"][0]["function"]["arguments"] == args));
        }
        assert!(!String::from_utf8_lossy(&output.stdout).contains(CREDENTIAL));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(CREDENTIAL));
        mock.assert_healthy();
    }
}

#[test]
fn dynamic_go_startup_errors_keep_provider_ownership_with_usable_local() {
    for (mode, expected, hits) in [
        ("offline", "offline", 0),
        ("missing-key", "cloud_credentials_unavailable", 0),
        ("discovery-401", "cloud_credentials_unavailable", 1),
        ("discovery-json", "cloud_discovery_invalid", 1),
    ] {
        for explicit in [false, true] {
            let replies = match mode {
                "discovery-401" => vec![Reply::error(401, "invalid_request_error", "Synthetic refusal")],
                "discovery-json" => vec![Reply::raw(
                    200,
                    "application/json",
                    b"invalid synthetic JSON".to_vec(),
                    vec![],
                )],
                _ => vec![],
            };
            let mock = MockOpenAi::start(replies);
            let env = TestEnv::new();
            setup(&env, &format!("http://{}", mock.address()));
            let path = env.home().join(".oxa/agent/settings.json");
            let mut settings: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            settings["oxa"]["engine"]["inProcessLocal"] = json!(true);
            settings["oxa"]["local"] = json!({"binary":"/synthetic/not-launched-oxe",
                "models":{"synthetic-local":{"model":"/synthetic/model.gguf"}},
                "limits":{"synthetic-local":{"context_window":4096,"max_output":64,
                    "allocation_budget":{"kv_bytes_per_position":1,"scratch_bytes":0,"mtp_state_bytes":0,"margin_bytes":1044480}}}});
            if explicit {
                settings["defaultModel"] = json!("synthetic-local");
            }
            fs::write(path, settings.to_string()).unwrap();
            if mode == "missing-key" {
                fs::remove_file(env.home().join(".config/oxc/llm/cloud-secrets.json")).unwrap();
            }
            let mut args = vec![];
            if mode == "offline" {
                args.push("--offline");
            }
            if explicit {
                args.extend(["--model", MODEL]);
            }
            args.extend(["--no-session", "--no-tools", "-p", "Synthetic Go routing"]);
            let output = run(&env, &args);
            assert_eq!(
                terminal(&output)["error_class"],
                expected,
                "mode={mode}, explicit={explicit}, stderr={}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!output.status.success());
            assert!(!String::from_utf8_lossy(&output.stdout).contains(CREDENTIAL));
            assert!(!String::from_utf8_lossy(&output.stderr).contains(CREDENTIAL));
            assert!(!env.home().join(".oxa/agent/local-engine").exists());
            assert!(!env.home().join(".oxa/agent/cloud-ledger").exists());
            let local = run(&env, &["--offline", "--list-models"]);
            assert!(
                local.status.success(),
                "{}",
                String::from_utf8_lossy(&local.stderr)
            );
            assert!(String::from_utf8_lossy(&local.stdout).contains("synthetic-local"));
            if mode == "offline" && !explicit {
                let scoped = run(
                    &env,
                    &[
                        "--offline",
                        "--models",
                        "synthetic-local",
                        "--no-session",
                        "--no-tools",
                        "-p",
                        "Synthetic local scope",
                    ],
                );
                // The explicitly scoped local backend reaches its missing-weight
                // guard, without an unused Go default hijacking the request.
                assert_eq!(terminal(&scoped)["error_class"], "not_found");
                assert!(String::from_utf8_lossy(&scoped.stderr).contains("Local weights are missing"));
            }
            assert_eq!(mock.requests().len(), hits);
            assert!(mock.requests().iter().all(|request| request.method == "GET"));
            mock.assert_healthy();
        }
    }
}

#[test]
fn local_go_namespace_alias_with_thinking_suffix_keeps_local_ownership_offline() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start(vec![]);
    setup(&env, &format!("http://{}", mock.address()));
    let path = env.home().join(".oxa/agent/settings.json");
    let mut settings: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    settings["oxa"]["engine"]["inProcessLocal"] = json!(true);
    settings["oxa"]["local"] = json!({"binary":"/synthetic/not-launched-oxe",
        "models":{"opencode/custom-local":{"model":"/synthetic/model.gguf"}},
        "limits":{"opencode/custom-local":{"context_window":4096,"max_output":64,
            "allocation_budget":{"kv_bytes_per_position":1,"scratch_bytes":0,"mtp_state_bytes":0,"margin_bytes":1044480}}}});
    fs::write(path, settings.to_string()).unwrap();
    for model in [
        "opencode/custom-local",
        "opencode/custom-local:high",
        "opencode/CUSTOM-LOCAL",
        "opencode/CUSTOM-LOCAL:high",
    ] {
        let output = run(
            &env,
            &[
                "--offline",
                "--model",
                model,
                "--no-session",
                "--no-tools",
                "-p",
                "Synthetic local suffix",
            ],
        );
        assert_eq!(
            terminal(&output)["error_class"],
            "not_found",
            "model={model}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("Local weights are missing"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains(CREDENTIAL));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(CREDENTIAL));
        assert!(!env.home().join(".oxa/agent/local-engine").exists());
        assert!(!env.home().join(".oxa/agent/cloud-ledger").exists());
        assert!(mock.requests().is_empty());
    }
    mock.assert_healthy();
}

#[test]
fn scoped_local_alias_is_not_treated_as_an_unavailable_go_model() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start(vec![]);
    setup(&env, &format!("http://{}", mock.address()));
    let path = env.home().join(".oxa/agent/settings.json");
    let mut settings: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    // Native local on, native cloud explicitly off, and a Go-shaped local alias.
    settings["oxa"]["engine"] = json!({"inProcessLocal": true, "inProcessCloud": false});
    settings["oxa"]["local"] = json!({"binary":"/synthetic/not-launched-oxe",
        "models":{"opencode/custom-local":{"model":"/synthetic/model.gguf"}},
        "limits":{"opencode/custom-local":{"context_window":4096,"max_output":64,
            "allocation_budget":{"kv_bytes_per_position":1,"scratch_bytes":0,"mtp_state_bytes":0,"margin_bytes":1044480}}}});
    fs::write(path, settings.to_string()).unwrap();
    // The singular and the scoped spelling both stay on the configured local alias.
    for args in [
        vec!["--offline", "--model", "opencode/custom-local"],
        vec!["--offline", "--models", "opencode/custom-local"],
    ] {
        let mut words = args.clone();
        words.extend(["--no-session", "--no-tools", "-p", "Synthetic local scope"]);
        let output = run(&env, &words);
        assert_eq!(
            terminal(&output)["error_class"],
            "not_found",
            "args={args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Local weights are missing"),
            "args={args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains(CREDENTIAL));
        assert!(!env.home().join(".oxa/agent/cloud-ledger").exists());
        assert!(mock.requests().is_empty());
    }
    mock.assert_healthy();
}

// #798 composition controls: the ordinary reader is the only credential reader.
// Fixtures stay outside source repositories and use only loopback HTTP.
fn go_diagnostic_env() -> TestEnv {
    let env = TestEnv::new();
    assert!(!env.root().ancestors().any(|p| p.join(".git").exists()));
    env
}

fn go_local_credential_failure_names_store(case: &str) {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let env = go_diagnostic_env();
    let mock = MockOpenAi::start(vec![]);
    setup(&env, &format!("http://{}", mock.address()));
    let path = env.home().join(".config/oxc/llm/cloud-secrets.json");
    match case {
        "missing" => fs::remove_file(&path).unwrap(),
        "missing-provider" => fs::write(&path, r#"{"synthetic":"not-a-provider"}"#).unwrap(),
        "mode" => fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(),
        "symlink" => {
            let target = env.root().join("synthetic-store.json");
            fs::rename(&path, &target).unwrap();
            symlink(target, &path).unwrap();
        }
        "repository" => fs::create_dir(path.parent().unwrap().join(".git")).unwrap(),
        _ => unreachable!(),
    }
    let output = run(&env, &["--no-session", "--no-tools", "-p", "Synthetic request"]);
    assert_eq!(output.status.code(), Some(5), "{case}");
    assert_eq!(terminal(&output)["error_class"], "cloud_credentials_unavailable");
    assert!(
        mock.requests().is_empty(),
        "local failure must precede HTTP: {case}"
    );
    assert!(!env.home().join(".oxa/agent/cloud-ledger").exists());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("attempted protected store"),
        "missing local stage: {case}: {stderr}"
    );
    assert!(
        stderr.contains(path.to_str().unwrap()),
        "missing attempted path: {case}"
    );
    assert!(!stderr.contains(CREDENTIAL));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(path.to_str().unwrap()));
    mock.assert_healthy();
}

#[test]
fn go_missing_store_diagnostic_precedes_any_http() {
    go_local_credential_failure_names_store("missing");
}
#[test]
fn go_missing_provider_diagnostic_precedes_any_http() {
    go_local_credential_failure_names_store("missing-provider");
}
#[test]
fn go_unsafe_mode_diagnostic_precedes_any_http() {
    go_local_credential_failure_names_store("mode");
}
#[test]
fn go_symlink_diagnostic_precedes_any_http() {
    go_local_credential_failure_names_store("symlink");
}
#[test]
fn go_repository_diagnostic_precedes_any_http() {
    go_local_credential_failure_names_store("repository");
}

#[test]
fn go_discovery_401_keeps_the_public_error_without_claiming_a_local_store_failure() {
    let env = go_diagnostic_env();
    let mock = MockOpenAi::start(vec![Reply::error(401, "invalid_request_error", CREDENTIAL)]);
    setup(&env, &format!("http://{}", mock.address()));
    let output = run(&env, &["--no-session", "--no-tools", "-p", "Synthetic request"]);
    assert_eq!(output.status.code(), Some(5));
    assert_eq!(terminal(&output)["error_class"], "cloud_credentials_unavailable");
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(mock.requests()[0].method, "GET");
    assert!(!env.home().join(".oxa/agent/cloud-ledger").exists());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("stage=discovery_http status=401 code=invalid_request_error"),
        "{stderr}"
    );
    assert!(!stderr.contains("attempted protected store"));
    assert!(!stderr.contains(CREDENTIAL));
    assert!(!stderr.contains(
        env.home()
            .join(".config/oxc/llm/cloud-secrets.json")
            .to_str()
            .unwrap()
    ));
    mock.assert_healthy();
}

#[test]
fn go_chat_401_keeps_the_public_error_without_claiming_a_local_store_failure() {
    let env = go_diagnostic_env();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            Reply::error(401, "invalid_request_error", CREDENTIAL),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    let output = run(&env, &["--no-session", "--no-tools", "-p", "Synthetic request"]);
    assert_eq!(
        output.status.code(),
        Some(5),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(terminal(&output)["error_class"], "cloud_credentials_unavailable");
    assert_eq!(mock.requests().len(), 2);
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0]["status"], 401);
    assert_eq!(ledger[0]["settled"], "refused");
    assert_eq!(ledger[0]["subscription_tokens"], 0);
    assert_eq!(ledger[0]["error"], "cloud_credentials_unavailable");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("attempted protected store"),
        "upstream401 mislabeled local: {stderr}"
    );
    assert!(!stderr.contains(CREDENTIAL));
    assert!(!stderr.contains(
        env.home()
            .join(".config/oxc/llm/cloud-secrets.json")
            .to_str()
            .unwrap()
    ));
    mock.assert_healthy();
}

#[test]
fn go_protocol_failure_retains_only_private_finite_http_facts() {
    let env = go_diagnostic_env();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            Reply::raw(
                200,
                "text/event-stream",
                b"data: invalid\n\n".to_vec(),
                vec![
                    ("Retry-After", "7".into()),
                    ("X-RateLimit-Limit", "3000".into()),
                    ("X-RateLimit-Remaining", "0".into()),
                    ("X-RateLimit-Reset", "1234".into()),
                    ("X-Private-Account", CREDENTIAL.into()),
                ],
            ),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    let output = run(&env, &["--no-tools", "--mode", "json", "-p", "Synthetic request"]);
    assert!(!output.status.success());
    assert_eq!(terminal(&output)["error_class"], "stream_broken");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "stage=chat_http status=200 retry_after=7 rate_limit=3000 rate_remaining=0 rate_reset=1234"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("attempted protected store"));
    assert!(!stderr.contains(CREDENTIAL));
    assert!(!stderr.contains("invalid\n"));
    assert_eq!(mock.requests().len(), 2);
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0]["status"], 502);
    assert_eq!(ledger[0]["settled"], "reserved");
    assert_eq!(ledger[0]["usage_missing"], true);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("native diagnostic"));
    assert!(!stdout.contains("retry_after"));
    let files = session_files(&env.home().join(".oxa/agent/sessions"));
    assert!(!files.is_empty(), "actual session history must exist");
    for path in files {
        let history = fs::read_to_string(path).unwrap();
        assert!(!history.contains("native diagnostic"));
        assert!(!history.contains("rate_reset"));
        assert!(!history.contains(CREDENTIAL));
    }
    mock.assert_healthy();
}

fn session_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.extend(session_files(&path));
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                files.push(path);
            }
        }
    }
    files
}

#[test]
fn go_http_diagnostic_rejects_unknown_codes_and_non_numeric_or_duplicate_headers() {
    let env = go_diagnostic_env();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            Reply::raw(
                429,
                "application/json",
                json!({"error":{"type":CREDENTIAL,"message":CREDENTIAL}})
                    .to_string()
                    .into_bytes(),
                vec![
                    ("Retry-After", "7".into()),
                    ("Retry-After", "8".into()),
                    ("X-RateLimit-Limit", "4294967296".into()),
                    ("X-RateLimit-Remaining", CREDENTIAL.into()),
                    ("X-RateLimit-Reset", "1234,1235".into()),
                ],
            ),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    let output = run(&env, &["--no-session", "--no-tools", "-p", "Synthetic request"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("oxa native diagnostic: stage=chat_http status=429\n"),
        "{stderr}"
    );
    assert!(!stderr.contains("retry_after="));
    assert!(!stderr.contains("rate_limit="));
    assert!(!stderr.contains("rate_remaining="));
    assert!(!stderr.contains("rate_reset="));
    assert!(!stderr.contains(CREDENTIAL));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("native diagnostic"));
    assert_eq!(mock.requests().len(), 2);
    mock.assert_healthy();
}

#[test]
fn go_in_stream_error_records_known_code_without_publishing_its_body() {
    let env = go_diagnostic_env();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            Reply::raw(
                200,
                "text/event-stream",
                format!(
                    "data: {}\n\n",
                    json!({"error":{"type":"inference_failed","message":CREDENTIAL,"account":CREDENTIAL}})
                )
                .into_bytes(),
                vec![],
            ),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    let output = run(&env, &["--no-session", "--no-tools", "-p", "Synthetic request"]);
    assert_eq!(terminal(&output)["error_class"], "stream_broken");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("stage=chat_http status=200 code=inference_failed"),
        "{stderr}"
    );
    assert!(!stderr.contains(CREDENTIAL));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("inference_failed"));
    assert!(!stdout.contains(CREDENTIAL));
    assert_eq!(mock.requests().len(), 2);
    assert_eq!(rows(&env)[0]["error"], "cloud_upstream_stream_broken");
    mock.assert_healthy();
}

#[test]
fn go_private_discovery_diagnostic_is_absent_for_rpc_and_supervised_children() {
    for child in [false, true] {
        let env = go_diagnostic_env();
        let mock = MockOpenAi::start(vec![Reply::error(401, "authentication_error", CREDENTIAL)]);
        setup(&env, &format!("http://{}", mock.address()));
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
        cmd.current_dir(env.work())
            .env_clear()
            .envs(env.envs("http://127.0.0.1:9"));
        if child {
            cmd.env("OXA_SUBAGENT_DEPTH", "1")
                .args(["-p", "Synthetic request"]);
        } else {
            cmd.args(["--mode", "rpc"]);
        }
        let output = cmd.stdin(Stdio::null()).output().unwrap();
        assert!(!output.status.success());
        for bytes in [&output.stdout, &output.stderr] {
            let text = String::from_utf8_lossy(bytes);
            assert!(!text.contains("native diagnostic"), "{text}");
            assert!(!text.contains("attempted protected store"));
            assert!(!text.contains(CREDENTIAL));
        }
        assert_eq!(mock.requests().len(), 1);
        mock.assert_healthy();
    }
}

fn check_private_stream_cause(cause: &str, reply: Reply) {
    check_private_stream_details(cause, None, reply);
}
fn check_private_stream_details(cause: &str, check: Option<&str>, reply: Reply) {
    let env = go_diagnostic_env();
    let mock = MockOpenAi::start_with(|base| {
        let mut replies = vec![Reply::json(discovery(base)), reply];
        if check.is_some() {
            replies.push(Reply::json(discovery(base)));
            replies.push(stream(json!({"content":"Synthetic continuation"}), "stop"));
        }
        replies
    });
    setup(&env, &format!("http://{}", mock.address()));
    if check.is_some() {
        set_one_request_token_cap(&env);
    }
    let output = run(&env, &["--no-tools", "--mode", "json", "-p", "Synthetic request"]);
    assert!(!output.status.success());
    assert_eq!(
        terminal(&output)["error_class"],
        if cause == "transport" {
            "engine_http"
        } else {
            "stream_broken"
        }
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!(
            "cause={cause}{}\n",
            check.map(|s| format!(" terminal_check={s}")).unwrap_or_default()
        )),
        "missing finite cause {cause}"
    );
    assert!(stderr.contains("stage=chat_http status=200"));
    assert!(!stderr.contains(CREDENTIAL));
    assert!(!stderr.contains("synthetic-private-frame"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("cause="));
    assert!(!stdout.contains("terminal_check="));
    assert!(!stdout.contains(CREDENTIAL));
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0]["status"], 502);
    assert_eq!(ledger[0]["settled"], "reserved");
    assert_eq!(ledger[0]["usage_missing"], true);
    assert_eq!(ledger[0]["subscription_tokens"], 33280);
    for path in session_files(&env.home().join(".oxa/agent/sessions")) {
        let history = fs::read_to_string(path).unwrap();
        assert!(!history.contains("cause="));
        assert!(!history.contains("terminal_check="));
        assert!(!history.contains(CREDENTIAL));
    }
    assert_eq!(mock.requests().len(), 2);
    if check.is_some() {
        let continued = run(
            &env,
            &["--no-session", "--no-tools", "-p", "Synthetic continuation"],
        );
        let stderr = String::from_utf8_lossy(&continued.stderr);
        assert!(continued.status.success(), "{stderr}");
        assert!(stderr.contains("projected usage reached 100%"), "{stderr}");
        assert_eq!(mock.requests().iter().filter(|r| r.method == "POST").count(), 2);
        let ledger = rows(&env);
        assert_eq!(ledger.len(), 2);
        assert_eq!(ledger[0]["subscription_tokens"], 33280);
        assert_eq!(ledger[1]["subscription_tokens"], 105);
    }
    mock.assert_healthy();
}
fn cause_reply(bytes: &[u8]) -> Reply {
    Reply::raw(200, "text/event-stream", bytes.to_vec(), vec![])
}
#[test]
fn go_private_stream_cause_content_type() {
    check_private_stream_cause(
        "content_type",
        Reply::raw(
            200,
            "application/json",
            b"synthetic-private-frame".to_vec(),
            vec![],
        )
        .buffer_header_refusal(),
    );
}
#[test]
fn go_private_stream_cause_transport() {
    let mut reply = cause_reply(b"data: ");
    reply.truncate_transfer = true;
    check_private_stream_cause("transport", reply);
}
#[test]
fn go_private_stream_cause_eof() {
    check_private_stream_cause("eof", cause_reply(b""));
}
#[test]
fn go_private_stream_cause_frame_shape() {
    check_private_stream_cause("frame_shape", cause_reply(b"data: synthetic-private-frame\n\n"));
}
#[test]
fn go_private_stream_cause_tool_shape() {
    check_private_stream_cause("tool_shape", stream(json!({"tool_calls":{}}), "stop"));
}
#[test]
fn go_private_stream_cause_tool_arguments() {
    check_private_stream_cause(
        "tool_arguments",
        stream(
            json!({"tool_calls":[{"index":0,"id":"synthetic-call","type":"function","function":{"name":"read","arguments":json!({"path":CREDENTIAL}).to_string()}}]}),
            "tool_calls",
        ),
    );
}
#[test]
fn go_private_stream_cause_terminal_usage() {
    check_private_stream_details("terminal_usage",Some("done_without_usage"),cause_reply(b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"));
}

fn terminal_frames(frames: &[Value], done: bool) -> Reply {
    let mut bytes = Vec::new();
    for frame in frames {
        bytes.extend_from_slice(format!("data: {frame}\n\n").as_bytes());
    }
    if done {
        bytes.extend_from_slice(b"data: [DONE]\n\n");
    }
    cause_reply(&bytes)
}
fn terminal_finish() -> Value {
    json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
}
#[test]
fn go_private_terminal_done_without_finish() {
    check_private_stream_details(
        "terminal_usage",
        Some("done_without_finish"),
        terminal_frames(&[], true),
    );
}
#[test]
fn go_private_terminal_done_without_usage() {
    check_private_stream_details(
        "terminal_usage",
        Some("done_without_usage"),
        terminal_frames(&[terminal_finish()], true),
    );
}
#[test]
fn go_private_terminal_frame_after_usage() {
    check_private_stream_details(
        "terminal_usage",
        Some("frame_after_usage"),
        terminal_frames(
            &[
                terminal_finish(),
                json!({"choices":[],"usage":usage()}),
                json!({"choices":[]}),
            ],
            true,
        ),
    );
}
#[test]
fn go_private_terminal_choices_after_finish() {
    check_private_stream_details(
        "terminal_usage",
        Some("choices_after_finish"),
        terminal_frames(
            &[
                terminal_finish(),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":null}]}),
            ],
            true,
        ),
    );
}
#[test]
fn go_private_terminal_usage_before_finish() {
    check_private_stream_details(
        "terminal_usage",
        Some("usage_before_finish"),
        terminal_frames(&[json!({"choices":[],"usage":usage()})], true),
    );
}
#[test]
fn go_private_terminal_usage_record_invalid() {
    let mut invalids = Vec::new();
    for mutation in 0..6 {
        let mut invalid = usage();
        match mutation {
            0 => invalid["cost"] = json!("synthetic-private-frame"),
            1 => invalid["cost"] = json!(-1),
            2 => invalid["opencode"]["allowance"] = json!("synthetic-private-frame"),
            3 => {
                invalid.as_object_mut().unwrap().remove("prompt_tokens");
            }
            4 => {
                invalid["prompt_tokens"] = json!(u64::MAX);
                invalid["completion_tokens"] = json!(1);
            }
            _ => invalid["prompt_tokens_details"] = json!([]),
        }
        invalids.push(invalid);
    }
    for invalid in invalids {
        check_private_stream_details(
            "terminal_usage",
            Some("usage_record_invalid"),
            terminal_frames(&[terminal_finish(), json!({"choices":[],"usage":invalid})], true),
        );
    }
}

#[test]
fn go_private_terminal_valid_usage_has_no_diagnostic_and_settles_exactly() {
    let env = go_diagnostic_env();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            stream(json!({"content":"Synthetic success"}), "stop"),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    let output = run(&env, &["--no-session", "--no-tools", "-p", "Synthetic request"]);
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("native diagnostic"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("terminal_check="));
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0]["status"], 200);
    assert_eq!(ledger[0]["subscription_tokens"], 105);
    assert_eq!(ledger[0]["settled"], "usage");
    assert!(ledger[0].get("usage_missing").is_none());
    assert_eq!(mock.requests().len(), 2);
    mock.assert_healthy();
}

#[test]
fn a_fresh_go_catalog_cache_serves_the_next_start_without_discovery() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start_with(|base| vec![Reply::json(discovery(base))]);
    setup(&env, &format!("http://{}", mock.address()));
    let settings = env.home().join(".oxa/agent/settings.json");
    let mut doc: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    doc["oxa"]["cloud"]["discover_models"] = json!(true);
    fs::write(settings, doc.to_string()).unwrap();
    let first = run(&env, &["--list-models"]);
    assert_eq!(
        first.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stdout).contains(MODEL));
    assert_eq!(mock.requests().len(), 1, "the first start discovers live");
    assert!(
        env.agent_dir().join("go-catalog-cache.json").is_file(),
        "live discovery publishes an entry"
    );
    let second = run(&env, &["--list-models"]);
    assert_eq!(
        second.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(String::from_utf8_lossy(&second.stdout).contains(MODEL));
    assert_eq!(
        mock.requests().len(),
        1,
        "a fresh entry is consumed without a network call"
    );
    assert!(!String::from_utf8_lossy(&second.stderr).contains("Using a stale OpenCode model catalog."));
    mock.assert_healthy();
}

/// #1435: a user-configured scope is the owner's too. `enabledModels` in the user settings
/// selects the Go transport without `--models` on the command line.
#[test]
fn a_user_model_scope_selects_go_without_a_cli_scope() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            stream(json!({"content":"synthetic plan reply"}), "stop"),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    let path = env.agent_dir().join("settings.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    settings.as_object_mut().unwrap().remove("defaultModel");
    settings["oxa"].as_object_mut().unwrap().remove("engine");
    // The stored key and the user's own scope are the only authorization inputs.
    settings["oxa"]["cloud"]["opencode"]
        .as_object_mut()
        .unwrap()
        .remove("enabled");
    settings["enabledModels"] = json!([MODEL]);
    fs::write(&path, settings.to_string()).unwrap();
    let output = run(&env, &["--no-tools", "-p", "Synthetic task"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("synthetic plan reply"));
    assert_eq!(rows(&env)[0]["remote_provider"], "opencode");
    mock.assert_healthy();
}

/// #1435: a fuzzy local alias does not own the exact Go name, so it cannot hide that model: the
/// exact `opencode/…` request still goes to Go, and only an exact local alias stays local.
#[test]
fn a_fuzzy_local_alias_does_not_shadow_the_exact_go_model() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            stream(json!({"content":"synthetic plan reply"}), "stop"),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    let path = env.agent_dir().join("settings.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    settings.as_object_mut().unwrap().remove("defaultModel");
    // Native local on, cloud enablement omitted and no Go setting: the stored key is the only
    // Go fact in play, and the fuzzy alias is a real local candidate.
    settings["oxa"]["engine"] = json!({"inProcessLocal":true});
    settings["oxa"]["cloud"]["opencode"]
        .as_object_mut()
        .unwrap()
        .remove("enabled");
    let alias = format!("{MODEL}-local");
    settings["oxa"]["local"] = json!({
        "binary":"/synthetic/not-launched-oxe",
        "models":{(alias.clone()):{"model":"/synthetic/missing.gguf"}},
        "limits":{(alias.clone()):{"context_window":4096,"max_output":64,"thinking":true,
            "allocation_budget":{"kv_bytes_per_position":1,"scratch_bytes":0,
                                "mtp_state_bytes":0,"margin_bytes":1044480}}}
    });
    fs::write(&path, settings.to_string()).unwrap();
    // The fuzzy alias does not own the exact Go name: the request goes to Go and completes.
    let output = run(&env, &["--model", MODEL, "--no-tools", "-p", "Synthetic task"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("Local weights are missing"),
        "the Go model must not be shadowed by {alias}: {stderr}"
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("synthetic plan reply"));
    assert_eq!(rows(&env)[0]["remote_provider"], "opencode");
    mock.assert_healthy();
}

/// #1435: an endpoint-qualified spelling (`oxe/opencode/x`) names the same local alias, so it
/// stays local instead of being refused as an unavailable Go model.
#[test]
fn an_endpoint_qualified_local_alias_stays_local() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            stream(json!({"content":"synthetic plan reply"}), "stop"),
        ]
    });
    setup(&env, &format!("http://{}", mock.address()));
    let path = env.agent_dir().join("settings.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    settings.as_object_mut().unwrap().remove("defaultModel");
    settings["oxa"]["engine"] = json!({"inProcessLocal":true, "inProcessCloud":false});
    settings["oxa"]["cloud"]["opencode"]
        .as_object_mut()
        .unwrap()
        .remove("enabled");
    let alias = "opencode/custom-local".to_string();
    settings["oxa"]["local"] = json!({
        "binary":"/synthetic/not-launched-oxe",
        "models":{(alias.clone()):{"model":"/synthetic/missing.gguf"}},
        "limits":{(alias.clone()):{"context_window":4096,"max_output":64,"thinking":true,
            "allocation_budget":{"kv_bytes_per_position":1,"scratch_bytes":0,
                                "mtp_state_bytes":0,"margin_bytes":1044480}}}
    });
    fs::write(&path, settings.to_string()).unwrap();
    let output = run(
        &env,
        &[
            "--model",
            "oxe/opencode/custom-local",
            "--no-tools",
            "-p",
            "Synthetic task",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Local weights are missing"), "{stderr}");
    assert!(!stderr.contains("OpenCode Go"), "{stderr}");
    mock.assert_healthy();
}
