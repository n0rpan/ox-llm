//! C7 acceptance tests: Tool selection and mutation queue. Owner: task C7.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_core::{resolve_toolset, MutationQueue, ToolSelection, Toolset};
use oxa_proto::{Access, Exposure, Source, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Value};
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A tool that does nothing; only its spec matters for selection.
struct FakeTool(ToolSpec);

impl Tool for FakeTool {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    fn access(&self, _args: &Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        Ok(Vec::new())
    }
    fn run(&self, _args: &Value, _cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        ToolOutput::ok("")
    }
}

fn tool(name: &str, source: Source, exposure: Exposure) -> Arc<dyn Tool> {
    Arc::new(FakeTool(ToolSpec {
        name: name.to_string(),
        description: format!("{name} tool"),
        schema_json: "{}".to_string(),
        class: ToolClass::Read,
        exposure,
        source,
    }))
}

fn builtin(name: &str) -> Arc<dyn Tool> {
    tool(name, Source::Builtin, Exposure::Direct)
}

/// The oxa built-ins, plus one built-in extension (off by default), one extension tool, three
/// MCP tools with different exposures and one hidden tool.
fn fixture() -> Vec<Arc<dyn Tool>> {
    vec![
        builtin("read"),
        builtin("bash"),
        builtin("edit"),
        builtin("write"),
        builtin("grep"),
        builtin("find"),
        builtin("ls"),
        tool(
            "codemode",
            Source::Extension("builtin:codemode".into()),
            Exposure::Direct,
        ),
        tool(
            "ext_tool",
            Source::Extension("/x/ext.js".into()),
            Exposure::Direct,
        ),
        tool(
            "mcp__radius__echo",
            Source::Mcp("radius".into()),
            Exposure::Direct,
        ),
        tool(
            "mcp__radius__ping",
            Source::Mcp("radius".into()),
            Exposure::Codemode,
        ),
        tool("mcp__other__x", Source::Mcp("other".into()), Exposure::Deferred),
        tool("debug", Source::Builtin, Exposure::Hidden),
    ]
}

fn sel_tools(tools: &[&str]) -> ToolSelection {
    ToolSelection {
        tools: Some(tools.iter().map(|s| s.to_string()).collect()),
        ..Default::default()
    }
}

fn names(ts: &[Arc<dyn Tool>]) -> Vec<String> {
    ts.iter().map(|t| t.spec().name.clone()).collect()
}

fn v(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

fn resolve(sel: &ToolSelection) -> (Toolset, Vec<String>) {
    resolve_toolset(&fixture(), sel)
}

/// the table of cli.md
#[test]
fn default_tools_and_cli_matrix() {
    // No flags: the seven built-ins, the extension tool and the directly exposed MCP tool.
    let (ts, w) = resolve(&ToolSelection::default());
    assert!(w.is_empty(), "no warnings expected: {w:?}");
    assert_eq!(
        names(&ts.direct),
        v(&[
            "bash",
            "edit",
            "ext_tool",
            "find",
            "grep",
            "ls",
            "mcp__radius__echo",
            "read",
            "write"
        ])
    );
    assert_eq!(names(&ts.indirect), v(&["mcp__other__x", "mcp__radius__ping"]));

    // --tools read,codemode: named tools only; MCP stays callable but is never declared.
    let (ts, _) = resolve(&sel_tools(&["read", "codemode"]));
    assert_eq!(names(&ts.direct), v(&["codemode", "read"]));
    assert_eq!(
        names(&ts.indirect),
        v(&["mcp__other__x", "mcp__radius__echo", "mcp__radius__ping"])
    );

    // --tools mcp__*: the MCP tools are filtered by the glob and declared by their exposure.
    let (ts, _) = resolve(&sel_tools(&["mcp__*"]));
    assert_eq!(names(&ts.direct), v(&["mcp__radius__echo"]));
    assert_eq!(names(&ts.indirect), v(&["mcp__other__x", "mcp__radius__ping"]));

    // --no-tools: nothing at all.
    let (ts, _) = resolve(&ToolSelection {
        no_tools: true,
        ..Default::default()
    });
    assert!(ts.direct.is_empty() && ts.indirect.is_empty());

    // --no-tools with --tools read: only the named tool.
    let (ts, _) = resolve(&ToolSelection {
        no_tools: true,
        ..sel_tools(&["read"])
    });
    assert_eq!(names(&ts.direct), v(&["read"]));
    assert!(ts.indirect.is_empty());

    // --no-builtin-tools: built-ins gone, extension and MCP tools stay.
    let (ts, _) = resolve(&ToolSelection {
        no_builtin: true,
        ..Default::default()
    });
    assert_eq!(names(&ts.direct), v(&["ext_tool", "mcp__radius__echo"]));
    assert_eq!(names(&ts.indirect), v(&["mcp__other__x", "mcp__radius__ping"]));

    // --no-builtin-tools with --tools read,grep: the named built-ins survive.
    let (ts, _) = resolve(&ToolSelection {
        no_builtin: true,
        ..sel_tools(&["read", "grep"])
    });
    assert_eq!(names(&ts.direct), v(&["grep", "read"]));

    // defaultTools as a list: only those built-ins; extension tools are still kept.
    let (ts, _) = resolve(&ToolSelection {
        default_tools: Some(v(&["read"])),
        ..Default::default()
    });
    assert_eq!(names(&ts.direct), v(&["ext_tool", "mcp__radius__echo", "read"]));

    // --tools +codemode,-write patches the default selection (cli.md).
    let (ts, w) = resolve(&sel_tools(&["+codemode", "-write"]));
    assert!(w.is_empty(), "{w:?}");
    assert_eq!(
        names(&ts.direct),
        v(&[
            "bash",
            "codemode",
            "edit",
            "ext_tool",
            "find",
            "grep",
            "ls",
            "mcp__radius__echo",
            "read"
        ])
    );
}

/// --exclude-tools over --tools
#[test]
fn exclusion_wins() {
    // Exclusion removes a named tool.
    let sel = ToolSelection {
        exclude: v(&["bash"]),
        ..sel_tools(&["read", "bash"])
    };
    let (ts, _) = resolve(&sel);
    assert_eq!(names(&ts.direct), v(&["read"]));

    // Exclusion removes MCP tools, including the callable-only ones.
    let sel = ToolSelection {
        exclude: v(&["mcp__*"]),
        ..Default::default()
    };
    let (ts, _) = resolve(&sel);
    assert!(!names(&ts.direct).iter().any(|n| n.starts_with("mcp__")));
    assert!(ts.indirect.is_empty());

    // Exclusion beats --tools mcp__* and removes a glob-named tool.
    let sel = ToolSelection {
        exclude: v(&["mcp__radius__*"]),
        ..sel_tools(&["mcp__*"])
    };
    let (ts, _) = resolve(&sel);
    assert!(ts.direct.is_empty());
    assert_eq!(names(&ts.indirect), v(&["mcp__other__x"]));
}

/// mcp__*
#[test]
fn globs_match_names() {
    let (ts, _) = resolve(&sel_tools(&["mcp__radius__*"]));
    assert_eq!(names(&ts.direct), v(&["mcp__radius__echo"]));
    assert_eq!(names(&ts.indirect), v(&["mcp__radius__ping"]));

    let (ts, _) = resolve(&sel_tools(&["rea*"]));
    assert_eq!(names(&ts.direct), v(&["read"]));

    let (ts, _) = resolve(&sel_tools(&["*"]));
    assert_eq!(
        names(&ts.direct),
        v(&[
            "bash",
            "codemode",
            "edit",
            "ext_tool",
            "find",
            "grep",
            "ls",
            "mcp__radius__echo",
            "read",
            "write"
        ])
    );
}

/// -nbt with --tools
#[test]
fn no_builtin_keeps_named() {
    let sel = ToolSelection {
        no_builtin: true,
        ..sel_tools(&["read"])
    };
    let (ts, w) = resolve(&sel);
    assert!(w.is_empty(), "{w:?}");
    assert_eq!(names(&ts.direct), v(&["read"]));
    assert!(ts.get("bash").is_none());
}

/// exposures
#[test]
fn hidden_dropped_indirect_separated() {
    let (ts, _) = resolve(&sel_tools(&["*"]));
    // Hidden tools are in neither list.
    assert!(ts.get("debug").is_none() && ts.get_indirect("debug").is_none());
    // Declared tools are all Direct; callable-only tools are all Codemode or Deferred.
    assert!(ts.direct.iter().all(|t| t.spec().exposure == Exposure::Direct));
    // Callable-only tools here are MCP tools: Codemode / Deferred ones, and Direct ones that
    // --tools kept without naming them (never declared, reachable through codemode only).
    assert!(ts
        .indirect
        .iter()
        .all(|t| matches!(t.spec().source, Source::Mcp(_))));
    assert!(!ts.indirect.iter().any(|t| t.spec().exposure == Exposure::Direct));
    // The two lists do not overlap.
    for t in &ts.direct {
        assert!(ts.get_indirect(&t.spec().name).is_none());
    }
    assert_eq!(
        ts.get_indirect("mcp__radius__ping").map(|t| t.spec().exposure),
        Some(Exposure::Codemode)
    );
    assert_eq!(ts.specs().len(), ts.direct.len());
}

/// warnings returned
#[test]
fn unknown_names_warn() {
    let (_, w) = resolve(&sel_tools(&["read", "nope"]));
    assert_eq!(w, v(&["unknown tool in --tools: nope"]));

    let sel = ToolSelection {
        exclude: v(&["ghost"]),
        default_tools: Some(v(&["zzz"])),
        ..Default::default()
    };
    let (_, w) = resolve(&sel);
    assert!(
        w.contains(&"unknown tool in --exclude-tools: ghost".to_string()),
        "{w:?}"
    );
    assert!(
        w.contains(&"unknown tool in defaultTools: zzz".to_string()),
        "{w:?}"
    );

    // Globs that match nothing are not warnings.
    let (_, w) = resolve(&sel_tools(&["mcp__nothing__*"]));
    assert!(w.is_empty(), "{w:?}");

    // A patch with an unknown name warns.
    let (_, w) = resolve(&sel_tools(&["+nosuch"]));
    assert_eq!(w, v(&["unknown tool: nosuch"]));
}

/// same path sequential, different paths parallel
#[test]
fn mutation_queue_serialises_per_path() {
    let q = Arc::new(MutationQueue::new());
    let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let p1 = Path::new("/oxa-c7-test/nonexistent/one.txt");
    let p2 = Path::new("/oxa-c7-test/nonexistent/two.txt");

    // A holds p1 until released.
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (a_done_tx, a_done_rx) = mpsc::channel();
    {
        let (q, log) = (Arc::clone(&q), Arc::clone(&log));
        std::thread::spawn(move || {
            q.with(p1, || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                log.lock().unwrap().push("A");
            });
            a_done_tx.send(()).unwrap();
        });
    }
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("A started");

    // B on another path runs while A still holds p1.
    let (b_done_tx, b_done_rx) = mpsc::channel();
    {
        let (q, log) = (Arc::clone(&q), Arc::clone(&log));
        std::thread::spawn(move || {
            q.with(p2, || log.lock().unwrap().push("B"));
            b_done_tx.send(()).unwrap();
        });
    }
    b_done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("B ran in parallel with A");

    // C on p1 must wait for A.
    let (c_done_tx, c_done_rx) = mpsc::channel();
    {
        let (q, log) = (Arc::clone(&q), Arc::clone(&log));
        std::thread::spawn(move || {
            q.with(p1, || log.lock().unwrap().push("C"));
            c_done_tx.send(()).unwrap();
        });
    }
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        c_done_rx.try_recv().is_err(),
        "C must wait while A holds the path"
    );

    release_tx.send(()).unwrap();
    a_done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("A finished");
    c_done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("C finished after A");

    let log = log.lock().unwrap().clone();
    let pos = |s: &str| log.iter().position(|x| *x == s).unwrap();
    assert!(pos("A") < pos("C"), "same path runs in order: {log:?}");
    assert!(pos("B") < pos("A"), "different path did not wait for A: {log:?}");
}

/// A panic inside the queue releases the path for the next caller.
#[test]
fn mutation_queue_releases_after_panic() {
    let q = Arc::new(MutationQueue::new());
    let p = Path::new("/oxa-c7-test/nonexistent/panic.txt");
    let qc = Arc::clone(&q);
    let r = std::thread::spawn(move || qc.with(p, || panic!("boom"))).join();
    assert!(r.is_err());
    let got = q.with(p, || 7);
    assert_eq!(got, 7);
}

/// oxa default
#[test]
fn all_seven_default() {
    let all: Vec<Arc<dyn Tool>> = ["grep", "ls", "write", "read", "find", "edit", "bash"]
        .iter()
        .map(|n| builtin(n))
        .collect();
    let (ts, w) = resolve_toolset(&all, &ToolSelection::default());
    assert!(w.is_empty());
    // Sorted by name, whatever the input order.
    assert_eq!(
        names(&ts.direct),
        v(&["bash", "edit", "find", "grep", "ls", "read", "write"])
    );
    assert!(ts.indirect.is_empty());
}

/// The selection does not depend on the order the tools were registered in (W3).
#[test]
fn selection_order_is_stable() {
    let mut reversed = fixture();
    reversed.reverse();
    let (a, _) = resolve(&ToolSelection::default());
    let (b, _) = resolve_toolset(&reversed, &ToolSelection::default());
    assert_eq!(names(&a.direct), names(&b.direct));
    assert_eq!(names(&a.indirect), names(&b.indirect));
}
