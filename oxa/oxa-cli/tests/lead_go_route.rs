//! The lead owner conversation through the real native gateway: the metered
//! guard stays intact without a daily USD cap, and the user-only explicit route
//! selects the approved Go subscription route. Mock upstreams only; no network.
#![cfg(target_os = "linux")]
#![allow(dead_code, unexpected_cfgs)] // Shared gateway mocks; the CLI exercises JSON, not auth forms.
#[path = "../../oxc-gateway/tests/common/anthropic.rs"]
mod anthropic;
#[path = "../../oxc-gateway/tests/common/openai.rs"]
mod upstream;
use anthropic::MockAnthropic;
use oxa_fake::TestEnv;
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    process::{Command, Output, Stdio},
};
use upstream::{MockOpenAi, Reply};

const GO_MODEL: &str = "opencode/synthetic-flash";
const METERED_MODEL: &str = "synthetic-cloud";
const CREDENTIAL: &str = "synthetic-provider-credential";

fn discovery(base: &str) -> Value {
    json!({"data":[{"id":"synthetic-flash","canonical_id":"synthetic/canonical","limit":{"context":32768,"output":512},
        "api":{"chat-completions":{"base_url":format!("{base}/inference/go/openai/v1")}},"cost":{"input":0.1},"capabilities":{"tools":true},"modalities":{"input":["text"],"output":["text"]}}]})
}
fn usage() -> Value {
    json!({"prompt_tokens":100,"completion_tokens":5,"total_tokens":105,"prompt_tokens_details":{"cached_tokens":80},"cost":0,
        "opencode":{"billing_source":"go","list_cost":0.001,"allowance":0.0008,
            "tokens":{"input":20,"output":5,"reasoning":1,"cache_read":80,"cache_write_5m":0,"cache_write_1h":0}}})
}
fn stream(text: &str) -> Reply {
    let chunks = [
        json!({"id":"synthetic-chat","choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]}),
        json!({"id":"synthetic-chat","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":usage()}),
    ];
    let mut bytes = Vec::new();
    for chunk in chunks {
        bytes.extend_from_slice(format!("data: {chunk}\r\n\r\n").as_bytes());
    }
    bytes.extend(b"data: [DONE]\r\n\r\n");
    Reply::raw(200, "text/event-stream", bytes, vec![])
}

/// Settings keep the metered default model and the Go subscription route, and
/// deliberately omit `daily_budget_usd`: the absent cap is the owner restriction
/// under test, not something this change may invent or bypass.
fn settings(env: &TestEnv, go: &str, lead: Value) {
    let mut value = json!({"defaultProjectTrust":"always","defaultModel":METERED_MODEL,
        "defaultThinkingLevel":"off","permissions":{"defaultMode":"auto"},
        "retry":{"enabled":false},"oxa":{"engine":{"inProcessCloud":true},
        "cloud":{"models":{METERED_MODEL:{
            "provider":"anthropic","upstream_model":"synthetic-anthropic",
            "base_url":"http://127.0.0.1:1","context_window":128000,"max_output":16,
            "pricing":{"input_per_million":2,"output_per_million":3,
                "cache_read_per_million":0.2,"cache_write_per_million":2.5}}},
            "opencode":{"enabled":true,"base_url":format!("{go}/inference/go")},
            "subscription_limits":{"daily_requests":20,"daily_tokens":2000000}}}});
    if !lead.is_null() {
        value["oxa"]["lead"] = lead;
    }
    env.write("home/.oxa/agent/settings.json", &value.to_string());
    fs::set_permissions(
        env.agent_dir().join("settings.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
}

fn write_key(env: &TestEnv) {
    let path = env.home().join(".config/oxc/llm/cloud-secrets.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    write!(file, "{}", json!({"anthropic":CREDENTIAL,"opencode":CREDENTIAL})).unwrap();
}

fn run(env: &TestEnv, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
    cmd.args(args)
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn rows(env: &TestEnv) -> Vec<Value> {
    fs::read_dir(env.home().join(".oxa/agent/cloud-ledger"))
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
                .then(|| fs::read_to_string(path).unwrap())
        })
        .flat_map(|text| {
            text.lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn go_chat_requests(mock: &MockOpenAi) -> Vec<upstream::Request> {
    mock.requests()
        .into_iter()
        .filter(|request| request.target.contains("chat/completions"))
        .collect()
}

#[test]
fn lead_chat_keeps_the_metered_guard_and_routes_the_go_subscription_choice() {
    // Credentials and the ledger deliberately refuse repository paths; keep HOME
    // outside the checkout as the other in-process cloud proofs do.
    let env = TestEnv::new_in(std::path::Path::new("/tmp"));
    fs::set_permissions(&env.agent_dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let metered = MockAnthropic::start(vec![]);
    let go = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            Reply::json(discovery(base)),
            stream(r#"{"text":"go route reply","facts":[],"action":null}"#),
            Reply::json(discovery(base)),
            stream(r#"{"text":"one-shot go reply","facts":[],"action":null}"#),
        ]
    });
    settings(&env, &format!("http://{}", go.address()), Value::Null);
    write_key(&env);

    let initialized = run(&env, &["lead", "chat", "--init"]);
    assert!(initialized.status.success(), "{initialized:?}");
    let id = String::from_utf8(initialized.stdout).unwrap().trim().to_owned();

    // The dedicated Lead Session is bound, but no route is chosen yet: the
    // startup model is metered and the daily USD cap is absent, so the guard
    // refuses exactly as before. No fallback, no upstream request.
    settings(
        &env,
        &format!("http://{}", go.address()),
        json!({"sessionId": id}),
    );
    let refused = run(&env, &["lead", "chat", "Which items need attention?"]);
    assert!(!refused.status.success(), "{refused:?}");
    let diagnostic = String::from_utf8_lossy(&refused.stderr);
    assert!(diagnostic.contains("oxa.cloud.daily_budget_usd"), "{diagnostic}");
    assert!(metered.requests().is_empty());
    assert!(go_chat_requests(&go).is_empty());
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 1, "{ledger:?}");
    assert_eq!(ledger[0]["status"], 403);
    assert_eq!(ledger[0]["error"], "budget_not_configured");

    // The owner's user-only explicit choice takes the approved Go subscription
    // route: independent positive limits, no USD hold with the cap absent.
    settings(
        &env,
        &format!("http://{}", go.address()),
        json!({"sessionId": id, "model": GO_MODEL}),
    );
    let answered = run(&env, &["lead", "chat", "Which items need attention?"]);
    assert!(answered.status.success(), "{answered:?}");
    assert!(String::from_utf8_lossy(&answered.stdout).contains("go route reply"));
    let chats = go_chat_requests(&go);
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].body["model"], "synthetic-flash");
    assert!(metered.requests().is_empty());

    // The restored one-shot drives the same bounded path through the gateway.
    let one_shot = run(&env, &["lead", "One-shot question for the lead"]);
    assert!(one_shot.status.success(), "{one_shot:?}");
    assert!(String::from_utf8_lossy(&one_shot.stdout).contains("one-shot go reply"));
    assert_eq!(go_chat_requests(&go).len(), 2);
    assert!(metered.requests().is_empty());

    // Both settled rows are subscription spends; the refusal row stays refused.
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 3, "{ledger:?}");
    assert_eq!(ledger[0]["status"], 403);
    for row in &ledger[1..] {
        assert_eq!(row["billing"], "subscription", "{row:?}");
        assert_eq!(row["remote_provider"], "opencode", "{row:?}");
    }
    go.assert_healthy();
    metered.finish();
}
