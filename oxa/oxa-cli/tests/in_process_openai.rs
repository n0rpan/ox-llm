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
