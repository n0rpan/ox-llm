//! Milestone A's real-binary proof. This is deliberately not a passing claim on pre-S7 main.
//! The ignored case must be enabled and its user-settings fixture checked against landed S7.
#![cfg(unix)]

use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use serde_json::json;
use std::fs::{self, File};
use std::io;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn python_shims(env: &TestEnv) -> std::path::PathBuf {
    let bin = env.root().join("shim-bin");
    fs::create_dir(&bin).expect("shim directory");
    for name in ["python", "python3"] {
        let path = bin.join(name);
        fs::write(
            &path,
            "#!/bin/sh\nprintf '%s\\n' \"$0\" >> \"$OXA_TEST_PYTHON_MARKER\"\nexit 126\n",
        )
        .expect("write marker shim");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("executable shim");
    }
    bin
}

fn no_controller_connection(controller: &TcpListener) {
    match controller.accept() {
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
        Ok(_) => panic!("oxa connected to the legacy HTTP controller instead of its native gateway"),
        Err(error) => panic!("controller trap failed: {error}"),
    }
}

/// Calibrate both forbidden executables so an unused/broken marker cannot give a false pass.
#[test]
fn both_python_names_write_the_marker() {
    let env = TestEnv::new();
    let bin = python_shims(&env);
    let marker = env.tmp().join("python-invoked");
    for name in ["python", "python3"] {
        let status = Command::new(name)
            .env_clear()
            .env("PATH", &bin)
            .env("OXA_TEST_PYTHON_MARKER", &marker)
            .status()
            .expect("run forbidden-executable control");
        assert_eq!(status.code(), Some(126));
    }
    assert_eq!(fs::read_to_string(marker).unwrap().lines().count(), 2);
}

/// The fallback trap is an owned ephemeral controller endpoint, never a real service on 8092.
#[test]
fn controller_trap_detects_a_connection_without_a_request() {
    let controller = TcpListener::bind("127.0.0.1:0").unwrap();
    controller.set_nonblocking(true).unwrap();
    no_controller_connection(&controller);
    let _connection = std::net::TcpStream::connect(controller.local_addr().unwrap()).unwrap();
    assert!(controller.accept().is_ok());
}

#[test]
#[ignore = "S7 production in-process gateway selection is not on main; confirm its user-settings fixture and remove this ignore when S7 lands (#331, #431)"]
fn print_uses_native_gateway_without_python_or_controller() {
    let env = TestEnv::new();
    let mock = FakeServer::start(
        vec![ScriptTurn::text("native-gateway-proof")],
        FakeOpts::default(),
    )
    .expect("loopback upstream mock");
    let controller = TcpListener::bind("127.0.0.1:0").expect("owned controller trap");
    controller.set_nonblocking(true).unwrap();
    let controller_url = format!("http://{}", controller.local_addr().unwrap());
    let bin = python_shims(&env);
    let marker = env.tmp().join("python-invoked");

    // Synthetic, private fixture only. No inherited provider key or sign-in is available.
    let secrets = env.write(
        "home/.oxa/agent/mock-secrets.json",
        r#"{"openrouter":"synthetic-mock-only"}"#,
    );
    fs::set_permissions(&secrets, fs::Permissions::from_mode(0o600)).unwrap();
    // The engine switch is the S7 handoff contract. Its cloud object must be checked against
    // S7's final settings parser before enabling this test; do not add fallback selection here.
    let settings = json!({
        "defaultProjectTrust": "always",
        "retry": {"enabled": false},
        "oxa": {
            "engine": {"inProcessCloud": true},
            "cloud": {
                "enabled": true,
                "daily_budget_usd": 1.0,
                "secrets_file": secrets,
                "ledger_dir": env.tmp().join("ledger"),
                "models": {"mock-native": {
                    "kind": "remote", "provider": "openrouter", "base_url": mock.url(),
                    "upstream_model": "mock-upstream", "context_window": 8192,
                    "max_output": 128,
                    "pricing": {"input_per_million": 1.0, "output_per_million": 1.0}
                }}
            }
        }
    });
    env.write("home/.oxa/agent/settings.json", &settings.to_string());
    let stdout_path = env.tmp().join("stdout");
    let stderr_path = env.tmp().join("stderr");
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args([
            "-p",
            "--no-session",
            "--tools",
            "",
            "--model",
            "mock-native",
            "reply briefly",
        ])
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs(&controller_url))
        // No ambient PATH: neither another interpreter nor a controller wrapper is inherited.
        .env("PATH", &bin)
        .env("OXA_TEST_PYTHON_MARKER", &marker)
        .stdin(Stdio::null())
        .stdout(File::create(&stdout_path).unwrap())
        .stderr(File::create(&stderr_path).unwrap())
        .spawn()
        .expect("spawn actual oxa binary");
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for owned oxa") {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().expect("stop timed-out owned oxa");
            child.wait().expect("reap timed-out owned oxa");
            no_controller_connection(&controller);
            panic!("native gateway proof timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    no_controller_connection(&controller);
    assert!(!Path::new(&marker).exists(), "a Python interpreter was invoked");
    let stdout = fs::read_to_string(stdout_path).unwrap();
    let stderr = fs::read_to_string(stderr_path).unwrap();
    assert!(status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("native-gateway-proof"), "{stdout}");
    assert_eq!(
        mock.requests().len(),
        1,
        "the native upstream must actually answer"
    );
}
