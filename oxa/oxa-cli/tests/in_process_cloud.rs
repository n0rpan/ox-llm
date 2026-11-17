//! The production binary selects the native gateway and executes a real tool loop.
#![cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "../../oxc-gateway/tests/common/anthropic.rs"]
mod anthropic;
#[path = "common/strace.rs"]
mod strace;
use anthropic::{MockAnthropic, Reply, CREDENTIAL};
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    process::{Command, Output},
};

const MODEL: &str = "synthetic-cloud";
const UPSTREAM: &str = "synthetic-anthropic";

fn cloud_env() -> TestEnv {
    // This Linux-only proof needs credentials outside any repository. Harnesses can
    // put TMPDIR inside the checkout; honoring it here would correctly trip the
    // production key-file guard (and inherit repository project settings).
    TestEnv::new_in(Path::new("/tmp"))
}

fn settings(env: &TestEnv, base: &str, enabled: bool) {
    env.write(
        "home/.oxa/agent/settings.json",
        &json!({"defaultProjectTrust":"always","defaultModel":MODEL,
        "defaultThinkingLevel":"off","permissions":{"defaultMode":"auto"},
        "retry":{"enabled":false},"oxa":{"engine":{"inProcessCloud":enabled},
        "cloud":{"daily_budget_usd":0.3201,"models":{MODEL:{
            "provider":"anthropic","upstream_model":UPSTREAM,"base_url":base,
            "context_window":128000,"max_output":16,"pricing":{
                "input_per_million":2,"output_per_million":3,
                "cache_read_per_million":0.2,"cache_write_per_million":2.5
            }}}}}})
        .to_string(),
    );
}
fn key_path(env: &TestEnv) -> std::path::PathBuf {
    env.home().join(".config/oxc/llm/cloud-secrets.json")
}
fn write_key(env: &TestEnv, text: &str) {
    let path = key_path(env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(text.as_bytes()).unwrap();
}
fn run(env: &TestEnv, args: &[&str], trace: Option<&Path>) -> Output {
    run_to(env, "http://127.0.0.1:9", args, trace)
}
fn run_to(env: &TestEnv, url: &str, args: &[&str], trace: Option<&Path>) -> Output {
    let mut cmd = if let Some(path) = trace {
        let mut cmd = Command::new(strace::executable());
        cmd.args(["-f", "-e", "trace=execve", "-o"])
            .arg(path)
            .arg(env!("CARGO_BIN_EXE_oxa"));
        cmd
    } else {
        Command::new(env!("CARGO_BIN_EXE_oxa"))
    };
    cmd.args(args).current_dir(env.work()).env_clear();
    for (key, value) in env.envs(url) {
        cmd.env(key, value);
    }
    cmd.output().unwrap_or_else(|error| {
        if trace.is_some() && error.kind() == std::io::ErrorKind::NotFound {
            panic!(
                "strace is required for the no-Python process proof; install strace on PATH or at \
                 .local-checks/strace/bin/strace (see CONTRIBUTING.md): {error}"
            );
        }
        panic!("failed to launch {cmd:?}: {error}");
    })
}
fn rows(env: &TestEnv) -> Vec<Value> {
    fs::read_dir(env.home().join(".oxa/agent/cloud-ledger"))
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            path.extension()
                .is_some_and(|s| s == "jsonl")
                .then(|| fs::read_to_string(path).unwrap())
        })
        .flat_map(|s| {
            s.lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}
fn success(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// These regressions must exercise the ordinary protected reader, not a test bypass.
fn diagnostic_env() -> TestEnv {
    let env = TestEnv::new();
    assert!(env.root().is_absolute());
    assert!(!env.root().ancestors().any(|p| p.join(".git").exists()));
    env
}

#[test]
fn native_local_credentials_diagnostic_names_attempted_store_only_on_stderr() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    for case in [
        "missing-parent",
        "missing-store",
        "missing-provider",
        "mode",
        "symlink",
        "repository",
    ] {
        let env = diagnostic_env();
        let mock = MockAnthropic::start(vec![]);
        settings(&env, &mock.base_url(), true);
        let path = key_path(&env);
        match case {
            "missing-parent" => {}
            "missing-store" => fs::create_dir_all(path.parent().unwrap()).unwrap(),
            "missing-provider" => write_key(&env, r#"{"TEST":"TEST"}"#),
            _ => {
                write_key(&env, r#"{"anthropic":{"api_key":"TEST"}}"#);
                match case {
                    "mode" => fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(),
                    "symlink" => {
                        let target = env.root().join("TEST-store.json");
                        fs::rename(&path, &target).unwrap();
                        symlink(target, &path).unwrap();
                    }
                    "repository" => fs::create_dir(path.parent().unwrap().join(".git")).unwrap(),
                    _ => unreachable!(),
                }
            }
        }
        // Save a session as well: the local diagnostic must never enter history.
        let output = run(&env, &["--no-tools", "-p", "TEST"], None);
        assert_eq!(output.status.code(), Some(5), "{case}: {output:?}");
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(
            diagnostic.contains(path.to_str().unwrap()),
            "{case}: {diagnostic}"
        );
        assert!(
            diagnostic.contains("Local cloud credentials unavailable; attempted protected store"),
            "{case}: {diagnostic}"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains(path.to_str().unwrap()));
        for session_dir in fs::read_dir(env.agent_dir().join("sessions")).unwrap() {
            for file in fs::read_dir(session_dir.unwrap().path()).unwrap() {
                let text = fs::read_to_string(file.unwrap().path()).unwrap();
                assert!(!text.contains(path.to_str().unwrap()), "path leaked to history");
            }
        }
        assert!(mock.requests().is_empty(), "{case}");
        assert_eq!(rows(&env)[0]["error"], "cloud_credentials_unavailable");

        // The same protected reader must still produce the fixed API refusal.
        use oxc_gateway::{
            catalog::{Access, Model, Provider},
            ledger::Context,
            pricing::Price,
            runtime::{Options, Runtime},
        };
        let model = Model::new(MODEL, Provider::Anthropic, UPSTREAM, &mock.base_url(), 128000, 16).unwrap();
        let runtime = Runtime::new(
            [(
                model,
                Price::from_model(&json!({"free":true})).map(oxc_gateway::billing::Billing::Metered),
            )],
            path,
            env.agent_dir().join("TEST-api-ledger"),
            Options {
                access: Access::Enabled,
                daily_budget_usd: Some(1.0),
                ledger_enabled: true,
                subscription: None,
                subscription_limits: None,
            },
        )
        .unwrap();
        let refusal = runtime
            .begin(&json!({"model":MODEL}), Context::default())
            .err()
            .unwrap();
        assert_eq!(refusal.status, 503);
        assert_eq!(
            refusal.body(),
            json!({"error":{
            "message":"Cloud credentials are not configured.", "type":"cloud_error",
            "code":"cloud_credentials_unavailable", "param":"model"}})
        );
    }
}

#[test]
fn native_credentials_diagnostic_does_not_relabel_upstream_401_or_protocol_failure() {
    for (reply, class, status, native_class) in [
        (
            Reply::error(401, "TEST upstream metadata"),
            "cloud_upstream_error",
            401,
            "engine_http",
        ),
        (
            Reply::wire("text/event-stream", b"data: TEST\n\n"),
            "cloud_upstream_stream_broken",
            502,
            "stream_broken",
        ),
    ] {
        let env = diagnostic_env();
        let mock = MockAnthropic::start(vec![reply]);
        settings(&env, &mock.base_url(), true);
        write_key(&env, r#"{"anthropic":{"api_key":"TEST"}}"#);
        let output = run(&env, &["--no-session", "--no-tools", "-p", "TEST"], None);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(
            !diagnostic.contains(key_path(&env).to_str().unwrap()),
            "{diagnostic}"
        );
        assert!(!diagnostic.contains("attempted protected store"), "{diagnostic}");
        assert!(!diagnostic.contains("TEST upstream metadata"), "{diagnostic}");
        assert!(
            diagnostic.contains(&format!("\"error_class\":\"{native_class}\"")),
            "{diagnostic}"
        );
        assert!(diagnostic.contains(&format!("HTTP {status}")), "{diagnostic}");
        let ledger = rows(&env);
        assert_eq!(ledger[0]["status"], status);
        assert_eq!(ledger[0]["error"], class);
        assert_eq!(mock.requests().len(), 1);
    }
}

#[test]
fn native_credentials_diagnostic_is_not_forwarded_from_supervised_children() {
    let env = diagnostic_env();
    let mock = MockAnthropic::start(vec![]);
    settings(&env, &mock.base_url(), true);
    let output = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["--no-session", "--no-tools", "-p", "TEST"])
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs("http://127.0.0.1:9"))
        .env("OXA_SUBAGENT_DEPTH", "1")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(5));
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(
        diagnostic.contains("Cloud credentials are not configured."),
        "{diagnostic}"
    );
    assert!(
        !diagnostic.contains(key_path(&env).to_str().unwrap()),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("attempted protected store"), "{diagnostic}");
    assert!(mock.requests().is_empty());
}

#[test]
fn native_credentials_diagnostic_keeps_json_protocol_errors_redacted() {
    let env = diagnostic_env();
    let mock = MockAnthropic::start(vec![]);
    settings(&env, &mock.base_url(), true);
    let output = run(
        &env,
        &["--no-session", "--no-tools", "--mode", "json", "-p", "TEST"],
        None,
    );
    assert_eq!(output.status.code(), Some(5));
    let protocol = String::from_utf8_lossy(&output.stdout);
    assert!(protocol.contains("\"type\":\"agent_settled\""), "{protocol}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("Cloud credentials are not configured."));
    assert!(!protocol.contains(key_path(&env).to_str().unwrap()), "{protocol}");
    assert!(!protocol.contains("attempted protected store"), "{protocol}");
    assert!(String::from_utf8_lossy(&output.stderr).contains(key_path(&env).to_str().unwrap()));
    assert!(mock.requests().is_empty());
}

#[test]
fn native_credentials_diagnostic_valid_store_succeeds() {
    let env = diagnostic_env();
    let mock = MockAnthropic::start(vec![Reply::text("Synthetic answer", 1, 0, 0, 1)]);
    settings(&env, &mock.base_url(), true);
    write_key(&env, r#"{"anthropic":{"api_key":"TEST"}}"#);
    let output = run(&env, &["--no-session", "--no-tools", "-p", "TEST"], None);
    success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Synthetic answer"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(key_path(&env).to_str().unwrap()));
    assert_eq!(mock.requests().len(), 1);
}

#[test]
fn http_credentials_refusal_does_not_name_a_local_store() {
    let env = diagnostic_env();
    settings(&env, "http://127.0.0.1:9", false);
    let http = FakeServer::start(
        vec![ScriptTurn::fault(oxa_fake::Fault::Http {
            status: 503,
            body: json!({"error":{"type":"cloud_error","code":"cloud_credentials_unavailable",
            "message":"Cloud credentials are not configured."}})
            .to_string(),
        })],
        FakeOpts::default(),
    )
    .unwrap();
    let output = run_to(
        &env,
        &http.url(),
        &["--no-session", "--no-tools", "-p", "TEST"],
        None,
    );
    assert_eq!(output.status.code(), Some(5));
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(
        diagnostic.contains("Cloud credentials are not configured."),
        "{diagnostic}"
    );
    assert!(
        !diagnostic.contains(key_path(&env).to_str().unwrap()),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("attempted protected store"), "{diagnostic}");
}

#[test]
fn native_openrouter_refuses_decoded_held_key_before_executable_write() {
    // Public synthetic fixture, created only under the isolated HOME outside the checkout.
    const HELD: &str = "TEST-held-openrouter-770-public";
    let escaped = HELD.replace('T', "\\u0054");
    let cases = [
        format!(r#"{{"path":"leak.txt","content":"{escaped}"}}"#),
        format!(
            r#"{{"path":"leak.txt","content":"clean","metadata":{{"items":[{{"value":"{escaped}"}}]}}}}"#
        ),
        format!(
            r#"{{"path":"leak.txt","content":"clean","metadata":{{"items":[{{"{escaped}":"value"}}]}}}}"#
        ),
        format!(r#"{{"path":"leak.txt","content":"clean","{escaped}":"value"}}"#),
        r#"{"path":"clean.txt","content":"benign \u03bb","meta\u0064ata":{"safe":"value"}}"#.to_owned(),
    ];
    for (index, arguments) in cases.into_iter().enumerate() {
        let benign = index == 4;
        let env = cloud_env();
        let reply = |delta: Value, reason: &str| {
            let mut wire = [
                json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}],
                    "usage":{"prompt_tokens":21,"completion_tokens":5}}),
            ]
            .iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect::<String>();
            wire.push_str("data: [DONE]\n\n");
            Reply::wire("text/event-stream", wire.as_bytes())
        };
        let mut replies = vec![reply(
            json!({"tool_calls":[{"index":0,"id":"synthetic-write","type":"function",
                "function":{"name":"write","arguments":arguments}}]}),
            "tool_calls",
        )];
        if benign {
            replies.push(reply(json!({"content":"Clean write completed"}), "stop"));
        }
        let mock = MockAnthropic::start(replies);
        settings(&env, &mock.base_url(), true);
        let path = env.home().join(".oxa/agent/settings.json");
        let mut config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        config["oxa"]["cloud"]["models"][MODEL]["provider"] = json!("openrouter");
        // Admit the existing OpenRouter reservation, including its 25% headroom.
        config["oxa"]["cloud"]["daily_budget_usd"] = json!(1.0);
        fs::write(path, config.to_string()).unwrap();
        write_key(&env, &json!({"openrouter":{"api_key":HELD}}).to_string());
        let output = run(&env, &["--no-session", "-p", "Synthetic write control"], None);
        assert!(
            !env.work().join("leak.txt").exists(),
            "case {index} executed an unsafe write"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains(HELD));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(HELD));
        if benign {
            success(&output);
            assert_eq!(
                fs::read_to_string(env.work().join("clean.txt")).unwrap(),
                "benign λ"
            );
            assert_eq!(mock.requests().len(), 2);
            let paid = rows(&env);
            assert_eq!(paid.len(), 2);
            assert!(paid.iter().all(|row| row["settled"] == "usage"));
        } else {
            assert!(!output.status.success(), "case {index} must refuse the call");
            assert!(!env.work().join("clean.txt").exists());
            assert_eq!(mock.requests().len(), 1);
            let refused = rows(&env);
            assert_eq!(refused.len(), 1);
            assert_eq!(refused[0]["status"], 502);
            assert_eq!(refused[0]["settled"], "reserved");
        }
        for request in mock.requests() {
            assert_eq!(request.target, "/v1/chat/completions");
            assert_eq!(
                request.header("authorization"),
                Some(format!("Bearer {HELD}").as_str())
            );
            // O-384: with no preferences configured the request carries no provider object.
            assert!(request.body.get("provider").is_none(), "{}", request.body);
        }
        assert!(!json!(rows(&env)).to_string().contains(HELD));
        mock.finish();
    }
}

#[test]
fn native_openrouter_discovery_tool_loop_and_settlement() {
    let env = cloud_env();
    env.write("work/note.txt", "SYNTHETIC ROUTER PROOF\n");
    let reply = |delta: Value, reason: &str| {
        let chunks = [
            json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]}),
            json!({"choices":[],"usage":{"prompt_tokens":21,"completion_tokens":5,
                "prompt_tokens_details":{"cached_tokens":7}}}),
        ];
        let mut wire = chunks
            .iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect::<String>();
        wire.push_str("data: [DONE]\n\n");
        Reply::wire("text/event-stream", wire.as_bytes())
    };
    let mock = MockAnthropic::start(vec![
        reply(
            json!({"tool_calls":[{"index":0,"id":"read-note","type":"function",
            "function":{"name":"read","arguments":"{\"path\":\"note.txt\"}"}}]}),
            "tool_calls",
        ),
        reply(
            json!({"content":format!("Native router answer {CREDENTIAL}")}),
            "stop",
        ),
    ]);
    settings(&env, &mock.base_url(), true);
    let path = env.home().join(".oxa/agent/settings.json");
    let mut value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    value["oxa"]["cloud"]["models"][MODEL]["provider"] = json!("openrouter");
    // Includes OpenRouter's existing 25% reservation headroom.
    value["oxa"]["cloud"]["daily_budget_usd"] = json!(0.40012);
    fs::write(path, value.to_string()).unwrap();
    // Project settings must not redirect a user-selected native route.
    env.write(
        "work/.oxa/settings.json",
        &json!({"oxa":{"engine":{"inProcessCloud":false},
        "cloud":{"models":{MODEL:{"provider":"anthropic","base_url":"http://127.0.0.1:9"}}}}})
        .to_string(),
    );
    write_key(&env, "invalid synthetic JSON");
    let listed = run(&env, &["--list-models"], None);
    success(&listed);
    assert!(String::from_utf8_lossy(&listed.stdout).contains(MODEL));
    assert!(mock.requests().is_empty());
    assert!(!env.home().join(".oxa/agent/cloud-ledger").exists());

    let offline = run(&env, &["--offline", "--list-models"], None);
    assert!(!offline.status.success());
    assert!(String::from_utf8_lossy(&offline.stderr).contains("offline"));
    assert!(mock.requests().is_empty());

    write_key(&env, &json!({"openrouter":{"api_key":CREDENTIAL}}).to_string());
    let output = run(&env, &["--no-session", "-p", "Read note.txt"], None);
    success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Native router answer"), "{stdout}");
    assert!(!stdout.contains(CREDENTIAL));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(CREDENTIAL));
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.target, "/v1/chat/completions");
        assert_eq!(
            request.header("authorization"),
            Some(format!("Bearer {CREDENTIAL}").as_str())
        );
        assert_eq!(request.body["model"], UPSTREAM);
        assert_eq!(request.body["max_tokens"], 16);
        // O-384: no configured preferences, so no provider object is sent.
        assert!(request.body.get("provider").is_none(), "{}", request.body);
        assert_eq!(request.body["stream_options"]["include_usage"], true);
        assert_eq!(request.body["reasoning"], json!({"enabled":false}));
        assert!(request.body.get("reasoning_effort").is_none());
    }
    assert!(requests[1].body.to_string().contains("SYNTHETIC ROUTER PROOF"));
    let paid = rows(&env);
    assert_eq!(paid.len(), 2);
    for row in paid {
        assert_eq!(row["status"], 200);
        assert_eq!(row["remote_provider"], "openrouter");
        assert_eq!(row["origin"], "oxa");
        assert_eq!(row["settled"], "usage");
        assert_eq!(row["prompt_tokens"], 21);
        assert_eq!(row["cached_tokens"], 7);
        assert_eq!(row["completion_tokens"], 5);
        assert!((row["cost_usd"].as_f64().unwrap() - 0.0000444).abs() < 1e-12);
    }
    let exhausted = run(
        &env,
        &["--no-session", "--no-tools", "-p", "Next synthetic turn"],
        None,
    );
    assert_eq!(exhausted.status.code(), Some(oxa_proto::EXIT_BUDGET));
    assert_eq!(mock.requests().len(), 2);
    let refused = rows(&env).pop().unwrap();
    assert_eq!(refused["error"], "budget_exceeded");
    assert_eq!(refused["remote_provider"], "openrouter");
    assert_eq!(refused["cost_usd"], 0.0);
    mock.finish();
}

/// O-384: the user's OpenRouter provider routing preferences reach the wire exactly as
/// configured (global merged with the model's own), and the model view shows them.
#[test]
fn native_openrouter_provider_routing_reaches_the_wire_and_the_model_view() {
    let env = cloud_env();
    let wire = [
        json!({"choices":[{"index":0,"delta":{"content":"routed answer"},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":5,
            "prompt_tokens_details":{"cached_tokens":7}}}),
    ]
    .iter()
    .map(|chunk| format!("data: {chunk}\n\n"))
    .collect::<String>()
        + "data: [DONE]\n\n";
    let mock = MockAnthropic::start(vec![Reply::wire("text/event-stream", wire.as_bytes())]);
    settings(&env, &mock.base_url(), true);
    let path = env.home().join(".oxa/agent/settings.json");
    let mut value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    value["oxa"]["cloud"]["models"][MODEL]["provider"] = json!("openrouter");
    value["oxa"]["cloud"]["daily_budget_usd"] = json!(1.0);
    value["oxa"]["cloud"]["provider_routing"] = json!({"zdr":true,"data_collection":"deny","sort":"price"});
    value["oxa"]["cloud"]["models"][MODEL]["provider_routing"] =
        json!({"sort":":nitro","only":["synthetic-a"]});
    fs::write(path, value.to_string()).unwrap();
    write_key(&env, &json!({"openrouter":{"api_key":CREDENTIAL}}).to_string());

    // The model view shows the effective preferences; listing reads no key and no provider.
    let listed = run(&env, &["--list-models"], None);
    success(&listed);
    let stdout = String::from_utf8_lossy(&listed.stdout);
    let row = stdout
        .lines()
        .find(|line| line.split_whitespace().nth(1) == Some(MODEL))
        .unwrap_or_else(|| panic!("no {MODEL} row in {stdout}"));
    assert!(
        row.ends_with("zdr=true,data=deny,sort=throughput,only=synthetic-a"),
        "{row}"
    );
    assert!(mock.requests().is_empty());

    let output = run(
        &env,
        &["--no-session", "--no-tools", "-p", "Synthetic routed request"],
        None,
    );
    success(&output);
    let request = mock.requests().pop().unwrap();
    assert_eq!(request.target, "/v1/chat/completions");
    assert_eq!(
        request.body["provider"],
        json!({"zdr":true,"data_collection":"deny","sort":"throughput","only":["synthetic-a"]})
    );
    mock.finish();
}

/// O-384: an invalid preference, or one on a route that cannot use it, is refused while
/// the settings are read, before any credential or provider I/O.
#[test]
fn native_openrouter_provider_routing_refuses_invalid_settings() {
    for (routing, provider, expected) in [
        (json!({"sort":"cheapest"}), "openrouter", "provider_routing.sort"),
        (json!({"zdr":"yes"}), "openrouter", "provider_routing.zdr"),
        (
            json!({"only":["synthetic-a"],"ignore":["synthetic-b"]}),
            "openrouter",
            "provider_routing.only",
        ),
        (
            json!({"zdr":true}),
            "anthropic",
            "applies only to an openrouter route",
        ),
    ] {
        let env = cloud_env();
        let mock = MockAnthropic::start(vec![]);
        settings(&env, &mock.base_url(), true);
        let path = env.home().join(".oxa/agent/settings.json");
        let mut value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        value["oxa"]["cloud"]["models"][MODEL]["provider"] = json!(provider);
        value["oxa"]["cloud"]["models"][MODEL]["provider_routing"] = routing.clone();
        fs::write(path, value.to_string()).unwrap();
        write_key(&env, &json!({"openrouter":{"api_key":CREDENTIAL}}).to_string());
        let output = run(
            &env,
            &["--no-session", "--no-tools", "-p", "Synthetic refused request"],
            None,
        );
        assert!(!output.status.success(), "{routing}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{routing}: {stderr}");
        assert!(!stderr.contains(CREDENTIAL), "{routing}: {stderr}");
        assert!(mock.requests().is_empty(), "{routing}");
        assert!(!env.home().join(".oxa/agent/cloud-ledger").exists(), "{routing}");
    }
}

#[test]
fn native_silence_timeout_honors_effective_settings_and_provider_override() {
    for (idle, provider, project, times_out) in [
        (json!(40), None, false, true),
        (json!(5000), Some(40), false, true),
        (json!(5000), Some(40), true, true),
        (json!(0), None, false, false),
        (json!("disabled"), None, false, false),
    ] {
        let env = cloud_env();
        let (reply, release) = Reply::text("delayed native answer", 11, 7, 3, 5).gated();
        let mock = MockAnthropic::start(vec![reply]);
        settings(&env, &mock.base_url(), true);
        let path = env.home().join(".oxa/agent/settings.json");
        let mut value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        value["httpIdleTimeoutMs"] = idle;
        if !project {
            if let Some(ms) = provider {
                value["retry"]["provider"] = json!({"timeoutMs":ms});
            }
        }
        fs::write(path, value.to_string()).unwrap();
        if project {
            env.write(
                "work/.oxa/settings.json",
                &json!({"retry":{"provider":{"timeoutMs":provider.unwrap()}}}).to_string(),
            );
        }
        write_key(&env, &json!({"anthropic":{"api_key":CREDENTIAL}}).to_string());
        let output = std::thread::scope(|scope| {
            scope.spawn(|| {
                mock.wait_for_requests(1);
                std::thread::sleep(std::time::Duration::from_millis(200));
                let _ = release.send(());
            });
            run(
                &env,
                &["--no-session", "--no-tools", "-p", "Synthetic timeout request"],
                None,
            )
        });
        if times_out {
            assert!(
                !output.status.success(),
                "a configured silence limit must stop the gated stream"
            );
            let rows = rows(&env);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["status"], 502);
            assert_eq!(rows[0]["error"], "cloud_upstream_unavailable");
            assert_eq!(rows[0]["settled"], "reserved");
            assert_eq!(rows[0]["cost_usd"], json!(0.320048));
        } else {
            success(&output);
            assert!(String::from_utf8_lossy(&output.stdout).contains("delayed native answer"));
            assert_eq!(rows(&env)[0]["settled"], "usage");
        }
    }
}

#[test]
fn missing_native_budget_names_oxa_settings_before_reading_an_invalid_key() {
    let env = cloud_env();
    let mock = MockAnthropic::start(vec![]);
    settings(&env, &mock.base_url(), true);
    let path = env.home().join(".oxa/agent/settings.json");
    let mut value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    value["oxa"]["cloud"]
        .as_object_mut()
        .unwrap()
        .remove("daily_budget_usd");
    fs::write(path, value.to_string()).unwrap();
    write_key(&env, "invalid synthetic JSON");
    let output = run(
        &env,
        &["--no-session", "--no-tools", "-p", "Synthetic budget setup"],
        None,
    );
    assert!(!output.status.success());
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("oxa.cloud.daily_budget_usd"), "{diagnostic}");
    assert!(!diagnostic.contains(" in oxc"), "{diagnostic}");
    assert!(mock.requests().is_empty());
    let rows = rows(&env);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"], 403);
    assert_eq!(rows[0]["error"], "budget_not_configured");
    assert_eq!(rows[0]["cost_usd"], 0.0);
}

#[test]
fn native_discovery_tool_loop_and_budget_settlement_use_no_python_process() {
    let env = cloud_env();
    env.write("work/note.txt", "SYNTHETIC PROOF\n");
    let tool_reply = Reply::events(vec![
        json!({"type":"message_start","message":{"usage":{"input_tokens":11,
            "cache_read_input_tokens":7,"cache_creation_input_tokens":3,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use",
            "id":"read-note","name":"read","input":{"path":"note.txt"}}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use",
            "id":"read-private","name":"read","input":{"path":key_path(&env)}}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ]);
    let mock = MockAnthropic::start(vec![
        tool_reply,
        Reply::text(&format!("Native λ answer {CREDENTIAL}"), 11, 7, 3, 5),
    ]);
    settings(&env, &mock.base_url(), true);
    env.write(
        "work/.oxa/settings.json",
        &json!({"oxa":{
            "engine":{"inProcessCloud":false},"cloud":{"daily_budget_usd":0,
            "models":{MODEL:{"base_url":"http://127.0.0.1:9","upstream_model":"wrong"}}}
        }})
        .to_string(),
    );
    write_key(&env, "invalid synthetic JSON");
    let listed = run(&env, &["--list-models"], None);
    success(&listed);
    assert!(String::from_utf8_lossy(&listed.stdout).contains(MODEL));
    assert!(mock.requests().is_empty());
    assert!(!env.home().join(".oxa/agent/cloud-ledger").exists());

    write_key(&env, &json!({"anthropic":{"api_key":CREDENTIAL}}).to_string());
    let trace = env.work().join("native-exec.trace");
    let output = run(&env, &["--no-session", "-p", "Read note.txt"], Some(&trace));
    success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("Native λ answer"), "{stdout}");
    assert!(!stdout.contains(CREDENTIAL));
    assert!(!stderr.contains(CREDENTIAL));
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.target, "/v1/messages");
        assert_eq!(request.header("x-api-key"), Some(CREDENTIAL));
        assert_eq!(request.body["model"], UPSTREAM);
        assert_eq!(request.body["max_tokens"], 16);
    }
    let continuation = requests[1].body.to_string();
    assert!(continuation.contains("SYNTHETIC PROOF"));
    let private_result = requests[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .find(|part| part["type"] == "tool_result" && part["tool_use_id"] == "read-private")
        .expect("the private read has a tool result");
    assert!(
        private_result["content"]
            .to_string()
            .to_lowercase()
            .contains("denied"),
        "{private_result}"
    );
    assert!(
        !continuation.contains(CREDENTIAL),
        "a tool must not expose the key file"
    );
    let paid = rows(&env);
    assert_eq!(paid.len(), 2);
    for row in &paid {
        assert_eq!(row["status"], 200);
        assert_eq!(row["origin"], "oxa");
        assert_eq!(row["remote_provider"], "anthropic");
        assert_eq!(row["settled"], "usage");
        assert_eq!(row["prompt_tokens"], 21);
        assert_eq!(row["cached_tokens"], 7);
        assert_eq!(row["cache_write_tokens"], 3);
        assert_eq!(row["completion_tokens"], 5);
        assert!((row["cost_usd"].as_f64().unwrap() - 0.0000459).abs() < 1e-12);
    }
    let ledger = env.home().join(".oxa/agent/cloud-ledger");
    assert_eq!(fs::metadata(&ledger).unwrap().mode() & 0o077, 0);

    let exhausted = run(
        &env,
        &["--no-session", "--no-tools", "-p", "Synthetic next turn"],
        None,
    );
    assert_eq!(exhausted.status.code(), Some(oxa_proto::EXIT_BUDGET));
    let diagnostic = String::from_utf8_lossy(&exhausted.stderr);
    assert!(diagnostic.contains("oxa.cloud.daily_budget_usd"), "{diagnostic}");
    assert!(!diagnostic.contains(" in oxc"), "{diagnostic}");
    assert_eq!(mock.requests().len(), 2);
    let all = rows(&env);
    assert_eq!(all.len(), 3);
    assert_eq!(all[2]["status"], 429);
    assert_eq!(all[2]["error"], "budget_exceeded");
    assert_eq!(all[2]["cost_usd"], 0.0);

    let trace = fs::read_to_string(trace).unwrap();
    let programs: Vec<_> = trace
        .lines()
        .filter_map(|line| {
            let path = line.split("execve(\"").nth(1)?.split('"').next()?;
            Path::new(path).file_name()?.to_str()
        })
        .collect();
    assert!(programs.contains(&"oxa"), "{trace}");
    assert!(programs.iter().all(|name| !name.starts_with("python")), "{trace}");
    mock.finish();
}

#[test]
fn absent_disabled_wrong_type_and_project_only_settings_keep_http() {
    for (enabled, project) in [
        (None, false),
        (Some(json!(false)), true),
        (None, true),
        (Some(json!("true")), true),
    ] {
        let env = TestEnv::new();
        let mut settings = json!({"defaultProjectTrust":"always","retry":{"enabled":false},
            "oxa":{"cloud":"invalid if the factory is opened"}});
        if let Some(value) = enabled {
            settings["oxa"]["engine"]["inProcessCloud"] = value;
        }
        env.write("home/.oxa/agent/settings.json", &settings.to_string());
        if project {
            env.write(
                "work/.oxa/settings.json",
                r#"{"oxa":{"engine":{"inProcessCloud":true}}}"#,
            );
        }
        let http = FakeServer::start(vec![ScriptTurn::text("HTTP selection")], FakeOpts::default()).unwrap();
        let result = run_to(
            &env,
            &http.url(),
            &["--no-session", "--no-tools", "-p", "Synthetic prompt"],
            None,
        );
        success(&result);
        assert!(String::from_utf8_lossy(&result.stdout).contains("HTTP selection"));
        assert_eq!(http.requests().len(), 1);
        assert!(!env.home().join(".oxa/agent/cloud-ledger").exists());
    }
}

#[test]
fn offline_refuses_before_invalid_factory_configuration() {
    for user_offline in [false, true] {
        let env = TestEnv::new();
        env.write(
            "home/.oxa/agent/settings.json",
            &json!({"oxa":{
            "offline":user_offline,"engine":{"inProcessCloud":true},"cloud":{}}})
            .to_string(),
        );
        let args = if user_offline {
            vec!["--list-models"]
        } else {
            vec!["--offline", "--list-models"]
        };
        let result = run(&env, &args, None);
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(error.contains("offline"), "{error}");
        assert!(!error.contains("Invalid user cloud"), "{error}");
        assert!(!env.home().join(".oxa/agent/cloud-ledger").exists());
    }
}

#[test]
fn invalid_enabled_factory_refuses_without_http_fallback() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        &json!({"defaultProjectTrust":"always",
        "oxa":{"engine":{"inProcessCloud":true},"cloud":{}}})
        .to_string(),
    );
    let http = FakeServer::start(vec![ScriptTurn::text("unexpected fallback")], FakeOpts::default()).unwrap();
    let result = run_to(
        &env,
        &http.url(),
        &["--no-session", "--no-tools", "-p", "Synthetic prompt"],
        None,
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Invalid user cloud configuration"));
    assert!(http.requests().is_empty());
}

#[test]
fn composed_factory_lists_all_configured_providers_without_credentials_or_http() {
    let env = TestEnv::new();
    let mut models = serde_json::Map::new();
    for provider in ["anthropic", "openrouter", "openai", "chatgpt"] {
        models.insert(
            format!("synthetic-{provider}"),
            json!({"provider":provider,"upstream_model":"synthetic-model",
                "context_window":32768,"max_output":64,"pricing":{
                    "input_per_million":1,"output_per_million":2}}),
        );
    }
    env.write(
        "home/.oxa/agent/settings.json",
        &json!({"oxa":{"engine":{"inProcessCloud":true,"inProcessLocal":false},
            "cloud":{"models":models,"daily_budget_usd":1,
                "subscription_limits":{"daily_requests":3,"daily_tokens":100000}}}})
        .to_string(),
    );
    let http = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let result = run_to(&env, &http.url(), &["--list-models"], None);
    success(&result);
    let output = String::from_utf8_lossy(&result.stdout);
    for provider in ["anthropic", "openrouter", "openai", "chatgpt"] {
        assert!(output.contains(&format!("synthetic-{provider}")), "{output}");
    }
    assert_eq!(http.hits("GET", "/v1/models"), 0);
    assert!(http.requests().is_empty());
    assert!(!key_path(&env).exists());
    assert!(!env.home().join(".config/oxc/llm/chatgpt-auth.json").exists());
    assert!(!env.home().join(".oxa/agent/cloud-ledger").exists());
}
