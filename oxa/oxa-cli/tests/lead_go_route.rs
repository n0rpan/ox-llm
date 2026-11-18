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
use oxa_session::SessionStore;
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
    json!({"data":[{"id":"synthetic-flash","canonical_id":"synthetic/canonical","limit":{"context":32768,"output":8192},
        "api":{"chat-completions":{"base_url":format!("{base}/inference/go/openai/v1")}},"cost":{"input":0.1},"capabilities":{"tools":true},"modalities":{"input":["text"],"output":["text"]}}]})
}
fn usage() -> Value {
    json!({"prompt_tokens":100,"completion_tokens":5,"total_tokens":105,"prompt_tokens_details":{"cached_tokens":80},"cost":0,
        "opencode":{"billing_source":"go","list_cost":0.001,"allowance":0.0008,
            "tokens":{"input":20,"output":5,"reasoning":1,"cache_read":80,"cache_write_5m":0,"cache_write_1h":0}}})
}
fn stream_with_usage(text: &str, usage: Value) -> Reply {
    let chunks = [
        json!({"id":"synthetic-chat","choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]}),
        json!({"id":"synthetic-chat","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":usage}),
    ];
    let mut bytes = Vec::new();
    for chunk in chunks {
        bytes.extend_from_slice(format!("data: {chunk}\r\n\r\n").as_bytes());
    }
    bytes.extend(b"data: [DONE]\r\n\r\n");
    Reply::raw(200, "text/event-stream", bytes, vec![])
}
fn stream(text: &str) -> Reply {
    stream_with_usage(text, usage())
}

/// Settings keep the metered default model and the Go subscription route, and
/// deliberately omit `daily_budget_usd`: an absent cap means no budget check, so
/// the metered default is admitted (its unreachable upstream then fails) and only
/// the owner's explicit Go choice avoids it.
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

/// The exact recorded upstream sequence of one chat process. Naming the whole
/// sequence keeps a blind reply queue honest: one catalog discovery per process,
/// then the bounded completion. A fixture that queued replies without this check
/// could feed a JSON catalog body to the completion stream.
fn assert_sequence(mock: &MockOpenAi, expected: &[&str]) {
    let recorded: Vec<String> = mock
        .requests()
        .iter()
        .map(|request| format!("{} {}", request.method, request.target))
        .collect();
    assert_eq!(recorded, expected);
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
    // startup model is metered and the daily USD cap is absent: no cap is no
    // budget check, so the request is admitted to the metered route (whose
    // upstream is unreachable here) instead of being refused. No fallback to Go.
    settings(
        &env,
        &format!("http://{}", go.address()),
        json!({"sessionId": id}),
    );
    let refused = run(&env, &["lead", "chat", "Which items need attention?"]);
    assert!(!refused.status.success(), "{refused:?}");
    let diagnostic = String::from_utf8_lossy(&refused.stderr);
    assert!(!diagnostic.contains("oxa.cloud.daily_budget_usd"), "{diagnostic}");
    assert!(metered.requests().is_empty());
    assert!(go_chat_requests(&go).is_empty());
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 1, "{ledger:?}");
    assert_ne!(ledger[0]["status"], 403, "{ledger:?}");
    assert_ne!(ledger[0]["error"], "budget_not_configured", "{ledger:?}");

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
    assert_eq!(chats[0].body["max_tokens"], 4096, "the one bounded turn");
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
    // Three chat processes: each one discovers the Go catalog once, and only
    // the two routed turns reach the completion stream.
    assert_sequence(
        &go,
        &[
            "GET /inference/go/v1/models",
            "GET /inference/go/v1/models",
            "POST /inference/go/openai/v1/chat/completions",
            "GET /inference/go/v1/models",
            "POST /inference/go/openai/v1/chat/completions",
        ],
    );
    go.assert_healthy();
    metered.finish();
}

/// One Go reply that stops at the output limit after generating the whole
/// bounded budget, as the live DeepSeek route did: HTTP 200, 512 generated
/// tokens, `finish_reason: "length"`, partial content only.
fn length_stream(partial: &str) -> Reply {
    let usage = json!({"prompt_tokens":100,"completion_tokens":4096,"total_tokens":4196,
        "prompt_tokens_details":{"cached_tokens":80},"cost":0,
        "opencode":{"billing_source":"go","list_cost":0.001,"allowance":0.0008,
            "tokens":{"input":20,"output":4096,"reasoning":4000,"cache_read":80,"cache_write_5m":0,"cache_write_1h":0}}});
    let chunks = [
        json!({"id":"synthetic-chat","choices":[{"index":0,"delta":{"role":"assistant","content":partial},"finish_reason":null}]}),
        json!({"id":"synthetic-chat","choices":[{"index":0,"delta":{},"finish_reason":"length"}]}),
        json!({"choices":[],"usage":usage}),
    ];
    let mut bytes = Vec::new();
    for chunk in chunks {
        bytes.extend_from_slice(format!("data: {chunk}\r\n\r\n").as_bytes());
    }
    bytes.extend(b"data: [DONE]\r\n\r\n");
    Reply::raw(200, "text/event-stream", bytes, vec![])
}

#[test]
fn a_length_stopped_bounded_reply_fails_typed_without_an_impossible_retry() {
    let env = TestEnv::new_in(std::path::Path::new("/tmp"));
    fs::set_permissions(&env.agent_dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let metered = MockAnthropic::start(vec![]);
    let go = MockOpenAi::start_with(|base| {
        // One chat process: one catalog discovery, then the one bounded turn.
        // No recovery reply is queued, so a forwarded recovery request would be
        // an unexpected upstream request rather than a silent pass.
        vec![
            Reply::json(discovery(base)),
            length_stream(r#"{"text":"a concise answer that ran"#),
        ]
    });
    settings(&env, &format!("http://{}", go.address()), Value::Null);
    // The live incident ran with the ordinary thinking and retry policy: the
    // bounded turn spent its shared 512-token budget on reasoning, the visible
    // answer was cut, and the core then scheduled its length recovery.
    let path = env.agent_dir().join("settings.json");
    let arrange = |path: &std::path::Path| {
        let mut value: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        value["defaultThinkingLevel"] = json!("medium");
        value["retry"] = json!({"enabled": true});
        fs::write(path, value.to_string()).unwrap();
        fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    };
    arrange(&path);
    write_key(&env);

    let initialized = run(&env, &["lead", "chat", "--init"]);
    assert!(initialized.status.success(), "{initialized:?}");
    let id = String::from_utf8(initialized.stdout).unwrap().trim().to_owned();
    settings(
        &env,
        &format!("http://{}", go.address()),
        json!({"sessionId": id, "model": GO_MODEL}),
    );
    arrange(&path);

    let refused = run(&env, &["lead", "chat", "A concise question?"]);
    assert_sequence(
        &go,
        &[
            "GET /inference/go/v1/models",
            "POST /inference/go/openai/v1/chat/completions",
        ],
    );
    assert!(!refused.status.success(), "{refused:?}");
    let diagnostic = String::from_utf8_lossy(&refused.stderr);
    // The typed local literal-limit reason, never the metered USD reason.
    assert!(
        diagnostic.contains("bounded output limit"),
        "the bounded literal-limit reason must reach the CLI: {diagnostic}"
    );
    assert!(
        !diagnostic.contains("daily cloud budget"),
        "a local literal limit is not a spent metered budget: {diagnostic}"
    );
    assert_eq!(refused.status.code(), Some(1), "not the spent-budget exit");

    // The recovery request can never be reserved, so it is never forwarded and
    // its internal instructions never enter the protected Session.
    let chats = go_chat_requests(&go);
    assert_eq!(chats.len(), 1, "no impossible recovery request: {chats:?}");
    // The gateway normalizes the bounded turn to one fixed bound: the visible
    // reply plus reasoning headroom. The Go adapter removes `reasoning_effort:
    // none`, so this headroom (not a stripped control) is the viable path.
    assert_eq!(chats[0].body["max_tokens"], 4096, "{}", chats[0].body);
    let dirs = oxa_config::Dirs::under(env.root());
    let store = oxa_session::FsStore::new(dirs.sessions_dir);
    let selected = store.resolve(&id).unwrap();
    let logs = fs::read_to_string(&selected.path).unwrap();
    assert!(
        !logs.contains("reached its output limit and was discarded"),
        "the internal recovery note must not enter the protected Session: {logs}"
    );
    assert!(
        logs.contains("a concise answer that ran"),
        "the cut is kept as an audit entry: {logs}"
    );

    // Actual accounting is preserved: the one forwarded request settled its
    // real 200/512 usage against the subscription route, not a refusal.
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 1, "{ledger:?}");
    assert_eq!(ledger[0]["settled"], "usage");
    assert_eq!(ledger[0]["billing"], "subscription");
    assert_eq!(ledger[0]["subscription_tokens"], 4196);
    assert!(metered.requests().is_empty());
    go.assert_healthy();
    metered.finish();
}

#[test]
fn a_reasoning_heavy_go_reply_still_completes_inside_the_bounded_turn() {
    let env = TestEnv::new_in(std::path::Path::new("/tmp"));
    fs::set_permissions(&env.agent_dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let metered = MockAnthropic::start(vec![]);
    let reasoning_usage = json!({"prompt_tokens":100,"completion_tokens":3050,"total_tokens":3150,
        "prompt_tokens_details":{"cached_tokens":80},"cost":0,
        "opencode":{"billing_source":"go","list_cost":0.001,"allowance":0.0008,
            "tokens":{"input":20,"output":3050,"reasoning":3000,"cache_read":80,"cache_write_5m":0,"cache_write_1h":0}}});
    let go = MockOpenAi::start_with(|base| {
        // One chat process: one catalog discovery, then the one bounded turn.
        vec![
            Reply::json(discovery(base)),
            stream_with_usage(
                r#"{"text":"a concise reasoning answer","facts":[],"action":null}"#,
                reasoning_usage,
            ),
        ]
    });
    settings(&env, &format!("http://{}", go.address()), Value::Null);
    // The live incident's ordinary thinking policy: reasoning stays on for the Go
    // route (its adapter drops `reasoning_effort: none`), so the turn must carry
    // enough bounded headroom for reasoning plus the visible answer.
    let path = env.agent_dir().join("settings.json");
    let arrange = |path: &std::path::Path| {
        let mut value: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        value["defaultThinkingLevel"] = json!("medium");
        fs::write(path, value.to_string()).unwrap();
        fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    };
    arrange(&path);
    write_key(&env);

    let initialized = run(&env, &["lead", "chat", "--init"]);
    assert!(initialized.status.success(), "{initialized:?}");
    let id = String::from_utf8(initialized.stdout).unwrap().trim().to_owned();
    settings(
        &env,
        &format!("http://{}", go.address()),
        json!({"sessionId": id, "model": GO_MODEL}),
    );
    arrange(&path);

    let answered = run(&env, &["lead", "chat", "A concise question?"]);
    assert_sequence(
        &go,
        &[
            "GET /inference/go/v1/models",
            "POST /inference/go/openai/v1/chat/completions",
        ],
    );
    assert!(answered.status.success(), "{answered:?}");
    assert!(String::from_utf8_lossy(&answered.stdout).contains("a concise reasoning answer"));
    let chats = go_chat_requests(&go);
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].body["max_tokens"], 4096, "{}", chats[0].body);
    // The reasoning-heavy reply settled its real usage inside the same bound.
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 1, "{ledger:?}");
    assert_eq!(ledger[0]["settled"], "usage");
    assert_eq!(ledger[0]["billing"], "subscription");
    assert_eq!(ledger[0]["subscription_tokens"], 3150);
    assert!(metered.requests().is_empty());
    go.assert_healthy();
    metered.finish();
}
