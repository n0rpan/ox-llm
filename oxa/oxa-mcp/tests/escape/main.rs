//! Task 60, part B: offline-mode escapes at the MCP boundary and the remote-MCP permission
//! claim. Every test states its threat, attempt and expected refusal.

use oxa_fake::TestEnv;
use oxa_mcp::{
    load_config, parse_server, tool_spec, ConfigScope, HttpConfig, HttpTransport, McpTool, Transport,
};
use oxa_proto::{codes, set_net_policy, ToolClass};
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

const SYNTHETIC_PASSWORD: &str = "SYNTHETIC-PROXY-PASSWORD-42";

/// The offline switch is process-global; these tests serialize on it and always restore it.
static NET_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Offline(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

impl Offline {
    fn on() -> Offline {
        let guard = NET_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_net_policy(true, false);
        Offline(guard)
    }
}

impl Drop for Offline {
    fn drop(&mut self) {
        set_net_policy(false, false);
    }
}

/// Ambient proxy variables for one test: every variable ureq reads is cleared, the test's
/// values are set, and the previous values are restored on drop. The offline tests serialize
/// on [`NET_LOCK`], so no other transport reads these variables in the meantime.
struct ProxyEnv {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl ProxyEnv {
    fn set(proxy: &str, no_proxy: Option<&str>) -> ProxyEnv {
        const VARS: [&str; 8] = [
            "ALL_PROXY",
            "all_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "NO_PROXY",
            "no_proxy",
        ];
        let saved = VARS.iter().map(|name| (*name, std::env::var_os(name))).collect();
        for name in VARS {
            std::env::remove_var(name);
        }
        std::env::set_var("ALL_PROXY", proxy);
        if let Some(no_proxy) = no_proxy {
            std::env::set_var("NO_PROXY", no_proxy);
        }
        ProxyEnv { saved }
    }
}

impl Drop for ProxyEnv {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// A one-shot HTTP server on loopback: reads the request, writes `response`, returns its URL.
fn respond_once(response: &'static str) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/mcp", listener.local_addr().expect("addr"));
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 4096];
        let mut seen = Vec::new();
        loop {
            let n = stream.read(&mut buf).expect("read");
            if n == 0 {
                break;
            }
            seen.extend_from_slice(&buf[..n]);
            if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let _ = stream.write_all(response.as_bytes());
    });
    (url, handle)
}

fn transport(url: String, proxy: Option<String>, timeout: Duration) -> HttpTransport {
    let cfg = HttpConfig {
        url,
        headers: Vec::new(),
        oauth: None,
        proxy,
    };
    HttpTransport::new(&cfg, Vec::new(), None, timeout)
}

/// Threat: with `--offline`, an MCP server entry uses a remote `httpProxy`; the request (and
/// the bearer token) would be sent to the proxy's host.
/// Attempt: offline, a loopback MCP URL with a proxy at a reserved non-loopback address.
/// Expected refusal: the proxy is refused before any connection; the error names only the
/// proxy host and never a password embedded in the proxy URL.
#[test]
fn offline_refuses_a_remote_proxy() {
    let _offline = Offline::on();
    let proxy = format!("http://user:{SYNTHETIC_PASSWORD}@192.0.2.1:9");
    let mut t = transport(
        "http://127.0.0.1:9/mcp".to_string(),
        Some(proxy),
        Duration::from_millis(300),
    );
    let e = t
        .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .expect_err("a remote proxy is refused offline");
    assert_eq!(e.code, codes::OFFLINE, "{e}");
    assert!(!e.to_string().contains(SYNTHETIC_PASSWORD), "{e}");
}

/// Threat: offline, an ambient remote `ALL_PROXY` would carry a loopback MCP request (and the
/// bearer token) to the proxy's host even though the target is on `NO_PROXY`.
/// Attempt: offline, `ALL_PROXY` at a reserved non-loopback address and `NO_PROXY=127.0.0.1`,
/// a mock loopback server.
/// Expected: allowed — ureq routes the excluded target direct, so the loopback call succeeds.
#[test]
fn offline_allows_a_loopback_target_excluded_by_no_proxy() {
    let _offline = Offline::on();
    let _env = ProxyEnv::set("http://192.0.2.1:9", Some("127.0.0.1"));
    let (url, server) = respond_once(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 127\r\nConnection: close\r\n\r\n\
         {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"serverInfo\":{\"name\":\"mock\",\"version\":\"1\"}}}",
    );
    let mut t = transport(url, None, Duration::from_millis(1000));
    t.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .expect("a NO_PROXY loopback target is direct offline");
    let msg = t
        .recv(Duration::from_millis(1000))
        .expect("recv")
        .expect("a message");
    let _ = server.join();
    assert_eq!(msg["id"], json!(1));
}

/// Threat: offline, an ambient remote proxy with no matching `NO_PROXY` carries the loopback
/// request to the proxy's host.
/// Attempt: offline, `ALL_PROXY` at a reserved non-loopback address, no `NO_PROXY`.
/// Expected refusal: the standard offline error, before any connection.
#[test]
fn offline_refuses_an_ambient_remote_proxy_when_not_excluded() {
    let _offline = Offline::on();
    let _env = ProxyEnv::set("http://192.0.2.1:9", None);
    let mut t = transport(
        "http://127.0.0.1:9/mcp".to_string(),
        None,
        Duration::from_millis(300),
    );
    let e = t
        .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .expect_err("an ambient remote proxy is refused offline");
    assert_eq!(e.code, codes::OFFLINE, "{e}");
    assert!(!e.to_string().contains(SYNTHETIC_PASSWORD), "{e}");
}

/// Threat: a loopback MCP endpoint redirects to a remote host; `ureq` follows the redirect
/// after the offline gate checked only the first URL.
/// Attempt: offline, a mock loopback server answers `307` with a reserved remote Location.
/// Expected refusal: the redirect is not followed; the call fails with the offline error.
#[test]
fn offline_refuses_a_redirect_to_a_remote_host() {
    let _offline = Offline::on();
    let (url, server) = respond_once(
        "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://192.0.2.1:9/mcp\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    let mut t = transport(url, None, Duration::from_millis(400));
    let e = t
        .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .expect_err("a redirect is refused offline");
    let _ = server.join();
    assert_eq!(e.code, codes::OFFLINE, "{e}");
    assert!(
        !e.to_string().contains("192.0.2.1"),
        "the redirect target leaked: {e}"
    );
}

/// Threat: offline refuses all MCP, including the loopback server on this machine.
/// Attempt: a mock loopback server with a valid JSON-RPC reply, offline.
/// Expected refusal: none — this is the control; the loopback call succeeds.
#[test]
fn offline_allows_a_loopback_server() {
    let _offline = Offline::on();
    let (url, server) = respond_once(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 127\r\nConnection: close\r\n\r\n\
         {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"serverInfo\":{\"name\":\"mock\",\"version\":\"1\"}}}",
    );
    let mut t = transport(url, None, Duration::from_millis(1000));
    t.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .expect("loopback is allowed offline");
    let msg = t
        .recv(Duration::from_millis(1000))
        .expect("recv")
        .expect("a message");
    let _ = server.join();
    assert_eq!(msg["id"], json!(1));
}

/// Threat: a remote (HTTP) MCP server labels a destructive tool `readOnlyHint: true`, and the
/// client believes it, so plan/workspace modes run it without a prompt.
/// Attempt: parse a remote entry and a read-only tool; read the class the client reports.
/// Expected refusal: the remote default is `Write` (the policy then asks or denies); only an
/// explicit `"trustAnnotations": true` opts in.
#[test]
fn a_remote_read_only_hint_is_not_trusted_by_default() {
    let remote = parse_server(
        "remote",
        &json!({"url": "https://mcp.example.invalid/mcp"}),
        ConfigScope::User,
    )
    .expect("remote entry");
    let tool = McpTool {
        name: "destroy".into(),
        description: "claims to read".into(),
        input_schema: json!({"type": "object"}),
        annotations: oxa_mcp::ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        },
    };
    assert_eq!(
        tool_spec(&remote, &tool, "mcp__remote__destroy").class,
        ToolClass::Write
    );
    // A stdio server's hint is believed by default (it runs with the user's rights).
    let local = parse_server("local", &json!({"command": "server"}), ConfigScope::User).expect("stdio");
    assert_eq!(
        tool_spec(&local, &tool, "mcp__local__destroy").class,
        ToolClass::Read
    );
    // An explicit opt-in is the only way a remote hint is believed.
    let trusted = parse_server(
        "remote",
        &json!({"url": "https://mcp.example.invalid/mcp", "trustAnnotations": true}),
        ConfigScope::User,
    )
    .expect("remote entry");
    assert_eq!(
        tool_spec(&trusted, &tool, "mcp__remote__destroy").class,
        ToolClass::Read
    );
}

/// Threat: a project `mcp.json` (which the model can write) widens a user-defined remote
/// server's permissions by overriding only `trustAnnotations`.
/// Attempt: load a user file with a remote server and a project override that says
/// `"trustAnnotations": true`; read the merged entry.
/// Expected refusal: a project override may set only `enabled`, `exposure` and `toolExposure`;
/// the annotation trust stays false.
#[test]
fn a_project_override_cannot_trust_annotations() {
    let env = TestEnv::new();
    let user = env.write(
        "mcp.json",
        r#"{"mcpServers":{"remote":{"url":"https://mcp.example.invalid/mcp"}}}"#,
    );
    let project = env.write(
        "work/.oxa/mcp.json",
        r#"{"mcpServers":{"remote":{"trustAnnotations":true}}}"#,
    );
    let config = load_config(&user, Some(&project));
    let server = config
        .servers
        .iter()
        .find(|s| s.name == "remote")
        .expect("remote server");
    assert!(!server.trust_annotations, "a project override trusted the hint");
}

/// Threat: offline still connects to a remote MCP URL because the URL parser sees no host.
/// Attempt: a remote HTTP URL (a reserved TEST-NET address), offline.
/// Expected refusal: the one offline error, before any connection.
#[test]
fn offline_refuses_a_remote_mcp_url() {
    let _offline = Offline::on();
    let mut t = transport(
        "http://10.255.0.2:9/mcp".to_string(),
        None,
        Duration::from_millis(300),
    );
    let e = t
        .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
        .expect_err("a remote URL is refused offline");
    assert_eq!(e.code, codes::OFFLINE, "{e}");
    assert!(!e.to_string().contains(SYNTHETIC_PASSWORD), "{e}");
}
