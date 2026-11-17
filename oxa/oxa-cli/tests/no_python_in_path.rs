//! Milestone A native-cloud real-binary proof against the merged S7 factory.
//! Complementary policy/tool-denial controls remain in in_process_cloud.rs.
#![cfg(target_os = "linux")]

#[allow(dead_code)]
#[path = "../../oxc-gateway/tests/common/anthropic.rs"]
mod anthropic;
use anthropic::{MockAnthropic, Reply, CREDENTIAL};
use oxa_fake::TestEnv;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read};
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

fn verified_artifact(path: Option<&Path>, expected: Option<&str>) -> Result<PathBuf, &'static str> {
    let path = proof_binary(path, true)?;
    let expected = expected
        .filter(|value| value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or("release artifact SHA256 is required")?;
    let mut file = File::open(&path).map_err(|_| "release artifact is unreadable")?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 16384];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| "release artifact is unreadable")?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    if format!("{:x}", hash.finalize()) != expected.to_ascii_lowercase() {
        return Err("release artifact SHA256 mismatch");
    }
    Ok(path)
}

#[test]
fn release_hash_is_mandatory_and_a_wrong_artifact_cannot_substitute() {
    let env = TestEnv::new();
    let path = env.write("artifact", "abc");
    let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(verified_artifact(Some(&path), Some(expected)).unwrap(), path);
    assert!(verified_artifact(None, Some(expected)).is_err());
    assert!(verified_artifact(Some(&path), None).is_err());
    assert!(verified_artifact(Some(&path), Some("invalid")).is_err());
    fs::write(&path, "wrong artifact").unwrap();
    assert_eq!(
        verified_artifact(Some(&path), Some(expected)).unwrap_err(),
        "release artifact SHA256 mismatch"
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
    let required = std::env::var_os("OXA_TEST_REQUIRE_RELEASE_ARTIFACT").is_some();
    let binary = proof_binary(release.as_deref().map(Path::new), required).expect("select proof binary");
    // Workspace mode still exercises every existing cloud control. Release mode
    // additionally requires the exact matched native pair; missing inputs fail.
    let engine = required.then(|| {
        let oxa_hash = std::env::var("OXA_TEST_RELEASE_OXA_SHA256").ok();
        verified_artifact(Some(&binary), oxa_hash.as_deref()).expect("verify release oxa");
        let engine = std::env::var_os("OXA_TEST_RELEASE_OXE_SERVER");
        let engine_hash = std::env::var("OXA_TEST_RELEASE_OXE_SERVER_SHA256").ok();
        let engine = verified_artifact(engine.as_deref().map(Path::new), engine_hash.as_deref())
            .expect("verify release oxe-server");
        assert_eq!(binary.file_name().unwrap(), "oxa");
        assert_eq!(engine.file_name().unwrap(), "oxe-server");
        assert_eq!(binary.parent(), engine.parent(), "matched extracted archive pair");
        engine
    });
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
    // Actual S7 contract: native Anthropic, fixed protected key and agent-ledger paths.
    let settings = json!({
        "defaultProjectTrust": "always",
        "defaultThinkingLevel": "off",
        "retry": {"enabled": false},
        "oxa": {
            "engine": {"inProcessCloud": true},
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
    if let Some(engine) = engine {
        shipped_engine_authenticated_props(&engine);
        native_local_release_pair(&binary, &engine, false);
        native_local_release_pair(&binary, &engine, true);
    }
}

fn required_tracer() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("test-tool PATH"))
        .filter(|p| p.is_absolute())
        .map(|p| p.join("strace"))
        .find(|p| p.is_file())
        .expect("strace is mandatory")
}

// Always reap the traced process on assertion failure; strace's EXITKILL also
// terminates its tracees, including the supervised child's separate group.
struct TracedChild(std::process::Child);
impl Drop for TracedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn shipped_engine_authenticated_props(engine: &Path) {
    use oxa_engine::{http_request, HttpRequest, Url};
    let env = TestEnv::new();
    let model = env.work().join("tokenizer-only.gguf");
    tokenizer_fixture(&model);
    // Only this test-created synthetic fixture is passed to the child. The
    // test never reads an existing credential or the supervisor's auth file.
    let auth = env.write("tmp/synthetic-auth", CREDENTIAL);
    fs::set_permissions(&auth, fs::Permissions::from_mode(0o600)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let bin = python_shims(&env);
    let marker = env.tmp().join("python-invoked");
    let trace = env.tmp().join("props.trace");
    let mut child = TracedChild(
        Command::new(required_tracer())
            .args(["--kill-on-exit", "-f", "-e", "trace=execve,connect", "-o"])
            .arg(&trace)
            .arg(engine)
            .args(["--engine", "stub", "--model"])
            .arg(&model)
            .args([
                "--alias",
                "synthetic",
                "--stub-reply",
                "model-free",
                "--reasoning",
                "off",
                "--host",
                "127.0.0.1",
                "--port",
            ])
            .arg(port.to_string())
            .arg("--api-key-file")
            .arg(&auth)
            .env_clear()
            .env("HOME", env.home())
            .env("PATH", bin)
            .env("OXA_TEST_PYTHON_MARKER", &marker)
            .current_dir(env.work())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn exact shipped engine props control"),
    );
    let url = Url::parse(&format!("http://127.0.0.1:{port}")).unwrap();
    let headers = [("Authorization", format!("Bearer {CREDENTIAL}"))];
    let deadline = Instant::now() + Duration::from_secs(20);
    let response = loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "shipped engine exited before props"
        );
        if let Ok(response) = http_request(&HttpRequest {
            method: "GET",
            url: &url,
            path: "/props",
            headers: &headers,
            body: &[],
            timeout_ms: 1000,
        }) {
            if response.status == 200 {
                break response;
            }
        }
        assert!(
            Instant::now() < deadline,
            "authenticated shipped engine props timed out"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let props: serde_json::Value = serde_json::from_slice(&response.body).expect("props JSON");
    assert_eq!(props["build_info"], concat!("oxe-", env!("CARGO_PKG_VERSION")));
    assert!(!String::from_utf8_lossy(&response.body).contains(CREDENTIAL));
    let denied = http_request(&HttpRequest {
        method: "GET",
        url: &url,
        path: "/props",
        headers: &[],
        body: &[],
        timeout_ms: 1000,
    })
    .expect("unauthenticated props control");
    assert_eq!(
        denied.status, 401,
        "props must require the synthetic child credential"
    );
    drop(child);
    let trace = fs::read_to_string(trace).unwrap();
    assert!(
        trace
            .lines()
            .any(|line| line.contains(&format!("execve(\"{}\"", engine.display())) && line.ends_with("= 0")),
        "selected shipped engine must execute"
    );
    for line in trace.lines().filter(|line| line.contains("execve(\"")) {
        let executable = line.split("execve(\"").nth(1).unwrap().split('"').next().unwrap();
        assert_eq!(
            Path::new(executable),
            engine,
            "unexpected props-control executable"
        );
    }
    assert!(!marker.exists());
    println!("PASS exact shipped oxe-server authenticated props/build identity; model-free only");
}

// Same tokenizer-only, tensor-free fixture as the existing installed stub smoke.
fn tokenizer_fixture(path: &Path) {
    fn string(value: &str) -> Vec<u8> {
        let mut bytes = (value.len() as u64).to_le_bytes().to_vec();
        bytes.extend_from_slice(value.as_bytes());
        bytes
    }
    let mut rows = Vec::new();
    for (name, value) in [
        ("tokenizer.ggml.model", "gpt2"),
        ("tokenizer.ggml.pre", "qwen2"),
        ("tokenizer.chat_template", "{% for message in messages %}<|im_start|>{{ message['role'] }}\n{{ message['content'] }}<|im_end|>\n{% endfor %}{% if add_generation_prompt %}<|im_start|>assistant\n{% endif %}"),
    ] {
        let mut row = string(name);
        row.extend_from_slice(&8u32.to_le_bytes());
        row.extend(string(value));
        rows.push(row);
    }
    let visible = |b: u32| (33..127).contains(&b) || (161..173).contains(&b) || (174..256).contains(&b);
    let mut extra = 256;
    let mut tokens = Vec::new();
    for byte in 0..256 {
        let code = if visible(byte) {
            byte
        } else {
            let code = extra;
            extra += 1;
            code
        };
        tokens.push(char::from_u32(code).unwrap().to_string());
    }
    tokens.extend(
        [
            "<|im_start|>",
            "<|im_end|>",
            "<|endoftext|>",
            "<think>",
            "</think>",
        ]
        .map(str::to_owned),
    );
    for (name, values) in [
        ("tokenizer.ggml.tokens", tokens),
        ("tokenizer.ggml.merges", Vec::new()),
    ] {
        let mut row = string(name);
        row.extend_from_slice(&9u32.to_le_bytes());
        row.extend_from_slice(&8u32.to_le_bytes());
        row.extend_from_slice(&(values.len() as u64).to_le_bytes());
        for value in values {
            row.extend(string(&value));
        }
        rows.push(row);
    }
    let mut eos = string("tokenizer.ggml.eos_token_id");
    eos.extend_from_slice(&4u32.to_le_bytes());
    eos.extend_from_slice(&257u32.to_le_bytes());
    rows.push(eos);
    let mut bytes = b"GGUF".to_vec();
    bytes.extend_from_slice(&3u32.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&(rows.len() as u64).to_le_bytes());
    for row in rows {
        bytes.extend(row);
    }
    fs::write(path, bytes).unwrap();
}

// The shipped engine's existing stub is model-free. This is not a fake-child
// replacement and does not qualify weights, numerics or a real local-model task.
fn native_local_release_pair(binary: &Path, engine: &Path, missing_weights: bool) {
    let env = TestEnv::new();
    let model = env.work().join("tokenizer-only.gguf");
    if !missing_weights {
        tokenizer_fixture(&model);
    }
    let controller = TcpListener::bind("127.0.0.1:0").expect("owned controller trap");
    controller.set_nonblocking(true).unwrap();
    let controller_url = format!("http://{}", controller.local_addr().unwrap());
    let bin = python_shims(&env);
    let marker = env.tmp().join("python-invoked");
    env.write("home/.oxa/agent/settings.json", &json!({
        "defaultProjectTrust":"always", "defaultThinkingLevel":"off", "retry":{"enabled":false},
        "oxa":{"engine":{"inProcessLocal":true}, "local":{
            "binary":engine, "startup_timeout_seconds":10,
            "environment":{"PATH":bin,"OXA_TEST_PYTHON_MARKER":marker},
            "models":{"synthetic":{"model":model,"arguments":["--engine","stub","--stub-reply","native-local-proof","--reasoning","off"]}},
            "limits":{"synthetic":{"context_window":4096,"max_output":64,
                "allocation_budget":{"kv_bytes_per_position":1,"scratch_bytes":0,"mtp_state_bytes":0,"margin_bytes":1044480}}}
        }}
    }).to_string());
    let stdout = env.tmp().join("stdout");
    let stderr = env.tmp().join("stderr");
    let trace = env.tmp().join("local-process.trace");
    let mut child = Command::new(required_tracer())
        .args(["--kill-on-exit", "-f", "-e", "trace=execve,connect", "-o"])
        .arg(&trace)
        .arg(binary)
        .args([
            "-p",
            "--offline",
            "--no-session",
            "--no-tools",
            "--model",
            "synthetic",
            "reply briefly",
        ])
        .env_clear()
        .envs(env.envs(&controller_url))
        .env("PATH", &bin)
        .env("OXA_TEST_PYTHON_MARKER", &marker)
        .current_dir(env.work())
        .stdin(Stdio::null())
        .stdout(File::create(&stdout).unwrap())
        .stderr(File::create(&stderr).unwrap())
        .spawn()
        .expect("spawn exact release oxa");
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("native local release proof timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    no_controller_connection(&controller);
    let trace = fs::read_to_string(trace).unwrap();
    let mut executed = std::collections::BTreeSet::new();
    for line in trace
        .lines()
        .filter(|line| line.contains("execve(\"") && line.ends_with("= 0"))
    {
        let path = line.split("execve(\"").nth(1).unwrap().split('"').next().unwrap();
        assert!(
            Path::new(path) == binary || Path::new(path) == engine,
            "unexpected child executable"
        );
        executed.insert(PathBuf::from(path));
    }
    assert!(executed.contains(binary), "exact oxa must execute");
    for port in [8092, controller.local_addr().unwrap().port()] {
        assert!(
            !trace.contains(&format!("_port=htons({port})")),
            "legacy controller connection"
        );
    }
    assert!(!marker.exists(), "Python shim invoked");
    let stdout = fs::read_to_string(stdout).unwrap();
    let stderr = fs::read_to_string(stderr).unwrap();
    if missing_weights {
        assert!(!status.success(), "missing weights must refuse");
        assert!(stderr.contains("Local weights are missing"), "{stderr}");
        assert!(!stdout.contains("native-local-proof"));
        assert!(
            !executed.contains(engine),
            "missing weights must refuse before engine spawn"
        );
        assert!(
            !env.agent_dir().join("local-engine").exists(),
            "missing weights must refuse before child authentication"
        );
    } else {
        assert!(executed.contains(engine), "exact oxe-server must execute");
        assert!(
            trace.contains("connect("),
            "native loopback transport must be traced"
        );
        // LocalEngine startup requires authenticated /health and /props and
        // rejects a mismatched build_info before a chat request can proceed.
        assert!(status.success(), "native local proof failed: {stderr}");
        assert!(stdout.contains("native-local-proof"));
    }
    assert!(
        !env.home().join(".config/oxc").exists(),
        "local proof must need no cloud credentials"
    );
    println!("PASS exact release native model-free control: missing_weights={missing_weights}; no Python/controller; not real-model qualification");
}
