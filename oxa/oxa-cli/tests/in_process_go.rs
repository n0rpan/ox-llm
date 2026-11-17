//! Real native CLI, live discovery and chat tools through the shared loopback fixture.
#![cfg(target_os = "linux")]
#[allow(dead_code, unexpected_cfgs)] // Shared gateway mock; CLI exercises JSON, not feature-gated form auth.
#[path = "../../oxc-gateway/tests/common/openai.rs"]
mod upstream;
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
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
    cmd.current_dir(env.work())
        .env_clear()
        .envs(env.envs("http://127.0.0.1:9"));
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
fn region_and_allowance_refusals_stay_typed_without_retry_or_paid_fallback() {
    for (status,message,expected) in [(400,"Model is hosted outside Europe and the United States; https://example.invalid/workspaces/synthetic","region_blocked"),
        (429,"Synthetic subscription allowance exhausted","subscription_limit_reached")] {
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
fn live_discovery_lists_chat_models_only_and_offline_precedes_credentials() {
    let env = TestEnv::new();
    let mock = MockOpenAi::start_with(|base| vec![Reply::json(discovery(base))]);
    setup(&env, &format!("http://{}", mock.address()));
    let output = run(&env, &["--list-models"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(MODEL));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-messages"));
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
        (429, "subscription_limit_reached"),
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
    assert_eq!(output.status.code(), Some(5));
    assert_eq!(terminal(&output)["error_class"], "cloud_credentials_unavailable");
    assert_eq!(mock.requests().len(), 2);
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0]["status"], 503);
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
