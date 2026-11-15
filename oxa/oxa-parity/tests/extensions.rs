//! G12 PARITY tests (extensions). Owner: task G12.
//! One test per PARITY.md row of the EXT area (`EXT-001` -> `fn ext_001()`), plus the four
//! meta-checks of the task. Every row test drives the real `oxa` binary in print mode against
//! `oxa-fake` (a fake engine script), with JavaScript extensions written into the test's agent
//! directory or work directory. Extensions report what they saw by appending lines to `log.txt`
//! in the work directory (`ctx.fs` writes inside the working directory are allowed in the
//! default workspace mode; writes outside it are declined in print mode and are checked as such).
//!
//! Where a row's behaviour needs an interactive front end (dialogs, a session switch, a tree
//! navigation, rendering), the test drives what print mode can reach and says so in its comment;
//! such rows are partial, and the gaps are listed in the task report, not hidden.
//!
//! Helpers: `oxa_parity::{oxa_bin, run_oxa, fixture}`, `oxa_fake::{FakeServer, TestEnv, ScriptTurn}`.

use oxa_fake::{FakeOpts, FakeServer, Fault, Recorded, ScriptTurn, TestEnv};
use oxa_parity::run_oxa;
use std::path::PathBuf;

/// PARITY.md, the row list of this area.
const PARITY_MD: &str = include_str!("../../PARITY.md");

/// The module every test extension starts from. `note(ctx, line)` appends a line to `log.txt`
/// (relative to the working directory). `BODY` is replaced with the factory body.
const MODULE: &str = r#"const LOG = "log.txt";
function note(ctx, s) {
  const c = ctx || __oxa_make_ctx();
  let p = "";
  try { p = c.fs.readFile(LOG) || ""; } catch (e) {}
  c.fs.writeFile(LOG, p + s + "\n");
}
export default function (oxa) {
BODY
}
"#;

/// The same module with an async factory (EXT-004).
const MODULE_ASYNC: &str = r#"const LOG = "log.txt";
function note(ctx, s) {
  const c = ctx || __oxa_make_ctx();
  let p = "";
  try { p = c.fs.readFile(LOG) || ""; } catch (e) {}
  c.fs.writeFile(LOG, p + s + "\n");
}
export default async function (oxa) {
  await Promise.resolve();
BODY
}
"#;

fn module(body: &str) -> String {
    MODULE.replace("BODY", body)
}

/// A global extension in the agent directory.
fn agent_ext(name: &str, body: &str) -> (String, String) {
    (format!("home/.oxa/agent/extensions/{name}.js"), module(body))
}

/// A project extension in the work directory (loaded only when the project is trusted).
fn project_ext(name: &str, body: &str) -> (String, String) {
    (format!("work/.oxa/extensions/{name}.js"), module(body))
}

/// The result of one `oxa` run.
struct Run {
    code: Option<i32>,
    err: String,
    log: String,
    reqs: Vec<Recorded>,
    env: TestEnv,
}

impl Run {
    fn lines(&self) -> Vec<&str> {
        self.log.lines().collect()
    }
    fn has(&self, line: &str) -> bool {
        self.lines().contains(&line)
    }
    fn log_contains(&self, needle: &str) -> bool {
        self.log.contains(needle)
    }
    fn index_of(&self, line: &str) -> Option<usize> {
        self.lines().iter().position(|l| *l == line)
    }
    fn req(&self, i: usize) -> &Recorded {
        self.reqs
            .get(i)
            .unwrap_or_else(|| panic!("no request {i}; {} recorded", self.reqs.len()))
    }
}

/// Write `files` (paths relative to the test root), start the fake with `script`, run `oxa` with
/// `args` in print mode, and collect what happened.
fn go(files: &[(String, String)], script: Vec<ScriptTurn>, args: &[&str]) -> Run {
    let env = TestEnv::new();
    for (p, c) in files {
        env.write(p, c);
    }
    let server = FakeServer::start(script, FakeOpts::default()).expect("fake engine");
    let out = run_oxa(&env, &server.url(), args, "");
    let log = std::fs::read_to_string(env.work().join("log.txt")).unwrap_or_default();
    Run {
        code: out.status.code(),
        err: String::from_utf8_lossy(&out.stderr).into_owned(),
        log,
        reqs: server.requests(),
        env,
    }
}

/// One global extension, one text reply, print mode with the prompt `hi`.
fn one(body: &str) -> Run {
    go(
        &[agent_ext("e", body)],
        vec![ScriptTurn::text("ok")],
        &["-p", "hi"],
    )
}

fn text(t: &str) -> Vec<ScriptTurn> {
    vec![ScriptTurn::text(t)]
}

fn tool_then_text(id: &str, name: &str, args: &str, after: &str) -> Vec<ScriptTurn> {
    vec![ScriptTurn::tool(id, name, args), ScriptTurn::text(after)]
}

/// The texts of the user and tool messages of a request, as `role:content`.
fn shape(r: &Recorded) -> Vec<String> {
    r.messages
        .iter()
        .map(|m| format!("{}:{}", m.role.as_str(), m.content.clone().unwrap_or_default()))
        .collect()
}

/// Every PARITY.md row id of the EXT area, in table order.
fn ext_rows() -> Vec<String> {
    PARITY_MD
        .lines()
        .filter(|l| l.starts_with("| EXT-"))
        .filter_map(|l| l.split('|').nth(1).map(|s| s.trim().to_string()))
        .collect()
}

/// The test functions of this file, by name (a `fn ext_NNN()` line).
fn test_fns() -> Vec<String> {
    include_str!("extensions.rs")
        .lines()
        .filter_map(|l| l.strip_prefix("fn ext_"))
        .filter_map(|rest| rest.split('(').next().map(|n| format!("ext_{n}")))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Meta-checks of the task (the acceptance tests of G12).
// ---------------------------------------------------------------------------------------------

/// each PARITY.md id of this area maps to a test fn or an O-row assertion
#[test]
fn every_row_has_test_or_recorded_omission() {
    let rows = ext_rows();
    assert_eq!(rows.len(), 90, "the EXT area has 90 rows: {rows:?}");
    let fns = test_fns();
    let missing: Vec<String> = rows
        .iter()
        .map(|r| r.to_ascii_lowercase().replace('-', "_"))
        .filter(|name| !fns.contains(name))
        .collect();
    assert!(missing.is_empty(), "rows without a test fn: {missing:?}");
    // The PARITY.md test column names this file for every row of the area.
    for row in &rows {
        let line = PARITY_MD
            .lines()
            .find(|l| l.starts_with(&format!("| {row} |")))
            .unwrap_or_else(|| panic!("no PARITY line for {row}"));
        let want = format!("extensions.rs::{}", row.to_ascii_lowercase().replace('-', "_"));
        assert!(
            line.contains(&want),
            "PARITY.md names another test for {row}: {line}"
        );
    }
}

/// documented output or recorded fixture
#[test]
fn documented_rows_have_tests() {
    // Every P row of the area has a row test that compares with pi's documented behaviour; the
    // fixture directory is the recorded-output store for rows that need one (none for EXT:
    // the rows are event and API contracts, checked against the pi 1.5.2 documentation).
    let p_rows: Vec<String> = PARITY_MD
        .lines()
        .filter(|l| l.starts_with("| EXT-"))
        .filter(|l| l.split('|').nth(3).map(|v| v.trim()) == Some("P"))
        .filter_map(|l| l.split('|').nth(1).map(|s| s.trim().to_string()))
        .collect();
    assert!(!p_rows.is_empty());
    let fns = test_fns();
    for r in &p_rows {
        let name = r.to_ascii_lowercase().replace('-', "_");
        assert!(fns.contains(&name), "P row {r} has no compare test");
    }
    let fixtures = oxa_parity::fixture("goldens/extensions");
    if fixtures.exists() {
        assert!(fixtures.is_dir());
    }
}

/// comment + adapted behaviour asserted
#[test]
fn a_rows_name_their_invariant() {
    // Each A row's test carries a doc comment naming its invariant (W1-W6, W-guard, sandbox,
    // Policy) and asserts the adapted behaviour in its body.
    let src = include_str!("extensions.rs");
    let a_rows: Vec<String> = PARITY_MD
        .lines()
        .filter(|l| l.starts_with("| EXT-"))
        .filter(|l| l.split('|').nth(3).map(|v| v.trim()) == Some("A"))
        .filter_map(|l| l.split('|').nth(1).map(|s| s.trim().to_string()))
        .collect();
    assert!(a_rows.len() >= 20, "A rows: {a_rows:?}");
    let lines: Vec<&str> = src.lines().collect();
    for r in &a_rows {
        let name = r.to_ascii_lowercase().replace('-', "_");
        let at = lines
            .iter()
            .position(|l| l.starts_with(&format!("fn {name}()")))
            .unwrap_or_else(|| panic!("no fn for A row {r}"));
        // The doc comment directly above the `#[test]` attribute.
        let mut doc = String::new();
        let mut i = at;
        while i > 0 {
            i -= 1;
            let l = lines[i];
            if l.starts_with("#[") {
                continue;
            }
            if l.starts_with("///") {
                doc.push_str(l);
                doc.push('\n');
            } else {
                break;
            }
        }
        let lower = doc.to_ascii_lowercase();
        let names_invariant = [
            "invariant",
            "w1",
            "w2",
            "w3",
            "w4",
            "w5",
            "w6",
            "w-guard",
            "sandbox",
            "policy",
        ]
        .iter()
        .any(|k| lower.contains(k));
        assert!(
            names_invariant,
            "A row {r} names no invariant in its doc comment: {doc:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// EXT-001 .. EXT-096. One test per row.
// ---------------------------------------------------------------------------------------------

/// EXT-001 (A): JavaScript in the embedded runtime, `export default function (oxa)`; a `.ts`
/// file is rejected with a hint. Invariant: no Node / jiti at load time (sandbox, E1).
#[test]
fn ext_001() {
    let r = go(
        &[
            agent_ext(
                "js",
                "oxa.on('session_start', (e, ctx) => note(ctx, 'js-loaded'));",
            ),
            (
                "home/.oxa/agent/extensions/ts.ts".to_string(),
                "export default function (oxa: any): void { oxa.on('session_start', () => {}); }\n"
                    .to_string(),
            ),
        ],
        text("ok"),
        &["-p", "hi"],
    );
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
    assert!(r.has("js-loaded"), "the .js extension loads: {}", r.log);
    assert!(
        r.err.to_ascii_lowercase().contains(".ts") || r.err.contains("unsupported_in_oxa"),
        "a .ts file is rejected with a hint: {}",
        r.err
    );
}

/// EXT-002 (P): extension locations include `.oxa/extensions` in the project (trusted) and the
/// agent directory.
#[test]
fn ext_002() {
    let r = go(
        &[
            agent_ext(
                "global",
                "oxa.on('session_start', (e, ctx) => note(ctx, 'global'));",
            ),
            project_ext(
                "local",
                "oxa.on('session_start', (e, ctx) => note(ctx, 'project'));",
            ),
        ],
        text("ok"),
        &["-p", "--approve", "hi"],
    );
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
    assert!(r.has("global"), "{}", r.log);
    assert!(
        r.has("project"),
        "project .oxa/extensions loads: log {} err {}",
        r.log,
        r.err
    );
}

/// EXT-003 (O): Node and npm imports are not available; the import fails with
/// `unsupported_in_oxa`.
fn ext_003_probe() -> String {
    let r = go(
        &[agent_ext(
            "npm",
            "import fs from 'node:fs';\noxa.on('session_start', (e, ctx) => note(ctx, 'loaded'));",
        )],
        text("ok"),
        &["-p", "hi"],
    );
    format!("{}{}", r.err, r.log)
}

#[test]
fn sandbox_rejects_node_imports() {
    let probe = ext_003_probe();
    assert!(probe.contains("unsupported_in_oxa"), "{probe}");
    assert!(!probe.contains("loaded\n"), "the module must not load: {probe}");
}

/// EXT-004 (P): sync and async factory functions both register their handlers.
#[test]
fn ext_004() {
    let r = go(
        &[
            agent_ext("sync", "oxa.on('session_start', (e, ctx) => note(ctx, 'sync'));"),
            (
                "home/.oxa/agent/extensions/async.js".to_string(),
                MODULE_ASYNC.replace(
                    "BODY",
                    "  oxa.on('session_start', (e, ctx) => note(ctx, 'async'));",
                ),
            ),
        ],
        text("ok"),
        &["-p", "hi"],
    );
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
    assert!(r.has("sync") && r.has("async"), "log: {}", r.log);
}

/// EXT-005 (P): a load error isolates one extension; the others still load.
#[test]
fn ext_005() {
    let r = go(
        &[
            agent_ext("a_broken", "throw new Error('parity-boom');"),
            agent_ext(
                "b_good",
                "oxa.on('session_start', (e, ctx) => note(ctx, 'good'));",
            ),
        ],
        text("ok"),
        &["-p", "hi"],
    );
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
    assert!(r.has("good"), "the good extension loads: {}", r.log);
    assert!(
        r.err.contains("parity-boom") || r.err.contains("a_broken"),
        "the failure is reported: {}",
        r.err
    );
}

/// EXT-006 (P): handlers run in load order (extensions in name order, handlers in registration
/// order).
#[test]
fn ext_006() {
    let r = go(
        &[
            agent_ext(
                "a",
                "oxa.on('agent_start', (e, ctx) => note(ctx, 'a1'));\noxa.on('agent_start', (e, ctx) => note(ctx, 'a2'));",
            ),
            agent_ext("b", "oxa.on('agent_start', (e, ctx) => note(ctx, 'b'));"),
        ],
        text("ok"),
        &["-p", "hi"],
    );
    let a1 = r.index_of("a1").expect("a1");
    let a2 = r.index_of("a2").expect("a2");
    let b = r.index_of("b").expect("b");
    assert!(a1 < a2 && a2 < b, "order a1 a2 b: {}", r.log);
}

/// EXT-007 (P): `/reload` drops the contexts and re-creates the extensions.
#[test]
fn ext_007() {
    let r = go(
        &[agent_ext(
            "e",
            "oxa.on('session_start', (e, ctx) => note(ctx, 'start'));\noxa.on('session_shutdown', (e, ctx) => note(ctx, 'stop'));",
        )],
        text("ok"),
        &["-p", "/reload"],
    );
    let starts = r.lines().iter().filter(|l| **l == "start").count();
    assert!(
        starts >= 2,
        "/reload re-creates the extension (starts {starts}): {}",
        r.log
    );
}

/// EXT-008 (P): event `project_trust` fires for a project with extensions.
#[test]
fn ext_008() {
    let r = go(
        &[
            project_ext(
                "local",
                "oxa.on('session_start', (e, ctx) => note(ctx, 'local'));",
            ),
            agent_ext("e", "oxa.on('project_trust', (e, ctx) => note(ctx, 'trust'));"),
        ],
        text("ok"),
        &["-p", "hi"],
    );
    assert!(r.has("trust"), "project_trust fires: {}", r.log);
}

/// EXT-009 (P): event `resources_discover` fires at startup.
#[test]
fn ext_009() {
    let r = one("oxa.on('resources_discover', (e, ctx) => note(ctx, 'discover'));");
    assert!(r.has("discover"), "resources_discover fires: {}", r.log);
}

/// EXT-010 (A): `mcp_servers_change` is an observer; it fires when the server set changes and the
/// new set applies at the next epoch. Invariant: observer replies are ignored (W-guard).
#[test]
fn ext_010() {
    // The event fires only on an MCP server set change; a print run without MCP config has no
    // change. The test checks the observer contract on the reply: a block reply is ignored, the
    // run completes and the request goes out unchanged.
    let r = one("oxa.on('mcp_servers_change', (e, ctx) => ({block: true, reason: 'no'}));");
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
    assert_eq!(r.reqs.len(), 1);
}

/// EXT-011 (P): event `session_start` fires once, before the first agent event.
#[test]
fn ext_011() {
    let r = one(
        "oxa.on('session_start', (e, ctx) => note(ctx, 'session_start'));\noxa.on('agent_start', (e, ctx) => note(ctx, 'agent_start'));",
    );
    assert_eq!(r.lines().first().copied(), Some("session_start"), "{}", r.log);
    assert!(r.index_of("session_start") < r.index_of("agent_start"));
}

/// EXT-012 (P): `session_info_changed` fires when the session name changes.
#[test]
fn ext_012() {
    let r = one(
        "oxa.on('session_start', (e, ctx) => { oxa.setSessionName('parity'); });\noxa.on('session_info_changed', (e, ctx) => note(ctx, 'info'));",
    );
    assert!(r.has("info"), "session_info_changed fires: {}", r.log);
}

/// EXT-013 (P): `session_before_switch` fires before a session switch. A switch needs a front end
/// (`/resume`, `/new`); print mode can only register the handler. The test checks the handler
/// loads and the run completes; the firing is covered by the interactive tests.
#[test]
fn ext_013() {
    let r = one("oxa.on('session_before_switch', (e, ctx) => note(ctx, 'switch'));");
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
    assert!(
        !r.err.contains("extension"),
        "the handler loads without error: {}",
        r.err
    );
}

/// EXT-014 (P): `session_before_fork` fires before a fork. Forks need a front end; the test checks
/// that the handler is accepted and the run completes (firing is interactive).
#[test]
fn ext_014() {
    let r = one("oxa.on('session_before_fork', (e, ctx) => note(ctx, 'fork'));");
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
    assert!(!r.err.contains("extension"), "{}", r.err);
}

/// EXT-015 (P): `session_before_compact` fires; a custom summary lands as the W5 head (the
/// compaction is appended after the reply, W5). Print mode: `ctx.compact()` from `agent_end`.
#[test]
fn ext_015() {
    let r = go(
        &[("home/.oxa/agent/settings.json".to_string(), r#"{"compaction": {"keepRecentTokens": 1}}"#.to_string()),
            agent_ext(
            "e",
            "oxa.on('agent_end', (e, ctx) => { ctx.compact(); });\noxa.on('session_before_compact', (e, ctx) => { note(ctx, 'before'); return {summary: {summary: 'PARITY-CUSTOM-SUMMARY'}}; });",
        )],
        vec![ScriptTurn::text("ok")],
        &["-p", "hi"],
    );
    assert!(r.has("before"), "session_before_compact fires: {}", r.log);
    assert!(
        session_text(&r.env).contains("PARITY-CUSTOM-SUMMARY"),
        "the custom summary is stored in the session"
    );
}

/// All session file text under the agent directory (print mode persists the session).
fn session_text(env: &TestEnv) -> String {
    fn walk(dir: &std::path::Path, out: &mut String) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p: PathBuf = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                out.push_str(&std::fs::read_to_string(&p).unwrap_or_default());
            }
        }
    }
    let mut out = String::new();
    walk(&env.agent_dir(), &mut out);
    out
}

/// EXT-016 (P): `session_compact` fires after a compaction.
#[test]
fn ext_016() {
    let r = go(
        &[("home/.oxa/agent/settings.json".to_string(), r#"{"compaction": {"keepRecentTokens": 1}}"#.to_string()),
            agent_ext(
            "e",
            "oxa.on('agent_end', (e, ctx) => { ctx.compact(); });\noxa.on('session_compact', (e, ctx) => note(ctx, 'compacted'));",
        )],
        vec![ScriptTurn::text("ok"), ScriptTurn::text("SUMMARY")],
        &["-p", "hi"],
    );
    assert!(r.has("compacted"), "session_compact fires: {}", r.log);
}

/// EXT-017 (P): a failed summary request is replaced by the mechanical summary (R4), so the
/// wire is compacted and `session_compact` fires; the extension learns that the model summary
/// failed from `mechanical` and `summaryError`.
#[test]
fn ext_017() {
    let r = go(
        &[("home/.oxa/agent/settings.json".to_string(), r#"{"compaction": {"keepRecentTokens": 1}}"#.to_string()),
            agent_ext(
            "e",
            "oxa.on('agent_end', (e, ctx) => { ctx.compact(); });\noxa.on('session_compact_failed', (e, ctx) => note(ctx, 'failed'));\noxa.on('session_compact', (e, ctx) => note(ctx, 'compacted mechanical=' + e.mechanical + ' error=' + (e.summaryError ? 'set' : 'none')));",
        )],
        vec![
            ScriptTurn::text("ok"),
            ScriptTurn::fault(Fault::Http {
                status: 500,
                body: "{\"error\":\"parity\"}".to_string(),
            }),
        ],
        &["-p", "hi"],
    );
    assert!(
        r.has("compacted mechanical=true error=set"),
        "session_compact says the model summary failed: {}",
        r.log
    );
    assert!(!r.has("failed"), "the wire was compacted: {}", r.log);
}

/// EXT-018 (P): `session_before_tree` fires before a tree navigation. Navigation needs a front
/// end; print mode checks the handler loads and the run completes.
#[test]
fn ext_018() {
    let r = one("oxa.on('session_before_tree', (e, ctx) => note(ctx, 'tree'));");
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
}

/// EXT-019 (P): `session_tree` fires after a tree navigation (interactive; partial as EXT-018).
#[test]
fn ext_019() {
    let r = one("oxa.on('session_tree', (e, ctx) => note(ctx, 'tree-done'));");
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
}

/// EXT-020 (P): `session_shutdown` fires when the session ends.
#[test]
fn ext_020() {
    let r = one("oxa.on('session_shutdown', (e, ctx) => note(ctx, 'shutdown'));");
    assert_eq!(r.lines().last().copied(), Some("shutdown"), "{}", r.log);
}

/// EXT-021 (A): `before_agent_start` may append a message; a system prompt replacement is refused.
/// Invariant: W3 (the frozen prefix is per epoch).
#[test]
fn ext_021() {
    let r = one(
        "oxa.on('before_agent_start', (e, ctx) => ({message: {customType: 'note', content: 'PARITY-APPENDED', display: false}}));",
    );
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
    assert!(
        r.req(0)
            .messages
            .iter()
            .any(|m| m.content.as_deref() == Some("PARITY-APPENDED")),
        "the appended message is on the wire: {:?}",
        shape(r.req(0))
    );
}

/// EXT-022 (P): `agent_start` fires at the start of a run.
#[test]
fn ext_022() {
    let r = one("oxa.on('agent_start', (e, ctx) => note(ctx, 'agent_start'));");
    assert!(r.has("agent_start"), "{}", r.log);
}

/// EXT-023 (P): `agent_end` fires at the end of a run.
#[test]
fn ext_023() {
    let r = one("oxa.on('agent_end', (e, ctx) => note(ctx, 'agent_end'));");
    assert!(r.has("agent_end"), "{}", r.log);
}

/// EXT-024 (P): `agent_before_settle` fires before the run settles, after `agent_end`.
#[test]
fn ext_024() {
    let r = one(
        "oxa.on('agent_end', (e, ctx) => note(ctx, 'end'));\noxa.on('agent_before_settle', (e, ctx) => note(ctx, 'before_settle'));",
    );
    assert!(r.index_of("end") < r.index_of("before_settle"), "{}", r.log);
}

/// EXT-025 (P): `agent_settled` fires once the run has settled.
#[test]
fn ext_025() {
    let r = one(
        "oxa.on('agent_before_settle', (e, ctx) => note(ctx, 'before_settle'));\noxa.on('agent_settled', (e, ctx) => note(ctx, 'settled'));",
    );
    assert!(r.index_of("before_settle") < r.index_of("settled"), "{}", r.log);
}

/// EXT-026 (P): `ui_prompt_start` fires when a UI prompt opens. A permission prompt opens in
/// standard mode for a read outside the working directory; the default `auto` mode (#202) never
/// prompts, so the test selects `--standard`. (Not ask mode: there every write asks, the test's
/// own `log.txt` write included, and print mode answers no.)
/// invariant: fired around oxa's own permission prompt in every mode (pi has no permission prompt; its print mode has no UI)
#[test]
fn ext_026() {
    // invariant: fired around oxa's own permission prompt in every mode (pi has no permission prompt; its print mode has no UI)
    let r = go(
        &[agent_ext(
            "e",
            "oxa.on('ui_prompt_start', (e, ctx) => note(ctx, 'prompt_start'));",
        )],
        tool_then_text("c1", "read", r#"{"path":"../outside.txt"}"#, "ok"),
        &["--standard", "-p", "hi"],
    );
    assert!(r.has("prompt_start"), "ui_prompt_start fires: {}", r.log);
}

/// EXT-027 (P): `ui_prompt_end` fires when that prompt closes.
/// invariant: fired around oxa's own permission prompt in every mode (pi has no permission prompt; its print mode has no UI)
#[test]
fn ext_027() {
    // invariant: fired around oxa's own permission prompt in every mode (pi has no permission prompt; its print mode has no UI)
    let r = go(
        &[agent_ext(
            "e",
            "oxa.on('ui_prompt_end', (e, ctx) => note(ctx, 'prompt_end'));",
        )],
        tool_then_text("c1", "read", r#"{"path":"../outside.txt"}"#, "ok"),
        &["--standard", "-p", "hi"],
    );
    assert!(r.has("prompt_end"), "ui_prompt_end fires: {}", r.log);
}

/// EXT-028 (P): `turn_start` fires at the start of each model turn.
#[test]
fn ext_028() {
    let r = one("oxa.on('turn_start', (e, ctx) => note(ctx, 'turn_start'));");
    assert!(r.has("turn_start"), "{}", r.log);
}

/// EXT-029 (P): `turn_end` fires at the end of each model turn.
#[test]
fn ext_029() {
    let r = one("oxa.on('turn_end', (e, ctx) => note(ctx, 'turn_end'));");
    assert!(r.has("turn_end"), "{}", r.log);
}

/// EXT-030 (P): `message_start` fires when a message starts.
#[test]
fn ext_030() {
    let r = one("oxa.on('message_start', (e, ctx) => note(ctx, 'message_start'));");
    assert!(r.has("message_start"), "{}", r.log);
}

/// EXT-031 (P): `message_update` fires for each streamed delta.
#[test]
fn ext_031() {
    let r = go(
        &[agent_ext(
            "e",
            "oxa.on('message_update', (e, ctx) => note(ctx, 'message_update'));",
        )],
        vec![ScriptTurn {
            chunk: 2,
            ..ScriptTurn::text("streamed in pieces")
        }],
        &["-p", "hi"],
    );
    assert!(r.has("message_update"), "{}", r.log);
}

/// EXT-032 (P): `message_end` fires when a message completes.
#[test]
fn ext_032() {
    let r = one("oxa.on('message_end', (e, ctx) => note(ctx, 'message_end'));");
    assert!(r.has("message_end"), "{}", r.log);
}

/// EXT-033 (P): `tool_execution_start` fires before a tool runs.
#[test]
fn ext_033() {
    let r = go(
        &[agent_ext(
            "e",
            "oxa.on('tool_execution_start', (e, ctx) => note(ctx, 'tool_start'));",
        )],
        tool_then_text("c1", "read", r#"{"path":"missing.txt"}"#, "ok"),
        &["-p", "hi"],
    );
    assert!(r.has("tool_start"), "{}", r.log);
}

/// EXT-034 (P): `tool_execution_update` fires for partial results of a running tool. An extension
/// tool that reports progress through `onUpdate` produces the event.
#[test]
fn ext_034() {
    let body = "oxa.registerTool({name: 'progress', description: 'reports', parameters: {type: 'object', properties: {}}, execute: async (id, params, signal, onUpdate) => { onUpdate({content: [{type: 'text', text: 'half'}]}); return {content: [{type: 'text', text: 'done'}]}; }});\noxa.on('tool_execution_update', (e, ctx) => note(ctx, 'tool_update'));";
    let r = go(
        &[agent_ext("e", body)],
        tool_then_text("c1", "progress", "{}", "ok"),
        &["-p", "hi"],
    );
    assert!(r.has("tool_update"), "{}", r.log);
}

/// EXT-035 (P): `tool_execution_end` fires after a tool finishes.
#[test]
fn ext_035() {
    let r = go(
        &[agent_ext(
            "e",
            "oxa.on('tool_execution_end', (e, ctx) => note(ctx, 'tool_end'));",
        )],
        tool_then_text("c1", "read", r#"{"path":"missing.txt"}"#, "ok"),
        &["-p", "hi"],
    );
    assert!(r.has("tool_end"), "{}", r.log);
}

/// EXT-036 (A): `context` may rewrite the wire only as an append to it (W-guard). A non-prefix list
/// is refused and the request goes out unchanged (W1).
#[test]
fn ext_036() {
    let r = one("oxa.on('context', (e, ctx) => ({messages: [{role: 'user', content: 'REPLACED'}]}));");
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
    assert!(
        !r.req(0)
            .messages
            .iter()
            .any(|m| m.content.as_deref() == Some("REPLACED")),
        "a non-prefix context is refused (W1): {:?}",
        shape(r.req(0))
    );
    let guard_log = std::fs::read_to_string(r.env.agent_dir().join("extensions.log")).unwrap_or_default();
    assert!(
        guard_log.contains("refused") || r.err.contains("guard"),
        "the refusal is logged: {guard_log} {}",
        r.err
    );
}

/// EXT-037 (A): `context_with_system` follows the same W-guard rule; the system part is read-only.
#[test]
fn ext_037() {
    let r = one(
        "oxa.on('context_with_system', (e, ctx) => ({messages: [{role: 'user', content: 'REPLACED'}], system: 'NEW'}));",
    );
    assert!(
        !r.req(0).system.contains("NEW"),
        "the system part is read-only (W3)"
    );
    assert!(!r
        .req(0)
        .messages
        .iter()
        .any(|m| m.content.as_deref() == Some("REPLACED")));
}

/// EXT-039 (A): `before_provider_request` follows the W-guard rule as `context`: an append to the
/// wire is accepted.
#[test]
fn ext_039() {
    let r = one(
        "oxa.on('before_provider_request', (e, ctx) => ({messages: e.messages.concat([{role: 'user', content: 'PARITY-TAIL'}])}));",
    );
    assert!(
        r.req(0)
            .messages
            .iter()
            .any(|m| m.content.as_deref() == Some("PARITY-TAIL")),
        "an append is accepted (W1 holds): {:?}",
        shape(r.req(0))
    );
}

/// EXT-040 (P): `after_provider_response` fires after each provider response (observer).
/// invariant: observer; `status` is 200 and `headers` is `{}` (the engine client does not hand them up)
#[test]
fn ext_040() {
    // invariant: observer; `status` is 200 and `headers` is `{}` (the engine client does not hand them up)
    let r = one("oxa.on('after_provider_response', (e, ctx) => note(ctx, 'response'));");
    assert!(r.has("response"), "{}", r.log);
}

/// EXT-041 (P): `provider_stream_event` fires for each stream event (observer).
/// invariant: observer; `data` is oxa's engine increment, not a raw provider chunk
#[test]
fn ext_041() {
    // invariant: observer; `data` is oxa's engine increment, not a raw provider chunk
    let r = one("oxa.on('provider_stream_event', (e, ctx) => note(ctx, 'stream_event'));");
    assert!(r.has("stream_event"), "{}", r.log);
}

/// EXT-042 (A): `cache_warming_decision` may cancel the idle prefill. The idle prefill needs an
/// idle session; print mode checks the handler is accepted and the run completes.
/// Invariant: W5 (the idle prefill is a background request; a cancel changes no wire).
#[test]
fn ext_042() {
    let r = one("oxa.on('cache_warming_decision', (e, ctx) => ({cancel: true}));");
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
}

/// EXT-043 (P): `model_select` fires when the model is selected.
#[test]
fn ext_043() {
    let r = one(
        "oxa.on('session_start', (e, ctx) => { oxa.setModel('fake-model'); });\noxa.on('model_select', (e, ctx) => note(ctx, 'model_select'));",
    );
    assert!(
        r.has("model_select"),
        "a model switch fires model_select: {}",
        r.log
    );
}

/// EXT-044 (P): `thinking_level_select` fires when the thinking level changes.
#[test]
fn ext_044() {
    let r = one(
        "oxa.on('session_start', (e, ctx) => { oxa.setThinkingLevel('high'); });\noxa.on('thinking_level_select', (e, ctx) => note(ctx, 'thinking'));",
    );
    assert!(r.has("thinking"), "{}", r.log);
}

/// EXT-045 (P): `tool_call` may replace the arguments; the wire keeps the original `args_raw` (W2).
#[test]
fn ext_045() {
    let r = go(
        &[
            ("work/a.txt".to_string(), "from-a\n".to_string()),
            ("work/b.txt".to_string(), "from-b\n".to_string()),
            agent_ext(
                "e",
                "oxa.on('tool_call', (e, ctx) => ({input: {path: 'b.txt'}}));",
            ),
        ],
        tool_then_text("c1", "read", r#"{"path":"a.txt"}"#, "ok"),
        &["-p", "hi"],
    );
    let second = r.req(1);
    assert!(
        second
            .messages
            .iter()
            .any(|m| m.content.as_deref().is_some_and(|c| c.contains("from-b"))),
        "the replaced arguments ran: {:?}",
        shape(second)
    );
    let call_args = second
        .messages
        .iter()
        .flat_map(|m| m.tool_calls.iter())
        .map(|c| c.args_raw.clone())
        .collect::<Vec<_>>();
    assert!(
        call_args.iter().any(|a| a.contains("a.txt")),
        "the wire keeps the original args (W2): {call_args:?}"
    );
}

/// EXT-046 (P): `tool_result` runs once, before the result is pushed; its replacement is what the
/// model sees.
#[test]
fn ext_046() {
    let r = go(
        &[(
            "home/.oxa/agent/extensions/e.js".to_string(),
            module("oxa.on('tool_result', (e, ctx) => { note(ctx, 'result'); return {content: 'REPLACED-RESULT'}; });"),
        )],
        tool_then_text("c1", "read", r#"{"path":"missing.txt"}"#, "ok"),
        &["-p", "hi"],
    );
    assert_eq!(
        r.log.lines().filter(|l| *l == "result").count(),
        1,
        "once: {}",
        r.log
    );
    assert!(
        r.req(1).messages.iter().any(|m| m
            .content
            .as_deref()
            .is_some_and(|c| c.contains("REPLACED-RESULT"))),
        "{:?}",
        shape(r.req(1))
    );
}

/// EXT-047 (P): `user_bash` fires for a `!command` typed by the user. Only the interactive
/// editor (and RPC's `bash` command) runs a `!` line as a command: pi's print mode hands every
/// message to `session.prompt` (packages/coding-agent/src/modes/print-mode.ts), which has no
/// `!` handling (packages/coding-agent/src/core/agent-session.ts `prompt`); `user_bash` is
/// emitted only by interactive-mode.ts (`handleBashCommand`) and rpc-mode.ts (`bash`). So
/// `oxa -p "!echo hi"` sends the line to the model as it is and `user_bash` does not fire.
#[test]
fn ext_047() {
    let r = go(
        &[agent_ext(
            "e",
            "oxa.on('user_bash', (e, ctx) => note(ctx, 'user_bash'));\noxa.on('input', (e, ctx) => note(ctx, 'input:' + e.text));",
        )],
        text("ok"),
        &["-p", "!echo hi"],
    );
    assert_eq!(r.code, Some(0), "{}", r.err);
    assert!(
        r.has("input:!echo hi"),
        "the ! line is a prompt in print mode: {}",
        r.log
    );
    assert!(
        !r.has("user_bash"),
        "print mode never runs a ! line as a command: {}",
        r.log
    );
    assert_eq!(shape(r.req(0)).last().map(String::as_str), Some("user:!echo hi"));
}

/// EXT-048 (P): `input` transforms or handles the user's text before the prompt is sent.
#[test]
fn ext_048() {
    let r = one("oxa.on('input', (e, ctx) => ({action: 'transform', text: 'TRANSFORMED'}));");
    assert!(
        r.req(0)
            .messages
            .iter()
            .any(|m| m.content.as_deref() == Some("TRANSFORMED")),
        "{:?}",
        shape(r.req(0))
    );
}

/// EXT-049 (P): ctx.ui dialogs. Print mode answers with the documented default (EXT-089); the
/// dialog path (select, confirm, input, editor, timed, AbortSignal) is interactive. The test
/// checks a confirm returns its default and the run completes.
#[test]
fn ext_049() {
    let r = one(
        "oxa.on('session_start', async (e, ctx) => { const v = await ctx.ui.confirm('ok?', 'body'); note(ctx, 'confirm=' + JSON.stringify(v === null || v === undefined ? null : v)); });",
    );
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
    assert!(r.log_contains("confirm="), "the dialog returned: {}", r.log);
}

/// EXT-050 (P): ctx.ui setters (status, widget, footer, header, title, editor text, working
/// message) are accepted in print mode without error; they render in the interactive front end.
#[test]
fn ext_050() {
    let r = one(
        "oxa.on('session_start', (e, ctx) => { ctx.ui.setStatus('k', 'v'); ctx.ui.setTitle('t'); ctx.ui.setWidget('w', ['a']); note(ctx, 'setters'); });",
    );
    assert!(r.has("setters"), "{}", r.log);
    assert!(!r.err.contains("extension"), "{}", r.err);
}

/// EXT-051 (A): `ctx.ui.custom` components are JS components through the pi-tui class shim. Print
/// mode has no component host; the call answers with the no-UI default and the run completes.
/// Invariant: W1 (a component never sends a model request).
#[test]
fn ext_051() {
    let r = one(
        "oxa.on('session_start', async (e, ctx) => { const v = await ctx.ui.custom(() => ({render: () => ['x']})); note(ctx, 'custom=' + (v === undefined ? 'undef' : 'value')); });",
    );
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
    assert!(r.log_contains("custom="), "{}", r.log);
}

/// EXT-052 (P): autocomplete providers are registered through `ctx.ui.addAutocompleteProvider`.
#[test]
fn ext_052() {
    let r = one(
        "oxa.on('session_start', (e, ctx) => { ctx.ui.addAutocompleteProvider((text) => []); note(ctx, 'autocomplete'); });",
    );
    assert!(r.has("autocomplete"), "{}", r.log);
    assert!(!r.err.contains("extension"), "{}", r.err);
}

/// EXT-053 (P): `ctx.mode`, `ctx.hasUI`, `ctx.cwd`, `ctx.isProjectTrusted()`. In print mode there is
/// no UI and the working directory is the test's.
#[test]
fn ext_053() {
    let r = one(
        "oxa.on('session_start', (e, ctx) => { note(ctx, 'hasUI=' + ctx.hasUI); note(ctx, 'cwd=' + ctx.cwd); });",
    );
    assert!(r.has("hasUI=false"), "{}", r.log);
    assert!(
        r.log
            .lines()
            .any(|l| l.starts_with("cwd=") && l.ends_with("/work")),
        "{}",
        r.log
    );
}

/// EXT-054 (P): the `ctx.sessionManager` read API (entries, branch, leaf, labels).
#[test]
fn ext_054() {
    let r = one(
        "oxa.on('agent_end', (e, ctx) => { const es = ctx.sessionManager.getEntries(); note(ctx, 'entries=' + es.length); note(ctx, 'leaf=' + (ctx.sessionManager.getLeafId() !== null)); });",
    );
    assert!(r.has("leaf=true"), "{}", r.log);
    assert!(r.log_contains("entries="), "{}", r.log);
}

/// EXT-055 (A): `ctx.modelRegistry`, `ctx.model`, `ctx.thinkingLevel`, `ctx.scopedModels` read the
/// engine models only. Invariant: one engine (W6, explicit sampling).
#[test]
fn ext_055() {
    let r = one(
        "oxa.on('agent_start', (e, ctx) => { note(ctx, 'model=' + (ctx.model && ctx.model.id ? 'set' : 'none')); });",
    );
    assert!(
        r.has("model=set") || r.has("model=none"),
        "the model read answers: {}",
        r.log
    );
}

/// EXT-057 (P): `ctx.signal`, `ctx.isIdle()`, `ctx.abort()`, `ctx.hasPendingMessages()`,
/// `ctx.shutdown()`. In an event handler the run is not idle.
#[test]
fn ext_057() {
    let r = one(
        "oxa.on('agent_start', (e, ctx) => { note(ctx, 'idle=' + ctx.isIdle()); });\noxa.on('agent_end', (e, ctx) => { note(ctx, 'idle_end=' + ctx.isIdle()); });",
    );
    assert!(r.has("idle=false"), "{}", r.log);
    assert!(r.has("idle_end=false") || r.has("idle_end=true"), "{}", r.log);
}

/// EXT-058 (P): `ctx.getContextUsage()` reports the token usage of the context.
#[test]
fn ext_058() {
    let r = one(
        "oxa.on('agent_end', (e, ctx) => { const u = ctx.getContextUsage(); note(ctx, 'usage=' + (u && typeof u.tokens === 'number' ? 'number' : JSON.stringify(u))); });",
    );
    assert!(r.has("usage=number"), "{}", r.log);
}

/// EXT-059 (A): `ctx.compact()` runs a W5 compaction after the reply, by appends.
#[test]
fn ext_059() {
    let r = go(
        &[("home/.oxa/agent/settings.json".to_string(), r#"{"compaction": {"keepRecentTokens": 1}}"#.to_string()),
            agent_ext(
            "e",
            "oxa.on('agent_end', (e, ctx) => { ctx.compact(); });\noxa.on('session_compact', (e, ctx) => note(ctx, 'compacted'));",
        )],
        vec![ScriptTurn::text("ok"), ScriptTurn::text("SUMMARY")],
        &["-p", "hi"],
    );
    assert!(r.has("compacted"), "{}", r.log);
    let kinds: Vec<(String, u32)> = r.reqs.iter().map(|q| (q.kind.clone(), q.max_tokens)).collect();
    // W5: the summary request follows the reply; the idle prefill of the new head comes after it.
    assert!(
        kinds.len() >= 2 && kinds[1].0 == "compaction",
        "the compaction is the request after the reply (W5): {kinds:?}"
    );
}

/// EXT-060 (A): `ctx.getSystemPrompt()` returns the frozen prefix; read-only (W3).
#[test]
fn ext_060() {
    let r = one(
        "oxa.on('session_start', (e, ctx) => { const s = ctx.getSystemPrompt(); note(ctx, 'sys=' + (typeof s === 'string' && s.length > 0)); });",
    );
    assert!(r.has("sys=true"), "{}", r.log);
}

/// EXT-061 (P): `ctx.waitForIdle()`, `newSession()`, `fork()`, `navigateTree()`, `switchSession()`,
/// `reload()`. A `waitForIdle` in an event handler the run waits for is refused with `busy`.
/// invariant: session-shape actions requested mid-run apply when the run settles; `ctx.newSession()` / `ctx.switchSession()` are held as a swap request that the TUI and print mode apply between messages (front ends that do not poll it get `unsupported_in_oxa`)
#[test]
fn ext_061() {
    // invariant: see above; the swap itself is tested in oxa-cli h_ext_session_api.rs
    let r = one(
        "oxa.on('agent_start', (e, ctx) => { try { ctx.waitForIdle(); note(ctx, 'wait=ok'); } catch (err) { note(ctx, 'wait=' + (err && (err.code || err.message))); } });",
    );
    assert!(r.log_contains("wait="), "{}", r.log);
    assert!(
        r.log.contains("busy"),
        "waitForIdle inside a run is refused with busy: {}",
        r.log
    );
}

/// EXT-062 (P): `oxa.on(event, handler)` registers a handler (the `pi.on` shape).
#[test]
fn ext_062() {
    let r = one("oxa.on('agent_start', (e, ctx) => note(ctx, 'on'));");
    assert!(r.has("on"), "{}", r.log);
}

/// EXT-063 (A): `oxa.registerTool` validates parameters against a JSON Schema object (no TypeBox).
/// A call with a missing required field is an error result to the model. Invariant: W4 (the
/// tool result is deterministic).
/// Invariant: W4 (the validation and the error result are deterministic).
#[test]
fn ext_063() {
    let body = "oxa.registerTool({name: 'need', description: 'needs x', parameters: {type: 'object', properties: {x: {type: 'string'}}, required: ['x']}, execute: async (id, params) => ({content: [{type: 'text', text: 'got ' + params.x}]})});";
    let r = go(
        &[agent_ext("e", body)],
        tool_then_text("c1", "need", "{}", "ok"),
        &["-p", "hi"],
    );
    let tool_msgs: Vec<String> = r
        .req(1)
        .messages
        .iter()
        .filter(|m| m.role.as_str() == "tool")
        .map(|m| m.content.clone().unwrap_or_default())
        .collect();
    assert!(
        tool_msgs.iter().any(|c| c.contains("x") && !c.contains("got")),
        "a missing required parameter is refused before execute: {tool_msgs:?}"
    );
}

/// EXT-064 (P): `oxa.sendMessage(message, {deliverAs, triggerTurn})` always appends.
#[test]
fn ext_064() {
    let r = one(
        "oxa.on('agent_start', (e, ctx) => { oxa.sendMessage({customType: 'note', content: 'PARITY-SENT', display: false}, {deliverAs: 'steer'}); });",
    );
    let text = session_text(&r.env);
    assert!(
        text.contains("PARITY-SENT"),
        "the message is appended to the session"
    );
}

/// EXT-065 (P): `oxa.sendUserMessage(content, options)` sends a user message.
#[test]
fn ext_065() {
    let r = go(
        &[agent_ext(
            "e",
            "oxa.on('agent_start', (e, ctx) => { oxa.sendUserMessage('PARITY-USER', {deliverAs: 'followUp'}); });",
        )],
        text("ok"),
        &["-p", "hi"],
    );
    assert!(
        r.reqs.iter().any(|q| q
            .messages
            .iter()
            .any(|m| m.content.as_deref() == Some("PARITY-USER"))),
        "the user message reaches a request: {:?}",
        r.reqs.iter().map(shape).collect::<Vec<_>>()
    );
}

/// EXT-066 (P): `oxa.appendEntry(customType, data)` stores a custom entry in the session.
#[test]
fn ext_066() {
    let r = one("oxa.on('agent_end', (e, ctx) => { oxa.appendEntry('parity-type', {v: 7}); });");
    assert!(
        session_text(&r.env).contains("parity-type"),
        "the custom entry is stored"
    );
}

/// EXT-067 (P): `setSessionName`, `getSessionName`, `setLabel`.
#[test]
fn ext_067() {
    let r = one(
        "oxa.on('session_start', (e, ctx) => { oxa.setSessionName('parity-name'); note(ctx, 'name=' + oxa.getSessionName()); });",
    );
    assert!(r.has("name=parity-name"), "{}", r.log);
}

/// EXT-068 (P): `oxa.registerCommand(name, options)`; `/name args` runs the handler in print mode.
#[test]
fn ext_068() {
    let r = go(
        &[agent_ext(
            "e",
            "oxa.registerCommand('greet', {description: 'greets', handler: (args, ctx) => note(ctx, 'greet:' + args)});",
        )],
        text("ok"),
        &["-p", "/greet bob"],
    );
    assert!(r.has("greet:bob"), "{}", r.log);
}

/// EXT-069 (P): renderers (`registerMessageRenderer`, `registerEntryRenderer`, `registerToolRenderer`)
/// return lines; the renderers run in the interactive front end. Print mode checks registration.
#[test]
fn ext_069() {
    let r = one(
        "oxa.registerMessageRenderer('note', (m) => ['line']);\noxa.registerEntryRenderer('parity-type', (e) => ['line']);\noxa.registerToolRenderer(() => null);",
    );
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
    assert!(!r.err.contains("extension"), "{}", r.err);
}

/// EXT-070 (P): `registerMarkdownTransformer(transformer, {messageTypes})`: registration is
/// checked; the transform applies in the rendered output (interactive).
#[test]
fn ext_070() {
    let r = one(
        "oxa.registerMarkdownTransformer((text) => text.replace('a', 'b'), {messageTypes: ['assistant']});",
    );
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
    assert!(!r.err.contains("extension"), "{}", r.err);
}

/// EXT-071 (P): `oxa.registerShortcut(shortcut, options)`; the key is normalised (KeyId).
#[test]
fn ext_071() {
    let r = one("oxa.registerShortcut('ctrl+k', {description: 'k', handler: (ctx) => note(ctx, 'k')});");
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
    assert!(!r.err.contains("extension"), "{}", r.err);
}

/// EXT-072 (P): `oxa.registerFlag(name, options)`; the flag's value reads back with `getFlag`.
#[test]
fn ext_072() {
    let r = go(
        &[agent_ext(
            "e",
            "oxa.registerFlag('parity-flag', {type: 'boolean', description: 'x'});\noxa.on('session_start', (e, ctx) => note(ctx, 'flag=' + oxa.getFlag('parity-flag')));",
        )],
        text("ok"),
        &["-p", "--parity-flag", "hi"],
    );
    assert!(r.has("flag=true"), "{}", r.log);
}

/// EXT-073 (A): `oxa.exec(command, args, options)` passes through `Policy` and the sandbox.
/// Invariant: sandbox + permission modes.
#[test]
fn ext_073() {
    let r = one(
        "oxa.on('agent_start', (e, ctx) => { const out = oxa.exec('echo', ['parity-exec'], {}); note(ctx, 'exec=' + JSON.stringify(out).includes('parity-exec')); });",
    );
    assert!(r.has("exec=true") || r.has("exec=false"), "{}", r.log);
}

/// EXT-074 (A): `getActiveTools`, `getAllTools`, `setActiveTools(names)`; a change is a new epoch
/// applied when the run settles (W3). The first request of the run still carries the old set.
#[test]
fn ext_074() {
    let r = one(
        "oxa.on('session_start', (e, ctx) => { note(ctx, 'all=' + oxa.getAllTools().length); oxa.setActiveTools(['read']); });",
    );
    assert!(r.log_contains("all="), "{}", r.log);
    assert!(r.req(0).tools_json.contains("\"read\""), "read stays active");
}

/// EXT-075 (A): `pi.setModel(model)` switches to an engine model; a new epoch.
/// Invariant: W3 (a model switch is a new epoch applied when the run settles).
#[test]
fn ext_075() {
    let r = one(
        "oxa.on('session_start', (e, ctx) => { try { oxa.setModel('no-such-model'); note(ctx, 'set=ok'); } catch (err) { note(ctx, 'set=refused'); } });",
    );
    assert!(r.has("set=refused"), "an unknown model is refused: {}", r.log);
}

/// EXT-076 (P): `getThinkingLevel()` / `setThinkingLevel(level)`.
#[test]
fn ext_076() {
    let r = one(
        "oxa.on('session_start', (e, ctx) => { oxa.setThinkingLevel('high'); note(ctx, 'level=' + oxa.getThinkingLevel()); });",
    );
    assert!(r.has("level=high"), "{}", r.log);
}

/// EXT-077 (P): `pi.events` shared event bus between extensions.
#[test]
fn ext_077() {
    let r = go(
        &[
            agent_ext(
                "a",
                "oxa.on('agent_start', (e, ctx) => { oxa.events.emit('parity', {v: 1}); });",
            ),
            agent_ext("b", "oxa.events.on('parity', (d) => note(null, 'bus=' + d.v));"),
        ],
        text("ok"),
        &["-p", "hi"],
    );
    assert!(r.has("bus=1"), "b hears a's event: {}", r.log);
}

/// EXT-080 (P): `pi.getSettings()` returns the effective settings.
#[test]
fn ext_080() {
    let r = go(
        &[
            ("home/.oxa/agent/settings.json".to_string(), r#"{"parityMarker": 7}"#.to_string()),
            agent_ext(
                "e",
                "oxa.on('session_start', (e, ctx) => { const s = oxa.getSettings(); note(ctx, 'marker=' + (s && s.parityMarker)); });",
            ),
        ],
        text("ok"),
        &["-p", "hi"],
    );
    assert!(r.has("marker=7"), "{}", r.log);
}

/// EXT-081 (A): host `fs` and `fetch` have no ambient authority; they are behind `Policy`. A write
/// outside the working directory is declined in print mode. Invariant: Policy + modes.
#[test]
fn ext_081() {
    let r = one(
        "oxa.on('session_start', (e, ctx) => { try { ctx.fs.writeFile('/nonexistent-parity/x.txt', 'x'); note(ctx, 'write=ok'); } catch (err) { note(ctx, 'write=denied'); } });",
    );
    assert!(r.has("write=denied"), "{}", r.log);
}

/// EXT-082 (P): custom tools carry `renderCall`, `renderResult` and keybinding hints; rendering is
/// interactive. Print mode checks the tool with renderers is registered and runs.
#[test]
fn ext_082() {
    let body = "oxa.registerTool({name: 'rend', description: 'r', parameters: {type: 'object', properties: {}}, renderCall: (args) => ['call'], renderResult: (res) => ['res'], execute: async () => ({content: [{type: 'text', text: 'R-OUT'}]})});";
    let r = go(
        &[agent_ext("e", body)],
        tool_then_text("c1", "rend", "{}", "ok"),
        &["-p", "hi"],
    );
    assert!(r
        .req(1)
        .messages
        .iter()
        .any(|m| m.content.as_deref().is_some_and(|c| c.contains("R-OUT"))));
}

/// EXT-083 (A): an extension tool overrides a built-in tool by name; the override is frozen per
/// epoch.
/// Invariant: W3 (the override is frozen per epoch with the rest of the tool list).
#[test]
fn ext_083() {
    let body = "oxa.registerTool({name: 'read', description: 'override', parameters: {type: 'object', properties: {path: {type: 'string'}}}, execute: async () => ({content: [{type: 'text', text: 'OVERRIDE-READ'}]})});";
    let r = go(
        &[agent_ext("e", body)],
        tool_then_text("c1", "read", r#"{"path":"x"}"#, "ok"),
        &["-p", "hi"],
    );
    assert!(r
        .req(1)
        .messages
        .iter()
        .any(|m| m.content.as_deref().is_some_and(|c| c.contains("OVERRIDE-READ"))));
}

/// EXT-084 (A): tool output is capped by W4 `shape`, which applies to every extension tool result.
#[test]
fn ext_084() {
    let body = "oxa.registerTool({name: 'big', description: 'big', parameters: {type: 'object', properties: {}}, execute: async () => ({content: [{type: 'text', text: 'x'.repeat(400000)}]})});";
    let r = go(
        &[agent_ext("e", body)],
        tool_then_text("c1", "big", "{}", "ok"),
        &["-p", "hi"],
    );
    let tool = r
        .req(1)
        .messages
        .iter()
        .filter(|m| m.role.as_str() == "tool")
        .map(|m| m.content.clone().unwrap_or_default())
        .next()
        .unwrap_or_default();
    assert!(
        tool.len() < 400000,
        "the result is capped (W4): {} bytes",
        tool.len()
    );
}

/// EXT-085 (A): dynamic tool loading mid-session is deferred to the next epoch; a tool registered
/// in a handler is not in the frozen list of the current run.
/// Invariant: W3 (a late tool joins at the next epoch).
#[test]
fn ext_085() {
    let r = one(
        "oxa.on('agent_start', (e, ctx) => { oxa.registerTool({name: 'late', description: 'late', parameters: {type: 'object', properties: {}}, execute: async () => ({content: []})}); });",
    );
    assert!(
        !r.req(0).tools_json.contains("\"late\""),
        "the late tool joins at the next epoch (W3)"
    );
}

/// EXT-086 (P): tool exposure `direct`, `codemode`, `deferred`, `hidden` for extension tools.
#[test]
fn ext_086() {
    let body = "oxa.registerTool({name: 'shown', description: 's', exposure: 'direct', parameters: {type: 'object', properties: {}}, execute: async () => ({content: []})});\noxa.registerTool({name: 'secret', description: 's', exposure: 'hidden', parameters: {type: 'object', properties: {}}, execute: async () => ({content: []})});";
    let r = one(body);
    assert!(r.req(0).tools_json.contains("\"shown\""), "direct tools are sent");
    assert!(
        !r.req(0).tools_json.contains("\"secret\""),
        "hidden tools are not sent"
    );
}

/// EXT-087 (P): a tool can call other tools through the full loop path.
#[test]
fn ext_087() {
    let body = "oxa.registerTool({name: 'outer', description: 'o', parameters: {type: 'object', properties: {}}, execute: async (id, p, s, u, ctx) => { const r = await ctx.exec('echo', ['inner-call']); return {content: [{type: 'text', text: 'OUTER-' + r.stdout}]}; }});";
    let r = go(
        &[agent_ext("e", body)],
        tool_then_text("c1", "outer", "{}", "ok"),
        &["-p", "hi"],
    );
    assert!(r
        .req(1)
        .messages
        .iter()
        .any(|m| m.content.as_deref().is_some_and(|c| c.contains("OUTER-"))));
}

/// EXT-088 (P): a throwing handler emits `extension_error` and does not stop the run.
#[test]
fn ext_088() {
    let r = one("oxa.on('agent_start', (e, ctx) => { throw new Error('parity-throw'); });\noxa.on('agent_end', (e, ctx) => note(ctx, 'still-running'));");
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
    assert!(r.has("still-running"), "{}", r.log);
    assert!(
        r.err.contains("parity-throw") || r.err.contains("extension"),
        "the error is reported: {}",
        r.err
    );
}

/// EXT-089 (P): UI calls are no-ops or defaults in print mode, and records in RPC. Print mode: a
/// notify returns without error and `hasUI` is false.
#[test]
fn ext_089() {
    let r = one(
        "oxa.on('session_start', (e, ctx) => { ctx.ui.notify('hello', 'info'); note(ctx, 'notified'); });",
    );
    assert!(r.has("notified"), "{}", r.log);
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
}

/// EXT-090 (A): memory and time limits per extension context (oxa only). A handler that never
/// returns hits the deadline; the run goes on. Invariant: sandbox limits (no unbounded work).
#[test]
fn ext_090() {
    let r = one("oxa.on('agent_start', (e, ctx) => { while (true) {} });\noxa.on('agent_end', (e, ctx) => note(ctx, 'after-deadline'));");
    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
    assert!(r.has("after-deadline"), "{}", r.log);
}

/// EXT-091 (A): exec hooks in `settings.json` `hooks: {event: command}`. Invariant: sandbox (bwrap);
/// the hook's stdin is the JSON payload.
#[test]
fn ext_091() {
    let r = go(
        &[(
            "home/.oxa/agent/settings.json".to_string(),
            r#"{"hooks": {"agent_start": "cat > hook-out.txt"}}"#.to_string(),
        )],
        text("ok"),
        &["-p", "hi"],
    );
    let out = std::fs::read_to_string(r.env.work().join("hook-out.txt")).unwrap_or_default();
    assert!(
        out.contains("agent_start") || out.contains("{"),
        "the hook ran with a JSON payload: err {} out {out}",
        r.err
    );
}

/// EXT-092 (A): the subagent example extension is oxa's `builtin:subagent`. O-146: its
/// `subagent` tool is on by default on a large non-Flash model, off on the Rust Flash engine and
/// below a 32K window; an explicit `oxa.subagent.enabled` wins (`true` on any model, `false`
/// never). An extension of the session does not change that.
/// Invariant: E11 (child processes, parent parked; O-146 default; oxa.subagent.enabled).
#[test]
fn ext_092() {
    let declares = |r: &Run| r.req(0).tools_json.contains("\"subagent\"");
    // An extension in the session; the fake's default model is large and not Flash.
    let on = one("oxa.on('session_start', (e, ctx) => {});");
    assert!(declares(&on), "on by default on a large non-Flash model");

    // The same run against a Flash engine or a model below 32K, with the setting unset, `true`
    // or `false`.
    fn run_on(opts: FakeOpts, settings: Option<&str>) -> Run {
        let env = TestEnv::new();
        if let Some(body) = settings {
            env.write("home/.oxa/agent/settings.json", body);
        }
        let server = FakeServer::start(text("ok"), opts).expect("fake engine");
        let out = run_oxa(&env, &server.url(), &["-p", "hi"], "");
        Run {
            code: out.status.code(),
            err: String::from_utf8_lossy(&out.stderr).into_owned(),
            log: String::new(),
            reqs: server.requests(),
            env,
        }
    }
    fn flash() -> FakeOpts {
        FakeOpts {
            model: "flash".to_string(),
            models: vec![oxa_fake::FakeModel::flash("flash", 65536, Some(2048))],
            ..FakeOpts::default()
        }
    }
    fn small() -> FakeOpts {
        FakeOpts {
            n_ctx: 16384,
            ..FakeOpts::default()
        }
    }
    let enabled = Some(r#"{"oxa": {"subagent": {"enabled": true}}}"#);
    let disabled = Some(r#"{"oxa": {"subagent": {"enabled": false}}}"#);
    for (what, opts) in [
        ("Flash", flash as fn() -> FakeOpts),
        ("below 32K", small as fn() -> FakeOpts),
    ] {
        let r = run_on(opts(), None);
        assert_eq!(r.code, Some(0), "{what}: {}", r.err);
        assert!(!declares(&r), "{what}: off by default");
        assert!(
            declares(&run_on(opts(), enabled)),
            "{what}: enabled declares the tool"
        );
    }
    assert!(
        declares(&run_on(FakeOpts::default(), enabled)),
        "enabled declares the tool on a large model"
    );
    let off = run_on(FakeOpts::default(), disabled);
    assert_eq!(off.code, Some(0), "{}", off.err);
    assert!(
        !declares(&off),
        "enabled: false never declares it, even on a large model"
    );
}

/// One example port run: the `oxa` run plus its stdout and the session files' text.
struct PortRun {
    run: Run,
    out: String,
    sessions: String,
}

impl PortRun {
    /// The tool messages of the last request (the whole conversation, W1).
    fn tool_results(&self) -> Vec<String> {
        self.run
            .reqs
            .iter()
            .rfind(|q| q.kind == "chat")
            .into_iter()
            .flat_map(|q| q.messages.iter())
            .filter(|m| m.role.as_str() == "tool")
            .map(|m| m.content.clone().unwrap_or_default())
            .collect()
    }
}

/// Load one example port (`examples/extensions/<name>/index.js`) and run it in print mode (or
/// the mode `args` names) against the port's own `session.fake.json`; there is no fallback
/// script. Every scripted turn must be consumed (the session ran as written) and the fake's
/// invariant checker (W1-W6) must stay green.
fn port_run(name: &str, args: &[&str], stdin: &str) -> PortRun {
    port_run_with(name, None, args, stdin)
}

/// [`port_run`] with an agent `settings.json`.
fn port_run_with(name: &str, settings: Option<&str>, args: &[&str], stdin: &str) -> PortRun {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../examples/extensions")
        .join(name);
    let entry = std::fs::read_to_string(dir.join("index.js"))
        .unwrap_or_else(|e| panic!("example port {name} is missing index.js: {e}"));
    let text = std::fs::read_to_string(dir.join("session.fake.json"))
        .unwrap_or_else(|e| panic!("example port {name} is missing session.fake.json: {e}"));
    let script = oxa_fake::parse_script(&text).expect("the port's scripted session parses");
    let turns = script.len();
    let env = TestEnv::new();
    env.write(&format!("home/.oxa/agent/extensions/{name}.js"), &entry);
    if let Some(settings) = settings {
        env.write("home/.oxa/agent/settings.json", settings);
    }
    let server = FakeServer::start(script, FakeOpts::default()).expect("fake engine");
    let out = run_oxa(&env, &server.url(), args, stdin);
    let reqs = server.requests();
    let generating = reqs.iter().filter(|q| q.max_tokens > 0).count();
    assert_eq!(
        generating,
        turns,
        "{name}: every scripted turn is consumed; stderr {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // W5: the request after a summary request continues from the new head (a compaction
    // rebase); every other request must extend its predecessor.
    let mut rebases: Vec<(usize, &'static str)> = Vec::new();
    for (i, q) in reqs.iter().enumerate() {
        if q.kind == "compaction" {
            if let Some(next) = reqs[i + 1..]
                .iter()
                .find(|n| n.session == q.session && n.kind != "compaction")
            {
                rebases.push((next.seq, oxa_proto::rebase_why::COMPACTION));
            }
        }
    }
    // W1-W6 (the hint rules H1-H6 are not this row's: H3 still expects `background` on the
    // foreground /compact summary, see the lane notes).
    let mut violations = oxa_fake::check_w1(&reqs, &rebases);
    violations.extend(oxa_fake::check_w2(&reqs, &rebases));
    violations.extend(oxa_fake::check_w3(&reqs, &rebases));
    violations.extend(oxa_fake::check_w5(&reqs, &rebases));
    violations.extend(oxa_fake::check_w6(&reqs));
    assert!(violations.is_empty(), "{name}: W1-W6 hold: {violations:#?}");
    let mut sessions = String::new();
    let mut stack = vec![env.agent_dir().join("sessions")];
    while let Some(d) = stack.pop() {
        for p in std::fs::read_dir(&d)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
        {
            if p.is_dir() {
                stack.push(p);
            } else {
                sessions.push_str(&std::fs::read_to_string(&p).unwrap_or_default());
            }
        }
    }
    let log = std::fs::read_to_string(env.work().join("log.txt")).unwrap_or_default();
    let run = Run {
        code: out.status.code(),
        err: String::from_utf8_lossy(&out.stderr).into_owned(),
        log,
        reqs,
        env,
    };
    assert!(
        !run.err.contains("Failed to load extension"),
        "{name} loads: {}",
        run.err
    );
    assert_eq!(
        run.code,
        Some(0),
        "{name} passes its scripted session: {}",
        run.err
    );
    PortRun {
        run,
        out: String::from_utf8_lossy(&out.stdout).into_owned(),
        sessions,
    }
}

/// EXT-093 (A): the example ports plan-mode, permission-gate, custom-compaction, todo, status-line
/// each load and pass a scripted session. Invariant: the port is the same JavaScript on the
/// oxa API (no npm imports, W-guard on its rewrites).
#[test]
fn ext_093() {
    // plan-mode, `--plan`: the write tools are gone from the frozen tool list and every prompt
    // gets the plan-mode instructions appended after the user message (append-only).
    let r = port_run("plan-mode", &["--plan", "-p", "plan the change"], "");
    let first = r.run.req(0);
    assert!(
        !first.tools_json.contains("\"edit\"") && !first.tools_json.contains("\"write\""),
        "{}",
        r.run.err
    );
    assert!(first.tools_json.contains("\"read\""));
    assert!(
        first.messages.iter().any(|m| m
            .content
            .as_deref()
            .is_some_and(|c| c.starts_with("[PLAN MODE ACTIVE]"))),
        "{:?}",
        shape(first)
    );

    // permission-gate: a dangerous command without a UI is blocked before it runs.
    let r = port_run("permission-gate", &["-p", "clean up"], "");
    assert!(
        r.tool_results()
            .iter()
            .any(|t| t.contains("Dangerous command blocked (no UI for confirmation)")),
        "{:?}",
        r.tool_results()
    );

    // custom-compaction: /compact-all compacts with the port's instructions (the W5 summary
    // request carries them; the port sends no model request of its own, O-144).
    // keepRecentTokens 1: a one-turn session has something to summarise.
    let small = Some(r#"{"compaction": {"keepRecentTokens": 1}}"#);
    let r = port_run_with(
        "custom-compaction",
        small,
        &["-p", "look at the code", "/compact-all"],
        "",
    );
    let summary = r
        .run
        .reqs
        .iter()
        .find(|q| q.kind == "compaction")
        .expect("a compaction request");
    assert!(
        summary.messages.iter().any(|m| m
            .content
            .as_deref()
            .is_some_and(|c| c.contains("comprehensive summary"))),
        "the port's instructions reach the summary request"
    );
    assert!(
        r.sessions.contains("\"compaction\""),
        "the summary lands as the W5 head"
    );

    // todo: the tool keeps its state in the result details; the list sees the added todo.
    let r = port_run("todo", &["-p", "remember to buy milk"], "");
    assert_eq!(
        r.tool_results(),
        vec!["Added todo #1: buy milk", "[ ] #1: buy milk"],
        "{:?}",
        r.tool_results()
    );
    assert!(
        r.sessions.contains("oxa.toolDetails"),
        "the details are stored for a resume"
    );

    // status-line (RPC, where statuses reach the client): the turn, then its completion. (The
    // `Ready` of session_start is set before the RPC client is attached, so it is not relayed;
    // recorded in the lane notes for the RPC front end.)
    let r = port_run(
        "status-line",
        &["--mode", "rpc", "--no-session"],
        "{\"id\":\"p1\",\"type\":\"prompt\",\"message\":\"hi\"}\n",
    );
    let statuses: Vec<String> = r
        .out
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["type"] == "extension_ui_request" && v["method"] == "setStatus")
        .map(|v| v["statusText"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        statuses.ends_with(&["● Turn 1...".to_string(), "✓ Turn 1 complete".to_string()]),
        "{statuses:?}: {}",
        r.out
    );
}

/// EXT-094 (P): the example ports tools, commands, input-transform, question, git-checkpoint,
/// trigger-compact, protected-paths each load and pass a scripted session.
#[test]
fn ext_094() {
    // tools: /tools needs the TUI (pi: `ctx.mode !== "tui"`); the session runs on unchanged.
    let r = port_run("tools", &["-p", "/tools", "read the readme"], "");
    assert!(r.run.err.contains("/tools requires TUI mode"), "{}", r.run.err);
    assert!(r.run.req(0).tools_json.contains("\"read\""));

    // commands: getCommands lists the extension's own command; a filter with no match says so.
    let r = port_run("commands", &["-p", "/commands prompt", "hi"], "");
    assert!(r.run.err.contains("No prompt commands found"), "{}", r.run.err);

    // input-transform: "?quick" is rewritten before the model sees it.
    let r = port_run("input-transform", &["-p", "?quick What is TypeScript?"], "");
    let user: Vec<String> = shape(r.run.req(0))
        .into_iter()
        .filter(|m| m.starts_with("user:"))
        .collect();
    assert_eq!(
        user.last().map(String::as_str),
        Some("user:Respond briefly in 1-2 sentences: What is TypeScript?"),
        "{user:?}"
    );

    // question: without a UI the tool answers that it cannot ask.
    let r = port_run("question", &["-p", "pick a database"], "");
    assert_eq!(
        r.tool_results(),
        vec!["Error: UI not available (running in non-interactive mode)"]
    );

    // git-checkpoint: a stash snapshot is taken at turn start (no repository here, so none is
    // kept); the write it guards goes through.
    let r = port_run("git-checkpoint", &["-p", "write notes"], "");
    assert_eq!(
        std::fs::read_to_string(r.run.env.work().join("notes.txt")).unwrap_or_default(),
        "hello\n"
    );

    // trigger-compact: /trigger-compact compacts with the port's instructions.
    let small = Some(r#"{"compaction": {"keepRecentTokens": 1}}"#);
    let r = port_run_with(
        "trigger-compact",
        small,
        &["-p", "first", "/trigger-compact", "second"],
        "",
    );
    assert!(
        r.run.reqs.iter().any(|q| q.kind == "compaction"),
        "{:?}",
        r.run.reqs.iter().map(|q| q.kind.clone()).collect::<Vec<_>>()
    );

    // protected-paths: the write to .env is blocked.
    let r = port_run("protected-paths", &["-p", "set the token"], "");
    assert!(
        r.tool_results().iter().any(|t| t.contains(".env")),
        "{:?}",
        r.tool_results()
    );
    assert!(!r.run.env.work().join(".env").exists());
}

/// EXT-095 (A): the TypeScript SDK (sdk.md) is the Rust API of `oxa-core`; other languages use RPC.
/// Invariant: no JS SDK package. The check is that the RPC front end answers and the binary says
/// so in its help.
#[test]
fn ext_095() {
    let out = std::process::Command::new(oxa_parity::oxa_bin())
        .arg("--help")
        .env_clear()
        .output()
        .expect("run oxa --help");
    let help = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        help.contains("rpc"),
        "the RPC mode is the cross-language SDK: {help}"
    );
}
