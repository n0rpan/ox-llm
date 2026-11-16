//! Lane `search` (O-169 / O-170) end to end: the real `oxa` binary against the fake engine and
//! a loopback mock of the Brave API (`OXA_BRAVE_BASE_URL`). No real call, no real key
//! (`test-key-not-real`); `TestEnv::envs` after `env_clear()`.

#[path = "common/http.rs"]
mod http;
use oxa_fake::{FakeOpts, FakeServer, Recorded, ScriptTurn, TestEnv};
use oxa_proto::Role;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const KEY: &str = "test-key-not-real";
const LIMIT: Duration = Duration::from_secs(60);
const LLM_BODY: &str = r#"{"grounding":{"generic":[{"url":"https://example.com/a","title":"Alpha page","snippets":["alpha snippet one","alpha snippet two"]},{"url":"https://example.com/b","title":"Beta page","snippets":["beta snippet"]}],"poi":null,"map":[]},"sources":{"https://example.com/a":{"title":"Alpha page","hostname":"example.com","age":["Monday, May 6, 2024","2024-05-06","190 days ago"]},"https://example.com/b":{"title":"Beta page","hostname":"example.com","age":[]}}}"#;
const EXPECTED: &str = "Web search results for: rust news\n\n[1] Alpha page\nhttps://example.com/a\nPublished: 2024-05-06\nalpha snippet one\nalpha snippet two\n\n[2] Beta page\nhttps://example.com/b\nbeta snippet";

/// A one-answer mock of the Brave API: every request gets `LLM_BODY`; the request lines and
/// headers are recorded.
struct Brave {
    url: String,
    seen: Arc<Mutex<Vec<String>>>,
}

fn brave() -> Brave {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut head = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            s.lock().unwrap().push(head);
            let mut out = conn;
            let _ = write!(
                out,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                LLM_BODY.len(),
                LLM_BODY
            );
        }
    });
    Brave { url, seen }
}

fn oxa(env: &TestEnv, engine: &str, extra_env: &[(&str, &str)], words: &[&str], stdin: &str) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxa"));
    cmd.args(words)
        .env_clear()
        .envs(http::envs(env, engine))
        .envs(extra_env.iter().map(|(k, v)| (k.to_string(), v.to_string())))
        .current_dir(env.work())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn oxa");
    let mut si = child.stdin.take().unwrap();
    si.write_all(stdin.as_bytes()).unwrap();
    drop(si);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    rx.recv_timeout(LIMIT)
        .unwrap_or_else(|_| panic!("oxa {words:?} did not finish in {LIMIT:?}"))
        .expect("wait")
}

fn tool_names(r: &Recorded) -> Vec<String> {
    let v: Vec<serde_json::Value> = serde_json::from_str(&r.tools_json).unwrap_or_default();
    v.iter()
        .filter_map(|t| {
            t.pointer("/function/name")
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
        .collect()
}

/// One `-p` run with an engine that calls `web_search` once; returns the requests and output.
fn search_run(
    env: &TestEnv,
    b: &Brave,
    extra_env: &[(&str, &str)],
    words: &[&str],
) -> (Vec<Recorded>, Output) {
    let server = FakeServer::start(
        vec![
            ScriptTurn::tool("c1", "web_search", r#"{"query":"rust news"}"#),
            ScriptTurn::text("done"),
        ],
        FakeOpts::default(),
    )
    .expect("fake engine");
    let mut vars = vec![("OXA_BRAVE_BASE_URL", b.url.as_str())];
    vars.extend_from_slice(extra_env);
    let out = oxa(env, &server.url(), &vars, words, "");
    (server.requests(), out)
}

fn tool_result(reqs: &[Recorded], id: &str) -> Option<String> {
    reqs.last()?
        .messages
        .iter()
        .find(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some(id))
        .and_then(|m| m.content.clone())
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            files_under(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn no_file_holds_the_key(root: &Path) {
    let mut files = Vec::new();
    files_under(root, &mut files);
    for f in files {
        let bytes = std::fs::read(&f).unwrap_or_default();
        assert!(
            !bytes.windows(KEY.len()).any(|w| w == KEY.as_bytes()),
            "{} holds the key",
            f.display()
        );
    }
}

#[test]
fn print_mode_calls_web_search_and_the_result_reaches_the_next_request() {
    let env = TestEnv::new();
    let b = brave();
    let (reqs, out) = search_run(&env, &b, &[("BRAVE_SEARCH_API_KEY", KEY)], &["-p", "go"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "done\n");
    assert!(tool_names(&reqs[0]).contains(&"web_search".to_string()));
    assert_eq!(tool_result(&reqs, "c1").as_deref(), Some(EXPECTED));
    let seen = b.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].starts_with("GET /res/v1/llm/context?"), "{}", seen[0]);
    assert!(seen[0]
        .to_ascii_lowercase()
        .contains(&format!("x-subscription-token: {KEY}")));
    // W4 and the secret: the key is in no file of the test tree (session file, logs).
    no_file_holds_the_key(env.root());
    assert!(!stderr.contains(KEY));

    // The HTML export of that session.
    let mut files = Vec::new();
    files_under(&env.agent_dir().join("sessions"), &mut files);
    let session = files
        .iter()
        .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .expect("a session file");
    let html = env.tmp().join("export.html");
    let exp = oxa(
        &env,
        "http://127.0.0.1:1",
        &[],
        &[
            "--export",
            &session.display().to_string(),
            &html.display().to_string(),
        ],
        "",
    );
    assert!(exp.status.success(), "{}", String::from_utf8_lossy(&exp.stderr));
    let page = std::fs::read_to_string(&html).unwrap();
    assert!(!page.contains(KEY));
    // The session data is base64 in `<script id="session-data">`: decode and check it too.
    let tag = r#"<script id="session-data" type="application/json">"#;
    let start = page.find(tag).expect("session data") + tag.len();
    let end = start + page[start..].find("</script>").unwrap();
    let data = String::from_utf8(base64_decode(&page[start..end])).unwrap();
    assert!(data.contains("Alpha page"), "the export holds the search result");
    assert!(!data.contains(KEY));
}

fn base64_decode(s: &str) -> Vec<u8> {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let Some(v) = A.iter().position(|&a| a == c) else {
            continue;
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

#[test]
fn json_mode_event_stream_never_holds_the_key() {
    let env = TestEnv::new();
    let b = brave();
    let (reqs, out) = search_run(
        &env,
        &b,
        &[("BRAVE_SEARCH_API_KEY", KEY)],
        &["--mode", "json", "--no-session", "go"],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("web_search"));
    assert!(!stdout.contains(KEY));
    assert_eq!(tool_result(&reqs, "c1").as_deref(), Some(EXPECTED));
}

#[test]
fn the_key_file_works_and_no_key_means_no_tool() {
    let env = TestEnv::new();
    let b = brave();
    let (reqs, _) = search_run(&env, &b, &[], &["-p", "--no-session", "go"]);
    assert!(
        !tool_names(&reqs[0]).contains(&"web_search".to_string()),
        "no key: not declared"
    );
    assert!(b.seen.lock().unwrap().is_empty());

    let key = env.write("home/.oxa/agent/brave-search.key", &format!("{KEY}\n"));
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let (reqs, out) = search_run(&env, &b, &[], &["-p", "--no-session", "go"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(tool_result(&reqs, "c1").as_deref(), Some(EXPECTED));

    // Group-readable: refused with a startup notice that names the file, never the key.
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    let (reqs, out) = search_run(&env, &b, &[], &["-p", "--no-session", "go"]);
    assert!(!tool_names(&reqs[0]).contains(&"web_search".to_string()));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("brave-search.key refused"), "{stderr}");
    assert!(!stderr.contains(KEY));
}

#[test]
fn every_switch_turns_the_tool_off() {
    let env = TestEnv::new();
    let b = brave();
    let key = ("BRAVE_SEARCH_API_KEY", KEY);
    type Case<'a> = (Vec<(&'a str, &'a str)>, Vec<&'a str>);
    let cases: Vec<Case> = vec![
        (vec![key], vec!["--no-web-search", "-p", "--no-session", "go"]),
        (
            vec![key, ("OXA_WEB_SEARCH", "0")],
            vec!["-p", "--no-session", "go"],
        ),
        (vec![key, ("OXA_OFFLINE", "1")], vec!["-p", "--no-session", "go"]),
        (vec![key], vec!["--offline", "-p", "--no-session", "go"]),
    ];
    for (vars, words) in cases {
        let (reqs, _) = search_run(&env, &b, &vars, &words);
        assert!(
            !tool_names(&reqs[0]).contains(&"web_search".to_string()),
            "{vars:?} {words:?}"
        );
    }
    for settings in [
        r#"{"oxa":{"webSearch":{"enabled":false}}}"#,
        r#"{"oxa":{"webSearch":{"provider":"none"}}}"#,
    ] {
        env.write("home/.oxa/agent/settings.json", settings);
        let (reqs, _) = search_run(&env, &b, &[key], &["-p", "--no-session", "go"]);
        assert!(
            !tool_names(&reqs[0]).contains(&"web_search".to_string()),
            "{settings}"
        );
    }
    assert!(b.seen.lock().unwrap().is_empty(), "nothing reached the provider");
}

#[test]
fn child_processes_do_not_inherit_the_key() {
    let env = TestEnv::new();
    let server = FakeServer::start(vec![], FakeOpts::default()).expect("fake engine");
    // RPC `bash` runs the user's command unsandboxed with the process environment.
    let cmd = r#"{"id":"b","type":"bash","command":"printenv BRAVE_SEARCH_API_KEY; echo end:$?"}"#;
    let out = oxa(
        &env,
        &server.url(),
        &[("BRAVE_SEARCH_API_KEY", KEY)],
        &["--mode", "rpc", "--no-session"],
        &format!("{cmd}\n"),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("end:1"),
        "the variable is unset in the child: {stdout}"
    );
    assert!(!stdout.contains(KEY), "{stdout}");
}

#[test]
fn isolation_refuses_the_real_host_in_the_binary() {
    // No OXA_BRAVE_BASE_URL: the default host; `TestEnv::envs` sets OXA_TEST_ISOLATION=1.
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![
            ScriptTurn::tool("c1", "web_search", r#"{"query":"x"}"#),
            ScriptTurn::text("done"),
        ],
        FakeOpts::default(),
    )
    .expect("fake engine");
    let _ = oxa(
        &env,
        &server.url(),
        &[("BRAVE_SEARCH_API_KEY", KEY)],
        &["-p", "--no-session", "go"],
        "",
    );
    let text = tool_result(&server.requests(), "c1").unwrap_or_default();
    assert!(
        text.starts_with("web_search refused: the real Brave Search host is not allowed"),
        "{text}"
    );
}

#[test]
fn report_issue_scrubs_the_brave_key() {
    let ctx = oxa_cli::ScrubCtx {
        home: None,
        user: None,
        host: None,
    };
    for text in [
        format!("BRAVE_SEARCH_API_KEY={KEY}"),
        format!("export BRAVE_SEARCH_API_KEY=\"{KEY}\""),
        format!("X-Subscription-Token: {KEY}"),
    ] {
        let out = oxa_cli::scrub(&text, &ctx);
        assert!(!out.contains(KEY), "{out}");
        assert!(out.contains("<secret>"), "{out}");
    }
}
