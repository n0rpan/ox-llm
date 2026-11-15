//! G2 PARITY tests (cli): CLI, MOD and ENV rows of PARITY.md.
//! One test per row (`CLI-012` -> `fn cli_012()`, `MOD-001` -> `fn mod_001()`, `ENV-001` ->
//! `fn env_001()`). Rows marked O without a test are recorded as `omission` in PARITY.md.
//! Rows that do not match pi (or the adapted behaviour) are `#[ignore = "RED <id>: ..."]`
//! with the precise reason; `cargo test -p oxa-parity --test cli -- --ignored` shows them fail.
//! Each row test names its source: `// pi: <doc page>` (P rows) or `// invariant: <W-id>` (A rows).
//! Helpers: `oxa_parity::{oxa_bin, fake_bin, run_oxa, fixture}`, `oxa_fake::TestEnv`.

use oxa_fake::{parse_script, FakeModel, FakeOpts, FakeServer, Recorded, TestEnv};
use oxa_parity::oxa_bin;
use oxa_proto::Role;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

// ----- helpers -------------------------------------------------------------------------------

/// Script lines (`{"text": ..}` per reply), one turn per reply, consumed in order.
fn turns(replies: &[&str]) -> String {
    replies
        .iter()
        .map(|r| serde_json::json!({ "text": r }).to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A bash tool call turn (the fake streams it verbatim).
fn bash_turn(command: &str) -> String {
    let args = serde_json::json!({ "command": command }).to_string();
    serde_json::json!({ "tool_calls": [{ "id": "c1", "name": "bash", "args": args }] }).to_string()
}

fn fake(script: &str) -> FakeServer {
    fake_opts(script, FakeOpts::default())
}

fn fake_opts(script: &str, opts: FakeOpts) -> FakeServer {
    FakeServer::start(parse_script(script).expect("script"), opts).expect("start fake engine")
}

/// A URL where nothing listens.
fn closed_url() -> String {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = l.local_addr().expect("addr").port();
    drop(l);
    format!("http://127.0.0.1:{port}")
}

/// Run `oxa <args>` with `TestEnv::envs`, extra variables, optional cwd and stdin.
fn exec(
    env: &TestEnv,
    url: &str,
    args: &[&str],
    stdin: &str,
    extra: &[(&str, &str)],
    cwd: Option<&Path>,
) -> Output {
    let dir = cwd.map(Path::to_path_buf).unwrap_or_else(|| env.work());
    let mut child = Command::new(oxa_bin())
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs(env.envs(url))
        .envs(oxa_parity::note_off_env())
        .envs(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oxa (build it first: cargo build -p oxa-cli -p oxa-fake)");
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(stdin.as_bytes());
    }
    child.wait_with_output().expect("wait for oxa")
}

/// Run with an empty stdin (a pipe, so print mode).
fn run(env: &TestEnv, url: &str, args: &[&str]) -> Output {
    exec(env, url, args, "", &[], None)
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

fn ok(o: &Output) {
    assert_eq!(code(o), 0, "exit code; stderr: {}", err(o));
}

fn user_texts(r: &Recorded) -> Vec<String> {
    r.messages
        .iter()
        .filter(|m| matches!(m.role, Role::User))
        .filter_map(|m| m.content.clone())
        .collect()
}

fn tool_names(r: &Recorded) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(&r.tools_json).unwrap_or(serde_json::Value::Null);
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    t.pointer("/function/name")
                        .or_else(|| t.get("name"))
                        .and_then(|n| n.as_str())
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

fn names(list: &[&str]) -> Vec<String> {
    sorted(list.iter().map(|s| s.to_string()).collect())
}

fn all_tools() -> Vec<String> {
    names(&["bash", "edit", "find", "grep", "ls", "read", "write"])
}

/// Every `.jsonl` file below the agent directory's `sessions`.
fn session_files(env: &TestEnv) -> Vec<PathBuf> {
    fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, found);
                } else if p.extension().is_some_and(|x| x == "jsonl") {
                    found.push(p);
                }
            }
        }
    }
    let mut v = Vec::new();
    walk(&env.agent_dir().join("sessions"), &mut v);
    v.sort();
    v
}

fn read_json(path: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path).expect("read json");
    serde_json::from_str(&text).expect("valid json")
}

fn mcp_json(env: &TestEnv) -> serde_json::Value {
    read_json(&env.agent_dir().join("mcp.json"))
}

/// The source of one test: its top-level comment block (`// pi:`, `// invariant:`) and its fn,
/// up to the next top-level comment or attribute.
fn body_of(name: &str) -> &'static str {
    let src: &'static str = include_str!("cli.rs");
    let at = src
        .find(&format!("\nfn {name}("))
        .unwrap_or_else(|| panic!("no fn {name} in cli.rs"))
        + 1;
    let start = src[..at].rfind("\n\n").map(|i| i + 2).unwrap_or(0);
    let end = ["\n// ", "\n#["]
        .iter()
        .filter_map(|pat| src[at..].find(pat))
        .min()
        .map(|rel| at + rel + 1)
        .unwrap_or(src.len());
    &src[start..end]
}

// ----- PARITY.md rows ------------------------------------------------------------------------

#[derive(Debug)]
struct Row {
    id: String,
    verdict: String,
    test: String,
}

/// The rows of the CLI, MOD and ENV sections of PARITY.md.
fn parity_rows() -> Vec<Row> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../PARITY.md");
    let text = std::fs::read_to_string(&path).expect("read PARITY.md");
    let mut rows = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if !l.starts_with('|') {
            continue;
        }
        let cells: Vec<String> = l
            .replace("\\|", "\u{1}")
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().replace('\u{1}', "|"))
            .collect();
        if cells.len() != 5 {
            continue;
        }
        let id = cells[0].clone();
        if !["CLI-", "MOD-", "ENV-"].iter().any(|p| id.starts_with(p)) {
            continue;
        }
        rows.push(Row {
            id,
            verdict: cells[2].clone(),
            test: cells[4].trim_matches('`').to_string(),
        });
    }
    rows
}

/// `CLI-012` -> `cli_012`.
fn fn_name(id: &str) -> String {
    id.to_lowercase().replace('-', "_")
}

/// The test fn of a row, when it has one.
fn test_fn(row: &Row) -> Option<String> {
    row.test.strip_prefix("cli.rs::").map(String::from)
}

// ----- meta checks (task acceptance) ---------------------------------------------------------

/// each PARITY.md id of this area maps to a test fn or an O-row assertion
#[test]
fn every_row_has_test_or_recorded_omission() {
    let rows = parity_rows();
    assert_eq!(rows.len(), 74, "CLI 57 + MOD 5 + ENV 12");
    let src = include_str!("cli.rs");
    for row in &rows {
        if row.test == "omission" {
            assert_eq!(
                row.verdict, "O",
                "{} is recorded as omission but is not O",
                row.id
            );
            continue;
        }
        let f = test_fn(row).unwrap_or_else(|| panic!("{}: test column {:?}", row.id, row.test));
        assert_eq!(f, fn_name(&row.id).to_string(), "{} maps to the wrong fn", row.id);
        assert!(src.contains(&format!("fn {f}(")), "{}: no test fn {f}", row.id);
    }
}

/// documented output or recorded fixture
#[test]
fn documented_rows_have_tests() {
    for row in parity_rows()
        .iter()
        .filter(|r| r.verdict == "P" && r.test != "omission")
    {
        let body = body_of(&fn_name(&row.id));
        assert!(
            body.contains("// pi:") || body.contains("fixture("),
            "{}: a P test must cite the pi documentation page (`// pi: ...`) or use a recorded fixture",
            row.id
        );
    }
}

/// comment + adapted behaviour asserted
#[test]
fn a_rows_name_their_invariant() {
    for row in parity_rows()
        .iter()
        .filter(|r| r.verdict == "A" && r.test != "omission")
    {
        let body = body_of(&fn_name(&row.id));
        assert!(
            body.contains("// invariant:"),
            "{}: an A test must name the invariant that forced the adaptation (`// invariant: ...`)",
            row.id
        );
    }
}

// ----- CLI rows ------------------------------------------------------------------------------

// pi: cli.md `message` arguments: each is one prompt in print mode
#[test]
fn cli_001() {
    let server = fake(&turns(&["reply-one", "reply-two"]));
    let env = TestEnv::new();
    let o = run(&env, &server.url(), &["-p", "--no-session", "one", "two"]);
    ok(&o);
    assert_eq!(out(&o).trim_end(), "reply-two");
    let reqs = server.requests();
    assert_eq!(reqs.len(), 2, "one request per message");
    assert_eq!(user_texts(&reqs[1]), vec!["one".to_string(), "two".to_string()]);
}

// pi: cli.md `@path` includes a text file in the first prompt
#[test]
fn cli_002() {
    let server = fake(&turns(&["ok"]));
    let env = TestEnv::new();
    env.write("work/notes.txt", "FILE-BODY-42\n");
    let o = run(
        &env,
        &server.url(),
        &["-p", "--no-session", "@notes.txt", "question"],
    );
    ok(&o);
    let first = user_texts(&server.requests()[0]).join("\n");
    assert!(
        first.contains("FILE-BODY-42"),
        "file body in first prompt: {first}"
    );
    assert!(first.contains("question"));
}

// invariant: W2 (stored and shown; the wire gets the fixed placeholder, never the bytes)
#[test]
fn cli_003() {
    let server = fake(&turns(&["ok"]));
    let env = TestEnv::new();
    // a 1x1 PNG
    let png: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00,
        0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00,
        0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xFF, 0xFF, 0x3F, 0x00, 0x05, 0xFE,
        0x02, 0xFE, 0xA7, 0x35, 0x81, 0x84, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60,
        0x82,
    ];
    std::fs::write(env.work().join("img.png"), png).expect("write png");
    let o = run(
        &env,
        &server.url(),
        &["-p", "--session-id", "img3", "look", "@img.png"],
    );
    ok(&o);
    let reqs = server.requests();
    let wire = user_texts(&reqs[0]).join("\n");
    assert!(!wire.contains("iVBOR"), "no image bytes on the wire");
    let stored = std::fs::read_to_string(&session_files(&env)[0]).expect("session");
    assert!(stored.contains("img.png"), "the image is stored with the session");
}

// pi: cli.md piped stdin is prepended to the first prompt
#[test]
fn cli_004() {
    let server = fake(&turns(&["ok"]));
    let env = TestEnv::new();
    let o = exec(
        &env,
        &server.url(),
        &["-p", "--no-session", "question"],
        "PIPED-CONTENT\n",
        &[],
        None,
    );
    ok(&o);
    let first = user_texts(&server.requests()[0]).join("\n");
    let (p, q) = (first.find("PIPED-CONTENT"), first.find("question"));
    assert!(p.is_some() && q.is_some(), "both parts present: {first}");
    assert!(p < q, "stdin comes before the message");
}

// pi: cli.md `--` stops option parsing; words after it are messages
#[test]
fn cli_005() {
    let server = fake(&turns(&["a", "b"]));
    let env = TestEnv::new();
    let o = run(
        &env,
        &server.url(),
        &["-p", "--no-session", "--", "-nt", "--mode"],
    );
    ok(&o);
    let reqs = server.requests();
    assert_eq!(user_texts(&reqs[0]), vec!["-nt".to_string()]);
    assert!(
        !tool_names(&reqs[0]).is_empty(),
        "-nt after -- is a message, not a flag"
    );
    assert_eq!(user_texts(&reqs[1]).last().map(String::as_str), Some("--mode"));
}

// pi: cli.md `-p` prints the final assistant text and exits
#[test]
fn cli_006() {
    let server = fake(&turns(&["final-reply"]));
    let env = TestEnv::new();
    let o = run(&env, &server.url(), &["-p", "--no-session", "hi"]);
    ok(&o);
    assert_eq!(out(&o).trim_end(), "final-reply");
}

// pi: cli.md `--mode text` is text output; with pipes (not terminals) print mode runs
#[test]
fn cli_007() {
    let server = fake(&turns(&["text-reply"]));
    let env = TestEnv::new();
    let o = run(&env, &server.url(), &["--mode", "text", "--no-session", "hi"]);
    ok(&o);
    assert_eq!(out(&o).trim_end(), "text-reply");
}

// pi: cli.md `--mode json` prints JSONL events then exits
#[test]
fn cli_008() {
    let server = fake(&turns(&["json-reply"]));
    let env = TestEnv::new();
    let o = run(&env, &server.url(), &["--mode", "json", "--no-session", "hi"]);
    ok(&o);
    let lines: Vec<serde_json::Value> = out(&o)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON: {l} ({e})")))
        .collect();
    assert!(
        lines.iter().all(|v| v.get("type").is_some()),
        "every line has a type"
    );
    assert!(lines.iter().any(|v| v["type"] == "agent_end"));
    assert_eq!(
        lines.last().map(|v| v["type"].clone()),
        Some(serde_json::json!("agent_settled"))
    );
}

// pi: rpc.md commands on stdin, one response per command; EOF on stdin is an orderly shutdown
#[test]
fn cli_009() {
    let server = fake(&turns(&[]));
    let env = TestEnv::new();
    let o = exec(
        &env,
        &server.url(),
        &["--mode", "rpc"],
        "{\"id\":\"1\",\"type\":\"get_state\"}\n",
        &[],
        None,
    );
    ok(&o);
    let first: serde_json::Value =
        serde_json::from_str(out(&o).lines().next().expect("a response")).expect("json");
    assert_eq!(first["type"], "response");
    assert_eq!(first["command"], "get_state");
    assert_eq!(first["success"], true);
}

// pi: cli.md `--export <input> [output]` writes HTML and exits without a request
#[test]
fn cli_010() {
    let server = fake(&turns(&["seed"]));
    let env = TestEnv::new();
    ok(&run(
        &env,
        &server.url(),
        &["-p", "--session-id", "exp10", "seed"],
    ));
    let session = session_files(&env)[0].clone();
    let before = server.requests().len();
    let o = run(
        &env,
        &server.url(),
        &["--export", session.to_str().expect("utf-8")],
    );
    ok(&o);
    assert_eq!(server.requests().len(), before, "export makes no request");
    let html: Vec<PathBuf> = std::fs::read_dir(env.work())
        .expect("work")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "html"))
        .collect();
    assert_eq!(html.len(), 1, "a derived output name in the cwd");
    let text = std::fs::read_to_string(&html[0]).expect("html");
    assert!(text.contains("<html") || text.contains("<!DOCTYPE"));
}

// pi: cli.md `--provider` selects a provider; oxa has one provider type, others are refused
// invariant: O-134 (one provider: the oxe engine)
// pi: models.md `--model` exact id, fuzzy match, `:<thinking>` suffix
// invariant: A-model resolved over the engine model list (/v1/models); provider/id only for an endpoint
#[test]
fn cli_012() {
    let server = fake(&turns(&["a", "b", "c", "d", "e"]));
    let env = TestEnv::new();
    ok(&run(
        &env,
        &server.url(),
        &["--model", "fake-model", "-p", "--no-session", "x"],
    ));
    ok(&run(
        &env,
        &server.url(),
        &["--model", "fake", "-p", "--no-session", "x"],
    ));
    ok(&run(
        &env,
        &server.url(),
        &["--model", "fake-model:high", "-p", "--no-session", "x"],
    ));
    ok(&run(
        &env,
        &server.url(),
        &["--model", "oxe/fake-model", "-p", "--no-session", "x"],
    ));
    let reqs = server.requests();
    assert!(reqs.iter().all(|r| r.model == "fake-model"));
    assert!(reqs[2].thinking, "the :high suffix turns thinking on");
    // pi settings.md: `defaultThinkingLevel` defaults to `medium`, which is on (W6).
    assert!(reqs[0].thinking);
    ok(&run(
        &env,
        &server.url(),
        &["--model", "fake-model:off", "-p", "--no-session", "x"],
    ));
    assert!(
        !server.requests()[4].thinking,
        "the :off suffix turns thinking off"
    );
    let bad = run(&env, &server.url(), &["--model", "nosuch", "-p", "x"]);
    assert_ne!(code(&bad), 0);
    assert!(err(&bad).contains("not found"));
}

// pi: cli.md `--api-key` sets the key for the provider
// invariant: A-key an optional bearer key for an engine endpoint (no provider selection)
#[test]
fn cli_013() {
    let server = fake(&turns(&["ok"]));
    let env = TestEnv::new();
    ok(&run(
        &env,
        &server.url(),
        &["--api-key", "secret-13", "-p", "--no-session", "x"],
    ));
    let auth = server.requests()[0]
        .headers
        .iter()
        .find(|(k, _)| k == "authorization")
        .map(|(_, v)| v.clone());
    assert_eq!(auth.as_deref(), Some("Bearer secret-13"));
}

// pi: models.md `--thinking` levels; oxa: off = enable_thinking false, any other level true
// invariant: W6 explicit sampling (boolean thinking on the wire)
#[test]
fn cli_014() {
    let server = fake(&turns(&["a", "b", "c"]));
    let env = TestEnv::new();
    for level in ["off", "high", "max"] {
        ok(&run(
            &env,
            &server.url(),
            &["--thinking", level, "-p", "--no-session", "x"],
        ));
    }
    let t: Vec<bool> = server.requests().iter().map(|r| r.thinking).collect();
    assert_eq!(t, vec![false, true, true]);
    let bad = run(&env, &server.url(), &["--thinking", "wat", "-p", "x"]);
    assert_eq!(code(&bad), 2);
}

// pi: cli.md `--models` comma-separated scope (exact, fuzzy, glob, `:<thinking>`)
// invariant: A-scope resolved over the engine model list
#[test]
fn cli_015() {
    // pi main.ts: the first scoped model is the startup model of a new session, with the
    // thinking level of its pattern unless --thinking is given; a pattern that matches nothing is
    // reported. (A second engine model would show the model choice itself; the fake lists one.)
    let server = fake(&turns(&["a", "b", "c", "d"]));
    let env = TestEnv::new();
    let o = run(
        &env,
        &server.url(),
        &["--models", "fake-model:high,fake*", "-p", "--no-session", "x"],
    );
    ok(&o);
    ok(&run(
        &env,
        &server.url(),
        &["--models", "fake*:off", "-p", "--no-session", "x"],
    ));
    ok(&run(
        &env,
        &server.url(),
        &[
            "--models",
            "fake*:off",
            "--thinking",
            "high",
            "-p",
            "--no-session",
            "x",
        ],
    ));
    let miss = run(
        &env,
        &server.url(),
        &[
            "--models",
            "zzz-none-15,fake-model:off",
            "-p",
            "--no-session",
            "x",
        ],
    );
    ok(&miss);
    let reqs = server.requests();
    assert!(reqs[0].thinking, "fake-model:high");
    assert!(
        !reqs[1].thinking,
        "the scoped pattern's :off is the startup level"
    );
    assert!(reqs[2].thinking, "--thinking wins over the pattern");
    assert!(!reqs[3].thinking);
    assert!(reqs.iter().all(|r| r.model == "fake-model"));
    assert!(err(&miss).contains("zzz-none-15"), "{}", err(&miss));
}

// pi: cli.md `--list-models [search]` lists and exits
// invariant: A-list the engine models only (/v1/models), no catalogs
#[test]
fn cli_016() {
    let server = fake(&turns(&[]));
    let env = TestEnv::new();
    let o = run(&env, &server.url(), &["--list-models"]);
    ok(&o);
    assert!(out(&o).contains("fake-model"));
    let miss = run(&env, &server.url(), &["--list-models", "zzz-no-such"]);
    ok(&miss);
    assert!(out(&miss).contains("No models matching"));
    assert!(server.requests().is_empty(), "listing makes no chat request");
}

// pi: sessions.md `-c` continues the most recent session of the cwd (stored bytes replayed)
// invariant: W3 stored prefix bytes are replayed
#[test]
fn cli_017() {
    let server = fake(&turns(&["r1", "r2"]));
    let env = TestEnv::new();
    ok(&run(&env, &server.url(), &["-p", "one"]));
    ok(&run(&env, &server.url(), &["-p", "-c", "two"]));
    let reqs = server.requests();
    assert_eq!(reqs[1].system, reqs[0].system, "the frozen prefix is replayed");
    assert_eq!(user_texts(&reqs[1]), vec!["one".to_string(), "two".to_string()]);
}

// pi: sessions.md `-r` opens the session picker (interactive); print mode refuses with a clear error
#[test]
fn cli_018() {
    let server = fake(&turns(&[]));
    let env = TestEnv::new();
    let o = run(&env, &server.url(), &["-r", "-p", "x"]);
    assert_eq!(code(&o), 2);
    assert!(err(&o).contains("picker"), "clear error: {}", err(&o));
}

// pi: sessions.md `--session <path|id>` opens by path or full/partial id; pi main.ts
// `resolveSessionPath`: an id found only in another directory prints "Session found in different
// project: <cwd>" and asks "Fork this session into current directory? [y/N]"
#[test]
fn cli_019() {
    let server = fake(&turns(&["r1", "r2", "r3", "r4"]));
    let env = TestEnv::new();
    ok(&run(
        &env,
        &server.url(),
        &["-p", "--session-id", "sess19", "one"],
    ));
    let path = session_files(&env)[0].clone();
    ok(&run(
        &env,
        &server.url(),
        &["-p", "--session", path.to_str().expect("utf-8"), "two"],
    ));
    ok(&run(&env, &server.url(), &["-p", "--session", "sess19", "three"]));
    let reqs = server.requests();
    assert_eq!(user_texts(&reqs[1]), vec!["one".to_string(), "two".to_string()]);
    assert_eq!(
        user_texts(&reqs[2]).len(),
        3,
        "a partial id reaches the same session"
    );
    assert_eq!(session_files(&env).len(), 1);

    // The same id from another directory: the fork offer. Declined: "Aborted.", exit 0,
    // nothing sent, nothing written.
    let other = env.root().join("other-19");
    std::fs::create_dir_all(&other).expect("other dir");
    let no = exec(
        &env,
        &server.url(),
        &["-p", "--session", "sess19", "four"],
        "n\n",
        &[],
        Some(&other),
    );
    ok(&no);
    assert!(
        out(&no).contains("Session found in different project:"),
        "{}",
        out(&no)
    );
    assert!(
        out(&no).contains("Fork this session into current directory? [y/N]"),
        "{}",
        out(&no)
    );
    assert!(out(&no).contains("Aborted."), "{}", out(&no));
    assert_eq!(server.requests().len(), 3, "nothing is sent");
    assert_eq!(session_files(&env).len(), 1, "nothing is written");
    // Accepted: a fork in the other directory carries the history.
    let yes = exec(
        &env,
        &server.url(),
        &["-p", "--session", "sess19", "four"],
        "y\n",
        &[],
        Some(&other),
    );
    ok(&yes);
    let reqs = server.requests();
    assert_eq!(
        user_texts(&reqs[3]),
        vec![
            "one".to_string(),
            "two".to_string(),
            "three".to_string(),
            "four".to_string()
        ]
    );
    assert_eq!(session_files(&env).len(), 2, "the fork is a new session");
}

// pi: sessions.md `--session-id <id>` opens or creates; allowed characters; start and end alphanumeric
#[test]
fn cli_020() {
    let server = fake(&turns(&["r1", "r2"]));
    let env = TestEnv::new();
    ok(&run(
        &env,
        &server.url(),
        &["-p", "--session-id", "a.b_c-1", "one"],
    ));
    ok(&run(
        &env,
        &server.url(),
        &["-p", "--session-id", "a.b_c-1", "two"],
    ));
    assert_eq!(session_files(&env).len(), 1, "open, not create again");
    assert_eq!(
        user_texts(&server.requests()[1]),
        vec!["one".to_string(), "two".to_string()],
        "the second run continues the session"
    );
    for bad in ["-x", "x.", "a b"] {
        let o = run(&env, &server.url(), &["-p", "--session-id", bad, "x"]);
        assert_eq!(code(&o), 2, "{bad} is not a valid id");
    }
}

// pi: sessions.md `--fork <path|id>` forks into a new session in the cwd
// invariant: W1 a fork is a rebase; the new session starts from the source's wire
#[test]
fn cli_021() {
    let server = fake(&turns(&["r1", "r2"]));
    let env = TestEnv::new();
    ok(&run(
        &env,
        &server.url(),
        &["-p", "--session-id", "seed21", "one"],
    ));
    let before = session_files(&env).len();
    ok(&run(&env, &server.url(), &["-p", "--fork", "seed21", "two"]));
    assert_eq!(session_files(&env).len(), before + 1, "a new session file");
    assert_eq!(
        user_texts(&server.requests()[1]),
        vec!["one".to_string(), "two".to_string()]
    );
}

// pi: cli.md `--session-dir` overrides the env and sessionDir
// invariant: env name OXA_SESSION_DIR
#[test]
fn cli_022() {
    let server = fake(&turns(&["r"]));
    let env = TestEnv::new();
    let flag_dir = env.tmp().join("flag-sessions");
    let env_dir = env.tmp().join("env-sessions");
    let o = exec(
        &env,
        &server.url(),
        &["-p", "--session-dir", flag_dir.to_str().expect("utf-8"), "x"],
        "",
        &[("OXA_SESSION_DIR", env_dir.to_str().expect("utf-8"))],
        None,
    );
    ok(&o);
    assert!(
        !session_files_in(&flag_dir).is_empty(),
        "written to the flag directory"
    );
    assert!(
        session_files_in(&env_dir).is_empty(),
        "the env directory is not used"
    );
}

fn session_files_in(dir: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                v.extend(session_files_in(&p));
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                v.push(p);
            }
        }
    }
    v
}

// pi: cli.md `--no-session` keeps the session in memory only
#[test]
fn cli_023() {
    let server = fake(&turns(&["r"]));
    let env = TestEnv::new();
    ok(&run(&env, &server.url(), &["-p", "--no-session", "x"]));
    assert!(session_files(&env).is_empty(), "no session file is written");
}

// pi: sessions.md `-n, --name` sets the session display name (a session_info entry)
#[test]
fn cli_024() {
    let server = fake(&turns(&["r"]));
    let env = TestEnv::new();
    ok(&run(&env, &server.url(), &["-p", "-n", "my-name-24", "x"]));
    let stored = std::fs::read_to_string(&session_files(&env)[0]).expect("session");
    assert!(stored.contains("session_info") && stored.contains("my-name-24"));
}

// pi: cli.md `--fork` conflicts with `--session`, `--continue`, `--resume`, `--no-session`
#[test]
fn cli_025() {
    let server = fake(&turns(&[]));
    let env = TestEnv::new();
    for extra in [
        ["--session", "s"],
        ["--continue", ""],
        ["--resume", ""],
        ["--no-session", ""],
    ] {
        let mut args = vec!["--fork", "x"];
        args.extend(extra.iter().copied().filter(|a| !a.is_empty()));
        let o = run(&env, &server.url(), &args);
        assert_eq!(code(&o), 2, "{args:?}");
        assert!(err(&o).contains("cannot be combined"), "{args:?}: {}", err(&o));
    }
}

// pi: cli.md `--session-id` conflicts with `--session`, `--continue`, `--resume`; combines with `--fork`
#[test]
fn cli_026() {
    let server = fake(&turns(&[]));
    let env = TestEnv::new();
    for extra in ["--session", "--continue", "--resume"] {
        let mut args = vec!["--session-id", "s26", extra];
        if extra == "--session" {
            args.push("x");
        }
        let o = run(&env, &server.url(), &args);
        assert_eq!(code(&o), 2, "{args:?}");
        assert!(err(&o).contains("cannot be combined"), "{args:?}: {}", err(&o));
    }
    // combines with --fork: no usage error (the fork itself then fails for the missing source)
    let o = run(
        &env,
        &server.url(),
        &["--session-id", "s26", "--fork", "nosuch-26", "-p", "x"],
    );
    assert!(!err(&o).contains("cannot be combined"), "{}", err(&o));
}

// pi: cli.md `-t, --tools` allowlist with `*` patterns; `+name` / `-name`-only list edits the defaults
// invariant: W3 selection resolved once per prefix epoch
#[test]
fn cli_027() {
    let server = fake(&turns(&["a", "b"]));
    let env = TestEnv::new();
    ok(&run(
        &env,
        &server.url(),
        &["-p", "--no-session", "-t", "rea*,ls", "x"],
    ));
    ok(&run(
        &env,
        &server.url(),
        &["-p", "--no-session", "-t", "-ls", "x"],
    ));
    let reqs = server.requests();
    assert_eq!(tool_names(&reqs[0]), names(&["read", "ls"]));
    let minus = tool_names(&reqs[1]);
    assert!(!minus.contains(&"ls".to_string()) && minus.contains(&"read".to_string()));
    // oxa's default selection (CLI-032): the seven built-ins plus `subagent` where O-146 offers
    // it, so on the default (large, non-Flash) fake model `-ls` leaves seven names.
    let default_minus_ls = names(&["bash", "edit", "find", "grep", "read", "subagent", "write"]);
    assert_eq!(minus, default_minus_ls, "the default selection minus ls");
    // Below 32K the default selection is the seven built-ins: minus ls leaves six.
    let small = fake_opts(
        &turns(&["a"]),
        FakeOpts {
            n_ctx: 16384,
            ..FakeOpts::default()
        },
    );
    ok(&run(
        &env,
        &small.url(),
        &["-p", "--no-session", "-t", "-ls", "x"],
    ));
    assert_eq!(
        tool_names(&small.requests()[0]),
        names(&["bash", "edit", "find", "grep", "read", "write"]),
        "the default selection minus ls on a model below 32K"
    );
    let mixed = run(&env, &server.url(), &["-p", "-t", "read,+ls", "x"]);
    assert_eq!(code(&mixed), 2, "plain names cannot mix with +/- entries");
}

/// The fixture MCP server of the mcp rows as a direct-exposure `mcp.json` entry named `name`.
fn fixture_mcp_server(env: &TestEnv, name: &str) {
    let script = env.write(
        "mcp/server.py",
        &std::fs::read_to_string(oxa_parity::fixture("goldens/mcp/server.py")).expect("fixture server"),
    );
    let rec = env.root().join(format!("rec-{name}"));
    std::fs::create_dir_all(&rec).expect("record dir");
    let entry = serde_json::json!({
        "command": "python3",
        "args": ["-I", script.to_str().expect("utf-8"), rec.to_str().expect("utf-8"), "plain"],
        "exposure": "direct",
    });
    env.write(
        "home/.oxa/agent/mcp.json",
        &serde_json::json!({ "mcpServers": { name: entry } }).to_string(),
    );
}

// pi: cli.md `--tools` keeps MCP tools unless an entry starts with `mcp__`
// invariant: A-mcp direct MCP tools are frozen at epoch start
#[test]
fn cli_028() {
    // With a running direct MCP server: no `--tools` declares its tools; `--tools read` keeps
    // them callable but declares only `read`; an `mcp__` entry filters and declares them.
    let server = fake(&turns(&["a", "b", "c", "d"]));
    let env = TestEnv::new();
    fixture_mcp_server(&env, "fx28");
    ok(&run(&env, &server.url(), &["-p", "--no-session", "x"]));
    let all = tool_names(&server.requests()[0]);
    assert!(all.contains(&"mcp__fx28__echo".to_string()), "{all:?}");
    ok(&run(
        &env,
        &server.url(),
        &["-p", "--no-session", "-t", "read", "x"],
    ));
    assert_eq!(
        tool_names(&server.requests()[1]),
        names(&["read"]),
        "kept, not declared"
    );
    ok(&run(
        &env,
        &server.url(),
        &["-p", "--no-session", "-t", "read,mcp__fx28__echo", "x"],
    ));
    assert_eq!(
        sorted(tool_names(&server.requests()[2])),
        names(&["mcp__fx28__echo", "read"]),
        "an mcp__ entry declares the named tool"
    );
}

// The configuration-only half of CLI-028 (no MCP server configured).
#[test]
fn cli_028_without_servers() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    ok(&run(
        &env,
        &server.url(),
        &["-p", "--no-session", "-t", "read,mcp__radius__*", "x"],
    ));
    assert_eq!(
        tool_names(&server.requests()[0]),
        names(&["read"]),
        "no MCP server is configured, none is declared"
    );
}

// pi: cli.md `-xt, --exclude-tools` is applied last, MCP tools included
#[test]
fn cli_029() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    ok(&run(
        &env,
        &server.url(),
        &["-p", "--no-session", "-t", "read,bash,ls", "-xt", "bash", "x"],
    ));
    assert_eq!(tool_names(&server.requests()[0]), names(&["ls", "read"]));
}

// pi: cli.md `-nbt, --no-builtin-tools`
#[test]
fn cli_030() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    ok(&run(&env, &server.url(), &["-p", "--no-session", "-nbt", "x"]));
    assert!(tool_names(&server.requests()[0]).is_empty());
}

// pi: cli.md `-nt, --no-tools` disables all tools by default
#[test]
fn cli_031() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    ok(&run(&env, &server.url(), &["-p", "--no-session", "-nt", "x"]));
    assert!(tool_names(&server.requests()[0]).is_empty());
}

// pi: cli.md default enabled tools are read, bash, edit, write (`defaultTools` changes them)
// invariant: A-tools all seven built-ins on by default, plus `subagent` where O-146 offers it
// (not on the Rust Flash engine, not below a 32K window); defaultTools restores pi's four
#[test]
fn cli_032() {
    let server = fake(&turns(&["a", "b"]));
    let env = TestEnv::new();
    ok(&run(&env, &server.url(), &["-p", "--no-session", "x"]));
    // The fake's default model is large and not Flash: the seven built-ins and `subagent`.
    let mut seven_and_subagent = all_tools();
    seven_and_subagent.push("subagent".to_string());
    seven_and_subagent.sort();
    assert_eq!(tool_names(&server.requests()[0]), seven_and_subagent);
    // On the Flash engine and on a model below 32K the default set is exactly the seven.
    let flash = FakeOpts {
        model: "flash".to_string(),
        models: vec![FakeModel::flash("flash", 65536, Some(2048))],
        ..FakeOpts::default()
    };
    let small = FakeOpts {
        n_ctx: 16384,
        ..FakeOpts::default()
    };
    for (what, opts) in [("Flash", flash), ("below 32K", small)] {
        let srv = fake_opts(&turns(&["a"]), opts);
        ok(&run(&env, &srv.url(), &["-p", "--no-session", "x"]));
        assert_eq!(
            tool_names(&srv.requests()[0]),
            all_tools(),
            "{what}: the seven built-ins"
        );
    }
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultTools": ["read","bash","edit","write"]}"#,
    );
    ok(&run(&env, &server.url(), &["-p", "--no-session", "x"]));
    assert_eq!(
        tool_names(&server.requests()[1]),
        names(&["bash", "edit", "read", "write"])
    );
}

// pi: cli.md the powershell tool exists on Windows only; oxa on Linux never declares it
// invariant: O-134 platform scope (Linux only); the O surface is the tool name, which is never declared
// (unsupported_in_oxa is not raised for an unknown tool name in -t: it is ignored)
// pi: cli.md codemode and tool_search are off by default
// invariant: A-tools built-in extensions off by default; MCP turns them on when needed
#[test]
fn cli_034() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    ok(&run(&env, &server.url(), &["-p", "--no-session", "x"]));
    let t = tool_names(&server.requests()[0]);
    assert!(!t.contains(&"codemode".to_string()) && !t.contains(&"tool_search".to_string()));
}

// pi: extensions.md `-e, --extension` loads a file, directory or builtin, repeatable
// invariant: A-ext JavaScript in QuickJS; .ts is rejected with a hint
#[test]
fn cli_035() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    let js = env.write("ext/hello.js", "export default function (oxa) {}\n");
    ok(&run(
        &env,
        &server.url(),
        &["-e", js.to_str().expect("utf-8"), "-p", "--no-session", "x"],
    ));
    let ts = env.write("ext/hello.ts", "export default function (oxa: any) {}\n");
    let o = run(
        &env,
        &server.url(),
        &["-e", ts.to_str().expect("utf-8"), "-p", "x"],
    );
    assert_ne!(code(&o), 0);
    assert!(
        err(&o).contains("compile the extension to .js"),
        "hint: {}",
        err(&o)
    );
}

// pi: cli.md `-ne` disables discovery; explicit `-e` still loads
// invariant: A-ext discovery off with -ne; the explicit path is loaded (a load failure is logged
// to <agent dir>/extensions.log, which names the file)
#[test]
fn cli_036() {
    let server = fake(&turns(&["a", "b", "c"]));
    let env = TestEnv::new();
    let log = env.agent_dir().join("extensions.log");
    let explicit = env.write(
        "ext/explicit36.js",
        "throw new Error(\"load-marker-36\");\nexport default function (oxa) {}\n",
    );
    env.write(
        "home/.oxa/agent/extensions/discovered36.js",
        "throw new Error(\"load-marker-36\");\n",
    );
    let e = explicit.to_str().expect("utf-8");
    // control: without -ne the discovered extension loads (and fails, logged)
    ok(&run(&env, &server.url(), &["-p", "--no-session", "x"]));
    let control = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        control.contains("discovered36"),
        "control: discovery loads the agent extension"
    );
    std::fs::remove_file(&log).ok();
    // -ne: the discovered one is skipped, the explicit one loads
    ok(&run(
        &env,
        &server.url(),
        &["-ne", "-e", e, "-p", "--no-session", "x"],
    ));
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        logged.contains("explicit36"),
        "the explicit extension loads: {logged}"
    );
    assert!(!logged.contains("discovered36"), "discovery is off: {logged}");
}

// pi: cli.md `--no-mcp` no servers connect, no MCP tools
#[test]
fn cli_037() {
    let server = fake(&turns(&["a", "b"]));
    let env = TestEnv::new();
    let marker = env.tmp().join("mcp-started-037");
    let cmd = format!("touch {}", marker.display());
    env.write(
        "home/.oxa/agent/mcp.json",
        &serde_json::json!({ "mcpServers": { "probe": { "command": "sh", "args": ["-c", cmd] } } })
            .to_string(),
    );
    ok(&run(&env, &server.url(), &["-p", "--no-session", "x"]));
    assert!(marker.exists(), "control: an enabled server starts in print mode");
    std::fs::remove_file(&marker).expect("remove marker");
    ok(&run(
        &env,
        &server.url(),
        &["--no-mcp", "-p", "--no-session", "x"],
    ));
    assert!(!marker.exists(), "--no-mcp starts no server");
    assert!(tool_names(&server.requests()[0])
        .iter()
        .all(|t| !t.starts_with("mcp__")));
}

// pi: skills.md `--skill <path>` loads a skill; `-ns` disables discovery
#[test]
fn cli_038() {
    let server = fake(&turns(&["a", "b", "c"]));
    let env = TestEnv::new();
    let skill = env.write(
        "skills/demo-38/SKILL.md",
        "---\nname: demo-38\ndescription: Demo skill for the parity suite\n---\nBody.\n",
    );
    let dir = skill.parent().expect("skill dir").to_path_buf();
    ok(&run(
        &env,
        &server.url(),
        &["--skill", dir.to_str().expect("utf-8"), "-p", "--no-session", "x"],
    ));
    assert!(
        server.requests()[0].system.contains("demo-38"),
        "the skill is listed in the prefix"
    );
    env.write(
        "home/.oxa/agent/skills/agent-38/SKILL.md",
        "---\nname: agent-38\ndescription: Agent skill\n---\nBody.\n",
    );
    ok(&run(&env, &server.url(), &["-p", "--no-session", "x"]));
    assert!(server.requests()[1].system.contains("agent-38"));
    ok(&run(&env, &server.url(), &["-ns", "-p", "--no-session", "x"]));
    assert!(
        !server.requests()[2].system.contains("agent-38"),
        "-ns turns discovery off"
    );
}

// pi: cli.md `--prompt-template <path>` expands `/name args`; `-np` disables discovered templates
// invariant: A-templates expand before the text becomes a message (explicit paths still load)
#[test]
fn cli_039() {
    let server = fake(&turns(&["a", "b", "c"]));
    let env = TestEnv::new();
    env.write(
        "templates/greet.md",
        "---\ndescription: greet\n---\nSay hi to $1\n",
    );
    let dir = env.root().join("templates");
    let d = dir.to_str().expect("utf-8");
    ok(&run(
        &env,
        &server.url(),
        &["--prompt-template", d, "-p", "--no-session", "/greet bob"],
    ));
    assert!(user_texts(&server.requests()[0])
        .iter()
        .any(|t| t.trim() == "Say hi to bob"));
    env.write("home/.oxa/agent/prompts/hello39.md", "Hello $1\n");
    ok(&run(&env, &server.url(), &["-p", "--no-session", "/hello39 ann"]));
    assert!(
        user_texts(&server.requests()[1])
            .iter()
            .any(|t| t.trim() == "Hello ann"),
        "discovered template expands"
    );
    ok(&run(
        &env,
        &server.url(),
        &["-np", "-p", "--no-session", "/hello39 ann"],
    ));
    assert!(
        user_texts(&server.requests()[2])
            .iter()
            .any(|t| t == "/hello39 ann"),
        "-np: not expanded"
    );
}

// pi: themes.md `--theme <path>`, `--use-theme <name>`, `--no-themes` (accepted in print mode)
#[test]
fn cli_040() {
    let server = fake(&turns(&["a", "b"]));
    let env = TestEnv::new();
    let theme = env.write("themes/t40.json", "{\"name\": \"t40\"}\n");
    let tdir = theme.parent().expect("dir").to_path_buf();
    ok(&run(
        &env,
        &server.url(),
        &[
            "--theme",
            tdir.to_str().expect("utf-8"),
            "--use-theme",
            "t40",
            "-p",
            "--no-session",
            "x",
        ],
    ));
    ok(&run(
        &env,
        &server.url(),
        &["--no-themes", "-p", "--no-session", "x"],
    ));
    assert_eq!(server.requests().len(), 2);
}

// pi: cli.md `-nc, --no-context-files` disables AGENTS.md and CLAUDE.md discovery
// invariant: AGENTS.md and CLAUDE.md are read as pi reads them (the context section of the prefix)
#[test]
fn cli_041() {
    let server = fake(&turns(&["a", "b"]));
    let env = TestEnv::new();
    env.write("work/AGENTS.md", "CTX-MARKER-41\n");
    ok(&run(&env, &server.url(), &["-p", "--no-session", "x"]));
    assert!(server.requests()[0].system.contains("CTX-MARKER-41"));
    ok(&run(&env, &server.url(), &["-nc", "-p", "--no-session", "x"]));
    assert!(!server.requests()[1].system.contains("CTX-MARKER-41"));
}

// pi: cli.md `--system-prompt <text|path>` replaces the system prompt (the preamble section)
// invariant: W3 the preamble is part of the frozen prefix
#[test]
fn cli_042() {
    let server = fake(&turns(&["a", "b"]));
    let env = TestEnv::new();
    ok(&run(
        &env,
        &server.url(),
        &["--system-prompt", "CUSTOM-SYS-42", "-p", "--no-session", "x"],
    ));
    let s = &server.requests()[0].system;
    assert!(s.contains("CUSTOM-SYS-42"));
    assert!(
        !s.contains("You are a coding assistant running inside oxa"),
        "the default preamble is replaced"
    );
    let file = env.write("sys42.md", "FILE-SYS-42\n");
    ok(&run(
        &env,
        &server.url(),
        &[
            "--system-prompt",
            file.to_str().expect("utf-8"),
            "-p",
            "--no-session",
            "x",
        ],
    ));
    assert!(server.requests()[1].system.contains("FILE-SYS-42"));
}

// pi: cli.md `--append-system-prompt <text|path>`, repeatable (the append section)
#[test]
fn cli_043() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    ok(&run(
        &env,
        &server.url(),
        &[
            "--append-system-prompt",
            "APP-A-43",
            "--append-system-prompt",
            "APP-B-43",
            "-p",
            "--no-session",
            "x",
        ],
    ));
    let s = &server.requests()[0].system;
    let (a, b) = (s.find("APP-A-43"), s.find("APP-B-43"));
    assert!(a.is_some() && b.is_some() && a < b, "both appended, in order");
}

// pi: cli.md `--tui-mode fullscreen|regular`
#[test]
fn cli_044() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    ok(&run(
        &env,
        &server.url(),
        &["--tui-mode", "regular", "-p", "--no-session", "x"],
    ));
    let bad = run(&env, &server.url(), &["--tui-mode", "weird", "-p", "x"]);
    assert_eq!(code(&bad), 2);
    assert!(err(&bad).contains("Valid values: regular, fullscreen"));
}

// pi: cli.md `--verbose` overrides quietStartup (print output is unchanged)
#[test]
fn cli_045() {
    let server = fake(&turns(&["same-45", "same-45"]));
    let env = TestEnv::new();
    let plain = run(&env, &server.url(), &["-p", "--no-session", "x"]);
    let verbose = run(&env, &server.url(), &["--verbose", "-p", "--no-session", "x"]);
    ok(&verbose);
    assert_eq!(out(&plain), out(&verbose), "stdout is the reply in both");
}

// pi: cli.md `-a, --approve` / `-na, --no-approve`: project trust for this process
#[test]
fn cli_046() {
    let server = fake(&turns(&["a", "b", "c"]));
    let env = TestEnv::new();
    env.write("work/.oxa/prompts/proj46.md", "Project $1\n");
    let msg = "/proj46 q";
    ok(&run(&env, &server.url(), &["-a", "-p", "--no-session", msg]));
    assert!(
        user_texts(&server.requests()[0])
            .iter()
            .any(|t| t.trim() == "Project q"),
        "trusted: expanded"
    );
    let o = run(&env, &server.url(), &["-na", "-p", "--no-session", msg]);
    ok(&o);
    assert!(
        user_texts(&server.requests()[1]).iter().any(|t| t == msg),
        "not trusted: literal"
    );
    assert!(err(&o).contains("not trusted"));
}

// pi: cli.md `--offline` "Disables automatic network activity" (oxa makes none; wave H P4:
// the flag is a hard switch, every connection of the harness is loopback-only, which keeps
// pi's meaning: the local engine still works)
// invariant: O-144 no automatic network activity; the engine request is the only request
#[test]
fn cli_047() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    ok(&run(
        &env,
        &server.url(),
        &["--offline", "-p", "--no-session", "x"],
    ));
    assert_eq!(server.requests().len(), 1);
    // A host that is not loopback is refused before any connect.
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"engine":{"allowRemote":true}}}"#,
    );
    let o = run(
        &env,
        "http://10.255.0.2:8092",
        &["--offline", "-p", "--no-session", "x"],
    );
    assert!(!o.status.success());
    assert!(err(&o).contains("offline mode is on"), "{}", err(&o));
}

// pi: cli.md `-h, --help` lists the options; `-v, --version` prints the version
#[test]
fn cli_048() {
    let server = fake(&turns(&[]));
    let env = TestEnv::new();
    let help = run(&env, &server.url(), &["--help"]);
    ok(&help);
    assert!(out(&help).contains("--extension, -e"));
    let ext = env.write("ext/flags48.js", "export default function (oxa) { oxa.registerFlag(\"widget48\", { type: \"string\", description: \"Widget name 48\" }); }\n");
    let with_ext = run(&env, &server.url(), &["-e", ext.to_str().expect("utf-8"), "-h"]);
    assert!(
        out(&with_ext).contains("--widget48"),
        "extension flags are listed"
    );
    let v = run(&env, &server.url(), &["-v"]);
    ok(&v);
    assert!(out(&v).starts_with("oxa "));
}

// pi: extensions.md extensions may register long options; unknown short options are rejected
#[test]
fn cli_049() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    let ext = env.write(
        "ext/flags49.js",
        "export default function (oxa) { oxa.registerFlag(\"widget49\", { type: \"string\" }); }\n",
    );
    let e = ext.to_str().expect("utf-8");
    ok(&run(
        &env,
        &server.url(),
        &["-e", e, "--widget49", "blue", "-p", "--no-session", "x"],
    ));
    let unknown = run(&env, &server.url(), &["-z", "-p", "x"]);
    assert_eq!(code(&unknown), 2);
    assert!(err(&unknown).contains("Unknown option: -z"));
}

// pi: cli.md mode choice by tty rules: with pipes (not terminals) print mode runs
#[test]
fn cli_050() {
    let server = fake(&turns(&["piped-50"]));
    let env = TestEnv::new();
    let o = run(&env, &server.url(), &["--no-session", "hi"]);
    ok(&o);
    assert_eq!(out(&o).trim_end(), "piped-50", "print mode, not the TUI");
}

// pi: packages.md `pi install <source>` / `pi remove` / `pi list`
// invariant: A-pkg git and local sources; `npm:` sources are refused (O-175, no network here)
#[test]
fn cli_051() {
    let server = fake(&turns(&[]));
    let env = TestEnv::new();
    let pkg = env.tmp().join("pkg-51");
    std::fs::create_dir_all(&pkg).expect("pkg");
    let p = pkg.to_str().expect("utf-8");
    ok(&run(&env, &server.url(), &["install", p]));
    let list = run(&env, &server.url(), &["list"]);
    ok(&list);
    assert!(out(&list).contains("pkg-51"));
    ok(&run(&env, &server.url(), &["remove", p]));
    assert!(!out(&run(&env, &server.url(), &["list"])).contains("pkg-51"));
    let npm = run(&env, &server.url(), &["install", "npm:pkg-51"]);
    assert!(!npm.status.success(), "npm: sources are refused");
    assert!(
        err(&npm).contains("npm package sources are not supported"),
        "{}",
        err(&npm)
    );
}

// pi: cli.md `pi config` chooses which package resources load (non-interactive: prints the settings)
#[test]
fn cli_052() {
    let server = fake(&turns(&[]));
    let env = TestEnv::new();
    let o = run(&env, &server.url(), &["config"]);
    ok(&o);
    assert!(out(&o).contains("settings from"));
}

// pi: cli.md `pi update` (self, --self, self, pi, --all) does not self-update
// invariant: O-144 no version check, no self-update, no network unless asked
// pi: cli.md `pi update --extensions` / `pi update <source>`
// invariant: A-pkg oxa update for git and local packages
#[test]
fn cli_055() {
    let server = fake(&turns(&[]));
    let env = TestEnv::new();
    let pkg = env.tmp().join("pkg-55");
    std::fs::create_dir_all(&pkg).expect("pkg");
    let p = pkg.to_str().expect("utf-8");
    ok(&run(&env, &server.url(), &["install", p]));
    ok(&run(&env, &server.url(), &["update", p]));
    ok(&run(&env, &server.url(), &["remove", p]));
}

// pi: cli.md `pi update --models` refreshes model catalogs (no catalogs in oxa)
// pi: cli.md `pi auth check|print-api-key|print-bearer-token` (no provider credentials in oxa)
// invariant: O-134 one provider type, no OAuth
// pi: cli.md `pi login --api-key`, `--no-browser` (no provider sign-in in oxa)
// invariant: O-134 no provider sign-in
// pi: mcp.md `pi mcp add <server> -- <command> [args]` with `--env` and `--cwd`
#[test]
fn cli_059() {
    let server = fake(&turns(&[]));
    let env = TestEnv::new();
    let cwd = env.tmp().join("mcp-cwd-59");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let c = cwd.to_str().expect("utf-8");
    ok(&run(
        &env,
        &server.url(),
        &[
            "mcp",
            "add",
            "srv59",
            "--env",
            "KEY59=VAL59",
            "--cwd",
            c,
            "--",
            "/bin/echo",
            "hi",
        ],
    ));
    let v = mcp_json(&env);
    assert_eq!(v["mcpServers"]["srv59"]["command"], "/bin/echo");
    assert_eq!(v["mcpServers"]["srv59"]["args"], serde_json::json!(["hi"]));
    assert_eq!(v["mcpServers"]["srv59"]["env"]["KEY59"], "VAL59");
    assert_eq!(v["mcpServers"]["srv59"]["cwd"], c);
}

// pi: mcp.md `pi mcp add <server> --url <url>` with `--header KEY=VALUE` and `--bearer-token-env-var`
#[test]
fn cli_060() {
    let server = fake(&turns(&[]));
    let env = TestEnv::new();
    ok(&run(
        &env,
        &server.url(),
        &[
            "mcp",
            "add",
            "web60",
            "--url",
            "http://127.0.0.1:9/mcp",
            "--header",
            "X-Team=blue",
            "--bearer-token-env-var",
            "TOK60",
        ],
    ));
    let v = mcp_json(&env);
    assert_eq!(v["mcpServers"]["web60"]["url"], "http://127.0.0.1:9/mcp");
    assert_eq!(v["mcpServers"]["web60"]["headers"]["X-Team"], "blue");
    assert_eq!(
        v["mcpServers"]["web60"]["headers"]["Authorization"],
        "Bearer ${TOK60}"
    );
}

// pi: mcp.md `pi mcp remove <server>` keeps unrelated content
#[test]
fn cli_061() {
    let server = fake(&turns(&[]));
    let env = TestEnv::new();
    ok(&run(
        &env,
        &server.url(),
        &["mcp", "add", "keep61", "--", "/bin/echo", "k"],
    ));
    ok(&run(
        &env,
        &server.url(),
        &["mcp", "add", "drop61", "--", "/bin/echo", "d"],
    ));
    ok(&run(&env, &server.url(), &["mcp", "remove", "drop61"]));
    let v = mcp_json(&env);
    assert!(v["mcpServers"].get("drop61").is_none());
    assert_eq!(
        v["mcpServers"]["keep61"]["args"],
        serde_json::json!(["k"]),
        "the other server is untouched"
    );
}

// pi: mcp.md `pi mcp list [--json]` connects to every enabled server; exit 1 when one is not connected
#[test]
fn cli_062() {
    let server = fake(&turns(&[]));
    let env = TestEnv::new();
    ok(&run(
        &env,
        &server.url(),
        &["mcp", "add", "bad62", "--", "/nonexistent/mcp-62"],
    ));
    let o = run(&env, &server.url(), &["mcp", "list"]);
    assert_eq!(code(&o), 1, "a server that is not connected: {}", out(&o));
    assert!(out(&o).contains("bad62: failed"), "{}", out(&o));

    // A server that starts is connected and its tools are listed; with only that one the
    // exit code is 0. (The fixture MCP server of the mcp rows, run by python3 -I.)
    let script = env.write(
        "mcp/server.py",
        &std::fs::read_to_string(oxa_parity::fixture("goldens/mcp/server.py")).expect("fixture server"),
    );
    let rec = env.root().join("rec62");
    std::fs::create_dir_all(&rec).expect("record dir");
    ok(&run(
        &env,
        &server.url(),
        &[
            "mcp",
            "add",
            "good62",
            "--",
            "python3",
            "-I",
            script.to_str().expect("utf-8"),
            rec.to_str().expect("utf-8"),
            "plain",
        ],
    ));
    ok(&run(&env, &server.url(), &["mcp", "remove", "bad62"]));
    let o = run(&env, &server.url(), &["mcp", "list"]);
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(out(&o).contains("good62: connected, "), "{}", out(&o));
    assert!(out(&o).contains("  tools: "), "{}", out(&o));
}

// pi: mcp.md `pi mcp logout <server>` deletes stored OAuth credentials (none stored: nothing to do).
// pi extensions/mcp/cli.ts: logout applies to OAuth servers (HTTP without an Authorization
// header); a stdio server "does not use OAuth" and exits 1.
#[test]
fn cli_063() {
    let server = fake(&turns(&[]));
    let env = TestEnv::new();
    ok(&run(
        &env,
        &server.url(),
        &["mcp", "add", "srv63", "--url", "https://127.0.0.1:9/mcp"],
    ));
    let o = run(&env, &server.url(), &["mcp", "logout", "srv63"]);
    ok(&o);
    assert!(out(&o).contains("No stored credentials"), "{}", out(&o));
    ok(&run(
        &env,
        &server.url(),
        &["mcp", "add", "stdio63", "--", "/bin/echo", "x"],
    ));
    let o = run(&env, &server.url(), &["mcp", "logout", "stdio63"]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("does not use OAuth"), "{}", err(&o));
    // `login` connects first; an unreachable server is reported, not signed in to.
    let o = run(&env, &server.url(), &["mcp", "login", "srv63", "--timeout", "5"]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("failed to connect"), "{}", err(&o));
}

// pi: cli.md No engine reachable is a one-line error with a hint
// invariant: the engine is the only provider (engine_down, exit 2)
#[test]
fn cli_064() {
    let env = TestEnv::new();
    let o = run(&env, &closed_url(), &["-p", "--no-session", "x"]);
    assert_eq!(code(&o), 2);
    let e = err(&o);
    assert_eq!(e.trim_end().lines().count(), 1, "one line: {e}");
    assert!(e.contains("cannot reach the engine"), "{e}");
}

// ----- MOD rows ------------------------------------------------------------------------------

// pi: models.md `/model` and the selector switch the model; oxa resolves it over the engine list
// invariant: W3 a switch is a new epoch
#[test]
fn mod_001() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    ok(&run(
        &env,
        &server.url(),
        &["--model", "fake-model", "-p", "--no-session", "x"],
    ));
    assert_eq!(server.requests()[0].model, "fake-model");
    let miss = run(&env, &server.url(), &["--model", "not-on-engine", "-p", "x"]);
    assert_eq!(code(&miss), 1);
    assert!(
        err(&miss).contains("available"),
        "lists the engine models: {}",
        err(&miss)
    );
}

// pi: models-configuration.md `models.json` custom models; oxa names engine endpoints with an optional bearer key
// invariant: O-144 only the engine endpoint can be called; other provider types are refused
#[test]
fn mod_002() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    let endpoint = server.url();
    env.write(
        "home/.oxa/agent/models.json",
        &serde_json::json!({
            "providers": {
                "lan": { "baseUrl": endpoint, "api": "openai-completions", "apiKey": "literal-key-002" },
                "claudeish": { "baseUrl": "https://api.anthropic.com", "api": "anthropic-messages" }
            }
        })
        .to_string(),
    );
    let o = run(
        &env,
        &server.url(),
        &["--model", "lan/fake-model", "-p", "--no-session", "x"],
    );
    ok(&o);
    let auth = server.requests()[0]
        .headers
        .iter()
        .find(|(k, _)| k == "authorization")
        .map(|(_, v)| v.clone());
    assert_eq!(auth.as_deref(), Some("Bearer literal-key-002"));
    assert!(
        err(&o).contains("claudeish") && err(&o).contains("not an engine endpoint"),
        "{}",
        err(&o)
    );
}

// pi: models.md thinking levels off..max, clamped per model; oxa: boolean on the wire
// invariant: W6 explicit sampling; thinkingBudgets are ignored with a warning
#[test]
fn mod_003() {
    let server = fake(&turns(&["a", "b"]));
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"thinkingBudgets": {"high": 1000}}"#,
    );
    let o = run(
        &env,
        &server.url(),
        &["--thinking", "low", "-p", "--no-session", "x"],
    );
    ok(&o);
    assert!(server.requests()[0].thinking);
    assert!(
        err(&o).contains("thinkingBudgets") && err(&o).contains("ignored"),
        "warning: {}",
        err(&o)
    );
    ok(&run(
        &env,
        &server.url(),
        &["--thinking", "off", "-p", "--no-session", "x"],
    ));
    assert!(!server.requests()[1].thinking);
}

// pi: models.md a model without thinking support defaults to off
// invariant: W6 oxa only: a model without preserved thinking starts with thinking off
#[test]
fn mod_004() {
    let flash = fake_opts(
        &turns(&["a"]),
        FakeOpts {
            model: "flash-004".to_string(),
            models: vec![FakeModel::flash("flash-004", 16384, Some(2048))],
            ..FakeOpts::default()
        },
    );
    let env = TestEnv::new();
    ok(&run(
        &env,
        &flash.url(),
        &["--model", "flash-004", "-p", "--no-session", "x"],
    ));
    let r = &flash.requests()[0];
    assert!(!r.thinking, "thinking defaults to off");
    assert_eq!(
        r.preserve_thinking, None,
        "the flash model does not preserve thinking"
    );
}

// pi: llama-cpp.md the router is managed by `/llama`; oxa: `/engine` shows status and loads a model
// invariant: A-engine status and model load over the controller's /_local/status
// headless: `oxa -p /engine` runs the built-in (oxa_core::run_builtin) and prints the status
#[test]
fn mod_010() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    let o = run(&env, &server.url(), &["-p", "--no-session", "/engine"]);
    ok(&o);
    assert!(out(&o).contains("fake-model"), "{}", out(&o));
}

// ----- ENV rows ------------------------------------------------------------------------------

// pi: environment-variables.md `PI_*` process marker for child processes: AI_AGENT=oxa
#[test]
fn env_001() {
    let server = fake(&format!(
        "{}\n{}",
        bash_turn("echo AI=$AI_AGENT"),
        turns(&["done"])
    ));
    let env = TestEnv::new();
    ok(&run(&env, &server.url(), &["-p", "--no-session", "x"]));
    let reqs = server.requests();
    assert!(reqs[1]
        .messages
        .iter()
        .any(|m| m.content.as_deref().unwrap_or("").contains("AI=oxa")));
}

// pi: environment-variables.md `PI_SESSION_ID`, `PI_SESSION_FILE` (unset when ephemeral)
#[test]
fn env_002() {
    let server = fake(&format!(
        "{}\n{}",
        bash_turn("echo FILE=$OXA_SESSION_FILE"),
        turns(&["done"])
    ));
    let env = TestEnv::new();
    ok(&run(&env, &server.url(), &["-p", "--session-id", "env2", "x"]));
    let reqs = server.requests();
    let tool = reqs[1]
        .messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::Tool))
        .and_then(|m| m.content.clone())
        .unwrap_or_default();
    assert!(
        tool.contains("FILE=") && tool.contains(".jsonl"),
        "shell tool sees its session file: {tool}"
    );
}

// pi: environment-variables.md `PI_CLI_*` model and reasoning level for shell tools
// invariant: W3 set per run, never on the wire
#[test]
fn env_003() {
    let server = fake(&format!(
        "{}\n{}",
        bash_turn("echo M=$OXA_MODEL R=$OXA_REASONING_LEVEL P=$OXA_PROVIDER"),
        turns(&["done"])
    ));
    let env = TestEnv::new();
    ok(&run(
        &env,
        &server.url(),
        &["--thinking", "high", "-p", "--no-session", "x"],
    ));
    let reqs = server.requests();
    let tool = reqs[1]
        .messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::Tool))
        .and_then(|m| m.content.clone())
        .unwrap_or_default();
    assert!(
        tool.contains("M=fake-model") && tool.contains("R=high") && tool.contains("P=oxe"),
        "{tool}"
    );
}

// pi: environment-variables.md `PI_CODING_AGENT_DIR` overrides the config directory
#[test]
fn env_004() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    let dir = env.tmp().join("agent-dir-004");
    ok(&exec(
        &env,
        &server.url(),
        &["-p", "x"],
        "",
        &[("OXA_AGENT_DIR", dir.to_str().expect("utf-8"))],
        None,
    ));
    assert!(
        !session_files_in(&dir.join("sessions")).is_empty(),
        "sessions live in the overridden directory"
    );
}

// pi: environment-variables.md `PI_CODING_AGENT_SESSION_DIR`, overridden by --session-dir
#[test]
fn env_005() {
    let server = fake(&turns(&["a", "b"]));
    let env = TestEnv::new();
    let env_dir = env.tmp().join("env-005");
    let flag_dir = env.tmp().join("flag-005");
    let e = env_dir.to_str().expect("utf-8");
    ok(&exec(
        &env,
        &server.url(),
        &["-p", "x"],
        "",
        &[("OXA_SESSION_DIR", e)],
        None,
    ));
    assert!(!session_files_in(&env_dir).is_empty());
    let f = flag_dir.to_str().expect("utf-8");
    ok(&exec(
        &env,
        &server.url(),
        &["-p", "--session-dir", f, "y"],
        "",
        &[("OXA_SESSION_DIR", e)],
        None,
    ));
    assert!(
        !session_files_in(&flag_dir).is_empty(),
        "the flag wins over the env"
    );
}

// pi: environment-variables.md `PI_PACKAGE_DIR` (package directory override)
// invariant: O-134 the override is accepted; no package store is read from it by the print path
#[test]
fn env_006() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    let o = exec(
        &env,
        &server.url(),
        &["-p", "--no-session", "x"],
        "",
        &[("OXA_PACKAGE_DIR", "/nonexistent/pkg-006")],
        None,
    );
    ok(&o);
    assert!(
        !err(&o).contains("unsupported"),
        "accepted without a warning: {}",
        err(&o)
    );
}

// pi: environment-variables.md `PI_OFFLINE` (oxa: OXA_OFFLINE accepted; no automatic network)
// invariant: O-144 no automatic network activity
#[test]
fn env_007() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    let o = exec(
        &env,
        &server.url(),
        &["-p", "--no-session", "x"],
        "",
        &[("OXA_OFFLINE", "1")],
        None,
    );
    ok(&o);
    assert_eq!(server.requests().len(), 1);
    assert!(!err(&o).contains("warning"), "{}", err(&o));
}

// pi: environment-variables.md `PI_CODEMODE_MAX_WORKERS` (default 2)
// invariant: A-codemode the worker cap is read from OXA_CODEMODE_MAX_WORKERS (codemode itself is off by default)
#[test]
fn env_009() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    let o = exec(
        &env,
        &server.url(),
        &["-p", "--no-session", "x"],
        "",
        &[("OXA_CODEMODE_MAX_WORKERS", "3")],
        None,
    );
    ok(&o);
    assert!(!err(&o).contains("unknown"), "accepted: {}", err(&o));
}

// pi: environment-variables.md `PI_CLEAR_ON_SHRINK`, `PI_HARDWARE_CURSOR` (TUI rendering, accepted in print mode)
#[test]
fn env_012() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    let o = exec(
        &env,
        &server.url(),
        &["-p", "--no-session", "x"],
        "",
        &[("OXA_CLEAR_ON_SHRINK", "1")],
        None,
    );
    ok(&o);
    assert_eq!(out(&o).trim_end(), "a");
}

// pi: environment-variables.md `PI_CLIPBOARD_OSC52`, `PI_HYPERLINKS`, `PI_PROGRAM_STATUS`, `PI_IMAGE_PROTOCOL`, `PI_TRUE_COLOR`
#[test]
fn env_013() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    let vars = [("OXA_CLIPBOARD_OSC52", "1")];
    let o = exec(&env, &server.url(), &["-p", "--no-session", "x"], "", &vars, None);
    ok(&o);
    assert_eq!(out(&o).trim_end(), "a");
}

// pi: environment-variables.md `VISUAL`, `EDITOR` external editor fallback (the editor runs only in the TUI)
#[test]
fn env_015() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    let o = exec(
        &env,
        &server.url(),
        &["-p", "--no-session", "x"],
        "",
        &[("VISUAL", "vi"), ("EDITOR", "nano")],
        None,
    );
    ok(&o);
    assert_eq!(out(&o).trim_end(), "a");
}

// pi: environment-variables.md `HTTP_PROXY`, `HTTPS_PROXY` apply to MCP HTTP only
// invariant: O-144 the engine endpoint is never routed through a proxy
#[test]
fn env_016() {
    let server = fake(&turns(&["a"]));
    let env = TestEnv::new();
    let dead = closed_url();
    let o = exec(
        &env,
        &server.url(),
        &["-p", "--no-session", "x"],
        "",
        &[("HTTP_PROXY", dead.as_str()), ("HTTPS_PROXY", dead.as_str())],
        None,
    );
    ok(&o);
    assert_eq!(
        out(&o).trim_end(),
        "a",
        "the engine request does not use the proxy"
    );
}

// invariant: A-docs the ported quickstart runs its own command lines against the one-slot fake engine
#[test]
fn out_011() {
    fn words(line: &str) -> Vec<String> {
        let (mut out, mut cur, mut have, mut quoted) = (Vec::new(), String::new(), false, false);
        for c in line.chars() {
            match c {
                '"' => {
                    quoted = !quoted;
                    have = true;
                }
                ' ' if !quoted => {
                    if have {
                        out.push(std::mem::take(&mut cur));
                        have = false;
                    }
                }
                c => {
                    cur.push(c);
                    have = true;
                }
            }
        }
        if have {
            out.push(cur);
        }
        out
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    for rel in ["docs/README.md", "docs/pages/quickstart.md"] {
        assert!(
            root.join(rel).is_file(),
            "{rel} is missing (the docs lane ports it)"
        );
    }
    let page = std::fs::read_to_string(root.join("docs/pages/quickstart.md")).expect("quickstart.md");
    // The `sh` fenced lines: `open` is Some(is_sh) inside a fence.
    let mut open: Option<bool> = None;
    let mut lines = Vec::new();
    for line in page.lines() {
        let t = line.trim();
        match (open, t.strip_prefix("```")) {
            (None, Some(info)) => open = Some(info.trim() == "sh"),
            (Some(_), Some(_)) => open = None,
            (Some(true), None) if !t.is_empty() => lines.push(t.to_string()),
            _ => {}
        }
    }
    assert!(!lines.is_empty(), "quickstart.md has no `sh` command lines");
    let env = TestEnv::new();
    let server = fake(&turns(&["ok"; 16]));
    for line in &lines {
        let rest = line
            .strip_prefix("oxa ")
            .unwrap_or_else(|| panic!("a sh line must start with `oxa `: {line}"));
        let args = words(rest);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let o = run(&env, &server.url(), &refs);
        assert_eq!(code(&o), 0, "`{line}` exit code; stderr: {}", err(&o));
    }
}
