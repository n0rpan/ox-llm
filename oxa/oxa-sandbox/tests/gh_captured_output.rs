//! #298 / #493: capture issue details without changing gh's default piped output.
//! Real gh + bwrap, synthetic API over a workspace Unix socket; no network or real credentials.

use oxa_fake::TestEnv;
use oxa_proto::{CancelToken, ExecSpec, Sandbox};
use oxa_sandbox::{Bwrap, Layout, LayoutInputs};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const TITLE: &str = "Synthetic issue with a deliberately long title that must survive a pipe without truncation at eighty columns";
const BODY: &str = "Synthetic issue body";
const COMMENT: &str = "Synthetic comment body";

struct MockApi {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MockApi {
    fn new(env: &TestEnv) -> Self {
        let socket = env.work().join("api.sock");
        env.write(
            "work/gh/config.yml",
            &format!("http_unix_socket: {}\n", socket.display()),
        );
        let listener = UnixListener::bind(socket).expect("mock API socket");
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("mock API accept: {e}"),
                };
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                let mut request_line = String::new();
                let mut length = 0;
                loop {
                    line.clear();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0, "incomplete HTTP headers");
                    if request_line.is_empty() {
                        request_line.clone_from(&line);
                    }
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap();
                        }
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let response = if request_line.starts_with("GET /api/v3/meta ") {
                    json!({})
                } else {
                    assert!(request_line.starts_with("POST /api/graphql "), "{request_line}");
                    let request: Value = serde_json::from_slice(&body).unwrap();
                    Self::response(&request)
                }
                .to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }

    fn response(request: &Value) -> Value {
        let query = request["query"].as_str().unwrap();
        if query.contains("Issue_fields") {
            return json!({"data": {"Issue": {"fields": []}}});
        }
        if query.contains("IssueProjectItems") {
            return json!({"data": {"repository": {"issue": {"projectItems": {
                "nodes": [], "totalCount": 0, "pageInfo": {"hasNextPage": false}
            }}}}});
        }
        let number = request["variables"]["number"].as_u64().unwrap_or(7);
        let comments = if number == 8 {
            vec![json!({"body": COMMENT, "author": {"login": "tester"},
                "createdAt": "2026-01-01T00:00:00Z"})]
        } else {
            vec![]
        };
        let issue = json!({
            "number": number, "title": TITLE, "body": BODY, "state": "OPEN",
            "url": "https://example.invalid/owner/repo/issues/7",
            "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-02T00:00:00Z",
            "author": {"login": "tester"}, "labels": {"nodes": [], "totalCount": 0},
            "assignees": {"nodes": [], "totalCount": 0},
            "comments": {"nodes": comments, "totalCount": comments.len(),
                "pageInfo": {"hasNextPage": false}}
        });
        if query.contains("query IssueList") {
            json!({"data": {"repository": {"hasIssuesEnabled": true, "issues": {
                "nodes": [issue], "totalCount": 1, "pageInfo": {"hasNextPage": false}
            }}}})
        } else {
            assert!(
                query.contains("query IssueByNumber"),
                "unexpected mock query: {query}"
            );
            json!({"data": {"repository": {"hasIssuesEnabled": true, "issue": issue}}})
        }
    }
}

impl Drop for MockApi {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let result = self.thread.take().unwrap().join();
        if !thread::panicking() {
            result.expect("mock API thread");
        }
    }
}

fn run(b: &Bwrap, env: &TestEnv, command: &str) -> String {
    let s = ExecSpec {
        command: command.into(),
        cwd: env.work(),
        writable: true,
        net: false,
        timeout_s: Some(20),
        env: vec![
            (
                "GH_CONFIG_DIR".into(),
                env.work().join("gh").display().to_string(),
            ),
            ("GH_HOST".into(), "example.invalid".into()),
            ("GH_ENTERPRISE_TOKEN".into(), "placeholder".into()),
            ("GH_PROMPT_DISABLED".into(), "1".into()),
            ("GH_NO_UPDATE_NOTIFIER".into(), "1".into()),
        ],
    };
    let r = b.run(&s, &CancelToken::new(), &mut |_| {});
    let out = String::from_utf8_lossy(&r.output).into_owned();
    assert_eq!(r.exit, Some(0), "{command}: {out}");
    out
}

#[test]
fn real_gh_scoped_issue_details_preserve_piped_tables_and_tool_environment() {
    let env = TestEnv::new();
    let path = std::env::var("PATH").unwrap_or_default();
    let mut b = match Bwrap::detect(&path, &env.home()) {
        Ok(b) => b,
        Err(e) => {
            assert!(
                !std::env::var("OXA_REQUIRE_BWRAP").is_ok_and(|v| v == "1"),
                "OXA_REQUIRE_BWRAP=1 but bwrap is not usable: {e}"
            );
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "gh_captured_output SKIPPED: bwrap is not usable: {e}"
            );
            return;
        }
    };
    // gh is installed on the CI runners. A version-manager user can put its real bin directory
    // on PATH; bind that directory read-only when it is hidden by the account-home mask.
    let gh_dir = std::env::split_paths(&path)
        .find(|p| p.is_absolute() && p.join("gh").is_file())
        .expect("gh must be installed on PATH for this regression test");
    b.layout = Layout::build(&LayoutInputs {
        home: env.home(),
        path_var: format!("{}:/usr/bin:/bin", gh_dir.display()),
        read_extra: vec![gh_dir],
        ..Default::default()
    });
    let _api = MockApi::new(&env);

    // #493 finding 1: forced TTY output changes field separators, dates and long titles.
    assert_eq!(
        run(&b, &env, "gh issue list -R owner/repo"),
        format!("7\tOPEN\t{TITLE}\t\t2026-01-02T00:00:00Z\n")
    );
    assert_eq!(run(&b, &env, "gh issue list -R owner/repo | cut -f1"), "7\n");
    // Finding 2: no global colour/pager/TTY policy is imposed on unrelated tools.
    run(
        &b,
        &env,
        r#"test "${GH_FORCE_TTY+x}${GH_PAGER+x}${NO_COLOR+x}" = "" && test ! -t 0 && test ! -t 1"#,
    );

    for number in [7, 8] {
        // Recommended machine-readable form: includes the body even with no comments.
        let out = run(
            &b,
            &env,
            &format!("gh issue view {number} -R owner/repo --json title,body,comments,state"),
        );
        let details: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(details["title"], TITLE);
        assert_eq!(details["body"], BODY);
        assert_eq!(
            details["comments"].as_array().unwrap().len(),
            usize::from(number == 8)
        );
        if number == 8 {
            assert_eq!(details["comments"][0]["body"], COMMENT);
        }
        // Human-readable alternative: scoped to ONE invocation, with real pipes and no ANSI.
        let out = run(
            &b,
            &env,
            &format!(
                "GH_FORCE_TTY=80 GH_PAGER=cat NO_COLOR=1 gh issue view {number} -R owner/repo --comments && \
             test ! -t 0 && test ! -t 1 && test \"${{NO_COLOR+x}}${{GH_FORCE_TTY+x}}${{GH_PAGER+x}}\" = \"\""
            ),
        );
        assert!(out.contains(BODY), "missing issue body: {out}");
        assert_eq!(out.contains(COMMENT), number == 8, "{out}");
        assert!(!out.contains('\u{1b}'), "unexpected ANSI escapes: {out:?}");
    }
}
