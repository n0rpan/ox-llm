//! Milestone A native-cloud real-binary proof against the merged S7 factory.
//! Complementary policy/tool-denial controls remain in in_process_cloud.rs.
#![cfg(target_os = "linux")]

#[allow(dead_code)]
#[path = "../../oxc-gateway/tests/common/anthropic.rs"]
mod anthropic;
use anthropic::{MockAnthropic, Reply, CREDENTIAL};
use oxa_fake::TestEnv;
use serde_json::json;
use std::fs::{self, File};
use std::io;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// The release workflow requires the binary extracted from its exact tarball.
// Normal workspace checks retain the Cargo-built binary; an explicit bad artifact
// never falls back to that binary and silently turns into a development proof.
fn proof_binary(release: Option<&Path>, required: bool) -> Result<PathBuf, &'static str> {
    let path = match release {
        Some(path) => path,
        None if required => return Err("release artifact binary is required"),
        None => Path::new(env!("CARGO_BIN_EXE_oxa")),
    };
    if !path.is_absolute() {
        return Err("proof binary path must be absolute");
    }
    let path = path.canonicalize().map_err(|_| "proof binary is unavailable")?;
    if !path.is_file() {
        return Err("proof binary must be a file");
    }
    Ok(path)
}

#[test]
fn required_release_artifact_cannot_fall_back_to_development_binary() {
    assert!(proof_binary(None, true).is_err());
    let env = TestEnv::new();
    assert!(proof_binary(Some(&env.root().join("missing-artifact")), true).is_err());
    assert!(proof_binary(Some(&env.root().join("missing-artifact")), false).is_err());
    assert!(proof_binary(Some(Path::new("relative-oxa")), true).is_err());
    assert!(proof_binary(Some(env.root()), true).is_err());
    assert_eq!(
        proof_binary(Some(Path::new(env!("CARGO_BIN_EXE_oxa"))), true).unwrap(),
        proof_binary(None, false).unwrap()
    );
}

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
fn print_uses_native_gateway_without_python_or_controller() {
    let release = std::env::var_os("OXA_TEST_RELEASE_OXA");
    let binary = proof_binary(
        release.as_deref().map(Path::new),
        std::env::var_os("OXA_TEST_REQUIRE_RELEASE_ARTIFACT").is_some(),
    )
    .expect("select proof binary");
    let env = TestEnv::new();
    let mock = MockAnthropic::start(vec![Reply::text("native-gateway-proof", 11, 7, 3, 5)]);
    let controller = TcpListener::bind("127.0.0.1:0").expect("owned controller trap");
    controller.set_nonblocking(true).unwrap();
    let controller_url = format!("http://{}", controller.local_addr().unwrap());
    let bin = python_shims(&env);
    let marker = env.tmp().join("python-invoked");

    // Synthetic, private fixture only. No inherited provider key or sign-in is available.
    let secrets = env.write(
        "home/.config/oxc/llm/cloud-secrets.json",
        &json!({"anthropic":{"api_key":CREDENTIAL}}).to_string(),
    );
    fs::set_permissions(&secrets, fs::Permissions::from_mode(0o600)).unwrap();
    // Release default: native Anthropic without a selection switch; fixed protected paths.
    let settings = json!({
        "defaultProjectTrust": "always",
        "defaultThinkingLevel": "off",
        "retry": {"enabled": false},
        "oxa": {
            "cloud": {
                "daily_budget_usd": 1.0,
                "models": {"mock-native": {
                    "provider": "anthropic", "base_url": mock.base_url(),
                    "upstream_model": "synthetic-anthropic", "context_window": 8192,
                    "max_output": 16,
                    "pricing": {"input_per_million": 2.0, "output_per_million": 3.0,
                        "cache_read_per_million": 0.2, "cache_write_per_million": 2.5}
                }}
            }
        }
    });
    env.write("home/.oxa/agent/settings.json", &settings.to_string());
    let stdout_path = env.tmp().join("stdout");
    let stderr_path = env.tmp().join("stderr");
    let trace_path = env.tmp().join("native-process.trace");
    // Resolve the test tool before replacing the child's PATH with only forbidden shims.
    let tool_path = std::env::var_os("PATH").expect("test-tool PATH");
    let tracer = std::env::split_paths(&tool_path)
        .filter(|p| p.is_absolute())
        .map(|p| p.join("strace"))
        .find(|p| p.is_file())
        .expect("strace is required for the native process/connection proof");
    let mut child = Command::new(tracer)
        .args(["-f", "-e", "trace=execve,connect", "-o"])
        .arg(&trace_path)
        .arg(&binary)
        .args([
            "-p",
            "--no-session",
            "--no-tools",
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
    let trace = fs::read_to_string(trace_path).expect("read only the synthetic process trace");
    assert!(trace.contains("execve("), "exec tracing must actually run");
    assert!(
        trace.contains(&format!("_port=htons({})", mock.address().port())),
        "the trace must include the actual upstream connection"
    );
    for port in [8092, controller.local_addr().unwrap().port()] {
        assert!(
            !trace.contains(&format!("_port=htons({port})")),
            "connected to controller port {port}"
        );
    }
    let mut selected_binary_executed = false;
    for line in trace.lines().filter(|line| line.contains("execve(\"")) {
        let executable = line.split("execve(\"").nth(1).unwrap().split('"').next().unwrap();
        selected_binary_executed |= Path::new(executable) == binary;
        let name = Path::new(executable)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_lowercase();
        assert!(!name.starts_with("python"), "a Python-named executable ran");
    }
    assert!(
        selected_binary_executed,
        "the selected artifact must actually execute"
    );
    assert!(!Path::new(&marker).exists(), "a Python interpreter was invoked");
    let stdout = fs::read_to_string(stdout_path).unwrap();
    let stderr = fs::read_to_string(stderr_path).unwrap();
    assert!(status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("native-gateway-proof"), "{stdout}");
    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "the native upstream must actually answer");
    assert_eq!(requests[0].target, "/v1/messages");
    assert_eq!(requests[0].header("x-api-key"), Some(CREDENTIAL));
    assert_eq!(requests[0].body["model"], "synthetic-anthropic");
    assert!(!stdout.contains(CREDENTIAL));
    assert!(!stderr.contains(CREDENTIAL));
    // Read only the synthetic usage ledger, never the credential fixture.
    let rows: Vec<serde_json::Value> = fs::read_dir(env.agent_dir().join("cloud-ledger"))
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            (path.extension().is_some_and(|e| e == "jsonl")).then(|| fs::read_to_string(path).unwrap())
        })
        .flat_map(|text| {
            text.lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["remote_provider"], "anthropic");
    assert_eq!(rows[0]["status"], 200);
    assert_eq!(rows[0]["settled"], "usage");
    assert!((rows[0]["cost_usd"].as_f64().unwrap() - 0.0000459).abs() < 1e-12);
    assert!(!rows[0].to_string().contains(CREDENTIAL));
    mock.finish();
}
