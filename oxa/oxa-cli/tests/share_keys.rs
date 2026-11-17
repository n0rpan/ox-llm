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
    assert!(String::from_utf8_lossy(&offline.stderr).contains("offline"));
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
fn foreground_serve_lists_only_the_owner_selected_local_models_on_loopback() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::process::{Child, Stdio};
    use std::time::Duration;

    struct Serving(Child);
    impl Drop for Serving {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

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
    let bearer = key["api_key"].as_str().unwrap();

    let mut serving = Serving(
        Command::new(env!("CARGO_BIN_EXE_oxa"))
            .args([
                "share",
                "serve",
                "--model",
                "synthetic-visible",
                "--bind",
                "127.0.0.1",
            ])
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
    let address: SocketAddr = line
        .trim()
        .strip_prefix("Guest OpenAI API listening at http://")
        .unwrap()
        .strip_suffix("/v1")
        .unwrap()
        .parse()
        .unwrap();
    assert!(address.ip().is_loopback());

    let mut client = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    write!(
        client,
        "GET /v1/models HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {bearer}\r\n\r\n"
    )
    .unwrap();
    let mut reply = String::new();
    client.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 200"));
    assert!(reply.contains("synthetic-visible"));
    assert!(!reply.contains("synthetic-hidden"));
    assert!(!reply.contains(bearer));
}
