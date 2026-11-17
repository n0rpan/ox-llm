//! L4 (budget), end to end: a session with `oxa.compaction.softTokens: 8000` never hands the
//! model a tool result longer than the budget of that working size (25% of 8000 tokens at
//! 3 bytes, 6000 bytes), for every built-in: `read`, `bash`, `grep`, `find` and `ls`. The output
//! shaper (`exec`) applies the cap; `bash` keeps the tail and names a spill file that holds the
//! full output. Offline: `bash` runs `sh -c` in the test's own directory through a test
//! sandbox, nothing else leaves the test.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{CancelToken, Engine, ExecResult, ExecSpec, HeadlessUi, OxaError, Role, Sandbox, VecSink};
use oxa_resources::{discover, DiscoverOpts};
use oxa_session::{MemStore, SessionStore};
use std::path::Path;
use std::sync::Arc;

/// Runs `sh -c <command>` in the spec's directory. Test-only: the output is stdout and stderr.
struct ShSandbox;

impl Sandbox for ShSandbox {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&s.command)
            .current_dir(&s.cwd)
            .output()
            .expect("sh runs");
        let mut output = out.stdout;
        output.extend_from_slice(&out.stderr);
        ExecResult {
            output,
            exit: out.status.code(),
            timed_out: false,
        }
    }
}

/// The 67,000-byte source-like file of the token dump: short Rust functions and a last comment.
fn source_text() -> String {
    let mut s = String::new();
    let mut i = 0;
    while s.len() < 66_900 {
        s.push_str(&format!(
            "pub fn item_{i:04}(x: u32) -> u32 {{ x.wrapping_mul({i}) + 1 }}\n"
        ));
        i += 1;
    }
    let rest = 67_000 - s.len();
    s.push_str("//");
    s.push_str(&" ".repeat(rest - 3));
    s.push('\n');
    s
}

/// 30 files with 10 `needle` matches each, as the token dump's tree.
fn write_tree(env: &TestEnv) {
    for i in 0..30 {
        let body: String = (0..40)
            .map(|k| {
                if k % 4 == 2 {
                    format!("needle item {i:02}-{k:02} in the component\n")
                } else {
                    format!("filler text line {k:02} of file {i:02}\n")
                }
            })
            .collect();
        env.write(
            &format!("work/project/module_{i:02}/src/components/nested/deeper/file_{i:02}.txt"),
            &body,
        );
    }
    env.write("work/project/src/large.rs", &source_text());
    // 300 entries in one directory: the `ls` listing is over the cap.
    for i in 0..300 {
        env.write(
            &format!("work/project/flat/entry_with_a_long_name_{i:04}.txt"),
            "x",
        );
    }
}

/// One turn per built-in, then a short answer.
fn script() -> Vec<ScriptTurn> {
    vec![
        ScriptTurn::tool("c1", "read", r#"{"path":"project/src/large.rs"}"#),
        ScriptTurn::tool("c2", "bash", r#"{"command":"seq 1 20000"}"#),
        ScriptTurn::tool(
            "c3",
            "grep",
            r#"{"pattern":"needle","context":2,"path":"project"}"#,
        ),
        ScriptTurn::tool("c4", "find", r#"{"pattern":"*","limit":1000,"path":"."}"#),
        ScriptTurn::tool("c5", "ls", r#"{"path":"project/flat","limit":1000}"#),
        ScriptTurn::text("done"),
    ]
}

/// Run `script` in a session with `settings` and an engine that publishes `compact_at`; the tool
/// results the model received, in order.
fn tool_results(
    env: &TestEnv,
    cwd: &Path,
    settings: &str,
    script: Vec<ScriptTurn>,
    compact_at: Option<u32>,
) -> Vec<String> {
    let dirs = Dirs::under(env.root());
    let path = dirs.settings_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, settings).unwrap();
    let settings = Settings::load(&dirs, cwd, true);
    let opts = DiscoverOpts {
        trusted: true,
        ..Default::default()
    };
    let resources = discover(&dirs, cwd, &settings, &opts);
    let mut scripted = ScriptedEngine::new(script).with_limits(131_072, 1024);
    if let Some(c) = compact_at {
        scripted = scripted.with_compact_at(c);
    }
    let eng = Arc::new(scripted);
    let engine: Arc<dyn Engine> = eng.clone();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let mut s = Session::open(
        Deps {
            engine,
            store,
            tools: oxa_tools::builtin_tools(),
            hooks: Vec::new(),
            policy: Box::new(ModePolicy::new()),
            sandbox: Arc::new(ShSandbox),
            settings,
            dirs,
            cwd: cwd.to_path_buf(),
            selection: ToolSelection::default(),
            resources,
            model: None,
            thinking: None,
            mode: None,
            session_id: Some("s1".to_string()),
            jobs: oxa_core::fresh_jobs(),
        },
        Open::New,
    )
    .expect("open");
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    s.prompt("go".to_string(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt");
    assert_eq!(eng.remaining(), 0, "the script is used up");
    // The tool results of the request with the most of them: the history of the last real
    // request (prefill-only requests carry an older history, and are not the answer).
    let reqs = eng.requests();
    let mut best: Vec<String> = Vec::new();
    let mut summary = Vec::new();
    for r in reqs.iter().filter(|r| r.max_tokens > 0) {
        let tools: Vec<String> = r
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| m.content.clone().unwrap_or_default())
            .collect();
        summary.push(format!("msgs {} tool results {}", r.messages.len(), tools.len()));
        if tools.len() > best.len() {
            best = tools;
        }
    }
    eprintln!("requests: {}", summary.join("; "));
    best
}

#[test]
fn with_a_soft_point_of_8000_caps_every_builtin_result_at_6000_bytes() {
    let env = TestEnv::new();
    write_tree(&env);
    let cwd = env.work();
    let results = tool_results(
        &env,
        &cwd,
        // The five results are compared in one request: the mid-run compaction (L5, on by
        // default) would fold the earlier ones between steps, so it is off here.
        r#"{"oxa": {"compaction": {"softTokens": 8000, "midRun": false}}}"#,
        script(),
        None,
    );
    assert_eq!(results.len(), 5, "one result per call");
    let names = ["read", "bash", "grep", "find", "ls"];
    for (name, out) in names.iter().zip(&results) {
        eprintln!("soft 8000, {name}: returned {} bytes", out.len());
        assert!(out.len() <= 6_000, "{name}: {} bytes", out.len());
    }
    // read: the first page is cut by bytes and the notice names the next offset.
    assert!(results[0].contains("Use offset="), "read: {}", results[0]);
    // bash: the tail is kept, and the notice names the spill file with the full output.
    let bash = &results[1];
    assert!(bash.contains("[output truncated:"), "bash: {bash}");
    assert!(bash.contains("\n20000\n"), "bash keeps the tail: {bash}");
    // Issue #992: the exit-status line sits after the kept tail and survives the cap.
    assert!(
        bash.trim_end().ends_with("Command exited with code 0"),
        "bash keeps the status line: {bash}"
    );
    let spill = bash
        .split("full output in ")
        .nth(1)
        .and_then(|r| r.split(']').next())
        .expect("the spill path is named");
    let full = std::fs::read_to_string(spill).expect("the spill file exists");
    assert!(
        full.starts_with("1\n2\n3\n"),
        "the spill file holds the raw output"
    );
    assert_eq!(full.lines().count(), 20_000, "the spill file holds every line");
    // grep: the counts form over the limit; find and ls: their notices name the narrower search.
    assert!(
        results[2].starts_with("300 matches in total"),
        "grep: {}",
        results[2]
    );
    assert!(
        results[3].contains("Narrow the search to see the rest"),
        "find: {}",
        results[3]
    );
    assert!(
        results[4].contains("Narrow the search to see the rest"),
        "ls: {}",
        results[4]
    );
    // The tools cut themselves at the cap with their own notice: the shaper never cuts again.
    for (name, out) in names.iter().zip(&results) {
        if *name != "bash" {
            assert!(!out.contains("[output truncated"), "{name}: a second cut: {out}");
        }
    }
}

#[test]
fn on_a_large_window_cuts_grep_find_ls_and_an_explicit_read_at_16_kib_not_8() {
    let env = TestEnv::new();
    write_tree(&env);
    let cwd = env.work();
    let results = tool_results(
        &env,
        &cwd,
        r#"{"oxa": {}}"#,
        vec![
            ScriptTurn::tool("c1", "read", r#"{"path":"project/src/large.rs"}"#),
            ScriptTurn::tool("c2", "read", r#"{"path":"project/src/large.rs","offset":1}"#),
            ScriptTurn::tool(
                "c3",
                "grep",
                r#"{"pattern":"needle","context":2,"limit":1000,"path":"project"}"#,
            ),
            ScriptTurn::tool("c4", "find", r#"{"pattern":"*","limit":1000,"path":"."}"#),
            ScriptTurn::tool("c5", "ls", r#"{"path":"project/flat","limit":1000}"#),
            ScriptTurn::text("done"),
        ],
        None,
    );
    assert_eq!(results.len(), 5, "one result per call");
    let names = ["first read", "explicit read", "grep", "find", "ls"];
    for (name, out) in names.iter().zip(&results) {
        eprintln!("large window, {name}: returned {} bytes", out.len());
        assert!(out.len() <= 16_384, "{name}: {} bytes", out.len());
        assert!(!out.contains("[output truncated"), "{name}: a shaper cut: {out}");
    }
    // Only the first page of read is the 8 KiB page.
    assert!(
        results[0].len() > 7_000 && results[0].len() <= 8_192,
        "first read"
    );
    assert!(results[0].contains("Or use grep to find the part you need."));
    // An explicit read, grep and find are over 16 KiB: each is cut by its own tool near the
    // 16 KiB cap, with its notice inside it.
    assert!(results[1].len() > 15_000, "explicit read: {}", results[1].len());
    assert!(
        results[1].contains("(16.0KB limit). Use offset="),
        "explicit read"
    );
    for (name, out) in names.iter().zip(&results).skip(2).take(2) {
        assert!(out.len() > 15_000, "{name}: {}", out.len());
        assert!(
            out.contains("16.0KB limit reached. Narrow the search to see the rest]"),
            "{name}: {}",
            &out[out.len().saturating_sub(300)..]
        );
    }
    // ls (about 10 KB) is under the cap: shown whole, no cut (it was cut at 8 KiB before).
    assert!(results[4].len() > 8_192, "ls: {}", results[4].len());
    assert!(!results[4].contains("limit reached"), "ls: no cut");
}

/// The first `read` of the 67,000-byte file (no offset or limit) under `settings` and an engine
/// that publishes `compact_at`: the result's length, a 131,072-token window.
fn first_read_len(settings: &str, compact_at: Option<u32>) -> usize {
    let env = TestEnv::new();
    write_tree(&env);
    let cwd = env.work();
    let results = tool_results(
        &env,
        &cwd,
        settings,
        vec![
            ScriptTurn::tool("c1", "read", r#"{"path":"project/src/large.rs"}"#),
            ScriptTurn::text("done"),
        ],
        compact_at,
    );
    results[0].len()
}

#[test]
fn a_soft_point_only_derived_from_the_window_does_not_size_the_budget() {
    // The default reserve (16384) derives a soft point of about 85000 tokens on this window.
    // The budget ignores a derived point: the cap is pi's 16 KiB and the first page 8 KiB.
    let len = first_read_len(r#"{"oxa": {}}"#, None);
    eprintln!("derived soft only, first read: {len} bytes");
    assert!(len > 7_000 && len <= 8_192, "got {len}");
}

#[test]
fn an_explicit_soft_point_of_8000_caps_the_first_read_at_6000() {
    let len = first_read_len(r#"{"oxa": {"compaction": {"softTokens": 8000}}}"#, None);
    eprintln!("softTokens 8000, first read: {len} bytes");
    assert!(len > 5_000 && len <= 6_000, "got {len}");
}

#[test]
fn an_engine_compact_at_of_8000_caps_the_first_read_at_6000() {
    let len = first_read_len(r#"{"oxa": {}}"#, Some(8_000));
    eprintln!("engine compact_at 8000, first read: {len} bytes");
    assert!(len > 5_000 && len <= 6_000, "got {len}");
}

#[test]
fn an_explicit_soft_point_of_100000_gives_the_16_kib_cap_and_the_8_kib_page() {
    let len = first_read_len(r#"{"oxa": {"compaction": {"softTokens": 100000}}}"#, None);
    eprintln!("softTokens 100000, first read: {len} bytes");
    assert!(len > 7_000 && len <= 8_192, "got {len}");
}
