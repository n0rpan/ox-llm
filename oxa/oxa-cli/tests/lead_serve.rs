//! Drive the registered real binary with private local observations and no cloud/GitHub.
use oxa_fake::TestEnv;
use oxa_session::SessionStore;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::{MetadataExt, PermissionsExt},
    process::{Child, Command, Stdio},
    time::Duration,
};

fn command(test: &TestEnv) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command
        .env_clear()
        .envs(test.envs("http://127.0.0.1:1"))
        .env("TZ", "UTC0")
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
thread_local! {
    static AGENT: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}
/// The bearer header of the local client token the running lead wrote for its owner.
/// A local lead publishes the token before it reports its listener; the brief wait covers a
/// read that races the atomic replace. Every test here records the agent dir it started.
fn bearer() -> String {
    let dir = AGENT.with(|agent| {
        agent
            .borrow()
            .clone()
            .expect("the lead start recorded its agent dir")
    });
    for _ in 0..200 {
        if let Ok(token) = std::fs::read_to_string(dir.join("lead-client-token")) {
            return format!("Authorization: Bearer {}\r\n", token.trim());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("the running lead published no client token")
}
fn get(port: u16, path: &str, authority: &str, extra: &str) -> String {
    get_raw(port, path, authority, &format!("{}{extra}", bearer()))
}
fn get_raw(port: u16, path: &str, authority: &str, extra: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: {authority}\r\n{extra}\r\n").unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    reply
}

fn start(test: &TestEnv, sample: &std::path::Path) -> Running {
    start_with_situation(test, sample, None)
}
fn start_with_situation(
    test: &TestEnv,
    sample: &std::path::Path,
    situation: Option<&std::path::Path>,
) -> Running {
    AGENT.with(|agent| *agent.borrow_mut() = Some(test.agent_dir()));
    let mut invocation = command(test);
    if let Some(path) = situation {
        invocation.env("OXA_LEAD_SITUATION", path);
    }
    let mut child = Running(
        invocation
            .args(["lead", "serve"])
            .env("OXA_LEAD_BOARD_LOG", sample)
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(test.root().join("lead.stderr")).unwrap())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    // Keep reading until the child exits. After the listener line a local lead prints the URL
    // that carries the client token; a closed read end fails that write and the lead exits
    // before it serves anything.
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if send.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let line = receive
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .expect("lead serve did not report its listener");
        if line.contains("listening") {
            break;
        }
    }
    child
}

fn post(port: u16, path: &str, host: &str, origin: &str, extra: &str, body: &str) -> String {
    post_raw(port, path, host, origin, &format!("{}{extra}", bearer()), body)
}
fn post_raw(port: u16, path: &str, host: &str, origin: &str, extra: &str, body: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(stream, "POST {path} HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n{extra}\r\n{body}", body.len()).unwrap();
    // Refusals may close with the rejected request body still unread.
    let mut input = BufReader::new(stream);
    let mut response = String::new();
    while !response.ends_with("\r\n\r\n") {
        assert!(input.read_line(&mut response).unwrap() > 0);
    }
    let length: usize = response
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .unwrap()
        .parse()
        .unwrap();
    let mut body = vec![0; length];
    input.read_exact(&mut body).unwrap();
    response.push_str(std::str::from_utf8(&body).unwrap());
    response
}

fn form_nonce(page: &str) -> &str {
    page.split("name=csrf value=\"")
        .nth(1)
        .expect("no-JS form must be present")
        .split('"')
        .next()
        .unwrap()
}

fn with_fixed_chat(run: impl FnOnce(u16, &str, &std::path::Path)) {
    let test = TestEnv::new();
    // `start_with_situation` names the agent dir the token went to.
    std::fs::set_permissions(test.agent_dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    test.write(
        "home/.oxa/agent/settings.json",
        &serde_json::json!({"oxa":{"lead":{"serve":{
            "enabled":true,"fixed_prompts":true,"port":port
        }}}})
        .to_string(),
    );
    let directory = test.root().join("inputs");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let board = directory.join("board.jsonl");
    std::fs::write(
        &board,
        b"{\"schema_version\":1,\"observed_at\":10,\"board\":{\"observed_at\":10,\"entries\":[]}}\n",
    )
    .unwrap();
    let situation = directory.join("situation.json");
    let snapshot = include_str!("../../oxa-proto/tests/fixtures/work/evening.jsonl")
        .lines()
        .find_map(|line| oxa_proto::work::Snapshot::from_json(line.as_bytes()).ok())
        .unwrap();
    std::fs::write(&situation, snapshot.to_json().unwrap()).unwrap();
    for path in [&board, &situation] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    drop(reservation);
    // `start_with_situation` names the agent dir the token went to.
    let _server = start_with_situation(&test, &board, Some(&situation));
    let authority = format!("127.0.0.1:{port}");
    run(port, &authority, &test.root().join("lead.stderr"));
}

#[test]
fn fixed_chat_form_posts_without_js_and_reuses_protected_observations() {
    with_fixed_chat(|port, authority, _agent_dir| {
        let page = get(port, "/chat", authority, "");
        let nonce = form_nonce(&page);
        assert_eq!(nonce.len(), 43);
        assert!(page.contains("method=post action=/chat"));
        assert!(page.contains("form-action 'self'"));
        let origin = format!("http://{authority}");
        for prompt in ["status", "workers", "attention"] {
            let reply = post(
                port,
                "/chat",
                authority,
                &origin,
                "",
                &format!("csrf={nonce}&prompt={prompt}"),
            );
            assert_eq!(reply.lines().next(), Some("HTTP/1.1 303 See Other"));
            let target = reply
                .lines()
                .find_map(|line| line.strip_prefix("Location: "))
                .unwrap();
            assert_eq!(target, format!("/chat?prompt={prompt}"));
            let page = get(port, target, authority, "");
            assert!(page.contains("<!doctype html>") && page.contains("0 tokens"));
            let fact = if prompt == "status" {
                "0 items"
            } else {
                "Snapshot observations unavailable"
            };
            assert!(page.contains(fact), "{prompt}");
            assert!(get(port, &format!("{target}&fragment=1"), authority, "").contains(fact));
        }
        let refused = |path, host, source, extra, body: &str| {
            let reply = post(port, path, host, source, extra, body);
            assert!(reply.starts_with("HTTP/1.1 4"));
            assert!(!reply.contains("private-input-marker") && !reply.contains("0 tokens"));
        };
        let valid = format!("csrf={nonce}&prompt=status");
        refused("/chat", authority, "https://attacker.invalid", "", &valid);
        refused("/chat", "attacker.invalid", &origin, "", &valid);
        for body in [
            "csrf=wrong&prompt=status".into(),
            format!("{valid}&prompt=workers"),
            format!("csrf={nonce}&prompt=private-input-marker"),
            "x".repeat(257),
        ] {
            refused("/chat", authority, &origin, "", &body);
        }
        for extra in [
            "Origin: https://attacker.invalid\r\n",
            "Transfer-Encoding: chunked\r\n",
        ] {
            refused("/chat", authority, &origin, extra, &valid);
        }
        refused("/workers", authority, &origin, "", &valid);
    });
}

#[test]
fn fixed_form_response_preserves_browser_origin() {
    with_fixed_chat(|port, authority, _agent_dir| {
        assert!(get(port, "/chat", authority, "").contains("Referrer-Policy: same-origin\r\n"));
    });
}

#[test]
fn fixed_post_redirects_to_the_selected_refresh_url() {
    with_fixed_chat(|port, authority, _agent_dir| {
        let page = get(port, "/chat", authority, "");
        let nonce = form_nonce(&page);
        for prompt in ["status", "workers", "attention"] {
            let reply = post(
                port,
                "/chat",
                authority,
                &format!("http://{authority}"),
                "",
                &format!("csrf={nonce}&prompt={prompt}"),
            );
            assert_eq!(reply.lines().next(), Some("HTTP/1.1 303 See Other"));
            assert!(reply.contains(&format!("Location: /chat?prompt={prompt}\r\n")));
        }
    });
}

#[test]
fn same_uid_client_cannot_read_protected_situation() {
    with_fixed_chat(|port, authority, _agent_dir| {
        assert!(get(port, "/asks", authority, "").contains("Owner asks unavailable"));
    });
}

#[test]
fn same_uid_client_needs_the_minted_token_for_pages_and_posts() {
    with_fixed_chat(|port, authority, stderr| {
        let dir = AGENT.with(|dir| dir.borrow().clone().unwrap());
        let file = dir.join("lead-client-token");
        assert_eq!(std::fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
        let token = std::fs::read_to_string(&file).unwrap();
        // Without the token neither a page nor a form submission is served.
        assert!(get_raw(port, "/", authority, "").starts_with("HTTP/1.1 401"));
        assert!(get_raw(port, "/", authority, "Authorization: Bearer wrong\r\n").starts_with("HTTP/1.1 401"));
        let nonce_page = get(port, "/chat", authority, "");
        let nonce = form_nonce(&nonce_page);
        let origin = format!("http://{authority}");
        let body = format!("csrf={nonce}&prompt=status");
        let denied = post_raw(port, "/chat", authority, &origin, "", &body);
        assert!(denied.starts_with("HTTP/1.1 401"), "{denied}");
        // The printed URL exchanges the token for a cookie and redirects without the query.
        let opened = get_raw(port, &format!("/chat?t={token}"), authority, "");
        assert!(opened.starts_with("HTTP/1.1 303"), "{opened}");
        assert!(opened.contains("Location: /chat\r\n"), "{opened}");
        assert!(opened.contains(&format!(
            "Set-Cookie: oxa_lead={token}; HttpOnly; SameSite=Strict; Path=/"
        )));
        let cookie = format!("Cookie: oxa_lead={token}\r\n");
        assert!(get_raw(port, "/chat", authority, &cookie).starts_with("HTTP/1.1 200"));
        let accepted = post_raw(port, "/chat", authority, &origin, &cookie, &body);
        assert!(accepted.starts_with("HTTP/1.1 303"), "{accepted}");
        // No token bytes reach the service journal.
        let journal = std::fs::read_to_string(stderr).unwrap();
        assert!(!journal.contains(token.trim()), "{journal}");
        assert!(journal.contains("reason=client_token"), "{journal}");
    });
}

#[test]
fn registered_help_works_without_home_or_runtime_inputs() {
    let test = TestEnv::new();
    for args in [vec!["lead", "serve", "--help"], vec!["help", "lead", "serve"]] {
        let output = command(&test).args(args).env_remove("HOME").output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("lead serve"));
    }
    for command_name in ["board", "status", "asks", "chat", "run"] {
        let output = command(&test)
            .args(["lead", command_name])
            .env_remove("HOME")
            .output()
            .unwrap();
        assert!(!output.status.success());
        let diagnostic = match command_name {
            "asks" | "run" => "HOME is not set",
            _ => "unavailable",
        };
        assert!(String::from_utf8_lossy(&output.stderr).contains(diagnostic));
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
    assert!(!page.contains("method=post"));
    for (path, title) in [
        ("/", "Status"),
        ("/asks", "Asks"),
        ("/board", "Board"),
        ("/workers", "Workers"),
        ("/usage", "Usage"),
        ("/chat", "Chat with lead"),
    ] {
        for fragment in [false, true] {
            let path = if fragment {
                format!("{path}?fragment=1")
            } else {
                path.into()
            };
            let response = get(port, &path, &authority, "");
            assert!(
                response.starts_with("HTTP/1.1 200"),
                "{path}: {}",
                response.lines().next().unwrap_or("<empty response>")
            );
            assert!(response.contains(&format!(">{title}</h1>")), "{path}");
            assert_eq!(response.contains("<!doctype html>"), !fragment, "{path}");
        }
        assert!(get(port, path, "attacker.invalid", "").starts_with("HTTP/1.1 403"));
    }
    for prompt in ["status", "workers", "attention"] {
        for suffix in ["", "&fragment=1"] {
            let path = format!("/chat?prompt={prompt}{suffix}");
            let reply = get(port, &path, &authority, "");
            assert!(reply.starts_with("HTTP/1.1 200"), "{path}");
            assert!(reply.contains("0 tokens") && reply.contains("role=log"));
            if prompt == "status" {
                assert!(
                    reply.contains("0 items")
                        && reply.contains("href=\"/board\"")
                        && reply.contains("Board")
                        && reply.contains("observed 1970-01-01 00:00 UTC"),
                    "{reply}"
                );
            }
            assert_eq!(reply.contains("<!doctype html>"), suffix.is_empty());
            assert!(get(port, &path, "attacker.invalid", "").starts_with("HTTP/1.1 403"));
        }
    }
    assert!(get(port, "/chat?prompt=unknown", &authority, "").starts_with("HTTP/1.1 404"));
    let entries: Vec<_> = (1..=101)
        .map(|number| {
            serde_json::json!({
                "number":number,"head":"synthetic-head","release_pinned":false,
                "action":"ready","lease":null,"attempts_today":0,"eligible":true
            })
        })
        .collect();
    let record = serde_json::json!({"schema_version":1,"observed_at":10,
        "board":{"observed_at":10,"entries":entries}});
    std::fs::write(&sample, format!("{record}\n")).unwrap();
    for (path, first, last, absent) in [
        ("/board", 1, 50, 51),
        ("/board?page=1", 51, 100, 1),
        ("/board?page=2", 101, 101, 100),
    ] {
        let response = get(port, path, &authority, "");
        assert!(response.starts_with("HTTP/1.1 200"), "{path}");
        for number in [first, last] {
            assert!(response.contains(&format!("#{number}<")), "{path} #{number}");
        }
        assert!(!response.contains(&format!("#{absent}<")), "{path} #{absent}");
        let separator = if path.contains('?') { '&' } else { '?' };
        let fragment = get(port, &format!("{path}{separator}fragment=1"), &authority, "");
        assert!(fragment.starts_with("HTTP/1.1 200") && !fragment.contains("<!doctype"));
        assert!(get(port, path, "attacker.invalid", "").starts_with("HTTP/1.1 403"));
    }
    for page in ["-1", "01", "18446744073709551615"] {
        assert!(get(port, &format!("/board?page={page}"), &authority, "").starts_with("HTTP/1.1 404"));
    }
    let mut filtered = record.clone();
    for (index, row) in filtered["board"]["entries"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .enumerate()
    {
        row["action"] = if index < 60 {
            "fix_findings"
        } else if index < 100 {
            "wait_review"
        } else {
            "ready"
        }
        .into();
    }
    std::fs::write(&sample, format!("{filtered}\n")).unwrap();
    for (query, first, last, absent) in [
        ("f=fix", 1, 50, 51),
        ("f=fix&page=1", 51, 60, 61),
        ("f=waiting", 61, 100, 1),
        ("f=ready", 101, 101, 100),
    ] {
        let path = format!("/board?{query}");
        let reply = get(port, &path, &authority, "");
        assert!(reply.starts_with("HTTP/1.1 200"), "{path}");
        for text in ["Needs fix (60)", "Waiting (40)", "Ready (1)"] {
            assert!(reply.contains(text), "{text}");
        }
        for number in [first, last] {
            assert!(reply.contains(&format!("#{number}<")), "{path}");
        }
        assert!(!reply.contains(&format!("#{absent}<")), "{path}");
        let fragment = get(port, &format!("{path}&fragment=1"), &authority, "");
        assert!(fragment.starts_with("HTTP/1.1 200") && !fragment.contains("<!doctype"));
        assert!(get(port, &path, "attacker.invalid", "").starts_with("HTTP/1.1 403"));
    }
    assert!(get(port, "/board?f=blocked", &authority, "").contains("Blocked observations unavailable"));
    for query in [
        "f=unknown",
        "f=fix&f=ready",
        "page=1&page=2",
        "f=fix&other=x",
        "f=fix&page=01",
    ] {
        assert!(
            get(port, &format!("/board?{query}"), &authority, "").starts_with("HTTP/1.1 404"),
            "{query}"
        );
    }
    let record = serde_json::json!({"schema_version":1,"observed_at":10,"board":{
        "observed_at":10,"entries":[{"number":17,"head":"private-head-marker",
        "release_pinned":true,"action":"fix_findings","lease":null,
        "attempts_today":2,"eligible":false}]}});
    std::fs::write(&sample, format!("{record}\n")).unwrap();
    assert!(get(port, "/board", &authority, "").contains("href=/items/17"));
    for path in ["/items/17", "/items/17?fragment=1"] {
        let response = get(port, path, &authority, "");
        assert_eq!(response.lines().next(), Some("HTTP/1.1 200 OK"), "{path}");
        for text in [
            "Item #17",
            "Fix findings",
            "Release",
            "Unleased",
            "2",
            "aria-current=page>Board",
        ] {
            assert!(response.contains(text), "{text}");
        }
        assert!(!response.contains("private-head-marker"));
        assert!(get(port, path, "attacker.invalid", "").starts_with("HTTP/1.1 403"));
    }
    assert!(get(port, "/items/18", &authority, "").contains("Item unavailable"));
    for path in [
        "/items/0",
        "/items/01",
        "/items/-1",
        "/items/18446744073709551616",
        "/items/17/x",
    ] {
        assert!(
            get(port, path, &authority, "").starts_with("HTTP/1.1 404"),
            "{path}"
        );
    }
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
    assert!(event.contains("event: snapshot\ndata: <main>"));
    // #2342: a failed read keeps the last good board and names the allow-listed
    // cause; it never blanks the page nor reflects the path.
    assert!(event.contains("Board is last-good"));
    assert!(event.contains("not readable by this owner"));
    assert!(event.contains("href=/items/17"));
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

#[test]
fn refused_chat_posts_record_allowlisted_facts_in_the_real_service_journal() {
    with_fixed_chat(|port, authority, stderr| {
        let page = get(port, "/chat", authority, "");
        let nonce = form_nonce(&page);
        let origin = format!("http://{authority}");
        let valid = format!("csrf={nonce}&prompt=status");
        // Each refusal is one chat fact: identity, origin, CSRF and body.
        for (host, source, extra, body) in [
            ("attacker.invalid", origin.as_str(), "", valid.as_str()),
            (authority, "https://attacker.invalid", "", valid.as_str()),
            (authority, origin.as_str(), "", "csrf=wrong&prompt=status"),
            (authority, origin.as_str(), "", &format!("{valid}&extra=1")),
        ] {
            let reply = post(port, "/chat", host, source, extra, body);
            assert!(reply.starts_with("HTTP/1.1 403"), "{host} {body}: {reply}");
        }
        // A wrong-method chat request is also a chat refusal.
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(stream, "DELETE /chat HTTP/1.1\r\nHost: {authority}\r\n\r\n").unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 405"), "{reply}");

        let journal = std::fs::read_to_string(stderr).unwrap();
        let lines: Vec<&str> = journal
            .lines()
            .filter_map(|line| line.strip_prefix("lead chat refusal "))
            .collect();
        assert_eq!(lines.len(), 5, "{journal}");
        for (line, expected) in lines.iter().zip([
            "status=403 stage=identity reason=not_allowed fix=",
            "status=403 stage=origin reason=cross_origin fix=",
            "status=403 stage=csrf reason=invalid fix=",
            "status=403 stage=body reason=late_parse fix=",
            "status=405 stage=body reason=method fix=",
        ]) {
            assert!(
                line.starts_with(expected) && line.len() > expected.len(),
                "{line}"
            );
        }
        // No payload, CSRF, identity or authority bytes ever reach the journal.
        assert!(!journal.contains(nonce));
        assert!(!journal.contains("csrf=wrong"));
        assert!(!journal.contains("attacker.invalid"));
        assert!(!journal.contains("prompt"));
        assert!(!journal.contains(authority));
    });
}

/// One protected stuck-work record for the Lead view: panels (main checks,
/// verified merge, queue membership), landing (queue age at the exact head) and
/// queued-run pages, each independently optional (#1391).
fn stuck_record(
    now: u64,
    main_red: bool,
    merged_seconds_before: u64,
    queued_prs: Option<usize>,
    overdue: bool,
    runs: Option<bool>,
) -> serde_json::Value {
    let merged = (now.saturating_sub(merged_seconds_before)) * 1000;
    let merged = format!("{}Z", &oxa_session::iso_from_ms(merged)[..19]);
    let head = "a".repeat(40);
    let entries = [serde_json::json!({
        "number":17,"head":head,"release_pinned":false,"action":"ready",
        "lease":null,"attempts_today":0,"eligible":true
    })];
    let merge_queue = match queued_prs {
        None => serde_json::Value::Null,
        Some(count) => serde_json::json!({"entries":{"totalCount":count,"nodes":(0..count)
            .map(|_| serde_json::json!({"pullRequest":{"number":17,"headRefOid":head}}))
            .collect::<Vec<_>>()}}),
    };
    let mut record = serde_json::json!({
        "schema_version":1,"observed_at":now,
        "board":{"observed_at":now,"entries":entries},
        "panels":{
            "id":"example-repo",
            "main":{
                "refUpdateRule":{"requiredStatusCheckContexts":["gate"]},
                "rules":{"totalCount":0,"nodes":[]},
                "target":{
                    "oid":head,
                    "statusCheckRollup":{"contexts":{"pageInfo":{"hasNextPage":false},"nodes":[
                        {"name":"gate","status":"COMPLETED",
                         "conclusion": if main_red { "FAILURE" } else { "SUCCESS" }}
                    ]}},
                    "associatedPullRequests":{"pageInfo":{"hasNextPage":false},"nodes":[
                        {"repository":{"id":"example-repo"},"baseRefName":"main",
                         "mergeCommit":{"oid":head},"mergedAt":merged}
                    ]}
                }
            },
            "mergeQueue":merge_queue
        }
    });
    if overdue {
        record["landing"] = serde_json::json!({
            "17":{"next":"already_queued","queue_alarm":true,
                  "queue":{"enqueued_at":0,"group_head":null}}
        });
    } else {
        record["landing"] = serde_json::json!({
            "17":{"next":"already_queued","queue_alarm":false,
                  "queue":{"enqueued_at":now,"group_head":null}}
        });
    }
    if let Some(stalled) = runs {
        let mut rows = Vec::new();
        if stalled {
            let updated = (now.saturating_sub(3600)) * 1000;
            let updated = format!("{}Z", &oxa_session::iso_from_ms(updated)[..19]);
            rows.push(serde_json::json!({"id":29,"status":"queued","updated_at":updated}));
        }
        record["runs"] = serde_json::json!([{"total_count":rows.len(),"workflow_runs":rows}]);
    }
    record
}

fn write_record(directory: &std::path::Path, record: &serde_json::Value) -> std::path::PathBuf {
    let sample = directory.join("board.jsonl");
    std::fs::write(&sample, format!("{record}\n")).unwrap();
    std::fs::set_permissions(&sample, std::fs::Permissions::from_mode(0o600)).unwrap();
    sample
}

#[test]
fn protected_view_shows_every_stuck_signal_and_keeps_missing_sources_explicit() {
    let test = TestEnv::new();
    std::fs::set_permissions(test.agent_dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    test.write(
        "home/.oxa/agent/settings.json",
        &serde_json::json!({"oxa":{"lead":{"serve":{"enabled":true,"port":port}}}}).to_string(),
    );
    let directory = test.root().join("alarm-inputs");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let now = oxa_proto::time::now_ms() / 1000;
    let authority = format!("127.0.0.1:{port}");
    drop(reservation);

    // One active record carries every signal: main red, the 45-minute no-merge,
    // the over-age queue membership and a stalled queued run.
    let sample = write_record(
        &directory,
        &stuck_record(now, true, 46 * 60, Some(1), true, Some(true)),
    );
    let server = start(&test, &sample);
    let page = get(port, "/", &authority, "");
    for expected in [
        "role=alert",
        "Checks on main are failing.",
        "No PR merged into main for 45 minutes or more at the last observation.",
        "PR #17 has waited too long in the merge queue",
        "Workflow runs queued for more than 15 minutes (1)",
        "Run 29",
    ] {
        assert!(page.contains(expected), "{expected}");
    }
    drop(server);

    // A partial observation keeps the active alarms and names the missing source.
    let sample = write_record(&directory, &stuck_record(now, true, 46 * 60, Some(1), true, None));
    let server = start(&test, &sample);
    let page = get(port, "/", &authority, "");
    assert!(page.contains("Checks on main are failing."));
    assert!(page.contains("PR #17 has waited too long"));
    assert!(page.contains("Not in this sample:"), "{page}");
    assert!(page.contains("queued workflow runs"), "{page}");
    drop(server);

    // An absent panels record leaves its signals explicitly unavailable: the
    // landing age alone cannot claim queue membership it never observed. #2342:
    // the five missing sources collapse into one line naming them.
    let mut partial = stuck_record(now, false, 10 * 60, None, true, None);
    partial.as_object_mut().unwrap().remove("panels");
    let sample = write_record(&directory, &partial);
    let server = start(&test, &sample);
    let page = get(port, "/", &authority, "");
    for expected in [
        "main checks",
        "no-merge alarm",
        "merge queue membership",
        "queued workflow runs",
        "role=status",
    ] {
        assert!(page.contains(expected), "{expected}");
    }
    assert_eq!(page.matches("Not in this sample:").count(), 1, "{page}");
    assert!(!page.contains("waited too long"), "{page}");
    assert!(!page.contains("role=alert"), "{page}");
    drop(server);

    // A routine healthy read stays quiet: no attention section at all.
    let sample = write_record(
        &directory,
        &stuck_record(now, false, 10 * 60, Some(0), false, Some(false)),
    );
    let _server = start(&test, &sample);
    let page = get(port, "/", &authority, "");
    assert!(!page.contains("id=attention"), "{page}");
    assert!(page.contains("Queue empty at observation"), "{page}");
}

#[test]
fn authorized_durable_asks_reach_the_protected_view_as_a_live_readback() {
    // The one authenticated live readback (#1391): an ask the Lead loop wrote to
    // the durable Session is read live by the protected view through the
    // registered owner-bound caller, with its original revision.
    let test = TestEnv::new();
    let dirs = oxa_config::Dirs::under(test.root());
    std::fs::set_permissions(&dirs.agent_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let uid = std::fs::metadata(test.root()).unwrap().uid();
    let project = test.work().canonicalize().unwrap();
    let id = oxa_cli::LeadOwnerBinding::create(&dirs, &project, uid).unwrap();
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    let authority = "lead.example.invalid";
    let settings = test.write(
        "home/.oxa/agent/settings.json",
        &serde_json::json!({"oxa":{"lead":{
            "sessionId":id,
            "serve":{"enabled":true,"port":port,"identity":"tailscale","owner_chat":true,
                "allowed_identities":["owner@example.invalid"],
                "proxy":{"uid":uid,"authority":authority}}
        }}})
        .to_string(),
    );
    std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o600)).unwrap();
    let directory = test.root().join("live-inputs");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let now = oxa_proto::time::now_ms() / 1000;
    let sample = write_record(&directory, &stuck_record(now, true, 46 * 60, None, false, None));
    drop(reservation);

    // The Lead loop escalates the red unowned PR into the durable Session.
    let looped = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .env_clear()
        .envs(test.envs("http://127.0.0.1:1"))
        .current_dir(&project)
        .env("OXA_LEAD_BOARD_LOG", &sample)
        .args(["--offline", "lead", "run", "--once"])
        .output()
        .unwrap();
    assert!(looped.status.success(), "{looped:?}");
    let state: serde_json::Value = serde_json::from_slice(&looped.stdout).unwrap();
    assert_eq!(state["state"], "stuck_escalated", "{state}");
    let ask_id = state["stuck"]["escalated"][0].as_str().unwrap().to_owned();

    let mut invocation = command(&test);
    invocation
        .args(["lead", "serve"])
        .env("OXA_LEAD_BOARD_LOG", &sample)
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(test.root().join("lead-live.stderr")).unwrap());
    let mut child = Running(invocation.spawn().unwrap());
    let stdout = child.0.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    // Drain the whole start output, as `start_with_situation` does, so the URL line finds a
    // reader even though this test starts the service itself.
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if send.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let line = receive
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .expect("lead serve did not report its listener");
        if line.contains("listening") {
            break;
        }
    }

    // A proxy-mode lead mints no client token: its proof is the forwarded login.
    let login = "Tailscale-User-Login: owner@example.invalid\r\n";
    let page = get_raw(port, "/", authority, login);
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert!(page.contains("Checks on main are failing."), "{page}");
    assert!(page.contains("Unanswered owner decisions (1)"), "{page}");
    let asks = get_raw(port, "/asks", authority, login);
    assert!(asks.contains(&ask_id), "{asks}");
    assert!(asks.contains("revision"), "{asks}");
    assert!(
        asks.contains("read live from the authorized durable Session"),
        "{asks}"
    );
    // The runner situation stays a separate source: its absence never hides the
    // durable asks and never claims they were observed empty.
    assert!(asks.contains("Runner asks unavailable."), "{asks}");
    // The durable ask never reaches an unauthorized peer.
    let refused = get_raw(
        port,
        "/",
        authority,
        "Tailscale-User-Login: guest@example.invalid\r\n",
    );
    assert!(refused.starts_with("HTTP/1.1 403"));
    drop(child);
}

// ---------------------------------------------------------------------------------------------
// Synthetic lead frames (#2349). Six native fixture frames under tests/fixtures/lead/ render
// through the real routes: healthy, needs_you, alarm, stale, partial and first_run. The files
// hold the native wire formats (one Reader sample record, one runner situation document) at a
// synthetic reference epoch; `with_frame` rebases their timestamps so each frame renders the
// age it declares. Assertions are behavioural: rows survive staleness, every gap is explicit,
// an absent source never reads as healthy or alarming, and no frame fabricates data.
// ---------------------------------------------------------------------------------------------

fn fixture(name: &str) -> serde_json::Value {
    let text = match name {
        "healthy" => include_str!("fixtures/lead/healthy.json"),
        "needs_you" => include_str!("fixtures/lead/needs_you.json"),
        "alarm" => include_str!("fixtures/lead/alarm.json"),
        "stale" => include_str!("fixtures/lead/stale.json"),
        "partial" => include_str!("fixtures/lead/partial.json"),
        "first_run" => include_str!("fixtures/lead/first_run.json"),
        other => panic!("unknown lead fixture {other}"),
    };
    serde_json::from_str(text).expect("a lead fixture is valid JSON")
}

/// The native timestamp fields of a Reader sample and a runner situation. Shifting these (and
/// only these) rebases a frame without touching any identifier, count or policy field. ISO
/// fields keep the whole-second UTC form the parsers accept.
fn shift_timestamps(value: &mut serde_json::Value, shift: i64) {
    use serde_json::Value;
    const SECONDS: &[&str] = &[
        "observed_at",
        "expires_at",
        "claimed_at",
        "enqueued_at",
        "resets_at",
        "until",
        "oldest_at",
    ];
    const MILLIS: &[&str] = &["at_ms", "merged_at_ms", "started_ms"];
    const ISO: &[&str] = &["mergedAt", "updated_at"];
    match value {
        Value::Object(map) => {
            for (key, inner) in map.iter_mut() {
                if SECONDS.contains(&key.as_str()) {
                    if let Some(number) = inner.as_u64() {
                        *inner = Value::from((number as i64 + shift) as u64);
                    }
                } else if MILLIS.contains(&key.as_str()) {
                    if let Some(number) = inner.as_u64() {
                        *inner = Value::from((number as i64 + shift * 1000) as u64);
                    }
                } else if ISO.contains(&key.as_str()) {
                    if let Some(text) = inner.as_str() {
                        let secs = oxa_session::parse_utc_seconds(text)
                            .expect("a fixture ISO timestamp is whole-second UTC");
                        let shifted = (secs as i64 + shift) as u64;
                        *inner = Value::from(format!("{}Z", &oxa_session::iso_from_ms(shifted * 1000)[..19]));
                    }
                } else {
                    shift_timestamps(inner, shift);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                shift_timestamps(item, shift);
            }
        }
        _ => {}
    }
}

/// Serve one synthetic frame through the real binary and routes. The closure receives the
/// port, the authority and the private input directory (its `board.jsonl` may be absent, as
/// the first_run frame declares).
fn with_frame(name: &str, run: impl FnOnce(u16, &str, &std::path::Path)) {
    let mut frame = fixture(name);
    let test = TestEnv::new();
    std::fs::set_permissions(test.agent_dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    test.write(
        "home/.oxa/agent/settings.json",
        &serde_json::json!({"oxa":{"lead":{"serve":{
            "enabled":true,"fixed_prompts":true,"port":port
        }}}})
        .to_string(),
    );
    let directory = test.root().join("inputs");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let board = directory.join("board.jsonl");
    if !frame["board_record"].is_null() {
        let age = frame["age_secs"]
            .as_u64()
            .expect("a frame declares its board age");
        let observed = frame["board_record"]["observed_at"].as_u64().unwrap();
        let record = &mut frame["board_record"];
        shift_timestamps(record, now as i64 - observed as i64 - age as i64);
        let mut line = serde_json::to_string(record).unwrap();
        line.push('\n');
        std::fs::write(&board, line).unwrap();
        std::fs::set_permissions(&board, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let situation = if frame["situation"].is_null() {
        None
    } else {
        let age = frame["situation_age_secs"].as_u64().unwrap();
        let at_ms = frame["situation"]["data"]["at_ms"].as_u64().unwrap();
        let document = &mut frame["situation"];
        shift_timestamps(document, now as i64 - (at_ms / 1000) as i64 - age as i64);
        // The frame must be a native situation document, not an approximation of one.
        let bytes = serde_json::to_vec(document).unwrap();
        let snapshot =
            oxa_proto::work::Snapshot::from_json(&bytes).expect("a fixture situation decodes natively");
        let path = directory.join("situation.json");
        std::fs::write(&path, snapshot.to_json().unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        Some(path)
    };
    drop(reservation);
    let _server = start_with_situation(&test, &board, situation.as_deref());
    let authority = format!("127.0.0.1:{port}");
    run(port, &authority, &directory);
}

/// Every page and its fragment render for an authorized caller, and a wrong Host is refused.
fn frame_pages_render(port: u16, authority: &str) {
    for (path, title) in [
        ("/", "Status"),
        ("/asks", "Asks"),
        ("/board", "Board"),
        ("/workers", "Workers"),
        ("/usage", "Usage"),
        ("/chat", "Chat with lead"),
    ] {
        for fragment in [false, true] {
            let path = if fragment {
                format!("{path}?fragment=1")
            } else {
                path.into()
            };
            let response = get(port, &path, authority, "");
            assert!(response.starts_with("HTTP/1.1 200"), "{path}");
            assert!(response.contains(&format!(">{title}</h1>")), "{path}");
            assert_eq!(response.contains("<!doctype html>"), !fragment, "{path}");
        }
        assert!(get(port, path, "attacker.invalid", "").starts_with("HTTP/1.1 403"));
    }
}

/// The document names its stylesheet and script at content-hashed paths; a second visit
/// revalidates the document in one request and needs no asset request at all.
#[test]
fn healthy_frame_reuses_hashed_assets_and_revalidates_unchanged_pages() {
    with_frame("healthy", |port, authority, _| {
        let document = get(port, "/", authority, "");
        assert!(document.starts_with("HTTP/1.1 200"), "{document}");
        assert!(document.contains("Cache-Control: no-store"), "{document}");
        let header = |head: &str, name: &str| {
            head.lines()
                .filter_map(|line| line.split_once(": "))
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.to_owned())
        };
        let tag = header(&document, "ETag").expect("a page carries a validator");
        for asset in ["/a/", "style.css", "live.js"] {
            assert!(document.contains(asset), "{asset} is not named: {document:.400}");
        }
        let style = document
            .split("href=")
            .nth(1)
            .and_then(|rest| rest.split('>').next())
            .filter(|path| path.starts_with("/a/") && path.ends_with("style.css"))
            .expect("the stylesheet is a hashed asset");
        let served = get(port, style, authority, "");
        assert!(served.starts_with("HTTP/1.1 200"), "{served}");
        assert!(served.contains("Cache-Control: public, max-age=31536000, immutable"));
        // An unchanged document answers 304 with no body, after the same authorization.
        let repeat = get(port, "/", authority, &format!("If-None-Match: {tag}\r\n"));
        assert!(repeat.starts_with("HTTP/1.1 304 Not Modified"), "{repeat}");
        assert!(repeat.contains("Content-Length: 0"), "{repeat}");
        assert!(
            repeat.contains("Content-Security-Policy: default-src 'none'"),
            "{repeat}"
        );
        // Without the token the same validator buys nothing.
        let refused = get_raw(port, "/", authority, &format!("If-None-Match: {tag}\r\n"));
        assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");
        assert!(!refused.contains("ETag"), "{refused}");
        // A fragment update is cheaper than a document: still one request.
        let fragment = get(port, "/?fragment=1", authority, "");
        assert!(fragment.starts_with("HTTP/1.1 200"), "{fragment}");
    });
}

#[test]
fn lead_fixtures_are_native_records_with_synthetic_identities() {
    for name in ["healthy", "needs_you", "alarm", "stale", "partial", "first_run"] {
        let frame = fixture(name);
        if !frame["board_record"].is_null() {
            let record = &frame["board_record"];
            assert_eq!(record["schema_version"], serde_json::json!(1), "{name}");
            let board: oxa_work::board::Board =
                serde_json::from_value(record["board"].clone()).expect("a native shared board");
            assert_eq!(
                board.observed_at,
                record["observed_at"].as_u64().unwrap(),
                "{name}"
            );
            assert!(!board.entries.is_empty(), "{name}");
            for entry in &board.entries {
                assert_eq!(entry.head.len(), 40, "{name}");
                assert!(
                    entry
                        .head
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                    "{name}"
                );
            }
        }
        if !frame["situation"].is_null() {
            let bytes = serde_json::to_vec(&frame["situation"]).unwrap();
            oxa_proto::work::Snapshot::from_json(&bytes).expect("a native runner situation");
        }
    }
}

#[test]
fn healthy_frame_renders_quiet_pages_from_observed_sources() {
    with_frame("healthy", |port, authority, _inputs| {
        frame_pages_render(port, authority);
        let home = get(port, "/", authority, "");
        // Every alarm source is observed and quiet: a healthy system renders no attention block
        // and no stale notice.
        assert!(!home.contains("id=attention"), "{home}");
        assert!(!home.contains("Board observation needs refreshing"), "{home}");
        let board = get(port, "/board", authority, "");
        assert!(board.contains("GitHub snapshot") && !board.contains("Stale snapshot"));
        for number in [101, 102, 103] {
            assert!(board.contains(&format!("#{}", number)), "{number}");
        }
        assert!(board.contains("Showing 3 of 3 items"));
        assert!(board.contains("fixture-worker"));
        assert!(board.contains("1. PR #101"));
        let usage = get(port, "/usage", authority, "");
        assert!(usage.contains("4980 of 5000 remaining"));
        assert!(usage.contains("aria-label=\"GitHub App · REST core remaining\""));
        let workers = get(port, "/workers", authority, "");
        assert!(workers.contains("task-103") && workers.contains("Busy at observation"));
        assert!(workers.contains("Runner facts · sequence 7"));
        // Local-mode chat is the real no-JS fixed-prompt form, not a fabricated transcript.
        assert!(get(port, "/chat", authority, "").contains("method=post action=/chat"));
    });
}

#[test]
fn needs_you_frame_asks_for_decisions_without_alarming() {
    with_frame("needs_you", |port, authority, _inputs| {
        frame_pages_render(port, authority);
        let home = get(port, "/", authority, "");
        assert!(home.contains("id=attention role=status"));
        assert!(home.contains("Needs you"));
        assert!(home.contains("Unanswered owner decisions (2)"));
        assert!(!home.contains("role=alert"));
        let asks = get(port, "/asks", authority, "");
        assert!(asks.contains("Raise the merge target to 12 per hour?"));
        assert!(asks.contains("Accept the removal rationale on PR #103?"));
        assert!(asks.contains("Ask ask-103 · PR #103"));
        // The board itself stays normal while decisions wait.
        let board = get(port, "/board", authority, "");
        assert!(board.contains("#103") && board.contains("#104"));
    });
}

#[test]
fn alarm_frame_names_every_firing_source() {
    with_frame("alarm", |port, authority, _inputs| {
        frame_pages_render(port, authority);
        let home = get(port, "/", authority, "");
        assert!(home.contains("id=attention role=alert"));
        assert!(home.contains("Checks on main are failing."));
        assert!(home.contains("No PR merged into main for 45 minutes or more at the last observation."));
        assert!(home.contains("PR #102 has waited too long in the merge queue"));
        assert!(home.contains("Run 9002"));
        let workers = get(port, "/workers", authority, "");
        assert!(workers.contains("task-102") && workers.contains("Stall reported"));
        // An alarm never hides the board rows it is about.
        let board = get(port, "/board", authority, "");
        assert!(board.contains("#102") && board.contains("#105"));
    });
}

#[test]
fn stale_frame_keeps_its_rows_and_offers_a_retry() {
    with_frame("stale", |port, authority, _inputs| {
        frame_pages_render(port, authority);
        let board = get(port, "/board", authority, "");
        assert!(board.contains("Stale snapshot") && !board.contains("GitHub snapshot"));
        for number in [101, 102, 103] {
            assert!(board.contains(&format!("#{}", number)), "{number}");
        }
        let home = get(port, "/", authority, "");
        assert!(home.contains("<h2>Board observation needs refreshing</h2>"));
        assert!(home.contains("<a href=\"\">Retry</a>"));
        // The runner situation is a separate source and stays fresh.
        assert!(get(port, "/workers", authority, "").contains("Runner facts · sequence 7"));
    });
}

#[test]
fn partial_frame_keeps_known_sources_and_names_each_gap() {
    with_frame("partial", |port, authority, _inputs| {
        frame_pages_render(port, authority);
        let board = get(port, "/board", authority, "");
        assert!(board.contains("GitHub snapshot"));
        assert!(board.contains("#106") && board.contains("#107"));
        let home = get(port, "/", authority, "");
        // One absent source is an explicit state, never an alarm and never silence.
        // The frame has every alarm source but the queued-run capture, so the sample
        // names exactly that one (#2342) and says where the reader goes next.
        assert!(home.contains("id=attention role=status"));
        assert!(!home.contains("role=alert"));
        assert!(home.contains("<b>Not in this sample:</b> queued workflow runs."));
        assert!(home.contains("The latest board sample did not carry them."));
        assert!(home.contains("Last good value "));
        assert!(home.contains("Refresh to read a newer sample."));
        let usage = get(port, "/usage", authority, "");
        assert!(usage.contains("Deferred by the GitHub reserve"));
        assert!(usage.contains("queue-age (graphql)"));
        assert!(usage.contains("held back until the window resets"));
        assert!(usage.contains("GitHub App · GraphQL</dt><dd>Unknown"));
        assert!(usage.contains("4960 of 5000 remaining"));
    });
}

#[test]
fn first_run_frame_renders_explicit_missing_states() {
    with_frame("first_run", |port, authority, inputs| {
        assert!(!inputs.join("board.jsonl").exists());
        frame_pages_render(port, authority);
        let board = get(port, "/board", authority, "");
        assert!(board.contains("Board unavailable"));
        assert!(!board.contains("<tbody>"));
        let home = get(port, "/", authority, "");
        assert!(!home.contains("id=attention"));
        assert!(!home.contains("GitHub snapshot"));
        assert!(get(port, "/workers", authority, "").contains("Lane status unavailable"));
        assert!(get(port, "/usage", authority, "").contains("No GitHub quota observation reached this page."));
        assert!(get(port, "/asks", authority, "").contains("Owner asks unavailable"));
    });
}

#[test]
fn failed_board_replacement_degrades_explicitly_and_a_fresh_sample_recovers() {
    with_frame("healthy", |port, authority, inputs| {
        let board = inputs.join("board.jsonl");
        let good = std::fs::read(&board).unwrap();
        assert!(get(port, "/board", authority, "").contains("#101"));
        // A malformed replacement never fabricates rows or panics the service. The reader
        // keeps the one last-good sample (#2342), so its rows stay, labelled as last-good
        // and carrying the allow-listed cause and age; the rejected bytes never render.
        std::fs::write(&board, b"not a board sample\n").unwrap();
        let degraded = get(port, "/board", authority, "");
        assert!(degraded.contains("Board is last-good"));
        assert!(degraded.contains("The protected Board observation is not a valid board sample"));
        assert!(degraded.contains("Last good value "));
        assert!(degraded.contains("Last-good snapshot"));
        assert!(!degraded.contains("GitHub snapshot"));
        for number in [101, 102, 103] {
            assert!(degraded.contains(&format!("#{}", number)), "{number}");
        }
        assert!(!degraded.contains("not a board sample"));
        assert!(get(port, "/", authority, "").starts_with("HTTP/1.1 200"));
        std::fs::write(&board, &good).unwrap();
        let recovered = get(port, "/board", authority, "");
        assert!(recovered.contains("#101") && recovered.contains("GitHub snapshot"));
        assert!(!recovered.contains("Board is last-good"));
    });
}

#[test]
fn chat_page_renders_the_session_as_messages_with_one_composer() {
    // A real route, a real protected Session file and synthetic entries only: the thread
    // is bubbles, lead text is Markdown with code, consecutive steps fold into one line,
    // the owner message form is the only message form, and a credential is masked.
    let test = TestEnv::new();
    let dirs = oxa_config::Dirs::under(test.root());
    std::fs::set_permissions(&dirs.agent_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let uid = std::fs::metadata(test.root()).unwrap().uid();
    let project = test.work().canonicalize().unwrap();
    let id = oxa_cli::LeadOwnerBinding::create(&dirs, &project, uid).unwrap();
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    let authority = "lead.example.invalid";
    let settings = test.write(
        "home/.oxa/agent/settings.json",
        &serde_json::json!({"oxa":{"lead":{
            "sessionId":id,
            "serve":{"enabled":true,"port":port,"identity":"tailscale","owner_chat":true,
                "allowed_identities":["owner@example.invalid"],
                "proxy":{"uid":uid,"authority":authority}}
        }}})
        .to_string(),
    );
    std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o600)).unwrap();
    let store = oxa_session::FsStore::new(dirs.sessions_dir.clone());
    let session = store.resolve(&id).unwrap();
    let now_ms = oxa_proto::time::now_ms();
    let entry = |id: &str, parent: Option<&str>, ms: u64, body: oxa_proto::EntryBody| oxa_proto::Entry {
        id: id.into(),
        parent: parent.map(str::to_owned),
        ts_ms: now_ms - ms,
        body,
    };
    let message = |wire: oxa_proto::WireMsg| oxa_proto::EntryBody::Msg {
        wire,
        parts: Vec::new(),
        tool: None,
    };
    let mut call = oxa_proto::WireMsg::assistant("");
    call.tool_calls = vec![
        oxa_proto::ToolCall {
            id: "call-a".into(),
            name: "read".into(),
            args_raw: "{\"path\":\"oxa/src/fixture.rs\"}".into(),
        },
        oxa_proto::ToolCall {
            id: "call-b".into(),
            name: "bash".into(),
            args_raw: "{\"command\":\"cargo fmt --check\"}".into(),
        },
    ];
    let entries = [
        entry(
            "b1000000",
            None,
            90_000,
            message(oxa_proto::WireMsg::user("Why is the fixture stuck?")),
        ),
        entry("b1000001", Some("b1000000"), 80_000, message(call)),
        entry(
            "b1000002",
            Some("b1000001"),
            70_000,
            message(oxa_proto::WireMsg::tool_result(
                "call-a",
                "token=demo-secret-value sk-abcdefghijklmnopqrstuvwxyz12",
            )),
        ),
        entry(
            "b1000003",
            Some("b1000002"),
            60_000,
            message(oxa_proto::WireMsg::assistant(
                "Two findings are open:\n\n- the first\n- the second\n\n```rust\nlet path = \"<fixture>\";\n```",
            )),
        ),
    ];
    store.append_many(&session, &entries).unwrap();
    drop(reservation);

    let mut invocation = command(&test);
    invocation
        .args(["lead", "serve"])
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(test.root().join("lead-chat.stderr")).unwrap());
    let mut child = Running(invocation.spawn().unwrap());
    let stdout = child.0.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap_or_default();
            let found = line.contains("listening");
            let _ = send.send(line);
            if found {
                break;
            }
        }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let line = receive
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .expect("lead serve did not report its listener");
        if line.contains("listening") {
            break;
        }
    }

    let login = "Tailscale-User-Login: owner@example.invalid\r\n";
    let page = get_raw(port, "/chat", authority, login);
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    // One header strip, one thread, one composer, in that order.
    for part in [
        "<div class=chat-head>",
        "<div class=thread",
        "<div class=composer2>",
    ] {
        assert!(page.contains(part), "{part}: {page}");
    }
    // Bubbles both ways, Markdown with a list and a code block.
    assert!(page.contains("class=\"msg me\""), "{page}");
    assert!(page.contains("class=\"msg lead\""), "{page}");
    assert!(
        page.contains("<ul><li>the first</li><li>the second</li></ul>"),
        "{page}"
    );
    assert!(page.contains("<span class=lang>rust</span><pre><code>"), "{page}");
    // The call and its result fold into one details labelled by their own names.
    assert_eq!(page.matches("<details class=tools").count(), 1, "{page}");
    assert!(page.contains("2 steps · read +1, result"), "{page}");
    // Masking precedes markup: the credential never reaches the page.
    assert!(
        !page.contains("demo-secret-value") && !page.contains("sk-abcdefghij"),
        "{page}"
    );
    // The one message form is the composer's, with its CSRF field and textarea.
    assert_eq!(page.matches("method=post action=/chat").count(), 2, "{page}");
    // The header's session popover lists the owner's running sessions; a dedicated Lead
    // chat is reached through its own setting, not from that list.
    assert!(
        page.contains("<details class=sessions><summary>Sessions"),
        "{page}"
    );
    assert_eq!(page.matches("name=message").count(), 1, "{page}");
    assert!(page.contains("id=lead-message"), "{page}");
    // Session identity is not rendered: no session id and no project path.
    assert!(!page.contains(&id), "{page}");
    assert!(!page.contains(project.to_str().unwrap()), "{page}");
    // An unauthorized peer never reads the thread.
    let refused = get_raw(
        port,
        "/chat",
        authority,
        "Tailscale-User-Login: guest@example.invalid\r\n",
    );
    assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
    drop(child);
}
