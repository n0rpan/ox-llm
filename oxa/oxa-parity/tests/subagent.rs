//! Wave H PARITY tests (lane fmt): the `subagent` rows of PARITY.md section "ENG: Engine
//! support" (`ENG-010`..`ENG-019` -> `fn eng_010()`..), plus the four meta-checks every parity
//! file has.
//!
//! Every row test drives the real `oxa` binary as the parent (print, `--mode json` or
//! `--mode rpc`) against the in-process fake engine inside an isolated `TestEnv`; the parent
//! starts the real `oxa` again as each child (`--mode json -p --no-session`), and the children
//! talk to the same fake. One script serves parent and children in arrival order, which is
//! deterministic because the parent is blocked inside the tool call while a child runs.
//! Assertions are on the requests and headers the fake recorded, its timings and cache clears,
//! the parent's events and session file, and the processes left behind (none). Every wait is
//! bounded.
//!
//! The tool is on by default on a large non-Flash model (O-146; the fake's default model is
//! one), so no setting is needed unless a row is about the setting.
//! Conventions: `invariant: <name>` names what forces each adapted row (all rows here are A);
//! a row the product does not implement fails with a message that starts `RED <ID>:`.

use oxa_fake::{FakeCaps, FakeModel, FakeOpts, FakeServer, Fault, Recorded, ScriptTurn, TestEnv};
use oxa_parity::oxa_bin;
use oxa_proto::Role;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

/// Every wait of this file is bounded by this.
const LIMIT: Duration = Duration::from_secs(90);

/// The scout agent: `read` only, a system prompt of its own.
const SCOUT: &str = "---\nname: scout\ndescription: Looks around\ntools: read\n---\nYou are a scout.\n";

fn fake(script: Vec<ScriptTurn>, opts: FakeOpts) -> FakeServer {
    FakeServer::start(script, opts).expect("fake engine")
}

fn text(t: &str) -> ScriptTurn {
    ScriptTurn::text(t)
}

/// The parent's reply that calls `subagent` with `args`.
fn call(args: Value) -> ScriptTurn {
    ScriptTurn::tool("sub_1", "subagent", &args.to_string())
}

/// A test env with the scout agent and, when given, the agent `settings.json`.
fn env_with(settings: Option<&str>) -> TestEnv {
    let env = TestEnv::new();
    env.write("home/.oxa/agent/agents/scout.md", SCOUT);
    if let Some(s) = settings {
        env.write("home/.oxa/agent/settings.json", s);
    }
    env
}

/// One finished `oxa` run.
struct Ran {
    code: Option<i32>,
    out: String,
    err: String,
}

impl Ran {
    fn ok(&self) -> &Ran {
        assert_eq!(
            self.code,
            Some(0),
            "oxa failed\nstdout: {}\nstderr: {}",
            self.out,
            self.err
        );
        self
    }
    /// The JSON records of stdout (`--mode json`).
    fn records(&self) -> Vec<Value> {
        self.out
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect()
    }
}

/// `oxa <args>` in the work directory with `env.envs(url)` + `extra`,
/// stdin closed; killed and failed after [`LIMIT`].
fn oxa_with(env: &TestEnv, url: &str, args: &[&str], extra: &[(&str, &str)]) -> Ran {
    let mut child = Command::new(oxa_bin())
        .args(args)
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs(url))
        .envs(oxa_parity::note_off_env())
        .envs(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oxa (build it first: cargo build -p oxa-cli -p oxa-fake)");
    let mut so = child.stdout.take().expect("stdout");
    let mut se = child.stderr.take().expect("stderr");
    let to = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = so.read_to_string(&mut s);
        s
    });
    let te = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = se.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().expect("wait for oxa") {
            break st;
        }
        if start.elapsed() > LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("oxa {args:?} did not finish within {LIMIT:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ran {
        code: status.code(),
        out: to.join().unwrap_or_default(),
        err: te.join().unwrap_or_default(),
    }
}

/// The parent in JSON mode with one prompt.
fn parent(env: &TestEnv, url: &str, prompt: &str) -> Ran {
    oxa_with(env, url, &["--mode", "json", "-p", prompt], &[])
}

/// Every request, in arrival order.
fn sorted(server: &FakeServer) -> Vec<Recorded> {
    let mut reqs = server.requests();
    reqs.sort_by_key(|r| r.seq);
    reqs
}

fn header<'a>(r: &'a Recorded, name: &str) -> Option<&'a str> {
    r.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn declares_subagent(r: &Recorded) -> bool {
    r.tools_json.contains("\"name\":\"subagent\"")
}

/// The parent session id: the session of the first request.
fn parent_id(reqs: &[Recorded]) -> String {
    reqs[0].session.clone()
}

/// The requests of child `n` of `parent` (`<parent>.s<n>`).
fn child_reqs(reqs: &[Recorded], parent: &str, n: u32) -> Vec<Recorded> {
    let hint = format!("{parent}.s{n}");
    reqs.iter().filter(|r| r.session == hint).cloned().collect()
}

/// The requests of any child (a session hint `<id>.s<n>`).
fn any_child(reqs: &[Recorded]) -> Vec<Recorded> {
    reqs.iter()
        .filter(|r| {
            r.session
                .rsplit_once(".s")
                .is_some_and(|(_, n)| n.parse::<u32>().is_ok())
        })
        .cloned()
        .collect()
}

/// The text of the last tool message of a request (the `subagent` result on the wire).
fn tool_result(r: &Recorded) -> String {
    r.messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .and_then(|m| m.content.clone())
        .unwrap_or_else(|| panic!("a tool message in {:?}", r.messages))
}

/// The `tool_execution_end` record of the parent's JSON output.
fn tool_end(records: &[Value]) -> Value {
    records
        .iter()
        .find(|r| r["type"] == "tool_execution_end")
        .cloned()
        .unwrap_or_else(|| panic!("a tool_execution_end record: {records:#?}"))
}

/// The processes of this test env that are subagent children (`OXA_SESSION_HINT` in their
/// environment, `HOME` = the env's home).
fn live_children(env: &TestEnv) -> Vec<u32> {
    let home = format!("HOME={}", env.home().display());
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in rd.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(environ) = std::fs::read(e.path().join("environ")) else {
            continue;
        };
        let vars: Vec<String> = environ
            .split(|b| *b == 0)
            .map(|v| String::from_utf8_lossy(v).into_owned())
            .collect();
        let state = std::fs::read_to_string(e.path().join("stat")).unwrap_or_default();
        let zombie = state
            .rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with('Z'));
        if !zombie && vars.contains(&home) && vars.iter().any(|v| v.starts_with("OXA_SESSION_HINT=")) {
            out.push(pid);
        }
    }
    out
}

/// Wait (bounded) until `f` holds.
fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < LIMIT, "waited {LIMIT:?} for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A line-oriented driver of `oxa --mode rpc` with bounded waits; the child is killed on drop.
struct Rpc {
    child: Child,
    rx: Receiver<Value>,
    next: u64,
    events: Vec<Value>,
}

impl Rpc {
    fn start(env: &TestEnv, url: &str) -> Rpc {
        let mut child = Command::new(oxa_bin())
            .args(["--mode", "rpc"])
            .current_dir(env.work())
            .env_clear()
            .envs(env.envs(url))
            .envs(oxa_parity::note_off_env())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn oxa --mode rpc");
        let stdout = child.stdout.take().expect("rpc stdout");
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            }
        });
        Rpc {
            child,
            rx,
            next: 0,
            events: Vec::new(),
        }
    }

    fn next_record(&mut self) -> Value {
        self.rx.recv_timeout(LIMIT).unwrap_or_else(|_| {
            panic!(
                "no rpc record within {LIMIT:?}; events so far: {:#?}",
                self.events
            )
        })
    }

    fn call(&mut self, mut cmd: Value) -> Value {
        self.next += 1;
        let id = format!("sub-{}", self.next);
        cmd["id"] = json!(id);
        {
            let stdin = self.child.stdin.as_mut().expect("rpc stdin");
            writeln!(stdin, "{cmd}").expect("write rpc command");
            stdin.flush().expect("flush rpc command");
        }
        loop {
            let v = self.next_record();
            if v["type"] == "response" && v["id"] == json!(id) {
                return v;
            }
            self.events.push(v);
        }
    }

    fn ok(&mut self, cmd: Value) -> Value {
        let r = self.call(cmd.clone());
        assert_eq!(r["success"], json!(true), "{cmd} failed: {r}");
        r["data"].clone()
    }

    fn settle(&mut self) {
        if let Some(i) = self.events.iter().position(|e| e["type"] == "agent_settled") {
            self.events.remove(i);
            return;
        }
        loop {
            let v = self.next_record();
            if v["type"] == "agent_settled" {
                return;
            }
            self.events.push(v);
        }
    }

    fn prompt(&mut self, text: &str) {
        self.ok(json!({"type": "prompt", "message": text}));
        self.settle();
    }

    /// Close stdin (shutdown) and wait (bounded) for the exit; returns its code.
    fn finish(&mut self) -> Option<i32> {
        drop(self.child.stdin.take());
        let start = Instant::now();
        loop {
            if let Some(st) = self.child.try_wait().expect("wait for oxa") {
                return st.code();
            }
            assert!(
                start.elapsed() < LIMIT,
                "oxa --mode rpc did not exit after shutdown"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

// ---------------------------------------------------------------- rows

/// ENG-010: at most one child process alive; `tasks` run in call order and show as
/// `subagent k/n, queued`.
/// invariant: one engine slot: parallel children would only queue and park each other, so the
/// "parallel" shape is a queue (concurrency 1, E11).
#[test]
fn eng_010() {
    let env = env_with(None);
    let server = fake(
        vec![
            call(json!({"tasks": [
                {"agent": "scout", "task": "first"},
                {"agent": "scout", "task": "second"},
                {"agent": "scout", "task": "third"},
            ]})),
            text("one"),
            text("two"),
            text("three"),
            text("parent done"),
        ],
        FakeOpts::default(),
    );
    let run = parent(&env, &server.url(), "delegate three");
    run.ok();
    let reqs = sorted(&server);
    let p = parent_id(&reqs);
    // Call order: child n carries task n.
    for (n, task) in [(1, "Task: first"), (2, "Task: second"), (3, "Task: third")] {
        let c = child_reqs(&reqs, &p, n);
        assert_eq!(
            c.len(),
            1,
            "child {n}: {:?}",
            reqs.iter().map(|r| &r.session).collect::<Vec<_>>()
        );
        let user = c[0].messages[0].content.clone().unwrap_or_default();
        assert!(user.contains(task), "child {n} runs {task:?}: {user:?}");
    }
    let order: Vec<String> = any_child(&reqs).iter().map(|r| r.session.clone()).collect();
    assert_eq!(
        order,
        [".s1", ".s2", ".s3"].map(|s| format!("{p}{s}")),
        "call order"
    );
    // One at a time: each child's request ended before the next one started, and the engine
    // never had two requests at once.
    assert_eq!(server.max_in_flight(), 1, "one request at a time");
    let timing = |r: &Recorded| {
        server
            .timings()
            .into_iter()
            .find(|t| t.seq == Some(r.seq))
            .unwrap_or_else(|| panic!("a timing for request {}", r.seq))
    };
    let kids = any_child(&reqs);
    for w in kids.windows(2) {
        assert!(
            timing(&w[0]).end_ms <= timing(&w[1]).start_ms,
            "children do not overlap"
        );
    }
    assert!(live_children(&env).is_empty(), "no child is left behind");
    // The progress the parent streams: the tasks after the running one are queued.
    let progress: Vec<String> = run
        .records()
        .iter()
        .filter(|r| r["type"] == "tool_execution_update")
        .map(|r| r.to_string())
        .collect();
    assert!(
        progress
            .iter()
            .any(|p| p.contains("subagent 2/3, queued") && p.contains("subagent 3/3, queued")),
        "while task 1 runs, 2 and 3 show as queued: {progress:#?}"
    );
    assert!(
        progress
            .iter()
            .any(|p| p.contains("subagent 3/3, queued") && !p.contains("subagent 2/3, queued")),
        "while task 2 runs, 3 shows as queued: {progress:#?}"
    );
}

/// ENG-011: at most eight children per parent session: more than 8 `tasks` or `chain` steps are
/// refused as a tool error that starts nothing; eight are accepted.
/// invariant: O-146 (one engine slot; pi caps only the `tasks` array, oxa caps the chain too and
/// counts the children that run or wait per parent session).
#[test]
fn eng_011() {
    let nine: Vec<Value> = (1..=9)
        .map(|i| json!({"agent": "scout", "task": format!("t{i}")}))
        .collect();
    for (shape, want) in [
        ("tasks", "Too many parallel tasks (9). Max is 8."),
        ("chain", "Too many chain steps (9). Max is 8."),
    ] {
        let env = env_with(None);
        let server = fake(
            vec![call(json!({ shape: nine })), text("parent done")],
            FakeOpts::default(),
        );
        let run = parent(&env, &server.url(), "delegate nine");
        run.ok();
        let reqs = sorted(&server);
        assert!(any_child(&reqs).is_empty(), "{shape}: no child starts");
        assert!(
            !reqs.iter().any(|r| r.max_tokens == 0 && r.snapshot_keep),
            "{shape}: not even the park"
        );
        assert_eq!(tool_result(reqs.last().unwrap()), want, "{shape}");
        let end = tool_end(&run.records());
        assert_eq!(end["isError"], true, "{shape}: a tool error: {end}");
    }
    // Exactly eight run, one after another.
    let env = env_with(None);
    let eight: Vec<Value> = nine[..8].to_vec();
    let mut script = vec![call(json!({ "tasks": eight }))];
    script.extend((1..=8).map(|i| text(&format!("r{i}"))));
    script.push(text("parent done"));
    let server = fake(script, FakeOpts::default());
    parent(&env, &server.url(), "delegate eight").ok();
    let reqs = sorted(&server);
    assert_eq!(any_child(&reqs).len(), 8, "eight children");
    assert!(tool_result(reqs.last().unwrap()).starts_with("Parallel: 8/8 succeeded"));
}

/// ENG-012: each child sends `X-Session-Id: <parent>.s<n>` (n counts up per parent session),
/// `X-Oxe-Priority: background` and turn kind `subagent`.
/// invariant: the engine parks and restores per session; a human at another prompt goes first
/// (H3 priority).
#[test]
fn eng_012() {
    let env = env_with(None);
    let mut second = call(json!({"agent": "scout", "task": "second look"}));
    second.tool_calls[0].id = "sub_2".to_string();
    let server = fake(
        vec![
            call(json!({"agent": "scout", "task": "first look"})),
            text("found one"),
            second,
            text("found two"),
            text("parent done"),
        ],
        FakeOpts::default(),
    );
    parent(&env, &server.url(), "look twice").ok();
    let reqs = sorted(&server);
    let p = parent_id(&reqs);
    let file = session_id_of(&env);
    assert_eq!(p, file, "the parent's requests name its session");
    for r in reqs.iter().filter(|r| r.session == p && r.max_tokens > 0) {
        assert_eq!(header(r, "x-session-id"), Some(p.as_str()));
        assert!(!r.background, "the parent is foreground");
        assert_eq!(r.kind, "chat");
    }
    for n in [1, 2] {
        let hint = format!("{p}.s{n}");
        let c = child_reqs(&reqs, &p, n);
        assert_eq!(c.len(), 1, "child {n} (counted per parent session across calls)");
        assert_eq!(header(&c[0], "x-session-id"), Some(hint.as_str()));
        assert_eq!(header(&c[0], "x-oxe-priority"), Some("background"));
        assert!(c[0].background);
        assert_eq!(c[0].kind, "subagent", "turn kind subagent");
    }
}

/// The session id in the header of the env's one session file.
fn session_id_of(env: &TestEnv) -> String {
    let files = session_files(&env.agent_dir().join("sessions"));
    assert_eq!(
        files.len(),
        1,
        "one parent session (the children run --no-session): {files:?}"
    );
    let first = std::fs::read_to_string(&files[0]).unwrap();
    let h: Value = serde_json::from_str(first.lines().next().unwrap()).unwrap();
    h["id"].as_str().unwrap().to_string()
}

fn session_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(session_files(&p));
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
    out
}

/// ENG-013: depth 1: a child never loads `builtin:subagent`, even where the setting turns the
/// tool on.
/// invariant: a nested child would park its parent's child (one engine slot).
#[test]
fn eng_013() {
    // A helper agent with the default tool selection (no `tools:` list).
    let env = env_with(Some(r#"{"oxa": {"subagent": {"enabled": true}}}"#));
    env.write(
        "home/.oxa/agent/agents/helper.md",
        "---\nname: helper\ndescription: Helps\n---\nYou help.\n",
    );
    let server = fake(
        vec![
            call(json!({"agent": "helper", "task": "help"})),
            text("helped"),
            text("parent done"),
        ],
        FakeOpts::default(),
    );
    parent(&env, &server.url(), "get help").ok();
    let reqs = sorted(&server);
    let p = parent_id(&reqs);
    assert!(declares_subagent(&reqs[0]), "the parent declares the tool");
    let c = child_reqs(&reqs, &p, 1);
    assert_eq!(c.len(), 1);
    assert!(
        !declares_subagent(&c[0]),
        "the child does not: {}",
        c[0].tools_json
    );
    assert!(
        c[0].tools_json.contains("\"name\":\"read\""),
        "the child keeps its other tools"
    );
    // The same rule for any oxa started as a child (`OXA_SUBAGENT_DEPTH=1`).
    let server = fake(vec![text("ok")], FakeOpts::default());
    oxa_with(
        &env,
        &server.url(),
        &["-p", "--no-session", "hi"],
        &[("OXA_SUBAGENT_DEPTH", "1"), ("OXA_SESSION_HINT", "abc.s1")],
    )
    .ok();
    let r = &sorted(&server)[0];
    assert!(!declares_subagent(r), "depth 1 never declares subagent");
}

/// ENG-014: an agent's `model:` other than the session's is ignored with a warning; the child
/// runs on the session's model.
/// invariant: one model loaded; a swap cold-prefills the whole wire.
#[test]
fn eng_014() {
    let env = env_with(None);
    env.write(
        "home/.oxa/agent/agents/picky.md",
        "---\nname: picky\ndescription: Wants another model\nmodel: other-model\n---\nYou are picky.\n",
    );
    let server = fake(
        vec![
            call(json!({"agent": "picky", "task": "pick"})),
            text("picked"),
            text("parent done"),
        ],
        FakeOpts {
            models: vec![
                FakeModel::moe("fake-model", 1 << 20),
                FakeModel::moe("other-model", 1 << 20),
            ],
            ..FakeOpts::default()
        },
    );
    let run = parent(&env, &server.url(), "be picky");
    run.ok();
    let reqs = sorted(&server);
    let p = parent_id(&reqs);
    let c = child_reqs(&reqs, &p, 1);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].model, "fake-model", "the session's model");
    assert!(
        reqs.iter().all(|r| r.model == "fake-model"),
        "no request names the other model"
    );
    let warned = run.records().iter().any(|r| {
        let s = r.to_string();
        s.contains("picky") && s.contains("other-model") && s.contains("ignored")
    }) || (run.err.contains("other-model") && run.err.contains("ignored"));
    assert!(
        warned,
        "a warning names the ignored model\nstdout: {}\nstderr: {}",
        run.out, run.err
    );
}

/// ENG-015: child soft compaction limit = min(`oxa.subagent.softTokens` 24000, the parent's
/// `compact_at`, cap - 1); the child compacts by W5.
/// invariant: W5 compaction in the child; a 24K child costs about 2.3 min of prefill in total.
/// (The fake engine publishes no `compact_at`, so the parent's `compact_at` term is not driven
/// here; the default 24000 and the setting are.)
#[test]
fn eng_015() {
    /// Run one child with a task of `task_len` bytes; returns (the child's context after its
    /// reply, whether it compacted, the child's requests).
    fn one(soft_tokens: Option<u32>, task_len: usize) -> (u64, bool, Vec<Recorded>) {
        // A one-token kept tail, so a two-message conversation has something to summarise.
        let mut settings = json!({"compaction": {"keepRecentTokens": 1}});
        if let Some(n) = soft_tokens {
            settings["oxa"] = json!({"subagent": {"softTokens": n}});
        }
        let env = env_with(Some(&settings.to_string()));
        let task = "x".repeat(task_len);
        // The fake counts a reply's bytes as its tokens and cuts a reply above `max_tokens`;
        // this long tool call is reported as 64 tokens so it is delivered whole.
        let mut delegate = call(json!({"agent": "scout", "task": task}));
        delegate.completion_tokens = Some(64);
        let server = fake(
            vec![
                delegate,
                text("child reply"),
                text("CHILD SUMMARY"),
                text("parent done"),
            ],
            FakeOpts::default(),
        );
        let run = parent(&env, &server.url(), "delegate a long task");
        run.ok();
        let reqs = sorted(&server);
        let p = parent_id(&reqs);
        let kids = child_reqs(&reqs, &p, 1);
        let chat = kids
            .iter()
            .find(|r| r.kind == "subagent")
            .expect("the child's request");
        let u = chat.usage.expect("usage");
        let ctx = u64::from(u.prompt) + u64::from(u.completion);
        let compacted = kids.iter().any(|r| r.kind == "compaction");
        // The parent's own threshold is far above (its model's window is 1M tokens).
        assert!(
            !reqs.iter().any(|r| r.session == p && r.kind == "compaction"),
            "the parent does not compact"
        );
        assert_eq!(
            tool_result(reqs.iter().rev().find(|r| r.session == p).unwrap()),
            "child reply",
            "the child's answer is the result"
        );
        (ctx, compacted, kids)
    }

    // Default 24000: above it the child compacts, below it it does not.
    let (ctx, compacted, kids) = one(None, 23_000);
    assert!(ctx > 24_000, "the case is above the default limit: {ctx}");
    assert!(compacted, "a child above 24000 tokens compacts (context {ctx})");
    // W5: the summary request is the child's whole wire plus one instruction.
    let chat = kids.iter().find(|r| r.kind == "subagent").unwrap();
    let sum = kids.iter().find(|r| r.kind == "compaction").unwrap();
    let n = chat.messages.len();
    assert_eq!(sum.messages[..n], chat.messages[..], "the wire is the prefix");
    assert_eq!(sum.messages[n].content.as_deref(), Some("child reply"));
    assert_eq!(sum.messages.len(), n + 2, "plus one instruction");
    assert_eq!(sum.messages[n + 1].role, Role::User);
    assert_eq!(header(sum, "x-session-id"), header(chat, "x-session-id"));

    let (ctx, compacted, _) = one(None, 15_000);
    assert!(ctx < 24_000, "the case is below the default limit: {ctx}");
    assert!(!compacted, "below 24000 tokens no compaction (context {ctx})");

    // `oxa.subagent.softTokens` lowers it: the same task now compacts.
    let (ctx, compacted, _) = one(Some(12_000), 15_000);
    assert!(ctx > 12_000, "{ctx}");
    assert!(compacted, "softTokens 12000: the child compacts at {ctx}");
}

/// ENG-016: the tool result is each child's final assistant text only, capped at
/// `oxa.subagent.resultCap` (6 KiB) with a note naming the spill file.
/// invariant: W4: the result is prefilled at the parent's depth, so it is deterministic (no
/// timings, no usage, no intermediate text).
#[test]
fn eng_016() {
    let env = env_with(None);
    std::fs::write(env.work().join("notes.md"), "the notes\n").unwrap();
    let final_text = format!("{}TAIL-MARK", "y".repeat(8000));
    let mut look = ScriptTurn::tool("c_1", "read", r#"{"path":"notes.md"}"#);
    look.text = Some("INTERMEDIATE NOTE".to_string());
    let mut second = call(json!({"agent": "scout", "task": "short"}));
    second.tool_calls[0].id = "sub_2".to_string();
    let server = fake(
        vec![
            call(json!({"agent": "scout", "task": "long"})),
            look,
            text(&final_text),
            second,
            text("short result"),
            text("parent done"),
        ],
        FakeOpts::default(),
    );
    parent(&env, &server.url(), "delegate").ok();
    let reqs = sorted(&server);
    let p = parent_id(&reqs);
    let parent_reqs: Vec<&Recorded> = reqs
        .iter()
        .filter(|r| r.session == p && r.max_tokens > 0)
        .collect();
    // The long result: cut at 6144 bytes with the note naming the spill file.
    let long = tool_result(parent_reqs[1]);
    assert!(long.len() <= 6144, "capped at 6 KiB: {} bytes", long.len());
    assert!(long.starts_with("yyyy"), "the final text, from its start");
    assert!(
        !long.contains("INTERMEDIATE NOTE"),
        "only the final assistant text"
    );
    assert!(!long.contains("TAIL-MARK"), "cut");
    let note = long.rsplit("\n\n").next().unwrap();
    let path = note
        .strip_prefix(&format!(
            "[subagent output truncated: {} bytes, full text in ",
            final_text.len()
        ))
        .and_then(|r| r.strip_suffix(']'))
        .unwrap_or_else(|| panic!("the truncation note: {note:?}"));
    assert_eq!(
        std::fs::read_to_string(path).expect("the spill file"),
        final_text,
        "the spill file holds the whole text"
    );
    // The short result is the final text exactly: no timings, no usage.
    assert_eq!(tool_result(parent_reqs[2]), "short result");
}

/// ENG-017: the tool is not declared on a model whose context is below 32000 unless
/// `oxa.subagent.smallContext`; re-evaluated at a model switch.
/// invariant: W3 (the declaration is part of the frozen prefix and changes only with a new
/// epoch); Flash: 16K context, disk parks only.
#[test]
fn eng_017() {
    let opts = || FakeOpts {
        model: "big".to_string(),
        models: vec![FakeModel::moe("big", 65_536), FakeModel::moe("small", 30_000)],
        ..FakeOpts::default()
    };
    // A small reserve keeps the switch to the 30K model within its hard limit.
    let base = r#""compaction": {"reserveTokens": 1000}"#;
    for (settings, small_declares) in [
        (format!("{{{base}}}"), false),
        (
            format!("{{{base}, \"oxa\": {{\"subagent\": {{\"smallContext\": true}}}}}}"),
            true,
        ),
    ] {
        let env = env_with(Some(&settings));
        let server = fake(vec![text("one"), text("two"), text("three")], opts());
        let mut rpc = Rpc::start(&env, &server.url());
        rpc.prompt("on the big model");
        rpc.ok(json!({"type": "set_model", "provider": "oxe", "modelId": "small"}));
        rpc.prompt("on the small model");
        rpc.ok(json!({"type": "set_model", "provider": "oxe", "modelId": "big"}));
        rpc.prompt("big again");
        assert_eq!(rpc.finish(), Some(0));
        let reqs: Vec<Recorded> = sorted(&server).into_iter().filter(|r| r.max_tokens > 0).collect();
        assert_eq!(reqs.len(), 3, "{settings}");
        assert_eq!(reqs[1].model, "small");
        assert!(
            declares_subagent(&reqs[0]),
            "{settings}: declared on the 64K model"
        );
        assert_eq!(
            declares_subagent(&reqs[1]),
            small_declares,
            "{settings}: the 30K model (below 32000)"
        );
        assert!(
            declares_subagent(&reqs[2]),
            "{settings}: declared again after the switch back"
        );
    }
}

/// ENG-018: before the first child of a call the parent is parked (one zero-token request with
/// `X-Oxe-Snapshot: keep`, skipped without H6) and its next request reuses exactly the parked
/// tokens; after every child, also a failed one, its engine state is cleared by its hint, the
/// parent's state stays.
/// invariant: H6 + H4 park, EG5 hint clear.
#[test]
fn eng_018() {
    let env = env_with(None);
    let server = fake(
        vec![
            call(json!({"tasks": [
                {"agent": "scout", "task": "fine"},
                {"agent": "scout", "task": "fails"},
            ]})),
            text("fine result"),
            ScriptTurn::fault(Fault::Http {
                status: 400,
                body: r#"{"error":{"message":"PARITY-CHILD-REFUSED","type":"invalid_request_error","code":400}}"#
                    .to_string(),
            }),
            text("parent done"),
        ],
        FakeOpts::default(),
    );
    parent(&env, &server.url(), "delegate two").ok();
    let reqs = sorted(&server);
    let p = parent_id(&reqs);
    let first_child = reqs.iter().position(|r| r.session != p).expect("a child request");
    let parks: Vec<&Recorded> = reqs
        .iter()
        .filter(|r| r.session == p && r.max_tokens == 0)
        .collect();
    assert_eq!(parks.len(), 1, "one park per call");
    let park = parks[0];
    let park_at = reqs.iter().position(|r| r.seq == park.seq).unwrap();
    assert!(park_at < first_child, "the park comes before the first child");
    assert!(park.snapshot_keep, "X-Oxe-Snapshot: keep");
    assert_eq!(header(park, "x-oxe-snapshot"), Some("keep"));
    let n = reqs[0].messages.len();
    assert_eq!(
        park.messages[..n],
        reqs[0].messages[..],
        "the park is over the parent's wire"
    );
    assert_eq!(park.messages.len(), n + 1, "plus the reply that calls the tool");
    let next = reqs
        .iter()
        .filter(|r| r.session == p && r.max_tokens > 0)
        .nth(1)
        .expect("the parent's next request");
    assert_eq!(
        next.usage.expect("usage").cached,
        park.usage.expect("park usage").prompt,
        "the parent's next request reuses exactly the parked tokens"
    );
    assert!(
        tool_result(next).contains("### [scout] failed"),
        "{}",
        tool_result(next)
    );
    let clears = server.clears();
    assert_eq!(
        clears,
        vec![format!("{p}.s1"), format!("{p}.s2")],
        "each child, the failed one too"
    );
    assert!(!clears.contains(&p), "the parent's state stays");

    // Without H6 (an engine that advertises nothing) there is no park.
    let env = env_with(None);
    let server = fake(
        vec![
            call(json!({"agent": "scout", "task": "look"})),
            text("looked"),
            text("parent done"),
        ],
        FakeOpts {
            models: vec![FakeModel {
                alias: "fake-model".to_string(),
                cap: 1 << 20,
                max_output: None,
                caps: Some(FakeCaps::none()),
            }],
            ..FakeOpts::default()
        },
    );
    parent(&env, &server.url(), "delegate").ok();
    let reqs = sorted(&server);
    assert!(
        reqs.iter().all(|r| r.max_tokens > 0),
        "no zero-token request without H6"
    );
    assert_eq!(tool_result(reqs.last().unwrap()), "looked");
}

/// ENG-019: abort or shutdown kills the child's process group (TERM, then KILL after 2 s) and
/// starts no further task; the closed connection cancels the child's engine request.
/// invariant: cancel propagation (one engine slot: a child left running would hold it).
/// Shutdown here is the parent's orderly shutdown by SIGTERM (pi rpc.md: "SIGTERM and SIGHUP
/// also start an orderly shutdown"); a stdin EOF waits for the run by design (rpc.md: "waits
/// for pending work to finish"), so it is not a shutdown that kills.
#[test]
fn eng_019() {
    for how in ["abort", "shutdown"] {
        let env = env_with(None);
        let mut held = text("never sent");
        held.fault = Some(Fault::HoldHeaders { ms: 60_000 });
        let server = fake(
            vec![
                call(json!({"tasks": [
                    {"agent": "scout", "task": "slow"},
                    {"agent": "scout", "task": "never"},
                ]})),
                held,
                text("parent after abort"),
            ],
            FakeOpts::default(),
        );
        let mut rpc = Rpc::start(&env, &server.url());
        rpc.ok(json!({"type": "prompt", "message": "delegate"}));
        // The child's request is at the engine (held there), its process alive.
        wait_until("the child's request", || {
            !any_child(&server.requests()).is_empty()
        });
        wait_until("the child process", || !live_children(&env).is_empty());
        let started = Instant::now();
        if how == "abort" {
            rpc.ok(json!({"type": "abort"}));
            rpc.settle();
        } else {
            let pid = rpc.child.id().to_string();
            let sent = Command::new("kill").args(["-TERM", &pid]).status();
            assert!(sent.is_ok_and(|s| s.success()), "SIGTERM to the parent");
            wait_until("the parent to exit", || {
                matches!(rpc.child.try_wait(), Ok(Some(_)))
            });
        }
        // TERM, then KILL after 2 s: gone well within 5 s.
        while !live_children(&env).is_empty() && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let left = live_children(&env);
        assert!(
            left.is_empty(),
            "{how}: the child (pid {left:?}) is still running 5 s after the parent was shut down"
        );
        // The engine saw the connection close.
        let reqs = sorted(&server);
        let p = parent_id(&reqs);
        let child = child_reqs(&reqs, &p, 1).pop().expect("child 1's request");
        wait_until("the child's request to end", || {
            server.timings().iter().any(|t| t.seq == Some(child.seq))
        });
        let t = server
            .timings()
            .into_iter()
            .find(|t| t.seq == Some(child.seq))
            .unwrap();
        assert!(
            t.client_closed,
            "{how}: the closed connection cancels the engine request"
        );
        assert!(
            child_reqs(&sorted(&server), &p, 2).is_empty(),
            "{how}: the second task never starts"
        );
        if how == "abort" {
            assert!(
                server.clears().contains(&format!("{p}.s1")),
                "{how}: the aborted child's state is cleared"
            );
            let end = rpc
                .events
                .iter()
                .find(|e| e["type"] == "tool_execution_end")
                .cloned()
                .unwrap_or_else(|| panic!("a tool_execution_end: {:#?}", rpc.events));
            assert_eq!(end["isError"], true, "{end}");
            assert_eq!(rpc.finish(), Some(0));
        }
    }
}

// ---------------------------------------------------------------- meta checks

/// The PARITY.md file of the workspace root.
fn parity_md() -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("PARITY.md");
    std::fs::read_to_string(&root).unwrap_or_else(|e| panic!("read {}: {e}", root.display()))
}

/// The subagent rows of PARITY.md (ENG-010..ENG-019): (id, verdict, test column).
fn subagent_rows() -> Vec<(String, String, String)> {
    let mut rows = Vec::new();
    for line in parity_md().lines() {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        if cells.len() < 6 || !cells[1].starts_with("ENG-01") {
            continue;
        }
        rows.push((cells[1].to_string(), cells[3].to_string(), cells[5].to_string()));
    }
    rows
}

/// The source of this file.
const SELF: &str = include_str!("subagent.rs");

/// The source text of one test item: its doc comment, `#[test]` and `fn`. Items end at the
/// column-0 closing brace of the function.
fn fn_body(id: &str) -> &'static str {
    // Anchored at a line start: the module doc also names `fn eng_010()` in backticks.
    let needle = format!("\nfn {}()", id.to_ascii_lowercase().replace('-', "_"));
    let fn_at = SELF
        .find(&needle)
        .unwrap_or_else(|| panic!("no test fn for {id}"))
        + 1;
    let start = SELF[..fn_at].rfind("\n}\n").map_or(0, |i| i + 3);
    let end = fn_at + SELF[fn_at..].find("\n}\n").map_or(SELF.len() - fn_at, |i| i + 3);
    &SELF[start..end]
}

/// each PARITY.md id of this area maps to a test fn or an O-row assertion
#[test]
fn every_row_has_test_or_recorded_omission() {
    let rows = subagent_rows();
    assert_eq!(rows.len(), 10, "ENG-010..ENG-019 are 10 rows: {rows:?}");
    for (id, verdict, test) in &rows {
        let fn_name = id.to_ascii_lowercase().replace('-', "_");
        assert!(
            SELF.contains(&format!("fn {fn_name}()")),
            "{id}: test fn {fn_name} exists in subagent.rs"
        );
        assert!(
            matches!(verdict.as_str(), "P" | "A" | "O"),
            "{id} has a verdict P, A or O: {verdict}"
        );
        assert!(
            test.contains(&format!("subagent.rs::{fn_name}")),
            "{id} maps to subagent.rs::{fn_name} in PARITY.md (test column `{test}`)"
        );
    }
}

/// documented output or recorded fixture
#[test]
fn p_rows_compare_with_pi() {
    for (id, verdict, _) in subagent_rows() {
        if verdict != "P" {
            continue;
        }
        let body = fn_body(&id);
        assert!(
            body.contains("pi docs:"),
            "{id} (P) cites the pi documented behaviour it compares with"
        );
    }
}

/// comment + adapted behaviour asserted
#[test]
fn a_rows_name_their_invariant() {
    for (id, verdict, _) in subagent_rows() {
        if verdict != "A" {
            continue;
        }
        let body = fn_body(&id);
        assert!(
            body.contains("invariant:"),
            "{id} (A) names its invariant in a comment"
        );
        assert!(
            body.contains("assert") || body.contains("red("),
            "{id} (A) asserts its adapted behaviour"
        );
    }
}

/// unsupported_in_oxa
#[test]
fn o_rows_assert_unsupported_error() {
    for (id, verdict, _) in subagent_rows() {
        if verdict != "O" {
            continue;
        }
        assert!(
            fn_body(&id).contains("unsupported_in_oxa"),
            "{id} (O) asserts the unsupported_in_oxa error"
        );
    }
}
