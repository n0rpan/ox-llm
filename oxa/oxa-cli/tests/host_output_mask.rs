//! Known values must not reach the next model request from host bash.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::process::Command;

#[test]
fn attended_and_yolo_worker_bash_mask_known_values_without_inheriting_them() {
    let vars = [
        ("GH_TOKEN", "fixture-worker-value"),
        ("FIXTURE_PROVIDER_KEY", "fixture-provider-value"),
        ("fixture_secret_label", " fixture with spaces "),
        ("FIXTURE_PASSWORD_FILE", "fixture-password-value"),
        ("FIXTURE_ORDINARY", "ordinary-output"),
    ];
    let engine_key = "fixture-engine-value";
    for worker in [false, true] {
        let env = TestEnv::new();
        let contents = vars
            .iter()
            .map(|(_, value)| *value)
            .chain([engine_key])
            .collect::<Vec<_>>()
            .join("|");
        env.write("work/fixture.txt", &contents);
        let bash = serde_json::json!({
            "command": "test -z \"${GH_TOKEN+x}${FIXTURE_PROVIDER_KEY+x}\" && cat fixture.txt"
        });
        let server = FakeServer::start(
            vec![
                ScriptTurn::tool("host-mask", "bash", &bash.to_string()),
                ScriptTurn::text("done"),
            ],
            FakeOpts::default(),
        )
        .unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
        cmd.current_dir(env.work())
            .env_clear()
            .envs(env.envs(&server.url()))
            .envs(vars)
            .args(["-p", "--no-session", "--tools", "bash", "--api-key", engine_key]);
        if worker {
            cmd.env("OXA_UNATTENDED", "1").args(["--yolo"]);
        }
        let output = cmd.arg("Run the fixture command").output().unwrap();
        assert!(output.status.success(), "oxa did not complete");
        let requests = server.requests();
        let tool_text = requests
            .last()
            .unwrap()
            .messages
            .iter()
            .find(|msg| msg.tool_call_id.as_deref() == Some("host-mask"))
            .and_then(|msg| msg.content.as_deref())
            .expect("bash result reached the next turn");
        assert!(tool_text.contains("[redacted]|[redacted]|[redacted]|[redacted]|ordinary-output|[redacted]"));
        assert!(vars[..4].iter().all(|(_, value)| !tool_text.contains(*value)));
        assert!(!tool_text.contains(engine_key));
    }
}
