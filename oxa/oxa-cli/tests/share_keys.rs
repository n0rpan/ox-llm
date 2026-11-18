//! Real CLI, isolated home and generated guest bearers only. No model/network.
#[cfg(all(feature = "share", target_os = "linux"))]
#[path = "common/strace.rs"]
mod strace;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "oxa-share-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oxa"))
            .args(args)
            .env_clear()
            .env("HOME", &self.0)
            .env("OXA_TEST_ISOLATION", "1")
            .current_dir(&self.0)
            .output()
            .unwrap()
    }
    #[cfg(feature = "share")]
    fn marked(&self, args: &[&str], marker: &str, value: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oxa"))
            .args(args)
            .env_clear()
            .env("HOME", &self.0)
            .env("OXA_TEST_ISOLATION", "1")
            .env(marker, value)
            .current_dir(&self.0)
            .output()
            .unwrap()
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[cfg(not(feature = "share"))]
#[test]
fn default_build_refuses_share_without_creating_storage() {
    let home = Home::new();
    let result = home.run(&["share", "key", "list"]);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(!home.0.join(".oxa").exists());
}

#[cfg(feature = "share")]
const OWNER_REFUSAL: &str = "Guest key commands require the owner outside an agent tool";

#[cfg(feature = "share")]
fn assert_agent_refusal(output: &Output) {
    assert_eq!(output.status.code(), Some(1), "owner command must refuse");
    assert!(output.stdout.is_empty(), "owner output must stay private");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains(OWNER_REFUSAL), "{error}");
    assert!(
        !error.contains("synthetic-device"),
        "arguments must not be echoed"
    );
    assert!(!error.contains("synthetic-id"), "arguments must not be echoed");
}

#[cfg(feature = "share")]
#[test]
fn agent_markers_refuse_all_key_commands_without_creating_storage() {
    for marker in ["OXA_CODING_AGENT", "AI_AGENT", "OXA_SUBAGENT_DEPTH"] {
        for value in ["", "1"] {
            for args in [
                vec![
                    "share",
                    "key",
                    "add",
                    "synthetic-device",
                    "--daily-requests",
                    "1",
                    "--daily-tokens",
                    "10",
                ],
                vec!["share", "key", "list"],
                vec!["share", "key", "revoke", "synthetic-id"],
            ] {
                let home = Home::new();
                assert_agent_refusal(&home.marked(&args, marker, value));
                assert!(!home.0.join(".oxa").exists(), "refusal must precede storage");
                assert!(!home.0.join(".config").exists());
            }
        }
    }
}

#[cfg(all(feature = "share", target_os = "linux"))]
#[test]
fn agent_refusal_does_not_open_a_preexisting_synthetic_store() {
    use std::os::unix::fs::PermissionsExt;
    for marker in ["OXA_CODING_AGENT", "AI_AGENT", "OXA_SUBAGENT_DEPTH"] {
        for args in [
            vec!["share", "key", "list"],
            vec!["share", "key", "revoke", "synthetic-id"],
        ] {
            let home = Home::new();
            let store = home.0.join(".oxa/share");
            fs::create_dir_all(&store).unwrap();
            fs::set_permissions(&store, fs::Permissions::from_mode(0o700)).unwrap();
            // Deliberately invalid synthetic bytes: this is never a key registry.
            let sentinel = store.join("share-keys.json");
            fs::write(&sentinel, b"synthetic unopened store").unwrap();
            fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o600)).unwrap();
            let trace = home.0.join("store-opens.trace");
            let output = Command::new("timeout")
                .args(["--kill-after=1s", "5s"])
                .arg(strace::executable())
                .args(["-f", "-e", "trace=open,openat,openat2", "-o"])
                .arg(&trace)
                .arg(env!("CARGO_BIN_EXE_oxa"))
                .args(&args)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", &home.0)
                .env("OXA_TEST_ISOLATION", "1")
                .env(marker, "")
                .current_dir(&home.0)
                .output()
                .expect("timeout and strace are mandatory for the no-store-open proof");
            assert_agent_refusal(&output);
            let opens = fs::read_to_string(trace).unwrap();
            assert!(opens.contains("open"), "tracer must record process startup");
            assert!(!opens.contains(store.to_str().unwrap()), "{opens}");
            assert!(!store.join(".lock").exists());
            assert_eq!(fs::read(&sentinel).unwrap(), b"synthetic unopened store");
            assert_eq!(fs::read_dir(&store).unwrap().count(), 1);
            assert!(!home.0.join(".oxa/agent").exists());
        }
    }
}

#[cfg(feature = "share")]
#[test]
fn real_key_commands_create_list_revoke_without_reprinting_bearer() {
    use oxc_gateway::share::keys::Store;
    use serde_json::Value;
    use std::os::unix::fs::PermissionsExt;
    let home = Home::new();
    let created = home.run(&[
        "share",
        "key",
        "add",
        "Tablet",
        "--daily-requests",
        "10",
        "--daily-tokens",
        "20000",
    ]);
    assert!(created.status.success());
    assert!(created.stderr.is_empty());
    let value: Value = serde_json::from_slice(&created.stdout).unwrap();
    let bearer = value["api_key"].as_str().unwrap();
    let id = value["id"].as_str().unwrap();
    let directory = home.0.join(".oxa/share");
    let store = Store::open(&directory).unwrap();
    let info = store.authenticate(bearer).unwrap().unwrap();
    assert_eq!(info.name, "Tablet");
    assert_eq!(info.limits.daily_requests, 10);
    assert_eq!(info.limits.daily_tokens, 20000);
    assert_eq!(
        fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(directory.join("share-keys.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(!fs::read_to_string(directory.join("share-keys.json"))
        .unwrap()
        .contains(bearer));
    for args in [
        vec!["share", "key", "list"],
        vec!["share", "key", "revoke", id],
        vec!["share", "key", "list"],
    ] {
        let result = home.run(&args);
        assert!(result.status.success());
        assert!(result.stderr.is_empty());
        assert!(!String::from_utf8(result.stdout).unwrap().contains(bearer));
    }
    assert!(store.authenticate(bearer).unwrap().is_none());
    let other = Home::new();
    Store::initialize(&other.0.join(".oxa/share")).unwrap();
    let missing = other.run(&["share", "key", "revoke", id]);
    assert!(missing.status.success());
    let value: Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(value["revoked"], false);
}

#[cfg(feature = "share")]
#[test]
fn invalid_commands_and_unsafe_directory_never_issue_keys() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let home = Home::new();
    for args in [
        vec!["share", "key", "add", "Tablet"],
        vec![
            "share",
            "key",
            "add",
            "Tablet",
            "--daily-requests",
            "0",
            "--daily-tokens",
            "10",
        ],
        vec![
            "share",
            "key",
            "add",
            "Tablet",
            "--daily-requests",
            "1",
            "--daily-requests",
            "10",
        ],
        vec!["share", "key", "list", "unexpected"],
    ] {
        let result = home.run(&args);
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(!home.0.join(".oxa").exists());
    }
    let target = home.0.join("outside");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&target, home.0.join(".oxa")).unwrap();
    let result = home.run(&[
        "share",
        "key",
        "add",
        "Tablet",
        "--daily-requests",
        "1",
        "--daily-tokens",
        "10",
    ]);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(fs::read_dir(&target).unwrap().next().is_none());
}

#[cfg(feature = "share")]
#[test]
fn unsafe_modes_and_repository_homes_are_refused_without_repair() {
    use std::os::unix::fs::PermissionsExt;
    for repository in [false, true] {
        let home = Home::new();
        let directory = home.0.join(".oxa/share");
        if repository {
            fs::create_dir(home.0.join(".git")).unwrap();
        } else {
            fs::create_dir_all(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let result = home.run(&[
            "share",
            "key",
            "add",
            "Tablet",
            "--daily-requests",
            "1",
            "--daily-tokens",
            "10",
        ]);
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(!directory.join("share-keys.json").exists());
        if repository {
            assert!(!home.0.join(".oxa").exists());
        } else {
            assert_eq!(
                fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
    }
}

#[cfg(feature = "share")]
#[test]
fn output_failure_keeps_one_durable_key_without_retrying_or_leaking_diagnostics() {
    struct Closed;
    impl std::io::Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let home = Home::new();
    let directory = home.0.join(".oxa/share");
    let words = [
        "key",
        "add",
        "Tablet",
        "--daily-requests",
        "1",
        "--daily-tokens",
        "10",
    ]
    .map(String::from);
    let error = oxc_gateway::share::command::run(&words, &directory, &mut Closed).unwrap_err();
    assert!(matches!(error, oxc_gateway::share::command::Error::Output));
    assert_eq!(format!("{error:?}"), "Output");
    let store = oxc_gateway::share::keys::Store::open(&directory).unwrap();
    let rows = store.list().unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].revoked);
    assert!(store.revoke(&rows[0].id).unwrap());
}

#[cfg(feature = "share")]
#[test]
fn serving_refuses_agent_and_offline_before_opening_guest_storage() {
    let home = Home::new();
    let serve = ["share", "serve", "--model", "synthetic-model"];
    let agent = home.marked(&serve, "OXA_CODING_AGENT", "");
    assert!(!agent.status.success());
    assert!(String::from_utf8_lossy(&agent.stderr).contains("Guest serving requires the owner"));

    let offline = home.run(&["--offline", "share", "serve", "--model", "synthetic-model"]);
    assert!(!offline.status.success());
    assert!(String::from_utf8_lossy(&offline.stderr).contains("offline mode is on"));

    let offline_env = home.marked(&serve, "OXA_OFFLINE", "1");
    assert!(!offline_env.status.success());
    assert!(String::from_utf8_lossy(&offline_env.stderr).contains("offline mode is on"));
    assert!(!home.0.join(".oxa/share").exists());
}

#[cfg(feature = "share")]
#[test]
fn serving_requires_an_owner_selected_alias_and_enabled_native_engine() {
    let home = Home::new();
    let missing_alias = home.run(&["share", "serve"]);
    assert!(!missing_alias.status.success());
    assert!(String::from_utf8_lossy(&missing_alias.stderr).contains("--model ALIAS"));

    let disabled = home.run(&["share", "serve", "--model", "synthetic-model"]);
    assert!(!disabled.status.success());
    assert!(String::from_utf8_lossy(&disabled.stderr).contains("Enable the native local engine"));
    assert!(!home.0.join(".oxa/share").exists());
}

#[cfg(feature = "share")]
#[test]
fn native_cleartext_listener_refuses_wildcard_and_unverified_interfaces() {
    use oxa_proto::{CancelToken, ChatRequest, StreamEvent};
    use oxc_gateway::share::{
        keys::Store,
        listener::{Access, Binding, Config, Failure, Model, Tailnet},
        native::{self, Native},
    };
    use std::{net::IpAddr, sync::Arc};

    struct TailnetAddresses;
    impl Tailnet for TailnetAddresses {
        async fn addresses(&self) -> Result<Vec<IpAddr>, Failure> {
            Ok(vec!["100.64.0.2".parse().unwrap()])
        }
        async fn same_owner(&self, _: IpAddr) -> Result<bool, Failure> {
            unreachable!("no listener reaches peer authentication")
        }
    }

    let home = Home::new();
    let keys = Arc::new(Store::initialize(&home.0.join(".oxa/share")).unwrap());
    let answer = Arc::new(
        Native::new(
            vec![Model {
                alias: "synthetic".into(),
                context_window: 128,
                max_output: 8,
                access: Access::Local,
            }],
            |_: &ChatRequest<'_>, _: &CancelToken, _: &mut dyn FnMut(StreamEvent)| {
                unreachable!("the address must refuse before any guest connects")
            },
        )
        .unwrap(),
    );
    for address in ["0.0.0.0", "::", "192.0.2.11"] {
        let result = native::serve(
            keys.clone(),
            answer.clone(),
            Arc::new(TailnetAddresses),
            Config {
                binding: Binding::Explicit(address.parse().unwrap()),
                ..Config::default()
            },
            CancelToken::new(),
            |_, _| Err(Failure::Failed),
        );
        assert!(matches!(result, Err(Failure::Forbidden)), "{address}: {result:?}");
    }
}

#[cfg(feature = "share")]
struct ForegroundShare(std::process::Child);

#[cfg(feature = "share")]
impl Drop for ForegroundShare {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(feature = "share")]
impl ForegroundShare {
    fn spawn(home: &Home, models: &[&str]) -> (Self, String) {
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;
        use std::time::Duration;

        let mut serving = Self(
            Command::new(env!("CARGO_BIN_EXE_oxa"))
                .args(["share", "serve", "--bind", "127.0.0.1"])
                .args(models.iter().flat_map(|model| ["--model", *model]))
                .env_clear()
                .env("HOME", &home.0)
                .env("OXA_TEST_ISOLATION", "1")
                .current_dir(&home.0)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stdout = serving.0.stdout.take().unwrap();
        let (send, recv) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
            let _ = send.send(result);
        });
        let line = recv.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        reader.join().unwrap();
        (serving, line)
    }

    fn start(home: &Home, models: &[&str]) -> (Self, std::net::SocketAddr) {
        let (serving, line) = Self::spawn(home, models);
        assert!(
            !line.is_empty(),
            "share listener exited before announcing readiness"
        );
        let address: std::net::SocketAddr = line
            .trim()
            .strip_prefix("Guest OpenAI API listening at http://")
            .unwrap()
            .strip_suffix("/v1")
            .unwrap()
            .parse()
            .unwrap();
        assert!(address.ip().is_loopback());

        (serving, address)
    }
}

#[cfg(feature = "share")]
fn add_test_guest(home: &Home) -> String {
    let added = home.run(&[
        "share",
        "key",
        "add",
        "Device",
        "--daily-requests",
        "10",
        "--daily-tokens",
        "10000",
    ]);
    assert!(added.status.success());
    let key: serde_json::Value = serde_json::from_slice(&added.stdout).unwrap();
    key["api_key"].as_str().unwrap().to_owned()
}

#[cfg(feature = "share")]
fn guest_request(
    address: std::net::SocketAddr,
    method: &str,
    path: &str,
    bearer: &str,
    body: &str,
) -> String {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let mut client = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(
        client,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {bearer}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut reply = String::new();
    client.read_to_string(&mut reply).unwrap();
    reply
}

#[cfg(feature = "share")]
#[test]
fn foreground_serve_lists_only_the_owner_selected_local_models_on_loopback() {
    let home = Home::new();
    let agent = home.0.join(".oxa/agent");
    fs::create_dir_all(&agent).unwrap();
    let limit = serde_json::json!({
        "context_window":4096,"max_output":64,
        "allocation_budget":{"kv_bytes_per_position":1,"scratch_bytes":0,"mtp_state_bytes":0,"margin_bytes":1044480}
    });
    let settings = serde_json::json!({"oxa":{
        "engine":{"inProcessLocal":true},
        "local":{"binary":"/synthetic/oxe-server",
            "models":{
                "synthetic-visible":{"model":"/synthetic/visible.gguf","arguments":["--engine","stub"]},
                "synthetic-hidden":{"model":"/synthetic/hidden.gguf","arguments":["--engine","stub"]}
            },
            "limits":{"synthetic-visible":limit,"synthetic-hidden":limit}
        }
    }});
    fs::write(agent.join("settings.json"), settings.to_string()).unwrap();
    let bearer = add_test_guest(&home);
    let (_serving, address) = ForegroundShare::start(&home, &["synthetic-visible"]);
    let reply = guest_request(address, "GET", "/v1/models", &bearer, "");
    assert!(reply.starts_with("HTTP/1.1 200"));
    assert!(reply.contains("synthetic-visible"));
    assert!(!reply.contains("synthetic-hidden"));
    assert!(!reply.contains(&bearer));
}

#[cfg(feature = "share")]
#[allow(dead_code)]
#[path = "../../oxc-gateway/tests/common/anthropic.rs"]
mod metered_peer;

#[cfg(feature = "share")]
fn write_mock_provider_key(home: &Home, provider: &str, credential: &str) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let key_path = home.0.join(".config/oxc/llm/cloud-secrets.json");
    fs::create_dir_all(key_path.parent().unwrap()).unwrap();
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&key_path)
        .unwrap();
    write!(
        file,
        "{}",
        serde_json::json!({ (provider): {"api_key":credential} })
    )
    .unwrap();
}

#[cfg(feature = "share")]
#[test]
fn foreground_metered_cloud_requires_owner_allowlist_and_settles_a_guest_request() {
    use metered_peer::{MockAnthropic, Reply, CREDENTIAL};
    use serde_json::json;

    let mut events = vec![
        json!({"type":"message_start","message":{"id":"msg_synthetic","type":"message","role":"assistant",
            "model":"synthetic-anthropic","content":[],"stop_reason":null,
            "usage":{"input_tokens":7,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
    ];
    let parts: Vec<_> = (1..=12)
        .map(|part| format!("Synthetic metered reply part {part}. "))
        .collect();
    events.extend(parts.iter().map(
        |part| json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":part}}),
    ));
    events.extend([
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},
            "usage":{"output_tokens":24}}),
        json!({"type":"message_stop"}),
    ]);
    let upstream = MockAnthropic::start(vec![Reply::events(events).unfragmented()]);
    let home = Home::new();
    let agent = home.0.join(".oxa/agent");
    fs::create_dir_all(&agent).unwrap();
    let mut settings = json!({"oxa":{
        "engine":{"inProcessCloud":true},
        "cloud":{"daily_budget_usd":1.0,"models":{
            "synthetic-metered":{"provider":"anthropic","base_url":upstream.base_url(),
                "upstream_model":"synthetic-anthropic","context_window":4096,"max_output":64,
                "pricing":{"input_per_million":2,"output_per_million":3}},
            "synthetic-unselected":{"provider":"anthropic","base_url":upstream.base_url(),
                "upstream_model":"synthetic-other","context_window":4096,"max_output":16,
                "pricing":{"input_per_million":2,"output_per_million":3}}
        }}
    }});
    let settings_path = agent.join("settings.json");
    fs::write(&settings_path, settings.to_string()).unwrap();
    write_mock_provider_key(&home, "anthropic", CREDENTIAL);

    let bearer = add_test_guest(&home);
    let (mut disabled, announcement) = ForegroundShare::spawn(&home, &["synthetic-metered"]);
    assert!(announcement.is_empty());
    assert!(!disabled.0.wait().unwrap().success());
    assert!(upstream.requests().is_empty());

    settings["oxa"]["share"]["remoteModels"] = json!(["synthetic-metered"]);
    fs::write(&settings_path, settings.to_string()).unwrap();
    let (_serving, address) = ForegroundShare::start(&home, &["synthetic-metered"]);
    let models = guest_request(address, "GET", "/v1/models", &bearer, "");
    assert!(models.starts_with("HTTP/1.1 200"), "{models}");
    assert!(models.contains("synthetic-metered"));
    assert!(!models.contains("synthetic-unselected"));
    assert!(upstream.requests().is_empty());

    let completion = guest_request(
        address,
        "POST",
        "/v1/chat/completions",
        &bearer,
        r#"{"model":"synthetic-metered","messages":[{"role":"user","content":"Synthetic guest question"}],"max_tokens":32,"stream":true}"#,
    );
    assert!(completion.starts_with("HTTP/1.1 200"), "{completion}");
    assert!(completion.contains("[DONE]"), "{completion}");
    let text: String = completion
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str::<serde_json::Value>(data).unwrap())
        .filter_map(|chunk| {
            chunk["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(text, parts.concat());

    let calls = upstream.requests();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].body["model"], "synthetic-anthropic");
    assert!(calls[0].body["tools"].is_null());
    let rows: Vec<serde_json::Value> = fs::read_dir(agent.join("cloud-ledger"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .flat_map(|path| {
            fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["origin"], "api");
    assert_eq!(rows[0]["remote"], true);
    assert!(rows[0]["cost_usd"].as_f64().unwrap() > 0.0);
    assert_eq!(rows[0]["guest_key"].as_str().unwrap().len(), 32);
    for private in [
        CREDENTIAL,
        &bearer,
        "Device",
        "Synthetic guest question",
        "Synthetic metered reply",
    ] {
        assert!(!rows[0].to_string().contains(private));
    }
}

#[cfg(feature = "share")]
#[test]
fn metered_openrouter_forwards_only_supplied_guest_sampling() {
    use metered_peer::{MockAnthropic, Reply, CREDENTIAL};
    use serde_json::json;

    const SSE: &[u8] =
        br#"data: {"choices":[{"index":0,"delta":{"content":"Synthetic reply"},"finish_reason":null}]}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: {"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":2}}

data: [DONE]

"#;
    let cases = [
        (Some(0.0), None),
        (None, Some(0.5)),
        (Some(0.25), Some(0.75)),
        (None, None),
    ];
    let upstream = MockAnthropic::start(
        cases
            .iter()
            .map(|_| Reply::wire("text/event-stream", SSE))
            .collect(),
    );
    let home = Home::new();
    let agent = home.0.join(".oxa/agent");
    fs::create_dir_all(&agent).unwrap();
    fs::write(
        agent.join("settings.json"),
        json!({"oxa":{
            "engine":{"inProcessCloud":true},
            "cloud":{"daily_budget_usd":1.0,"models":{
                "synthetic-metered":{"provider":"openrouter","base_url":upstream.base_url(),
                    "upstream_model":"synthetic-openrouter","context_window":4096,"max_output":16,
                    "pricing":{"input_per_million":2,"output_per_million":3}}
            }},
            "share":{"remoteModels":["synthetic-metered"]}
        }})
        .to_string(),
    )
    .unwrap();
    write_mock_provider_key(&home, "openrouter", CREDENTIAL);

    let bearer = add_test_guest(&home);
    let (_serving, address) = ForegroundShare::start(&home, &["synthetic-metered"]);
    for (temperature, top_p) in cases {
        let mut body = json!({"model":"synthetic-metered","stream":true,"max_tokens":8,
            "messages":[{"role":"user","content":"Synthetic guest question"}]});
        for (field, value) in [("temperature", temperature), ("top_p", top_p)] {
            if let Some(value) = value {
                body[field] = json!(value);
            }
        }
        let reply = guest_request(
            address,
            "POST",
            "/v1/chat/completions",
            &bearer,
            &body.to_string(),
        );
        assert!(
            reply.starts_with("HTTP/1.1 200") && reply.contains("[DONE]"),
            "{reply}"
        );
    }
    let calls = upstream.requests();
    assert_eq!(calls.len(), cases.len());
    for (call, (temperature, top_p)) in calls.iter().zip(cases) {
        for (field, requested) in [("temperature", temperature), ("top_p", top_p)] {
            match requested {
                Some(value) => assert_eq!(call.body[field], json!(value), "{field}"),
                None => assert!(call.body.get(field).is_none(), "{field}"),
            }
        }
    }
    upstream.finish();
}

#[cfg(feature = "share")]
#[test]
fn owner_session_sharing_flags_refuse_noninteractive_and_agent_invocations() {
    let home = Home::new();
    let args = [
        "--share-model",
        "synthetic-visible",
        "--share-bind",
        "127.0.0.1",
        "-p",
        "probe",
    ];
    let headless = home.run(&args);
    assert!(!headless.status.success());
    assert!(String::from_utf8_lossy(&headless.stderr).contains("requires an interactive owner session"));
    let agent = home.marked(&args, "OXA_CODING_AGENT", "");
    assert!(!agent.status.success());
    assert!(String::from_utf8_lossy(&agent.stderr).contains("requires the owner outside an agent tool"));
    assert!(!home.0.join(".oxa/share").exists());
}

#[test]
fn owner_terminal_reports_share_status_only_when_enabled() {
    // The same openpty/select harness used for the terminal-loss controls.
    const PTY_STATUS: &str = r#"
import fcntl, os, pty, select, struct, subprocess, sys, termios, time
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 48, 140, 0, 0))
child = subprocess.Popen([
    sys.argv[1],
    *(['--share-model', 'synthetic-model', '--share-bind', '127.0.0.1']
      if sys.argv[2] == 'enabled' else []),
    '--model', 'synthetic-model', '--no-session', '--no-mcp', '--no-extensions',
    '--no-skills', '--no-context-files', '--tui-mode', 'regular',
], stdin=slave, stdout=slave, stderr=slave, close_fds=True)
os.close(slave)
try:
    output = b''
    def wait_for(*needles):
        global output
        deadline = time.monotonic() + 15
        while not all(needle in output for needle in needles):
            missing = [needle for needle in needles if needle not in output]
            assert child.poll() is None, ('interactive share exited early', missing, output[-4000:])
            assert time.monotonic() < deadline, ('interactive share status deadline', missing, output[-4000:])
            if select.select([master], [], [], .1)[0]:
                output += os.read(master, 65536)
    wait_for(b'READY')
    if sys.platform == 'linux':
        with open(f'/proc/{child.pid}/status', encoding='ascii') as status:
            resources = dict(line.split(':', 1) for line in status if ':' in line)
        rss_kib = int(resources['VmRSS'].split()[0])
        threads = int(resources['Threads'])
        assert rss_kib > 0 and threads > 0
    assert b'LAN sharing' not in output and b'Guest usage today' not in output
    os.write(master, b'/engine status\r')
    if sys.argv[2] == 'enabled':
        wait_for(b'LAN sharing', b'0 connections', b'Guest usage today (UTC)',
                 b'Synthetic Tablet', b'1/10 requests', b'7/100 tokens')
    else:
        wait_for(b'Tool call repairs:')
        assert b'LAN sharing' not in output and b'Guest usage today' not in output
    os.write(master, b'/quit\r')
    assert child.wait(timeout=5) == 0, 'owner terminal did not exit normally'
    if sys.platform == 'linux':
        print(f'Oxa share footprint {sys.argv[2]}: binary_bytes={os.stat(sys.argv[1]).st_size} '
              f'idle_rss_kib={rss_kib} idle_threads={threads}')
finally:
    os.close(master)
    if child.poll() is None:
        child.kill()
    child.wait(timeout=3)
"#;

    let env = oxa_fake::TestEnv::new();
    fs::write(
        env.agent_dir().join("settings.json"),
        r#"{"cacheWarming":"off","defaultProjectTrust":"always","oxa":{
            "engine":{"inProcessLocal":true},
            "local":{"binary":"/synthetic/oxe-server",
                "models":{"synthetic-model":{"model":"/synthetic/model.gguf","arguments":["--engine","stub"]}},
                "limits":{"synthetic-model":{"context_window":4096,"max_output":64,
                    "allocation_budget":{"kv_bytes_per_position":1,"scratch_bytes":0,
                                         "mtp_state_bytes":0,"margin_bytes":1044480}}}}
        }}"#,
    )
    .unwrap();
    let measure = |mode: &str| {
        use std::io::Write as _;

        let output = Command::new("python3")
            .args(["-I", "-c", PTY_STATUS, env!("CARGO_BIN_EXE_oxa"), mode])
            .env_clear()
            .envs(env.envs("http://127.0.0.1:9"))
            .env("TERM", "xterm-256color")
            .current_dir(env.work())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // Write only the numeric receipt so both existing hosted test lanes expose it.
        std::io::stdout().write_all(&output.stdout).unwrap();
    };
    let share_dir = env.home().join(".oxa/share");
    measure(if cfg!(feature = "share") {
        "disabled"
    } else {
        "feature-excluded"
    });
    assert!(!share_dir.exists());
    #[cfg(feature = "share")]
    {
        use oxc_gateway::share::keys::{Limits, Store};
        use std::time::{SystemTime, UNIX_EPOCH};

        let keys = Store::initialize(&share_dir).unwrap();
        let key = keys
            .create(
                "Synthetic Tablet",
                Limits {
                    daily_requests: 10,
                    daily_tokens: 100,
                },
            )
            .unwrap();
        let day = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() / 86400;
        keys.admit(key.bearer.expose(), day, 10)
            .unwrap()
            .unwrap()
            .settle(7)
            .unwrap();
        measure("enabled");
    }
}
