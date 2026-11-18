//! Production native factory, tools, persisted replay, caps and ledger across processes.
#![cfg(target_os = "linux")]
#[allow(dead_code, unexpected_cfgs)] // Shared gateway fixture owns a transport feature.
#[path = "../../oxc-gateway/tests/common/openai.rs"]
mod mock;
use oxa_fake::TestEnv;
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    process::{Command, Output},
};

const MODEL: &str = "synthetic-responses";
const KEY: &str = "synthetic-responses-credential";
fn private_file(path: &Path, text: &str) {
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
fn settings(env: &TestEnv, base: &str, subscription: bool) {
    let endpoint = if subscription {
        base.trim_end_matches("/v1").to_owned() + "/backend-api/codex"
    } else {
        base.into()
    };
    env.write("home/.oxa/agent/settings.json",&json!({
        "defaultProjectTrust":"always","defaultModel":MODEL,"defaultThinkingLevel":"off",
        "permissions":{"defaultMode":"auto"},"cacheWarming":"off","retry":{"enabled":false},
        "oxa":{"envNote":false,"bootPreload":false,"engine":{"inProcessCloud":true},
        // Full-window worst-cache reservation is 0.010288; two settled turns
        // leave room for the third, then actual spend refuses a fourth.
        "cloud":{"daily_budget_usd":0.010368,"subscription_limits":{"daily_requests":3,"daily_tokens":100000},
        "models":{MODEL:{"provider":if subscription{"chatgpt"}else{"openai"},"base_url":endpoint,
            "upstream_model":"synthetic-openai","context_window":4096,"max_output":16,
            "pricing":{"input_per_million":2,"output_per_million":3,"cache_read_per_million":0.2,"cache_write_per_million":2.5}}}}}}
    ).to_string());
}
fn run(env: &TestEnv, args: &[&str], trace: Option<&Path>) -> Output {
    let mut command = if let Some(trace) = trace {
        let mut command = Command::new("strace");
        command
            .args(["-f", "-e", "trace=execve", "-o"])
            .arg(trace)
            .arg(env!("CARGO_BIN_EXE_oxa"));
        command
    } else {
        Command::new(env!("CARGO_BIN_EXE_oxa"))
    };
    command
        .env_clear()
        .envs(env.envs("http://127.0.0.1:9"))
        .current_dir(env.work())
        .args(args)
        .output()
        .unwrap()
}
fn success(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn live_openai_ids_filter_the_configured_overlay_and_reuse_the_cache() {
    let env = TestEnv::new();
    let mock = mock::MockOpenAi::start(vec![mock::Reply::json(json!({
        "data": [{"id": "synthetic-openai"}, {"id": "synthetic-unknown-metadata"}],
    }))]);
    settings(&env, &mock.base_url(), false);
    let path = env.agent_dir().join("settings.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let cloud = &mut settings["oxa"]["cloud"];
    cloud["discover_models"] = json!(true);
    let mut unavailable = cloud["models"][MODEL].clone();
    unavailable["upstream_model"] = json!("synthetic-unavailable");
    cloud["models"]["synthetic-not-offered"] = unavailable;
    fs::write(path, settings.to_string()).unwrap();
    private_file(
        &env.home().join(".config/oxc/llm/cloud-secrets.json"),
        &json!({"openai": KEY}).to_string(),
    );
    for _ in 0..2 {
        let output = run(&env, &["--list-models"], None);
        success(&output);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains(MODEL));
        assert!(!stdout.contains("synthetic-not-offered"));
        assert!(!stdout.contains("synthetic-unknown-metadata"));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("1 offers with unknown pricing and 1 with unknown limits"),
            "{stderr}"
        );
        assert!(!stdout.contains(KEY) && !stderr.contains(KEY));
    }
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].target, "/v1/models");
    let offline = run(&env, &["--offline", "--list-models"], None);
    assert!(!offline.status.success());
    assert_eq!(mock.requests().len(), 1);
    mock.assert_healthy();
}

#[test]
fn live_openrouter_discovery_uses_the_key_scoped_priced_text_offers() {
    let env = TestEnv::new();
    let offer = |id: &str, price: &str| {
        json!({"id": id, "context_length": 4096,
            "top_provider": {"max_completion_tokens": 128},
            "pricing": {"prompt": price, "completion": "0.000003"},
            "supported_parameters": ["tools"]})
    };
    let mock = mock::MockOpenAi::start(vec![mock::Reply::json(json!({"data": [
        offer("vendor/synthetic-priced", "0.000002"),
        offer("vendor/synthetic-unpriced", "-1"),
    ]}))]);
    let base = format!("http://{}/api/v1", mock.address());
    env.write(
        "home/.oxa/agent/settings.json",
        &json!({"oxa":{"engine":{"inProcessCloud":true},"cloud":{
            "discover_models":true, "discovery":{"openrouter":{"base_url":base}},
        }}})
        .to_string(),
    );
    private_file(
        &env.home().join(".config/oxc/llm/cloud-secrets.json"),
        &json!({"openrouter": KEY}).to_string(),
    );
    for _ in 0..2 {
        let output = run(&env, &["--list-models"], None);
        success(&output);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stdout.contains("openrouter/vendor/synthetic-priced"), "{stdout}");
        assert!(!stdout.contains("synthetic-unpriced"));
        assert!(stderr.contains("1 offers with unknown pricing"), "{stderr}");
        assert!(!stdout.contains(KEY) && !stderr.contains(KEY));
    }
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].target, "/api/v1/models/user");
    mock.assert_healthy();
}
fn terminal(output: &Output) -> Value {
    let records: Vec<Value> = output
        .stderr
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .filter(|record| record["type"] == "oxa_result")
        .collect();
    assert_eq!(records.len(), 1);
    records.into_iter().next().unwrap()
}
fn final_reply(text: &str) -> mock::Reply {
    mock::Reply::events(vec![
        json!({"type":"response.completed","response":{"status":"completed",
        "output":[{"type":"message","id":"msg_done","role":"assistant","content":[{"type":"output_text","text":text}]}],
        "usage":{"input_tokens":7,"output_tokens":2,"total_tokens":9}}}),
    ])
}

#[test]
fn a_configured_openai_alias_in_the_go_namespace_keeps_its_provider() {
    let env = TestEnv::new();
    let peer = mock::MockOpenAi::start(vec![
        final_reply("Synthetic configured reply"),
        final_reply("Synthetic scoped reply"),
    ]);
    settings(&env, &peer.base_url(), false);
    let path = env.agent_dir().join("settings.json");
    let mut user: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let cloud = &mut user["oxa"]["cloud"];
    let models = cloud["models"].as_object_mut().unwrap();
    let model = models.remove(MODEL).unwrap();
    let alias = "opencode/synthetic-openai";
    models.insert(alias.into(), model);
    cloud["opencode"] = json!({"enabled": false});
    fs::write(path, user.to_string()).unwrap();
    private_file(
        &env.home().join(".config/oxc/llm/cloud-secrets.json"),
        &json!({"openai": KEY}).to_string(),
    );

    let output = run(
        &env,
        &["--model", alias, "--no-tools", "-p", "Synthetic request"],
        None,
    );
    success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Synthetic configured reply"));
    let scoped = run(
        &env,
        &["--models", alias, "--no-tools", "-p", "Synthetic scoped request"],
        None,
    );
    success(&scoped);
    assert!(String::from_utf8_lossy(&scoped.stdout).contains("Synthetic scoped reply"));
    assert_eq!(peer.requests().len(), 2);
    let ledger = rows(&env);
    assert_eq!(ledger.len(), 2);
    assert!(ledger.iter().all(|row| row["remote_provider"] == "openai"));
    peer.assert_healthy();
}

fn rows(env: &TestEnv) -> Vec<Value> {
    fs::read_dir(env.home().join(".oxa/agent/cloud-ledger"))
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            path.extension()
                .is_some_and(|ext| ext == "jsonl")
                .then(|| fs::read_to_string(path).unwrap())
        })
        .flat_map(|text| {
            text.lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn unpriced_discovery_uses_owner_rates_for_admission_and_estimated_ledger_spend() {
    let env = TestEnv::new();
    let peer = mock::MockOpenAi::start(vec![
        mock::Reply::json(json!({"data":[{"id":"synthetic-openai"}]})),
        final_reply("Synthetic priced estimate"),
    ]);
    settings(&env, &format!("http://{}/v1", peer.address()), false);
    let path = env.agent_dir().join("settings.json");
    let mut user: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let cloud = &mut user["oxa"]["cloud"];
    cloud["discover_models"] = json!(true);
    cloud["models"][MODEL].as_object_mut().unwrap().remove("pricing");
    let pricing = json!({MODEL:{"input_per_million":2,"output_per_million":3}});
    env.write(
        "work/.oxa/settings.json",
        &json!({"oxa":{"cloud":{"unpriced_discovered_pricing":pricing}}}).to_string(),
    );
    private_file(
        &env.home().join(".config/oxc/llm/cloud-secrets.json"),
        &json!({"openai":KEY}).to_string(),
    );
    fs::write(&path, user.to_string()).unwrap();
    let unpriced = run(&env, &["--list-models"], None);
    assert!(!String::from_utf8_lossy(&unpriced.stdout).contains(MODEL));
    assert_eq!(peer.requests().len(), 1);

    user["oxa"]["cloud"]["unpriced_discovered_pricing"] = pricing;
    user["oxa"]["cloud"]["daily_budget_usd"] = json!(0.008239);
    fs::write(&path, user.to_string()).unwrap();
    let args = ["-p", "--no-session", "--no-tools", "Synthetic prompt"];
    let refused = run(&env, &args, None);
    assert_eq!(refused.status.code(), Some(oxa_proto::EXIT_BUDGET));
    assert_eq!(peer.requests().len(), 1);

    user["oxa"]["cloud"]["daily_budget_usd"] = json!(0.00825);
    fs::write(&path, user.to_string()).unwrap();
    let output = run(&env, &args, None);
    success(&output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("owner-estimated prices"));
    let row = rows(&env).pop().unwrap();
    assert_eq!(row["settled"], "estimated");
    assert_eq!(row["estimated_cost_usd"], 0.000020);
    assert!(row.get("cost_usd").is_none() && row.get("catalog_cost_usd").is_none());
    let refused = run(&env, &args, None);
    assert_eq!(refused.status.code(), Some(oxa_proto::EXIT_BUDGET));
    assert_eq!(
        peer.requests().len(),
        2,
        "the persisted estimate counts against the cap"
    );
    peer.assert_healthy();
}

#[test]
fn chatgpt_token_read_failure_does_not_claim_an_api_key_store_failure() {
    let env = TestEnv::new();
    let upstream = mock::MockOpenAi::start(vec![]);
    settings(&env, &upstream.base_url(), true);
    let key = env.home().join(".config/oxc/llm/cloud-secrets.json");
    private_file(&key, &json!({"openai":KEY}).to_string());
    private_file(
        &key.parent().unwrap().join("chatgpt-auth.json"),
        "invalid synthetic JSON",
    );
    let output = run(
        &env,
        &["--no-tools", "--mode", "json", "-p", "Synthetic request"],
        None,
    );
    assert_eq!(output.status.code(), Some(5));
    assert_eq!(terminal(&output)["error_class"], "cloud_credentials_unavailable");
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains("attempted protected store"), "{text}");
        assert!(!text.contains("native diagnostic"), "{text}");
        assert!(!text.contains(key.to_str().unwrap()));
        assert!(!text.contains(KEY));
    }
    assert!(upstream.requests().is_empty());
    assert_eq!(rows(&env)[0]["settled"], "not_forwarded");
    upstream.assert_healthy();
}

#[test]
fn production_both_modes_execute_done_only_tools_resume_and_settle_without_python() {
    for subscription in [false, true] {
        let env = TestEnv::new();
        env.write("work/fixture.txt", "SYNTHETIC FIRST FILE\n");
        env.write("work/another-fixture.txt", "SYNTHETIC SECOND FILE\n");
        let peer = mock::MockOpenAi::start(vec![
            mock::Reply::multi_tool_no_deltas(),
            final_reply("Native first λ answer"),
            final_reply("Native resumed λ answer"),
        ]);
        settings(&env, &peer.base_url(), subscription);
        let key = env.home().join(".config/oxc/llm/cloud-secrets.json");
        let token = key.parent().unwrap().join("chatgpt-auth.json");
        private_file(&key, "invalid synthetic JSON");
        private_file(&token, "invalid synthetic JSON");
        success(&run(&env, &["--list-models"], None));
        assert!(peer.requests().is_empty());
        assert!(!env.home().join(".oxa/agent/cloud-ledger").exists());
        if subscription {
            private_file(&token,&json!({"access_token":KEY,"refresh_token":"synthetic-refresh","account_id":"synthetic-account","expires_at":2_000_000_000}).to_string());
        } else {
            private_file(&key, &json!({"openai":KEY}).to_string());
        }
        let trace = env.work().join("first-exec.trace");
        let first = run(
            &env,
            &[
                "-p",
                "--no-mcp",
                "--no-extensions",
                "--no-context-files",
                "--tools",
                "read,bash",
                "Read both fixture files and print synthetic",
            ],
            Some(&trace),
        );
        success(&first);
        assert!(String::from_utf8_lossy(&first.stdout).contains("Native first λ answer"));
        let first_result = terminal(&first);
        assert_eq!(first_result["requests"], 2);
        assert_eq!(first_result["outcome"], "done");
        let session = first_result["session_id"].as_str().unwrap();
        assert!(!session.is_empty());
        let continuation = format!("--continue={session}");
        let resumed_trace = env.work().join("resume-exec.trace");
        let second = run(
            &env,
            &[
                "-p",
                "--no-mcp",
                "--no-extensions",
                "--no-context-files",
                "--tools",
                "read,bash",
                &continuation,
                "Continue the synthetic task",
            ],
            Some(&resumed_trace),
        );
        success(&second);
        assert!(String::from_utf8_lossy(&second.stdout).contains("Native resumed λ answer"));
        assert_eq!(terminal(&second)["requests"], 1);
        assert_eq!(terminal(&second)["session_id"], session);
        let requests = peer.requests();
        assert_eq!(requests.len(), 3);
        assert!(requests
            .iter()
            .all(|request| request.header("authorization") == Some(&format!("Bearer {KEY}"))));
        let after_tools = requests[1].body["input"].as_array().unwrap();
        let results: Vec<_> = after_tools
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .collect();
        assert_eq!(results.len(), 3);
        let text = serde_json::to_string(&results).unwrap();
        assert!(text.contains("SYNTHETIC FIRST FILE"), "{text}");
        assert!(text.contains("SYNTHETIC SECOND FILE"), "{text}");
        assert!(text.contains("synthetic"), "{text}");
        let resumed = requests[2].body["input"].as_array().unwrap();
        assert_eq!(
            resumed
                .iter()
                .filter(|item| item["type"] == "function_call")
                .count(),
            3
        );
        assert_eq!(
            resumed
                .iter()
                .filter(|item| item["type"] == "function_call_output")
                .count(),
            3
        );
        assert!(resumed.iter().any(|item| item["id"] == "fc_synthetic_0"));
        let paid = rows(&env);
        assert_eq!(paid.len(), 3);
        assert!(paid
            .iter()
            .all(|row| row["status"] == 200 && row["settled"] == "usage" && row["origin"] == "oxa"));
        if subscription {
            assert!(paid
                .iter()
                .all(|row| row["cost_usd"] == 0.0 && row["subscription_requests"] == 1));
            assert_eq!(
                paid.iter()
                    .map(|row| row["subscription_tokens"].as_u64().unwrap())
                    .sum::<u64>(),
                39
            );
        } else {
            let total = paid
                .iter()
                .map(|row| row["cost_usd"].as_f64().unwrap())
                .sum::<f64>();
            assert!((total - 0.000091).abs() < 1e-12);
        }
        let refused = run(
            &env,
            &["-p", "--no-session", "--no-tools", "No allowance remains"],
            None,
        );
        assert_eq!(refused.status.code(), Some(oxa_proto::EXIT_BUDGET));
        assert_eq!(peer.requests().len(), 3);
        let row = rows(&env).pop().unwrap();
        assert_eq!(row["status"], 429);
        assert_eq!(
            row["error"],
            if subscription {
                "subscription_limit_exceeded"
            } else {
                "budget_exceeded"
            }
        );
        assert_eq!(row["cost_usd"], 0.0);
        for path in [trace, resumed_trace] {
            let trace = fs::read_to_string(path).unwrap();
            assert!(trace.contains("execve(") && trace.contains("oxa"));
            assert!(!trace.to_ascii_lowercase().contains("python"), "{trace}");
        }
        for output in [&first, &second, &refused] {
            let visible = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!visible.contains(KEY));
            assert!(!visible.contains("provider_history"));
            assert!(!visible.contains("fc_synthetic_"));
        }
        peer.assert_healthy();
    }
}

#[test]
fn new_process_chatgpt_continuation_is_pinned_to_the_saved_account() {
    let env = TestEnv::new();
    let reply = |text: &str| {
        mock::Reply::events(vec![json!({
            "type":"response.completed","response":{"status":"completed",
            "output":[{"type":"reasoning","id":"rs_account_pin","encrypted_content":"opaque-account-pin-fixture"},
                {"type":"message","id":"msg_account_pin","role":"assistant",
                 "content":[{"type":"output_text","text":text}]}],
            "usage":{"input_tokens":7,"output_tokens":2,"total_tokens":9}}
        })])
    };
    let peer = mock::MockOpenAi::start(vec![reply("First pinned answer"), reply("Same account answer")]);
    settings(&env, &peer.base_url(), true);
    let token = env.home().join(".config/oxc/llm/chatgpt-auth.json");
    let record = |account: &str, access: &str| {
        json!({"access_token":access,
        "refresh_token":"synthetic-refresh","account_id":account,"expires_at":2_000_000_000})
        .to_string()
    };
    private_file(&token, &record("synthetic-account-a", KEY));
    let args = [
        "-p",
        "--no-tools",
        "--no-mcp",
        "--no-extensions",
        "--no-context-files",
    ];
    let first = run(
        &env,
        &[args.as_slice(), &["Start the pinned task"]].concat(),
        None,
    );
    success(&first);
    let session = terminal(&first)["session_id"].as_str().unwrap().to_owned();
    let continuation = format!("--continue={session}");
    // A persisted refresh may rotate the access token. Replay belongs to the
    // account, not to one access-token generation.
    let rotated = "synthetic-rotated-access";
    private_file(&token, &record("synthetic-account-a", rotated));
    let same = run(
        &env,
        &[
            args.as_slice(),
            &[&continuation, "Continue with the same account"],
        ]
        .concat(),
        None,
    );
    success(&same);
    assert!(peer.requests()[1].body["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == "rs_account_pin"
            && item["encrypted_content"] == "opaque-account-pin-fixture"));
    assert!(peer
        .requests()
        .iter()
        .all(|request| request.header("chatgpt-account-id") == Some("synthetic-account-a")));
    assert_eq!(
        peer.requests()[0].header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert_eq!(
        peer.requests()[1].header("authorization"),
        Some(format!("Bearer {rotated}").as_str())
    );

    // A third allowance remains: this must be a history/account refusal, not
    // the exhausted-budget negative control in the existing composition test.
    private_file(&token, &record("synthetic-account-b", rotated));
    let before = fs::read(&token).unwrap();
    let changed = run(
        &env,
        &[
            args.as_slice(),
            &[&continuation, "Do not cross the account boundary"],
        ]
        .concat(),
        None,
    );
    assert!(!changed.status.success());
    assert_eq!(terminal(&changed)["outcome"], "failed");
    assert_eq!(
        peer.requests().len(),
        2,
        "account switch must not contact the upstream"
    );
    assert_eq!(
        fs::read(&token).unwrap(),
        before,
        "refusal must not replace the selected account"
    );
    let settled = rows(&env);
    assert_eq!(settled.len(), 3);
    assert_eq!(settled[2]["status"], 400);
    assert_eq!(settled[2]["error"], "provider_history_mismatch");
    assert_eq!(settled[2]["cost_usd"], 0.0);
    for output in [&first, &same, &changed] {
        let visible = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        for private in [
            KEY,
            rotated,
            "synthetic-account-a",
            "synthetic-account-b",
            "opaque-account-pin-fixture",
        ] {
            assert!(
                !visible.contains(private),
                "private account/replay data reached terminal output"
            );
        }
    }
    peer.assert_healthy();
}

// Preserve the native equivalent of the SDK controller contract's default output limit.
#[test]
fn catalog_default_output_limit_is_visible_and_reaches_a_native_response() {
    for (override_limit, expected) in [(None, 16), (Some(4), 4)] {
        let env = TestEnv::new();
        let peer = mock::MockOpenAi::start(vec![mock::Reply::events(vec![json!({
            "type":"response.incomplete",
            "response":{
                "status":"incomplete",
                "incomplete_details":{"reason":"max_output_tokens"},
                "output":[{"type":"message","id":"msg_capped","role":"assistant",
                    "content":[{"type":"output_text","text":"Synthetic capped answer"}]}],
                "usage":{"input_tokens":7,"output_tokens":expected,"total_tokens":7 + expected}
            }
        })])]);
        settings(&env, &peer.base_url(), false);
        if let Some(limit) = override_limit {
            let path = env.home().join(".oxa/agent/settings.json");
            let mut config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            config["oxa"]["cloud"]["models"][MODEL]["max_output"] = json!(limit);
            fs::write(path, config.to_string()).unwrap();
        }
        private_file(
            &env.home().join(".config/oxc/llm/cloud-secrets.json"),
            &json!({"openai":KEY}).to_string(),
        );

        let listing = run(&env, &["--list-models", MODEL], None);
        success(&listing);
        let listing = String::from_utf8(listing.stdout).unwrap();
        let model = listing.lines().nth(1).unwrap();
        let fields: Vec<_> = model.split_whitespace().collect();
        assert_eq!(fields[1], MODEL);
        assert_eq!(fields[3].parse::<u64>().unwrap(), expected);
        assert!(
            peer.requests().is_empty(),
            "listing must not send an upstream request"
        );

        let output = run(
            &env,
            &[
                "--mode",
                "json",
                "--no-session",
                "--no-tools",
                "-p",
                "Synthetic length request",
            ],
            None,
        );
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result = terminal(&output);
        assert_eq!(result["error_class"], "output_limit");
        assert_eq!(result["requests"], 1);
        let requests = peer.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].body["max_output_tokens"], expected);
        let messages: Vec<_> = output
            .stdout
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).unwrap())
            .filter(|event| event["type"] == "message_end" && event["message"]["role"] == "assistant")
            .collect();
        assert_eq!(messages.len(), 1);
        let message = &messages[0]["message"];
        assert_eq!(message["stopReason"], "length");
        assert_eq!(message["usage"]["output"], expected);
        assert_eq!(message["content"][0]["text"], "Synthetic capped answer");
        peer.assert_healthy();
    }
}

#[test]
fn both_provider_clients_honor_the_selected_silence_timeout() {
    for subscription in [false, true] {
        for disabled in [false, true] {
            let env = TestEnv::new();
            let (reply, release) = final_reply("Delayed native answer").gated();
            let peer = mock::MockOpenAi::start(vec![reply]);
            settings(&env, &peer.base_url(), subscription);
            let path = env.home().join(".oxa/agent/settings.json");
            let mut config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            config["httpIdleTimeoutMs"] = if disabled { json!("disabled") } else { json!(40) };
            fs::write(path, config.to_string()).unwrap();
            let key = env.home().join(".config/oxc/llm/cloud-secrets.json");
            if subscription {
                private_file(&key, "invalid synthetic JSON");
                private_file(&key.parent().unwrap().join("chatgpt-auth.json"),&json!({"access_token":KEY,"refresh_token":"synthetic-refresh","account_id":"synthetic-account","expires_at":2_000_000_000}).to_string());
            } else {
                private_file(&key, &json!({"openai":KEY}).to_string());
            }
            let output = std::thread::scope(|scope| {
                scope.spawn(|| {
                    peer.wait_for_requests(1);
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    let _ = release.send(());
                });
                run(
                    &env,
                    &["-p", "--no-session", "--no-tools", "Synthetic timeout request"],
                    None,
                )
            });
            let row = rows(&env).pop().unwrap();
            if disabled {
                success(&output);
                assert_eq!(row["status"], 200);
                assert_eq!(row["settled"], "usage");
            } else {
                assert!(
                    !output.status.success(),
                    "the provider ignored the selected silence limit"
                );
                assert_eq!(row["status"], 502);
                assert_eq!(row["error"], "cloud_upstream_unavailable");
                assert_eq!(row["settled"], "reserved");
                if subscription {
                    assert_eq!(row["subscription_tokens"], 4112);
                    assert_eq!(row["cost_usd"], 0.0);
                } else {
                    assert_eq!(row["cost_usd"], json!(0.010288));
                }
            }
        }
    }
}
