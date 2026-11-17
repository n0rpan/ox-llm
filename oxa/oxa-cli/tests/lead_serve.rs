//! Drive the registered real binary with private local observations and no cloud/GitHub.
use oxa_fake::TestEnv;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    time::Duration,
};

fn command(test: &TestEnv) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command
        .env_clear()
        .envs(test.envs("http://127.0.0.1:1"))
        .current_dir(test.work())
        .stdin(Stdio::null());
    command
}
struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn get(port: u16, path: &str, authority: &str, extra: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: {authority}\r\n{extra}\r\n").unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    reply
}

fn start(test: &TestEnv, sample: &std::path::Path) -> Running {
    let mut child = Running(
        command(test)
            .args(["lead", "serve"])
            .env("OXA_LEAD_BOARD_LOG", sample)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = send.send(line);
    });
    assert!(receive
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .contains("listening"));
    child
}

#[test]
fn registered_help_and_menu_work_without_home_or_runtime_inputs() {
    let test = TestEnv::new();
    for args in [
        vec!["lead"],
        vec!["lead", "serve", "--help"],
        vec!["help", "lead", "serve"],
    ] {
        let output = command(&test).args(args).env_remove("HOME").output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("lead serve"));
    }
    for command_name in ["board", "status", "asks", "chat"] {
        let output = command(&test)
            .args(["lead", command_name])
            .env_remove("HOME")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unavailable"));
    }
}

#[test]
fn real_server_is_user_enabled_authorized_and_rereads_private_board() {
    let test = TestEnv::new();
    test.write(
        "work/.oxa/settings.json",
        r#"{"oxa":{"lead":{"serve":{"enabled":true}}}}"#,
    );
    let disabled = command(&test).args(["lead", "serve"]).output().unwrap();
    assert!(!disabled.status.success());
    assert!(String::from_utf8_lossy(&disabled.stderr).contains("disabled in user settings"));
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    test.write(
        "home/.oxa/agent/settings.json",
        &serde_json::json!({"oxa":{"lead":{"serve":{"enabled":true,"port":port}}}}).to_string(),
    );
    for (args, input, expected) in [
        (vec!["--offline", "lead", "serve"], "", "offline"),
        (
            vec!["lead", "serve"],
            "relative-private-marker",
            "must be absolute",
        ),
    ] {
        let output = command(&test)
            .args(args)
            .env("OXA_LEAD_BOARD_LOG", input)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
    }
    let directory = test.root().join("board-input");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let sample = directory.join("board.jsonl");
    std::fs::write(
        &sample,
        b"{\"schema_version\":1,\"observed_at\":10,\"board\":{\"observed_at\":10,\"entries\":[]}}\n",
    )
    .unwrap();
    std::fs::set_permissions(&sample, std::fs::Permissions::from_mode(0o600)).unwrap();
    drop(reservation);
    let child = start(&test, &sample);
    let authority = format!("127.0.0.1:{port}");
    let page = get(port, "/", &authority, "");
    assert!(page.starts_with("HTTP/1.1 200"));
    assert!(page.contains("No open PRs in this snapshot"));
    assert!(get(port, "/", "attacker.invalid", "").starts_with("HTTP/1.1 403"));
    assert!(get(
        port,
        "/",
        &authority,
        "Tailscale-User-Login: owner@example.invalid\r\n"
    )
    .starts_with("HTTP/1.1 403"));
    std::fs::set_permissions(&sample, std::fs::Permissions::from_mode(0o644)).unwrap();
    let event = get(port, "/events", &authority, "");
    assert!(event.contains("event: snapshot\ndata: <main"));
    assert!(event.contains("Board unavailable"));
    assert!(!event.contains(sample.to_str().unwrap()));
    drop(child);
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::metadata(test.root()).unwrap().uid();
    let proxy = |uid| {
        serde_json::json!({"oxa":{"lead":{"serve":{
            "enabled":true,"port":port,"identity":"tailscale",
            "allowed_identities":["owner@example.invalid"],
            "proxy":{"uid":uid,"authority":"lead.example.invalid"}
        }}}})
        .to_string()
    };
    test.write("home/.oxa/agent/settings.json", &proxy(uid));
    let child = start(&test, &sample);
    let login = "Tailscale-User-Login: owner@example.invalid\r\n";
    assert!(get(port, "/", "lead.example.invalid", login).starts_with("HTTP/1.1 200"));
    for (host, header) in [
        ("attacker.invalid", login),
        ("lead.example.invalid", ""),
        (
            "lead.example.invalid",
            "Tailscale-User-Login: guest@example.invalid\r\n",
        ),
    ] {
        assert!(get(port, "/", host, header).starts_with("HTTP/1.1 403"));
    }
    drop(child);
    test.write("home/.oxa/agent/settings.json", &proxy(uid ^ 1));
    let _child = start(&test, &sample);
    assert!(get(port, "/", "lead.example.invalid", login).starts_with("HTTP/1.1 403"));
}
