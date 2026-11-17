//! Drive the real binary over its private proxy socket; no engine or tailnet calls.
use oxa_fake::TestEnv;
use std::{
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn command(env: &TestEnv) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
    cmd.current_dir(env.work())
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"));
    cmd
}
fn start(env: &TestEnv) -> Server {
    let mut server = Server(
        command(env)
            .args(["lead", "serve", "--enable-web"])
            .env("OXA_LEAD_OWNER_LOGIN", "owner@example.invalid")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let began = Instant::now();
    while !env.agent_dir().join("lead-web/http.sock").exists() {
        assert!(server.0.try_wait().unwrap().is_none(), "server exited");
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "server startup timeout"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    server
}
fn fetch(env: &TestEnv, headers: &str, path: &str) -> String {
    let mut socket = UnixStream::connect(env.agent_dir().join("lead-web/http.sock")).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        socket,
        "GET {path} HTTP/1.1\r\nHost: example.invalid\r\n{headers}\r\n"
    )
    .unwrap();
    let mut reply = String::new();
    socket.read_to_string(&mut reply).unwrap();
    reply
}

#[test]
fn owner_only_and_no_request_data_in_pages() {
    let env = TestEnv::new();
    let _server = start(&env);
    for headers in [
        "",
        "Tailscale-User-Login: guest@example.invalid\r\n",
        "Tailscale-User-Login: owner@example.invalid\r\nTailscale-User-Login: owner@example.invalid\r\n",
        "Tailscale-User-Login: OWNER@example.invalid\r\n",
    ] {
        assert!(fetch(&env, headers, "/").starts_with("HTTP/1.1 403"));
    }
    let headers =
        "tailscale-user-login: owner@example.invalid\r\nAuthorization: synthetic-private-marker\r\n";
    let reply = fetch(&env, headers, "/");
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    for private in [
        "synthetic-private-marker",
        "owner@example.invalid",
        "Authorization",
    ] {
        assert!(!reply.contains(private));
    }
    assert!(reply.contains("Cache-Control: no-store"));
    assert!(fetch(&env, headers, "/unknown").starts_with("HTTP/1.1 404"));
    assert_eq!(
        std::fs::metadata(env.agent_dir().join("lead-web"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(env.agent_dir().join("lead-web/http.sock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn opt_in_identity_and_offline_refusals_precede_socket_creation() {
    for (args, owner, offline) in [
        (vec!["--offline", "lead", "serve", "--enable-web"], true, false),
        (vec!["lead", "serve"], true, false),
        (vec!["lead", "serve", "--enable-web"], false, false),
        (vec!["lead", "serve", "--enable-web"], true, true),
    ] {
        let env = TestEnv::new();
        let mut cmd = command(&env);
        cmd.args(args);
        if owner {
            cmd.env("OXA_LEAD_OWNER_LOGIN", "owner@example.invalid");
        }
        if offline {
            cmd.env("OXA_OFFLINE", "1");
        }
        assert!(!cmd.output().unwrap().status.success());
        assert!(!env.agent_dir().join("lead-web").exists());
    }
}

#[test]
fn existing_socket_is_preserved() {
    let env = TestEnv::new();
    let _server = start(&env);
    let result = command(&env)
        .args(["lead", "serve", "--enable-web"])
        .env("OXA_LEAD_OWNER_LOGIN", "owner@example.invalid")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(fetch(&env, "Tailscale-User-Login: owner@example.invalid\r\n", "/").starts_with("HTTP/1.1 200"));
}
