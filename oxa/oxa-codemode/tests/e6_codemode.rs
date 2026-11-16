//! E6 acceptance tests: codemode.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_codemode::api::{load_store, ts_declaration};
use oxa_codemode::{
    format_outcome, parse_options_line, CodemodeConfig, CodemodeHooks, CodemodeTool, Namespace,
    ScriptOptions, ScriptOutcome, Shared, CODEMODE_TOOL,
};
use oxa_fake::TestEnv;
use oxa_proto::{
    codes, CancelToken, ExecResult, ExecSpec, Exposure, Hooks, Host, HostAction, Mode, OxaError, Sandbox,
    Source, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Value,
};
use oxa_script::Limits;
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A sandbox that is never asked to run anything: codemode runs no commands itself.
struct NoSandbox;

impl Sandbox for NoSandbox {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        ExecResult {
            output: Vec::new(),
            exit: None,
            timed_out: false,
        }
    }
}

static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static MAX_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// The tools of the test session. `codemode` is in the catalog on purpose: scripts must not see it.
fn specs() -> Vec<ToolSpec> {
    let spec = |name: &str, description: &str, schema: &str, exposure: Exposure, source: Source| ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        schema_json: schema.to_string(),
        class: ToolClass::Read,
        exposure,
        source,
    };
    vec![
        spec(
            "read",
            "Read a file from the workspace",
            r#"{"type":"object","properties":{"path":{"type":"string","description":"File path"}},"required":["path"]}"#,
            Exposure::Direct,
            Source::Builtin,
        ),
        spec(
            "slow",
            "Wait a little and answer",
            r#"{"type":"object","properties":{}}"#,
            Exposure::Direct,
            Source::Builtin,
        ),
        spec(
            "bash",
            "Run a shell command",
            r#"{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}"#,
            Exposure::Direct,
            Source::Builtin,
        ),
        spec(
            "mcp__dev-radius__search",
            "Search the Radius documentation for pages",
            r#"{"type":"object","properties":{"query":{"type":"string","description":"Words to look for"}}}"#,
            Exposure::Codemode,
            Source::Mcp("dev-radius".to_string()),
        ),
        spec(
            CODEMODE_TOOL,
            "Run a script",
            r#"{"type":"object","properties":{"code":{"type":"string"}}}"#,
            Exposure::Direct,
            Source::Builtin,
        ),
    ]
}

/// What the mock tools answer: `(text, is_error)`.
fn dispatch(name: &str, args: &Value) -> (String, bool) {
    match name {
        "read" => match args.get("path").and_then(Value::as_str) {
            Some("ok") => ("file contents".to_string(), false),
            Some("a") => ("A".to_string(), false),
            Some("b") => ("B".to_string(), false),
            Some("panic") => panic!("injected tool panic"),
            Some(p) => (format!("no such file: {p}"), true),
            None => ("path is required".to_string(), true),
        },
        "slow" => {
            let now = IN_FLIGHT.fetch_add(1, Ordering::SeqCst) + 1;
            MAX_IN_FLIGHT.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(150));
            IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
            ("slow done".to_string(), false)
        }
        "bash" => ("Permission denied: rm is blocked in this mode".to_string(), true),
        "mcp__dev-radius__search" => ("1 page: radius-config".to_string(), false),
        other => (format!("unknown tool {other}"), true),
    }
}

/// A host that runs tools in place and records what it was asked for.
#[derive(Default)]
struct MockHost {
    calls: Mutex<Vec<String>>,
    appended: Mutex<Vec<(String, Value)>>,
    branch: Mutex<Vec<Value>>,
}

impl Host for MockHost {
    fn call(&self, action: HostAction) -> Result<Value, OxaError> {
        match action {
            HostAction::CallTool { name, args } => {
                self.calls.lock().unwrap().push(name.clone());
                let (text, is_error) = dispatch(&name, &args);
                Ok(json!({"text": text, "isError": is_error}))
            }
            HostAction::AppendEntry { custom_type, data } => {
                self.appended.lock().unwrap().push((custom_type, data));
                Ok(Value::Null)
            }
            HostAction::GetBranch => Ok(Value::Array(self.branch.lock().unwrap().clone())),
            _ => Err(OxaError::new(codes::NOT_FOUND, "not mocked")),
        }
    }
}

/// A codemode hook set over the test catalog, attached to `host`.
fn hooks_with(host: &Arc<MockHost>, max_workers: usize, spill: PathBuf) -> CodemodeHooks {
    let catalog: oxa_codemode::Catalog = Arc::new(specs);
    let namespaces: oxa_codemode::Namespaces = Arc::new(|| {
        vec![Namespace {
            name: "dev-radius".to_string(),
            description: Some("Radius developer docs".to_string()),
            instructions: None,
        }]
    });
    let config = CodemodeConfig {
        codemode: true,
        tool_search: false,
        max_workers,
    };
    let hooks = CodemodeHooks::new(config, catalog, namespaces, spill);
    let dyn_host: Arc<dyn Host> = host.clone();
    hooks.attach(dyn_host);
    hooks
}

/// Run one script on the hook set's pool and format its outcome as the model sees it.
fn run_script(hooks: &CodemodeHooks, source: &str) -> (ScriptOutcome, String) {
    let (opts, body) = parse_options_line(source).expect("options line");
    let outcome = hooks
        .shared()
        .pool
        .run(hooks.shared(), body, &opts, &CancelToken::new());
    let text = format_outcome(&outcome, &opts, &hooks.shared().spill_dir, &|s: &str| {
        s.chars().count().div_ceil(4) as u32
    });
    (outcome, text)
}

fn fixture(env: &TestEnv) -> (Arc<MockHost>, CodemodeHooks) {
    let host = Arc::new(MockHost::default());
    let hooks = hooks_with(&host, 2, env.tmp());
    (host, hooks)
}

/// Run the `codemode` tool itself with `code`.
fn run_tool(hooks: &CodemodeHooks, code: &str) -> ToolOutput {
    let shared: Arc<Shared> = hooks.shared().clone();
    let tool = CodemodeTool::new(shared);
    let env = TestEnv::new();
    let cwd = env.work();
    let spill = env.tmp();
    let sandbox = NoSandbox;
    let cancel = CancelToken::new();
    let envs: Vec<(String, String)> = Vec::new();
    let cx = ToolCtx {
        cwd: &cwd,
        spill_dir: &spill,
        sandbox: &sandbox,
        mode: Mode::Workspace,
        cancel: &cancel,
        env: &envs,
        tune: Default::default(),
        hidden: None,
    };
    tool.run(&json!({ "code": code }), &cx, &mut |_| {})
}

/// The examples of docs/codemode.md, run as scripts against the mock tools.
/// Each example script is checked for its documented result.
#[test]
fn codemode_md_examples() {
    let env = TestEnv::new();
    let (_host, hooks) = fixture(&env);

    // Call tools in parallel and combine their results.
    let (o, text) = run_script(
        &hooks,
        r#"
const [a, b] = await Promise.all([tools.read({ path: "a" }), tools.read({ path: "b" })]);
text(a + b);
"#,
    );
    assert!(o.ok, "{o:?}");
    assert_eq!(text, "Script completed\nAB");

    // A top-level return adds the value like text(); other values as JSON.
    let (o, text) = run_script(&hooks, "text(\"one\");\nreturn { n: 2 };\n");
    assert!(o.ok);
    assert_eq!(
        text,
        "Script completed\n==> text 1/2 <==\none\n==> text 2/2 <==\n{\"n\":2}"
    );

    // exit() ends the script successfully; later output is not produced.
    let (o, text) = run_script(&hooks, "text(\"x\");\nexit();\ntext(\"y\");\n");
    assert!(o.ok, "{o:?}");
    assert_eq!(text, "Script completed\nx");

    // Console lines come after the other output in one block.
    let (o, text) = run_script(
        &hooks,
        "console.log(\"a\", 1);\nconsole.warn(\"w\");\ntext(\"t\");\n",
    );
    assert!(o.ok);
    assert_eq!(
        text,
        "Script completed\nt\n<console_output>\na 1\nw\n</console_output>"
    );

    // A failed call rejects with the tool's text; Promise.allSettled keeps the rest.
    let (o, text) = run_script(
        &hooks,
        r#"
const r = await Promise.allSettled([tools.read({ path: "ok" }), tools.read({ path: "missing" })]);
text(r.map((x) => x.status).join(","));
try { await tools.read({ path: "missing" }); } catch (e) { text(e.message); }
"#,
    );
    assert!(o.ok, "{o:?}");
    assert_eq!(
        text,
        "Script completed\n==> text 1/2 <==\nfulfilled,rejected\n==> text 2/2 <==\nno such file: missing"
    );

    // A script that throws keeps its partial output and reports the error.
    let (o, text) = run_script(&hooks, "text(\"before\");\nthrow new Error(\"boom\");\n");
    assert!(!o.ok);
    assert!(
        text.starts_with("Script failed\nbefore\nScript error: Error: boom"),
        "{text}"
    );
}

/// parsing rule
#[test]
fn options_line_only_on_line_1() {
    let (opts, code) =
        parse_options_line("// @options: {\"max_output_tokens\": 2000, \"timeout_ms\": 60000}\ntext(1);\n")
            .expect("valid options line");
    assert_eq!(opts.max_output_tokens, 2000);
    assert_eq!(opts.timeout_ms, Some(60000));
    assert_eq!(
        code, "\ntext(1);\n",
        "the options line is removed, line numbers kept"
    );

    let (opts, code) = parse_options_line("text(1);").expect("no options");
    assert_eq!(opts, ScriptOptions::default());
    assert_eq!(code, "text(1);");

    let misplaced = parse_options_line("text(1);\n// @options: {\"timeout_ms\": 5}\n");
    assert!(misplaced.unwrap_err().contains("must be the first line"));

    assert!(parse_options_line("// @options: {\"unknown\": 1}\ntext(1);").is_err());
    assert!(parse_options_line("// @options: {bad json}\ntext(1);").is_err());
    assert!(parse_options_line("// @options: {\"timeout_ms\": 0}\ntext(1);").is_err());
    assert!(parse_options_line("// @options: {\"max_output_tokens\": -1}\ntext(1);").is_err());
    assert!(
        parse_options_line("// @options: {\"timeout_ms\": 5}").is_err(),
        "options need source"
    );
    assert!(parse_options_line("   \n  ").is_err(), "empty source");
}

/// text() over the cap
#[test]
fn output_cap_and_temp_file() {
    let env = TestEnv::new();
    let (_host, hooks) = fixture(&env);
    let source =
        "// @options: {\"max_output_tokens\": 100}\nfor (let i = 0; i < 2000; i++) text(\"line \" + i);\n";
    let (o, text) = run_script(&hooks, source);
    assert!(o.ok, "{o:?}");
    assert_eq!(o.texts.len(), 2000, "the outcome keeps every item");
    assert!(
        text.starts_with("Script completed\n==> text 1/2000 <==\nline 0\n"),
        "{}",
        &text[..80]
    );
    assert!(text.contains("output truncated"), "{text}");
    assert!(text.trim_end().ends_with("line 1999"), "keeps the end");
    assert!(
        text.len() < 2_000,
        "the result is capped, got {} bytes",
        text.len()
    );

    let spill = text
        .split("the full output is in ")
        .nth(1)
        .and_then(|rest| rest.split('\n').next())
        .expect("marker names the spill file");
    let full = std::fs::read_to_string(spill).expect("spill file exists");
    assert!(full.contains("line 1000") && full.ends_with("line 1999"));
    assert!(full.len() > text.len());

    // Within the cap nothing is spilled and the text is complete.
    let (_o, small) = run_script(&hooks, "text(\"short\");\n");
    assert_eq!(small, "Script completed\nshort");
}

/// concurrent scripts limited
#[test]
fn worker_cap() {
    let env = TestEnv::new();
    let host = Arc::new(MockHost::default());
    let hooks = hooks_with(&host, 2, env.tmp());
    assert_eq!(hooks.shared().pool.max_workers(), 2);
    IN_FLIGHT.store(0, Ordering::SeqCst);
    MAX_IN_FLIGHT.store(0, Ordering::SeqCst);

    let shared = hooks.shared().clone();
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let shared = shared.clone();
            std::thread::spawn(move || {
                let opts = ScriptOptions::default();
                shared.pool.run(
                    &shared,
                    "const r = await tools.slow({}); text(r);",
                    &opts,
                    &CancelToken::new(),
                )
            })
        })
        .collect();
    for h in handles {
        let o = h.join().expect("thread");
        assert!(o.ok, "{o:?}");
        assert_eq!(o.texts, vec!["slow done".to_string()]);
    }
    let peak = MAX_IN_FLIGHT.load(Ordering::SeqCst);
    assert!(peak <= 2, "at most two scripts run at once, saw {peak}");
    assert_eq!(hooks.shared().pool.alive(), 0, "every worker has exited");
}

/// Promise.allSettled over tools (parallel tool calls)
#[test]
fn promise_all_settled_over_tools() {
    let env = TestEnv::new();
    let (host, hooks) = fixture(&env);
    let (o, _text) = run_script(
        &hooks,
        r#"
const results = await Promise.allSettled([
  tools.read({ path: "a" }),
  tools.bash({ command: "rm -rf /" }),
  tools.read({ path: "b" }),
]);
text(results.map((r) => (r.status === "fulfilled" ? r.value : "error")).join("|"));
"#,
    );
    assert!(o.ok, "{o:?}");
    assert_eq!(o.texts, vec!["A|error|B".to_string()]);
    assert_eq!(host.calls.lock().unwrap().as_slice(), ["read", "bash", "read"]);
}

/// store()
#[test]
fn store_persists_within_session() {
    let env = TestEnv::new();
    let (host, hooks) = fixture(&env);

    // A successful script stores; the tool appends the snapshot as a codemode-store entry.
    let out = run_tool(&hooks, "store(\"cursor\", { page: 2 }); text(\"stored\");");
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("stored"));
    let appended = host.appended.lock().unwrap().clone();
    assert_eq!(appended.len(), 1);
    assert_eq!(appended[0].0, oxa_codemode::STORE_ENTRY_TYPE);
    assert_eq!(appended[0].1, json!({"cursor": {"page": 2}}));

    // A later script of the session reads it.
    let out = run_tool(&hooks, "text(load(\"cursor\").page);");
    assert_eq!(out.text, "Script completed\n2");

    // A script that fails keeps nothing.
    let out = run_tool(&hooks, "store(\"lost\", 1); throw new Error(\"no\");");
    assert!(out.is_error);
    assert_eq!(
        host.appended.lock().unwrap().len(),
        1,
        "no entry for a failed script"
    );
    let out = run_tool(&hooks, "text(String(load(\"lost\")));");
    assert_eq!(
        out.text, "Script completed\nundefined",
        "a missing key loads as undefined (pi's documented result)"
    );

    // Storing null deletes the key.
    run_tool(&hooks, "store(\"cursor\", null);");
    let out = run_tool(&hooks, "text(String(load(\"cursor\")));");
    assert_eq!(out.text, "Script completed\nundefined");

    // Attaching again reloads the store from the newest entry of the active branch.
    let entry = json!({"type": "custom", "customType": oxa_codemode::STORE_ENTRY_TYPE, "data": {"k": "v"}});
    host.branch.lock().unwrap().push(entry);
    let reloaded = load_store(host.as_ref());
    assert_eq!(reloaded.get("k"), Some(&json!("v")));
    let dyn_host: Arc<dyn Host> = host.clone();
    hooks.attach(dyn_host);
    let out = run_tool(&hooks, "text(load(\"k\"));");
    assert_eq!(out.text, "Script completed\nv");
}

/// searchTools / describeTool / ALL_TOOLS
#[test]
fn search_and_describe_tools() {
    let env = TestEnv::new();
    let (_host, hooks) = fixture(&env);

    let (o, _text) = run_script(
        &hooks,
        r#"
const hits = await searchTools("radius documentation");
text(hits.map((h) => h.name).join(","));
const read = await describeTool("read");
text(read.declaration);
text(String(await describeTool("nope")));
text(ALL_TOOLS.map((t) => t.name).join(","));
"#,
    );
    assert!(o.ok, "{o:?}");
    assert_eq!(o.texts[0], "mcp__dev-radius__search");
    assert_eq!(
        o.texts[1],
        "declare function read(args: { path: string }): Promise<string>;"
    );
    assert_eq!(
        o.texts[2], "undefined",
        "an unknown tool describes as undefined (pi's documented result)"
    );
    assert_eq!(o.texts[3], "bash,mcp__dev-radius__search,read,slow");
    let (raw, _) = run_script(&hooks, r#"text(typeof __cm_load + "|" + typeof load);"#);
    assert_eq!(
        raw.texts[0], "undefined|function",
        "the raw lookups are not script globals"
    );
    assert!(
        !o.texts[3].contains("codemode"),
        "codemode is not callable from scripts"
    );

    let (o, text) = run_script(
        &hooks,
        "const ns = await describeNamespace(\"dev-radius\"); text(JSON.stringify(ns));",
    );
    assert!(o.ok, "{o:?}");
    assert!(text.contains("\"tools\":[\"mcp__dev-radius__search\"]"), "{text}");

    // $ref properties are declared with their targets.
    let spec = ToolSpec {
        name: "mcp__x__t".to_string(),
        description: String::new(),
        schema_json: r##"{"type":"object","properties":{"opts":{"$ref":"#/$defs/Opts"},"n":{"type":"integer"}},"required":["opts"],"$defs":{"Opts":{"type":"object","properties":{"deep":{"type":"boolean"}},"required":["deep"]}}}"##.to_string(),
        class: ToolClass::Read,
        exposure: Exposure::Codemode,
        source: Source::Mcp("x".to_string()),
    };
    assert_eq!(
        ts_declaration(&spec),
        "declare function mcp__x__t(args: { opts: { deep: boolean }; n?: number }): Promise<string>;"
    );
}

/// unsupported_in_oxa
#[test]
fn image_and_models_unsupported() {
    let env = TestEnv::new();
    let (_host, hooks) = fixture(&env);
    let (o, _text) = run_script(
        &hooks,
        r#"
try { image({ type: "image", data: "AAAA", mimeType: "image/png" }); } catch (e) { text(e.code); }
try { models.getModelsOfType("image"); } catch (e) { text(e.code); }
try { await models.classify({ provider: "p", id: "m" }, { state: {}, questions: {} }); } catch (e) { text(e.code); }
"#,
    );
    assert!(o.ok, "{o:?}");
    assert_eq!(
        o.texts,
        vec![
            "unsupported_in_oxa".to_string(),
            "unsupported_in_oxa".to_string(),
            "unsupported_in_oxa".to_string()
        ]
    );
}

/// Deny surfaces as a rejected promise
#[test]
fn tool_calls_pass_policy() {
    let env = TestEnv::new();
    let (host, hooks) = fixture(&env);
    let (o, _text) = run_script(
        &hooks,
        r#"
try { await tools.bash({ command: "rm -rf build" }); text("ran"); }
catch (e) { text("rejected: " + e.message); }
"#,
    );
    assert!(o.ok, "{o:?}");
    assert_eq!(
        o.texts,
        vec!["rejected: Permission denied: rm is blocked in this mode".to_string()]
    );
    assert_eq!(
        host.calls.lock().unwrap().as_slice(),
        ["bash"],
        "the call went through the host"
    );
}

/// limit applied
#[test]
fn memory_cap_256mb_setting() {
    assert_eq!(Limits::CODEMODE.memory_bytes, 256 << 20);
    assert_eq!(Limits::CODEMODE.deadline_ms, None);

    let env = TestEnv::new();
    let (_host, hooks) = fixture(&env);
    let (o, _text) = run_script(
        &hooks,
        r#"
const chunk = "x".repeat(1 << 20);
const kept = [];
for (let i = 0; i < 1000; i++) kept.push(chunk + i);
text(kept.length);
"#,
    );
    assert!(!o.ok, "a script past 256 MB must fail");
    assert!(o.error.is_some());

    // The worker limit is the documented default of two, not exceeded by a single script.
    assert_eq!(hooks.shared().pool.max_workers(), 2);
}

/// timeout_ms is a hard deadline for the whole script; a script that never ends fails.
#[test]
fn timeout_stops_a_runaway_script() {
    let env = TestEnv::new();
    let (_host, hooks) = fixture(&env);
    let started = std::time::Instant::now();
    let (o, text) = run_script(&hooks, "// @options: {\"timeout_ms\": 200}\nfor (;;) {}\n");
    assert!(!o.ok, "{o:?}");
    assert!(text.starts_with("Script failed"), "{text}");
    assert!(started.elapsed() < Duration::from_secs(20));
}

/// Tool names become identifiers by the pi rule.
#[test]
fn tool_names_become_identifiers() {
    use oxa_codemode::api::js_identifier;
    assert_eq!(
        js_identifier("mcp__dev-radius__search"),
        "mcp__dev_radius__search"
    );
    assert_eq!(js_identifier("1st.tool"), "_1st_tool");
    assert_eq!(js_identifier("read"), "read");
    assert_eq!(js_identifier(""), "_");
}

/// Wave H lane `harden` (pre-flight A1/A2): a tool that panics under a script is an exception
/// in the script (a caught one lets the script finish; an uncaught one fails it with the panic
/// named) and the pool goes on with the next script. Passes in debug and release.
#[test]
fn a_panicking_tool_under_a_script_is_contained() {
    oxa_proto::panic::install_hook();
    let env = TestEnv::new();
    let (_host, hooks) = fixture(&env);
    let (o, text) = run_script(
        &hooks,
        "try { await tools.read({ path: \"panic\" }); } catch (e) { text(String(e.message)); }\n",
    );
    assert!(o.ok, "{o:?}");
    assert!(text.contains("injected tool panic"), "{text}");
    let (o, _) = run_script(&hooks, "await tools.read({ path: \"panic\" });\n");
    assert!(!o.ok);
    assert!(
        o.error
            .as_deref()
            .unwrap_or_default()
            .contains("injected tool panic"),
        "{o:?}"
    );
    let (o, text) = run_script(&hooks, "text(await tools.read({ path: \"a\" }));\n");
    assert!(o.ok, "{o:?}");
    assert_eq!(text, "Script completed\nA");
}
