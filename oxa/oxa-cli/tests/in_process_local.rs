//! Real CLI selector controls. The native task/child protocol is tested in oxc-local.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use serde_json::{Value, json};
use std::process::{Command, Output};

fn local() -> Value {
    json!({"binary":"/synthetic/not-launched-oxe", "models":{"synthetic":{"model":"/synthetic/model.gguf","arguments":["--engine","stub","--ctx-size","4096"]}},
        "limits":{"synthetic":{"context_window":4096,"max_output":64,"required_available_bytes":1048576}}})
}
fn run(env: &TestEnv, url: &str, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
    cmd.args(args).current_dir(env.work()).env_clear();
    for (key, value) in env.envs(url) {
        cmd.env(key, value);
    }
    cmd.output().unwrap()
}
#[test]
fn native_local_discovery_is_user_only_non_launching_and_allowed_offline() {
    for (offline, explicit) in [(false, false), (false, true), (true, false), (true, true)] {
        let env = TestEnv::new();
        let mut settings =
            json!({"defaultProjectTrust":"always","oxa":{"engine":{"inProcessCloud":false},"local":local()}});
        if explicit {
            settings["oxa"]["engine"]["inProcessLocal"] = json!(true);
        }
        env.write("home/.oxa/agent/settings.json", &settings.to_string());
        let http = FakeServer::start(vec![], FakeOpts::default()).unwrap();
        let args = if offline {
            vec!["--offline", "--list-models"]
        } else {
            vec!["--list-models"]
        };
        let output = run(&env, &http.url(), &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("synthetic"));
        assert_eq!(http.hits("GET", "/v1/models"), 0);
        assert!(!env.home().join(".oxa/agent/local-engine").exists());
    }
}
#[test]
fn explicit_false_local_setting_keeps_http_despite_project_native_settings() {
    let env = TestEnv::new();
    let settings = json!({"defaultProjectTrust":"always","retry":{"enabled":false},"oxa":{"engine":{"inProcessCloud":false,"inProcessLocal":false},"local":"invalid when selected"}});
    env.write("home/.oxa/agent/settings.json", &settings.to_string());
    env.write(
        "work/.oxa/settings.json",
        &json!({"oxa":{"engine":{"inProcessLocal":true},"local":local()}}).to_string(),
    );
    let http = FakeServer::start(vec![ScriptTurn::text("Existing transport")], FakeOpts::default()).unwrap();
    let output = run(
        &env,
        &http.url(),
        &["--no-session", "--no-tools", "-p", "Synthetic prompt"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Existing transport"));
    assert_eq!(http.requests().len(), 1);
    assert!(!env.home().join(".oxa/agent/local-engine").exists());
}

#[test]
fn invalid_enabled_local_configuration_never_falls_back_to_http() {
    for enabled in [json!(true), json!("true")] {
        let env = TestEnv::new();
        env.write(
            "home/.oxa/agent/settings.json",
            &json!({"oxa":{"engine":{"inProcessCloud":false,"inProcessLocal":enabled},"local":{}}})
                .to_string(),
        );
        let http = FakeServer::start(vec![], FakeOpts::default()).unwrap();
        let output = run(&env, &http.url(), &["--list-models"]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Invalid user local configuration"));
        assert_eq!(http.hits("GET", "/v1/models"), 0);
    }
}
