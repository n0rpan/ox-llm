//! H (lane cm) PARITY tests (codemode). One test per PARITY.md row of the CM area
//! (`CM-001` -> `fn cm_001()`), plus the four meta-checks every area file has.
//!
//! Every row test drives the real `oxa` binary in print mode against `oxa-fake`: the scripted
//! model reply calls the `codemode` tool with `{"code": "<script>"}` (verdict A of the tool input:
//! over `/v1/chat/completions` the arguments are JSON, pi takes raw JavaScript), and the test
//! reads the tool result from the next request the fake recorded. Codemode is turned on through
//! the user settings `{"codemode": {"enabled": true}}`; extension tools that scripts call are
//! written into the agent directory. Every spawn adds `oxa_parity::profile_env()`, so the file
//! runs under `OXA_PARITY_PROFILE=pi` and `=local`.
//!
//! pi's behaviour: `packages/coding-agent/docs/codemode.md` of pi 1.5.2 (quoted in the doc
//! comments as `pi:`). pi prints the wall time on the first result line; oxa omits it (W4: the
//! result of a deterministic script is deterministic), so the comparisons start at
//! `Script completed` / `Script failed` without it.
//!
//! Helpers: `oxa_parity::{oxa_bin, profile_env, fixture}`, `oxa_fake::{FakeServer, TestEnv,
//! ScriptTurn}`.

use oxa_fake::{FakeOpts, FakeServer, Recorded, ScriptTurn, TestEnv};
use oxa_parity::{fixture, oxa_bin, profile_env};
use serde_json::{json, Value};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// PARITY.md, the row list of this area.
const PARITY_MD: &str = include_str!("../../PARITY.md");
/// This file, read by the meta-checks.
const SELF_SRC: &str = include_str!("codemode.rs");

// ---------------------------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------------------------

/// What one `oxa` run left behind.
struct Run {
    out: Output,
    reqs: Vec<Recorded>,
    env: TestEnv,
    elapsed: Duration,
}

impl Run {
    fn err(&self) -> String {
        String::from_utf8_lossy(&self.out.stderr).into_owned()
    }
    fn ok(&self) {
        assert!(
            self.out.status.success(),
            "oxa failed: status {:?}\nstderr:\n{}",
            self.out.status.code(),
            self.err()
        );
    }
    fn req(&self, i: usize) -> &Recorded {
        self.reqs.get(i).unwrap_or_else(|| {
            panic!(
                "no request {i}; {} recorded\nstderr:\n{}",
                self.reqs.len(),
                self.err()
            )
        })
    }
    /// The tool result that answers the `n`-th tool call of the script (0-based): the last
    /// `tool` message of request `n + 1`.
    fn result(&self, n: usize) -> String {
        let r = self.req(n + 1);
        r.messages
            .iter()
            .rev()
            .find(|m| m.role.as_str() == "tool")
            .and_then(|m| m.content.clone())
            .unwrap_or_else(|| panic!("request {} carries no tool result", n + 1))
    }
}

/// The user `settings.json` that turns codemode on.
fn on() -> Value {
    json!({ "codemode": { "enabled": true } })
}

/// A model turn that calls the `codemode` tool with `code`.
fn codemode_call(id: &str, code: &str) -> ScriptTurn {
    ScriptTurn::tool(id, "codemode", &json!({ "code": code }).to_string())
}

/// A model turn that calls tool `name` with the JSON arguments `args`.
fn call(id: &str, name: &str, args: Value) -> ScriptTurn {
    ScriptTurn::tool(id, name, &args.to_string())
}

/// Run `oxa <args>` in `env.work()` with only `env.envs(url)`, `extra` and the PARITY profile as
/// environment (the pattern of `oxa_parity::run_oxa`, plus variables a row needs).
fn spawn(env: &TestEnv, url: &str, args: &[&str], extra: &[(&str, &str)]) -> Output {
    Command::new(oxa_bin())
        .args(args)
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs(url))
        .envs(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())))
        .envs(profile_env())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn oxa (build it first: cargo build -p oxa-cli -p oxa-fake)")
}

/// Write `settings` and `files` (paths relative to the test root), start the fake with `turns`
/// followed by a final text reply, run `oxa -p hi` with `extra` environment, collect the run.
fn session_in(
    env: TestEnv,
    settings: &Value,
    files: &[(&str, &str)],
    turns: Vec<ScriptTurn>,
    args: &[&str],
    extra: &[(&str, &str)],
) -> Run {
    env.write("home/.oxa/agent/settings.json", &settings.to_string());
    for (p, c) in files {
        env.write(p, c);
    }
    let mut script = turns;
    script.push(ScriptTurn::text("done"));
    let server = FakeServer::start(script, FakeOpts::default()).expect("fake engine");
    let mut all: Vec<&str> = args.to_vec();
    all.extend(["-p", "hi"]);
    let started = Instant::now();
    let out = spawn(&env, &server.url(), &all, extra);
    let elapsed = started.elapsed();
    Run {
        out,
        reqs: server.requests(),
        env,
        elapsed,
    }
}

fn session(settings: &Value, files: &[(&str, &str)], turns: Vec<ScriptTurn>, extra: &[(&str, &str)]) -> Run {
    session_in(TestEnv::new(), settings, files, turns, &[], extra)
}

/// One codemode call per script in `codes`, codemode on, no extra files.
fn scripts(codes: &[&str]) -> Run {
    scripts_with(&[], codes)
}

fn scripts_with(files: &[(&str, &str)], codes: &[&str]) -> Run {
    let turns = codes
        .iter()
        .enumerate()
        .map(|(i, c)| codemode_call(&format!("c{i}"), c))
        .collect();
    session(&on(), files, turns, &[])
}

/// An extension in the agent directory that registers `tools` (JavaScript `registerTool`
/// statements, `oxa` in scope).
fn extension(body: &str) -> String {
    format!("export default function (oxa) {{\n{body}\n}}\n")
}

/// A `registerTool` statement for a tool that answers `answer` (a JavaScript expression over
/// the parameters `p`).
fn tool_js(name: &str, description: &str, exposure: &str, answer: &str) -> String {
    format!(
        "oxa.registerTool({{name: {name:?}, description: {description:?}, exposure: {exposure:?}, \
         parameters: {{type: 'object', properties: {{q: {{type: 'string', description: 'the query'}}}}}}, \
         execute: async (id, p) => ({{content: [{{type: 'text', text: String({answer})}}]}})}});"
    )
}

/// The tool names a request declares, sorted.
fn declared(r: &Recorded) -> Vec<String> {
    let tools: Value = serde_json::from_str(&r.tools_json).unwrap_or(Value::Null);
    let mut names: Vec<String> = tools
        .as_array()
        .map(|a| {
            a.iter()
                .map(|t| t["function"]["name"].as_str().unwrap_or("?").to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// The description of declared tool `name` in a request.
fn description_of(r: &Recorded, name: &str) -> String {
    let tools: Value = serde_json::from_str(&r.tools_json).unwrap_or(Value::Null);
    tools
        .as_array()
        .and_then(|a| a.iter().find(|t| t["function"]["name"] == name))
        .and_then(|t| t["function"]["description"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| panic!("{name} is not declared: {:?}", declared(r)))
}

/// A user `mcp.json` with the fixture MCP server (`fixtures/pi/mcp/server.py`) as `fake`.
fn mcp_server_files(env: &TestEnv) {
    let src = std::fs::read_to_string(fixture("pi/mcp/server.py")).expect("fixture server");
    let script = env.write("mcp/server.py", &src);
    let rec = env.root().join("rec");
    std::fs::create_dir_all(&rec).expect("record dir");
    let cfg = json!({ "mcpServers": { "fake": {
        "command": "python3",
        "args": ["-I", script.to_string_lossy(), rec.to_string_lossy(), "plain"],
    } } });
    env.write("home/.oxa/agent/mcp.json", &cfg.to_string());
}

/// Every PARITY.md row id of the CM area, in table order.
fn cm_rows() -> Vec<String> {
    PARITY_MD
        .lines()
        .filter(|l| l.starts_with("| CM-"))
        .filter_map(|l| l.split('|').nth(1).map(|s| s.trim().to_string()))
        .collect()
}

/// The CM row ids with verdict `v`.
fn rows_with(v: &str) -> Vec<String> {
    PARITY_MD
        .lines()
        .filter(|l| l.starts_with("| CM-"))
        .filter(|l| l.split('|').nth(3).map(|c| c.trim()) == Some(v))
        .filter_map(|l| l.split('|').nth(1).map(|s| s.trim().to_string()))
        .collect()
}

/// The test functions of this file, by name (a `fn cm_NNN()` line).
fn test_fns() -> Vec<String> {
    SELF_SRC
        .lines()
        .filter_map(|l| l.strip_prefix("fn cm_"))
        .filter_map(|rest| rest.split('(').next().map(|n| format!("cm_{n}")))
        .collect()
}

fn fn_name(id: &str) -> String {
    id.to_ascii_lowercase().replace('-', "_")
}

/// The doc comment directly above `fn <name>()` (attributes skipped).
fn doc_of(name: &str) -> String {
    let lines: Vec<&str> = SELF_SRC.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.starts_with(&format!("fn {name}()")))
        .unwrap_or_else(|| panic!("no fn {name}"));
    let mut doc = Vec::new();
    let mut i = at;
    while i > 0 {
        i -= 1;
        let l = lines[i];
        if l.starts_with("#[") {
            continue;
        }
        if l.starts_with("///") {
            doc.push(l);
        } else {
            break;
        }
    }
    doc.reverse();
    doc.join("\n")
}

/// The body of `fn <name>()` up to its closing brace at column 0.
fn body_of(name: &str) -> String {
    let start = SELF_SRC
        .find(&format!("\nfn {name}()"))
        .unwrap_or_else(|| panic!("no fn {name}"));
    let end = SELF_SRC[start + 1..]
        .find("\n}\n")
        .map(|i| start + 1 + i + 3)
        .unwrap_or(SELF_SRC.len());
    SELF_SRC[start..end].to_string()
}

// ---------------------------------------------------------------------------------------------
// Meta-checks.
// ---------------------------------------------------------------------------------------------

/// each PARITY.md id of this area maps to a test fn or an O-row assertion
#[test]
fn every_row_has_test_or_recorded_omission() {
    let rows = cm_rows();
    assert_eq!(rows.len(), 17, "the CM area has 17 rows: {rows:?}");
    let fns = test_fns();
    let missing: Vec<String> = rows
        .iter()
        .map(|r| fn_name(r))
        .filter(|name| !fns.contains(name))
        .collect();
    assert!(missing.is_empty(), "rows without a test fn: {missing:?}");
    // The PARITY.md test column names this file for every row of the area.
    for row in &rows {
        let line = PARITY_MD
            .lines()
            .find(|l| l.starts_with(&format!("| {row} |")))
            .unwrap_or_else(|| panic!("no PARITY line for {row}"));
        let want = format!("codemode.rs::{}", fn_name(row));
        assert!(
            line.contains(&want),
            "PARITY.md names another test for {row}: {line}"
        );
    }
}

/// documented output or recorded fixture
#[test]
fn p_rows_compare_with_pi() {
    // Every P row has a row test whose doc comment quotes pi's codemode.md (`pi:`); there is no
    // recorded fixture for CM (the rows are script-API contracts from the documentation).
    let p_rows = rows_with("P");
    assert!(p_rows.len() >= 10, "P rows: {p_rows:?}");
    let fns = test_fns();
    for r in &p_rows {
        let name = fn_name(r);
        assert!(fns.contains(&name), "P row {r} has no compare test");
        assert!(
            doc_of(&name).contains("pi:"),
            "P row {r} must cite pi's codemode.md (`pi:`) in its doc comment"
        );
    }
    let fixtures = fixture("pi/codemode");
    if fixtures.exists() {
        assert!(fixtures.is_dir());
    }
}

/// comment + adapted behaviour asserted
#[test]
fn a_rows_name_their_invariant() {
    let a_rows = rows_with("A");
    assert!(a_rows.len() >= 2, "A rows: {a_rows:?}");
    for r in &a_rows {
        let lower = doc_of(&fn_name(r)).to_ascii_lowercase();
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
            "A row {r} names no invariant in its doc comment: {lower:?}"
        );
    }
}

/// unsupported_in_oxa
#[test]
fn o_rows_assert_unsupported_error() {
    let o_rows = rows_with("O");
    assert!(o_rows.len() >= 2, "O rows: {o_rows:?}");
    let fns = test_fns();
    for r in &o_rows {
        let name = fn_name(r);
        assert!(fns.contains(&name), "O row {r} has no assertion");
        assert!(
            body_of(&name).contains("unsupported_in_oxa"),
            "O row {r} must assert the unsupported_in_oxa error"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// CM-001 .. CM-017. One test per row.
// ---------------------------------------------------------------------------------------------

/// CM-001 (P): the `codemode` tool runs a JavaScript script in QuickJS.
/// pi: "It runs as the body of an async function in a QuickJS sandbox, so top-level `await` and
/// `return` work. The sandbox has no Node APIs, file system, network, or timers". "The result
/// starts with `Script completed` or `Script failed` ... and the output."
#[test]
fn cm_001() {
    let off = session(&json!({}), &[], vec![], &[]);
    off.ok();
    assert!(
        !declared(off.req(0)).contains(&"codemode".to_string()),
        "codemode is off by default: {:?}",
        declared(off.req(0))
    );

    let r = scripts(&[
        "const v = await Promise.resolve(41);\ntext(String(v + 1));\n\
         text([typeof process, typeof require, typeof fetch, typeof setTimeout].join(' '));",
        "throw new Error('broken on purpose');",
    ]);
    r.ok();
    assert!(
        declared(r.req(0)).contains(&"codemode".to_string()),
        "codemode.enabled declares the tool: {:?}",
        declared(r.req(0))
    );
    assert_eq!(
        r.result(0),
        "Script completed\n==> text 1/2 <==\n42\n==> text 2/2 <==\nundefined undefined undefined undefined",
        "top-level await works; no Node APIs, network or timers"
    );
    let failed = r.result(1);
    assert!(
        failed.starts_with("Script failed\n")
            && failed.contains("Script error:")
            && failed.contains("broken on purpose"),
        "a throwing script fails with its error: {failed}"
    );
}

/// CM-002 (P): the options line is honoured on line 1 only (`max_output_tokens`, `timeout_ms`).
/// pi: "A script may start with an options line. It must be the first line; an options line
/// anywhere else fails the call before the script runs". "`timeout_ms` is a hard deadline for
/// the whole script".
#[test]
fn cm_002() {
    let long: String = (0..400).map(|i| format!("w{i:03} ")).collect();
    let r = scripts(&[
        // Line 1: honoured.
        &format!("// @options: {{\"max_output_tokens\": 40}}\ntext({long:?});"),
        // Line 2: the call fails before the script runs (the write never happens).
        "await tools.write({ path: 'side-effect.txt', content: 'ran' });\n// @options: {\"max_output_tokens\": 40}\ntext('ran');",
        // Unknown key.
        "// @options: {\"max_tokens\": 40}\ntext('ran');",
        // timeout_ms ends a runaway script.
        "// @options: {\"timeout_ms\": 300}\nwhile (true) {}",
    ]);
    r.ok();
    let capped = r.result(0);
    assert!(
        capped.starts_with("Script completed\nw000 ")
            && capped.contains("output truncated")
            && capped.len() < long.len(),
        "max_output_tokens on line 1 caps the output: {capped}"
    );
    let late = r.result(1);
    assert!(
        late.contains("must be the first line") && !late.contains("Script completed"),
        "an options line on line 2 fails the call: {late}"
    );
    assert!(
        !r.env.work().join("side-effect.txt").exists(),
        "the call failed before the script ran (no tool call happened)"
    );
    let unknown = r.result(2);
    assert!(
        unknown.contains("@options only supports") && unknown.contains("max_tokens"),
        "an unknown option is refused: {unknown}"
    );
    let timed = r.result(3);
    assert!(
        timed.starts_with("Script failed"),
        "timeout_ms stops the script: {timed}"
    );
    assert!(
        r.elapsed < Duration::from_secs(30),
        "the deadline ended the runaway script ({:?})",
        r.elapsed
    );
}

/// CM-003 (A): `tools.<name>(args)` resolves to the tool's result as the model sees it.
/// pi: "`bash` resolves to `{ output, truncated, full_output_path?, exit_code, wall_time_seconds }`,
/// also for non-zero exit codes." "MCP tools resolve to their `CallToolResult`, including
/// `isError` and `structuredContent`." "`read` resolves to the file's text". "A call that fails,
/// is blocked, or gets invalid arguments rejects with an `Error` that carries the tool's error
/// text."
/// oxa (adapted): one deterministic `shape` for every tool result (W4), so `bash`, MCP tools
/// and `read` resolve to the shaped text the model would see. A failing call (`bash` with a
/// non-zero exit, a tool error) rejects with an `Error` whose message is that text. There are
/// no structured `bash` objects and no MCP `CallToolResult` objects in the script.
#[test]
fn cm_003() {
    // invariant: W4 (one deterministic `shape` for every tool result; a failure rejects with an
    // `Error` carrying that text).
    let env = TestEnv::new();
    mcp_server_files(&env);
    let r = session_in(
        env,
        &on(),
        &[("work/note.txt", "hello from the read tool\n")],
        vec![
            codemode_call(
                "c0",
                "const t = await tools.read({ path: 'note.txt' });\n\
                 text(typeof t + '|' + t.includes('hello from the read tool'));\n\
                 try { await tools.read({ path: 'missing.txt' }); text('resolved'); }\n\
                 catch (e) { text('rejected|' + (e instanceof Error) + '|' + e.message); }",
            ),
            codemode_call(
                "c1",
                "const b = await tools.bash({ command: 'echo bash-out' });\n\
                 text(typeof b + '|' + b.includes('bash-out'));\n\
                 try { await tools.bash({ command: 'echo bash-err; exit 3' }); text('resolved'); }\n\
                 catch (e) { text('rejected|' + (e instanceof Error) + '|' + e.message); }",
            ),
            codemode_call(
                "c2",
                "const m = await tools.mcp__fake__echo({ text: 'mcp-hi' });\n\
                 text(typeof m + '|' + m.includes('mcp-hi'));",
            ),
        ],
        &[],
        &[],
    );
    r.ok();
    let read = r.result(0);
    assert!(
        read.contains("string|true"),
        "read resolves to the file's text: {read}"
    );
    assert!(
        read.contains("rejected|true|") && !read.contains("resolved"),
        "a failing call rejects with an Error carrying the tool's text: {read}"
    );

    let bash = r.result(1);
    assert!(
        bash.contains("string|true"),
        "bash resolves to its output text: {bash}"
    );
    assert!(
        bash.contains("rejected|true|") && bash.contains("bash-err") && !bash.contains("resolved\n"),
        "bash with exit 3 rejects with an Error carrying the shaped text: {bash}"
    );

    let mcp = r.result(2);
    assert!(
        mcp.contains("string|true"),
        "an MCP tool resolves to its text: {mcp}"
    );
}

/// CM-004 (P): `text(value)`, top-level `return`, `exit()`.
/// pi: "`text(value)` Add a text item to the output. Strings are added as is, other values as
/// JSON." "A top-level `return` adds the value like `text()`." "`exit()` End the script
/// successfully." "When the output has more than one text item ... each starts with a
/// `==> text N/M <==` line."
#[test]
fn cm_004() {
    let r = scripts(&[
        "text('plain');\ntext({ k: [1, 2] });\nreturn 7;",
        "text('before');\nexit();\ntext('after');",
        "return 'only';",
    ]);
    r.ok();
    assert_eq!(
        r.result(0),
        "Script completed\n==> text 1/3 <==\nplain\n==> text 2/3 <==\n{\"k\":[1,2]}\n==> text 3/3 <==\n7"
    );
    assert_eq!(
        r.result(1),
        "Script completed\nbefore",
        "exit() ends the script successfully"
    );
    assert_eq!(r.result(2), "Script completed\nonly", "one item has no header");
}

/// CM-005 (P): `console.log/info/warn/error/debug` into `<console_output>`.
/// pi: "`console` calls follow in one `<console_output>` block with one line per call."
/// "Arguments are joined with spaces; `info`, `warn`, `error`, and `debug` do the same." "A
/// failed script keeps its partial output, followed by `Script error:` and the error."
#[test]
fn cm_005() {
    let r = scripts(&[
        "console.log('l', 1, true);\nconsole.info('i');\ntext('out');\nconsole.warn('w');\nconsole.error('e');\nconsole.debug('d');",
        "text('partial');\nconsole.log('seen');\nthrow new Error('late failure');",
    ]);
    r.ok();
    assert_eq!(
        r.result(0),
        "Script completed\nout\n<console_output>\nl 1 true\ni\nw\ne\nd\n</console_output>"
    );
    let failed = r.result(1);
    assert!(
        failed.starts_with("Script failed\npartial\nScript error:")
            && failed.contains("late failure")
            && failed.ends_with("<console_output>\nseen\n</console_output>"),
        "a failed script keeps its partial output and console: {failed}"
    );
}

/// CM-006 (P): `store(key, value)` / `load(key)`.
/// pi: "`store(key, value)` keeps a JSON value under a string key for later `codemode` calls;
/// storing `undefined` deletes the key. `load(key)` returns the value, or `undefined`. Writes
/// are kept only when the script succeeds: each successful script that stores values appends a
/// `codemode-store` custom entry to the session, so resumed sessions keep the values".
#[test]
fn cm_006() {
    let r = scripts(&[
        "store('k', { n: 1, s: 'v' });\nstore('gone', 1);\ntext(JSON.stringify(load('k')));",
        "store('x', 'lost');\nthrow new Error('fails after store');",
        "text(JSON.stringify(load('k')) + '|' + String(load('x')));\nstore('gone', undefined);",
        "text(String(load('gone')) + '|' + String(load('never')));",
    ]);
    r.ok();
    assert_eq!(
        r.result(0),
        "Script completed\n{\"n\":1,\"s\":\"v\"}",
        "a script sees its own writes"
    );
    assert!(r.result(1).starts_with("Script failed"), "{}", r.result(1));
    let later = r.result(2);
    assert!(
        later.starts_with("Script completed\n{\"n\":1,\"s\":\"v\"}|"),
        "a later call of the session loads the value: {later}"
    );
    let missing = r.result(3);

    // A resumed session (`-c`) keeps the values: the store is a `codemode-store` entry.
    let env = r.env;
    let resumed = session_in(
        env,
        &on(),
        &[],
        vec![codemode_call("c0", "text(JSON.stringify(load('k')));")],
        &["-c"],
        &[],
    );
    resumed.ok();
    assert_eq!(
        resumed.result(0),
        "Script completed\n{\"n\":1,\"s\":\"v\"}",
        "the resumed session keeps the stored values"
    );

    assert!(
        later.ends_with("|undefined") && missing == "Script completed\nundefined|undefined",
        "RED CM-006: `load(key)` of a missing, deleted or failed-script key returns `null`, pi \
         returns `undefined` (oxa-codemode/src/api.rs `load` returns `Value::Null`, which \
         oxa-script maps to JS `null`; there is no undefined in the bridge).\nafter failed store: \
         {later}\ndeleted / never stored: {missing}"
    );
}

/// CM-007 (P): `ALL_TOOLS`, `searchTools(query, {limit, namespace})` (BM25, default 8),
/// `describeTool`, `describeNamespace`.
/// pi: "`ALL_TOOLS` Every callable tool as `{ name, description }`". "`searchTools(query, {
/// limit?, namespace? })` Rank callable tools by relevance (BM25, default limit 8). Resolves to
/// `{ name, description }[]`." "`describeTool(name)` Resolves to a tool's description and
/// TypeScript declaration, or `undefined`." "`describeNamespace(name)` Resolves to `{ name,
/// description?, instructions?, tools }` for a namespace such as an MCP server, or `undefined`."
#[test]
fn cm_007() {
    // BM25 weighs a term by its rarity: `common` is in nine tools, `rare` in one. A plain
    // term-overlap count ties `common_tool` and `rare_tool` (each matches one query term in its
    // name and description) and breaks the tie by name; BM25 ranks `rare_tool` first.
    let mut tools = vec![
        tool_js("common_tool", "A common helper.", "codemode", "'c'"),
        tool_js("rare_tool", "A rare helper.", "codemode", "'r'"),
        tool_js(
            "weather_lookup",
            "Look up the weather forecast for a city.",
            "codemode",
            "'sunny in ' + p.q",
        ),
    ];
    for i in 0..8 {
        tools.push(tool_js(
            &format!("filler_{i}"),
            "A common filler tool.",
            "codemode",
            "'f'",
        ));
    }
    let ext = extension(&tools.join("\n"));
    let env = TestEnv::new();
    mcp_server_files(&env);
    let r = session_in(
        env,
        &on(),
        &[("home/.oxa/agent/extensions/catalog.js", &ext)],
        vec![
            codemode_call(
                "c0",
                "const names = ALL_TOOLS.map((t) => t.name);\n\
                 text(['weather_lookup', 'read', 'bash', 'mcp__fake__echo'].every((n) => names.includes(n)) + '|' + names.includes('codemode') + '|' + (typeof ALL_TOOLS[0].description));\n\
                 const w = await searchTools('weather forecast');\n\
                 text(w[0].name + '|' + Object.keys(w[0]).sort().join(','));\n\
                 text((await searchTools('common filler helper')).length + '|' + (await searchTools('common filler helper', { limit: 3 })).length);\n\
                 const ns = await searchTools('echo text', { namespace: 'fake' });\n\
                 text(ns.length > 0 && ns.every((h) => h.name.startsWith('mcp__fake__')));\n\
                 const d = await describeTool('weather_lookup');\n\
                 text(d.description + '|' + d.declaration);\n\
                 const n = await describeNamespace('fake');\n\
                 text(n.name + '|' + n.tools.includes('mcp__fake__echo'));",
            ),
            codemode_call(
                "c1",
                "text((await searchTools('common rare'))[0].name);",
            ),
            codemode_call(
                "c2",
                "text(String(await describeTool('no_such_tool')) + '|' + String(await describeNamespace('no_such_ns')));",
            ),
        ],
        &[],
        &[],
    );
    r.ok();
    let out = r.result(0);
    let lines: Vec<&str> = out.lines().filter(|l| !l.starts_with("==>")).collect();
    assert_eq!(lines[0], "Script completed", "{out}");
    assert_eq!(
        lines[1], "true|false|string",
        "ALL_TOOLS lists the callable tools, not codemode: {out}"
    );
    assert_eq!(
        lines[2], "weather_lookup|description,name",
        "searchTools ranks and returns {{name, description}}: {out}"
    );
    assert_eq!(lines[3], "8|3", "default limit 8, `limit` honoured: {out}");
    assert_eq!(
        lines[4], "true",
        "`namespace` restricts to the server's tools: {out}"
    );
    assert!(
        lines[5].starts_with("Look up the weather forecast for a city.|declare function weather_lookup(")
            && lines[5].contains("q?: string"),
        "describeTool gives the description and a TypeScript declaration: {out}"
    );
    assert_eq!(
        lines[6], "fake|true",
        "describeNamespace lists the server's tools: {out}"
    );

    let ranked = r.result(1);
    let unknown = r.result(2);
    assert_eq!(
        ranked, "Script completed\nrare_tool",
        "RED CM-007: searchTools does not rank with BM25: the rarer term must weigh more \
         (oxa-codemode/src/api.rs `search_specs`, a term-overlap count marked `TODO(F4): replace \
         with crate::rank`)"
    );
    assert_eq!(
        unknown, "Script completed\nundefined|undefined",
        "RED CM-007: describeTool / describeNamespace of an unknown name give `null`, pi gives \
         `undefined` (oxa-codemode/src/api.rs returns `Value::Null`, mapped to JS `null` by \
         oxa-script)"
    );
}

/// CM-008 (O): `image(value)`: unsupported_in_oxa (text-only engine).
/// pi: "`image(value)` Add an image to the output". oxa: the call throws an `Error` whose
/// `code` is `unsupported_in_oxa`; the script can catch it and goes on.
#[test]
fn cm_008() {
    let r = scripts(&[
        "try { image('data:image/png;base64,iVBORw0KGgo='); text('shown'); }\n\
         catch (e) { text(e.code + '|' + e.message); }\n\
         try { image({ type: 'image', data: 'AAAA', mimeType: 'image/png' }); } catch (e) { text(e.code); }",
        "image({ type: 'image', data: 'AAAA', mimeType: 'image/png' });",
    ]);
    r.ok();
    let caught = r.result(0);
    assert!(
        caught.contains("unsupported_in_oxa|image() is not supported in oxa")
            && caught.ends_with("\nunsupported_in_oxa")
            && !caught.contains("shown"),
        "image() throws unsupported_in_oxa: {caught}"
    );
    let uncaught = r.result(1);
    assert!(
        uncaught.starts_with("Script failed") && uncaught.contains("not supported in oxa"),
        "an uncaught image() fails the script with the unsupported_in_oxa error: {uncaught}"
    );
}

/// CM-009 (O): `models.*` (list, classify, generate images): unsupported_in_oxa (one local
/// engine, no classifier or image models).
/// pi: "`models` reaches the model catalog and runs non-LLM models". oxa: every method throws
/// `unsupported_in_oxa`.
#[test]
fn cm_009() {
    let r = scripts(&[
        "const out = [];\n\
         for (const [m, args] of [['getModelsOfType', ['image']], ['getAvailableOfType', ['classifier']], \
         ['getModelOfType', ['image', 'p', 'm']], ['classify', [{ provider: 'p', id: 'm' }, { state: {}, questions: {} }]], \
         ['generateImages', [{ provider: 'p', id: 'm' }, { input: [] }]]]) {\n\
           try { await models[m](...args); out.push(m + ':ok'); } catch (e) { out.push(m + ':' + e.code); }\n\
         }\n\
         text(out.join(','));",
    ]);
    r.ok();
    assert_eq!(
        r.result(0),
        "Script completed\ngetModelsOfType:unsupported_in_oxa,getAvailableOfType:unsupported_in_oxa,\
getModelOfType:unsupported_in_oxa,classify:unsupported_in_oxa,generateImages:unsupported_in_oxa"
    );
}

/// CM-010 (A): the output cap keeps the start and the end; the full text goes to a temp file.
/// Invariant W4 (`shape`): the result is deterministic for a deterministic script (the spill
/// file is named by the content hash, no wall time), so the same script gives byte-identical
/// results.
/// pi: "Longer output keeps its start and end, and the full text is written to a temp file
/// whose path is included in the result."
#[test]
fn cm_010() {
    let code = "// @options: {\"max_output_tokens\": 60}\n\
                for (let i = 0; i < 300; i++) text('line-' + String(i).padStart(3, '0'));";
    let r = scripts(&[code, code]);
    r.ok();
    let a = r.result(0);
    assert!(a.starts_with("Script completed\n"), "{a}");
    assert!(a.contains("line-000"), "the start is kept: {a}");
    assert!(a.contains("line-299"), "the end is kept: {a}");
    assert!(!a.contains("line-150"), "the middle is cut: {a}");
    let marker = a
        .lines()
        .find(|l| l.contains("output truncated"))
        .unwrap_or_else(|| panic!("a truncation marker: {a}"));
    let path = marker
        .split("the full output is in ")
        .nth(1)
        .unwrap_or_else(|| panic!("the marker names the file: {marker}"));
    let full = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("spill file {path}: {e}"));
    for i in [0, 150, 299] {
        assert!(
            full.contains(&format!("line-{i:03}")),
            "the file holds the full text"
        );
    }
    assert_eq!(a, r.result(1), "W4: the same script gives the same result bytes");
}

/// CM-011 (P): 256 MB VM memory limit; out-of-memory error.
/// pi: "A script's VM has 256 MB of memory. Running out throws `InternalError: out of memory`".
#[test]
fn cm_011() {
    let r = scripts(&[
        // 100 MiB fits.
        "const s = 'x'.repeat(100 * 1024 * 1024);\ntext(String(s.length));",
        // Unbounded growth runs out.
        "const keep = [];\nfor (let i = 0; i < 4096; i++) keep.push('y'.repeat(1024 * 1024) + i);\ntext('never');",
        // The session goes on with a fresh VM.
        "text('after');",
    ]);
    r.ok();
    assert_eq!(
        r.result(0),
        "Script completed\n104857600",
        "100 MiB is within the limit"
    );
    let oom = r.result(1);
    assert!(
        oom.starts_with("Script failed")
            && oom.to_ascii_lowercase().contains("out of memory")
            && !oom.lines().any(|l| l == "never"),
        "running out of memory fails the script: {oom}"
    );
    assert_eq!(r.result(2), "Script completed\nafter");
}

/// CM-012 (P): worker cap (default 2, env override `OXA_CODEMODE_MAX_WORKERS`); waiters queue.
/// pi: "At most two codemode workers may be alive across all sessions in one pi process;
/// `PI_CODEMODE_MAX_WORKERS` changes the limit. ... At capacity, new scripts wait for a free
/// worker in arrival order. A script that gets no worker within 60 seconds, or before its
/// `timeout_ms`, fails without running."
///
/// A second script runs while the first holds a worker: the outer script calls an extension
/// tool that starts an inner codemode script through `ctx.executeTool`. With the default cap the
/// inner script gets the second worker; with `OXA_CODEMODE_MAX_WORKERS=1` it waits in the queue
/// until its `timeout_ms` passes and fails without running.
#[test]
fn cm_012() {
    let ext = extension(
        "oxa.registerTool({name: 'nest', description: 'Run an inner codemode script.', exposure: 'codemode', \
         parameters: {type: 'object', properties: {code: {type: 'string'}}, required: ['code']}, \
         execute: async (id, p, signal, onUpdate, ctx) => { const r = await ctx.executeTool('codemode', {code: p.code}); \
         return {content: [{type: 'text', text: 'inner=' + r.content[0].text}]}; }});",
    );
    let files = [("home/.oxa/agent/extensions/nest.js", ext.as_str())];
    let outer =
        "text(await tools.nest({ code: '// @options: {\"timeout_ms\": 2000}\\ntext(\"inner ran\")' }));";
    let run = |extra: &[(&str, &str)]| {
        let started = Instant::now();
        let r = session(&on(), &files, vec![codemode_call("c0", outer)], extra);
        (r, started.elapsed())
    };

    let (two, _) = run(&[]);
    two.ok();
    let res = two.result(0);
    assert!(
        res.starts_with("Script completed") && res.contains("inner=Script completed\ninner ran"),
        "default cap 2: the inner script gets the second worker: {res}"
    );

    let (one, took) = run(&[("OXA_CODEMODE_MAX_WORKERS", "1")]);
    one.ok();
    let res = one.result(0);
    assert!(
        res.contains("inner=Script failed")
            && res.contains("timeout_ms passed before a codemode worker became free")
            && !res.contains("inner ran"),
        "OXA_CODEMODE_MAX_WORKERS=1: the inner script waits for the busy worker and fails without running: {res}"
    );
    assert!(
        took >= Duration::from_millis(1900),
        "the inner script queued for its timeout_ms before failing ({took:?})"
    );
}

/// CM-013 (P): a promise that can never settle fails immediately; no timers.
/// pi: "A script that waits on a promise that can never settle (no tool call pending) fails
/// immediately, since there are no timers."
#[test]
fn cm_013() {
    let r = scripts(&[
        "text(typeof setTimeout + ' ' + typeof setInterval + ' ' + typeof setImmediate);",
        "text('waiting');\nawait new Promise(() => {});\ntext('never');",
        // A pending tool call is not "never settles".
        "text(await tools.bash({ command: 'sleep 1; echo slept' }));",
    ]);
    r.ok();
    assert_eq!(r.result(0), "Script completed\nundefined undefined undefined");
    let stuck = r.result(1);
    assert!(
        stuck.starts_with("Script failed\nwaiting\nScript error:") && !stuck.lines().any(|l| l == "never"),
        "a promise that can never settle fails the script: {stuck}"
    );
    assert!(
        r.elapsed < Duration::from_secs(20),
        "it fails at once, not at a timeout ({:?})",
        r.elapsed
    );
    let slept = r.result(2);
    assert!(
        slept.starts_with("Script completed") && slept.contains("slept"),
        "a script waiting on a tool call is not failed: {slept}"
    );
}

/// CM-014 (P): scripts cannot start other codemode scripts.
/// pi: "Scripts cannot start other `codemode` scripts."
#[test]
fn cm_014() {
    let r = scripts(&[
        "text(typeof tools.codemode + '|' + ALL_TOOLS.some((t) => t.name === 'codemode') + '|' + String(Object.keys(tools).includes('codemode')));",
        "await tools.codemode({ code: 'text(1)' });",
    ]);
    r.ok();
    assert_eq!(r.result(0), "Script completed\nundefined|false|false");
    let nested = r.result(1);
    assert!(
        nested.starts_with("Script failed") && !nested.contains("Script completed"),
        "calling tools.codemode fails: {nested}"
    );
    assert_eq!(r.reqs.len(), 3, "no other engine request was made by the scripts");
}

/// CM-015 (P): `Promise.allSettled` over tool calls; engine calls are not involved.
/// pi: "A call that fails, is blocked, or gets invalid arguments rejects with an `Error` that
/// carries the tool's error text. Use `Promise.allSettled()` to keep the results of the calls
/// that succeed."
#[test]
fn cm_015() {
    let r = scripts_with(
        &[
            ("work/a.txt", "alpha-content\n"),
            ("work/b.txt", "beta-content\n"),
        ],
        &["const rs = await Promise.allSettled([\n\
             tools.read({ path: 'a.txt' }),\n\
             tools.read({ path: 'no-such-file.txt' }),\n\
             tools.read({ path: 'b.txt' }),\n\
           ]);\n\
           text(rs.map((x) => x.status).join(','));\n\
           text(rs[0].value.includes('alpha-content') + '|' + rs[2].value.includes('beta-content'));\n\
           text((rs[1].reason instanceof Error) + '|' + (rs[1].reason.message.length > 0));"],
    );
    r.ok();
    assert_eq!(
        r.result(0),
        "Script completed\n==> text 1/3 <==\nfulfilled,rejected,fulfilled\n==> text 2/3 <==\ntrue|true\n==> text 3/3 <==\ntrue|true"
    );
    assert_eq!(
        r.reqs.len(),
        2,
        "the script's tool calls make no engine request (one for the codemode call, one after it)"
    );
}

/// CM-016 (P): `codemode.mode` `on|only`; the `codemode` description is one fixed text.
/// pi: "With `on` (default) declared tools stay declared ... With `only` they are hidden from
/// the model ... so the model calls them through scripts." Architect decision for oxa: no
/// inline tool-declaration listing (`codemode.inlineBudget` is not tested): the description is
/// frozen per epoch (W3) and does not grow or change with the tool catalogue; tools are found
/// with `searchTools()` / `describeTool()`.
#[test]
fn cm_016() {
    let catalog: String = (1..=5)
        .map(|i| {
            tool_js(
                &format!("cat_tool_{i}"),
                &format!("Catalog tool {i}."),
                "codemode",
                "'ok'",
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let ext = extension(&catalog);
    let with_mode = |mode: &str, files: &[(&str, &str)]| {
        session(
            &json!({ "codemode": { "enabled": true, "mode": mode } }),
            files,
            vec![codemode_call(
                "c0",
                "text(await tools.read({ path: 'note.txt' }));",
            )],
            &[],
        )
    };
    let note = ("work/note.txt", "note-body\n");

    let on_run = with_mode("on", &[note]);
    on_run.ok();
    let names = declared(on_run.req(0));
    for t in ["bash", "codemode", "edit", "read", "write"] {
        assert!(
            names.contains(&t.to_string()),
            "mode on keeps {t} declared: {names:?}"
        );
    }

    let only = with_mode("only", &[note]);
    only.ok();
    assert_eq!(
        declared(only.req(0)),
        ["codemode"],
        "mode only hides the active built-in tools from the model"
    );
    assert!(
        only.result(0).contains("note-body"),
        "hidden tools stay callable from scripts: {}",
        only.result(0)
    );

    let empty = description_of(on_run.req(0), "codemode");
    let with_catalog = with_mode("on", &[note, ("home/.oxa/agent/extensions/catalog.js", &ext)]);
    with_catalog.ok();
    let listed = description_of(with_catalog.req(0), "codemode");
    assert_eq!(
        empty, listed,
        "the codemode description is byte-identical with zero and with five codemode tools (W3)"
    );
    assert!(
        !listed.contains("cat_tool_"),
        "no tool is listed inline: {listed}"
    );
    assert_eq!(
        empty,
        description_of(only.req(0), "codemode"),
        "mode only does not change the description either"
    );
    assert_eq!(
        on_run.req(0).tools_json,
        on_run.req(1).tools_json,
        "the declarations are frozen across the run"
    );
}

/// CM-017 (A): `tool_search` returns matches as text; matched tools are called through one
/// frozen `call_tool {name, args}`. Invariant W3: pi declares matched tools for the next call,
/// which would change the prefix; here the prefix (system prompt + tool declarations) stays
/// byte-identical across the search and the call.
#[test]
fn cm_017() {
    let ext = extension(&tool_js(
        "deep_thought",
        "Answer the ultimate question of life.",
        "deferred",
        "'answer=42 for ' + p.q",
    ));
    let r = session(
        &json!({ "codemode": { "toolSearch": true } }),
        &[("home/.oxa/agent/extensions/deep.js", &ext)],
        vec![
            call(
                "c0",
                "call_tool",
                json!({ "name": "deep_thought", "args": { "q": "early" } }),
            ),
            call("c1", "tool_search", json!({ "query": "ultimate question" })),
            call(
                "c2",
                "call_tool",
                json!({ "name": "deep_thought", "args": { "q": "life" } }),
            ),
        ],
        &[],
    );
    r.ok();
    let names = declared(r.req(0));
    assert!(
        names.contains(&"tool_search".to_string()) && names.contains(&"call_tool".to_string()),
        "tool_search and call_tool are declared: {names:?}"
    );
    assert!(
        !names.contains(&"deep_thought".to_string()),
        "a deferred tool is not declared: {names:?}"
    );

    let early = r.result(0);
    assert!(
        !early.contains("answer=42"),
        "call_tool refuses a tool no search matched: {early}"
    );
    let found = r.result(1);
    assert!(
        found.contains("name: deep_thought") && found.contains("call_tool"),
        "the match comes back as text with how to call it: {found}"
    );
    assert_eq!(
        r.result(2),
        "answer=42 for life",
        "the matched tool runs through call_tool"
    );

    for i in 1..r.reqs.len() {
        assert_eq!(
            r.req(i).system,
            r.req(0).system,
            "W3: system prompt unchanged at request {i}"
        );
        assert_eq!(
            r.req(i).tools_json,
            r.req(0).tools_json,
            "W3: tool declarations unchanged at request {i}"
        );
    }
}
