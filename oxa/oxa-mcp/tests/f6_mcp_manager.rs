//! F6 acceptance tests: MCP manager. Owner: task F6.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.
//!
//! The fake servers are small `sh` scripts (like E7's): they read one JSON-RPC message per line,
//! find the method and the numeric id with `sed`, and answer from `<method>.json` files (the id
//! placeholder `"@ID@"` replaced) or run `<method>.sh` hooks for scripted behaviour.

use oxa_fake::TestEnv;
use oxa_mcp::*;
use oxa_proto::{
    codes, image_placeholder, Access, CancelToken, ExecResult, ExecSpec, Exposure, Hooks, Mode, OxaError,
    Sandbox, Tool, ToolClass, ToolCtx, Value,
};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const SERVER_SCRIPT: &str = r#"d="$(dirname "$0")"
if [ -f "$d/startup.sh" ]; then . "$d/startup.sh"; fi
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9]*\),.*/\1/p')
  m=$(printf '%s\n' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":[0-9]*,"method":"\([^"]*\)".*/\1/p')
  if [ -z "$m" ]; then continue; fi
  f=$(printf '%s' "$m" | tr '/' '_')
  if [ -f "$d/$f.sh" ]; then . "$d/$f.sh"
  elif [ -f "$d/$f.json" ]; then sed "s/\"@ID@\"/$id/" "$d/$f.json"
  fi
done
"#;

/// One scripted stdio server in the test's directory tree.
struct Fake {
    dir: PathBuf,
    script: PathBuf,
}

impl Fake {
    fn new(env: &TestEnv, name: &str, tools: Value, resources: bool) -> Fake {
        let dir = env.root().join(format!("srv-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("server.sh");
        std::fs::write(&script, SERVER_SCRIPT).unwrap();
        let f = Fake { dir, script };
        let caps = if resources {
            json!({ "tools": {}, "resources": {} })
        } else {
            json!({ "tools": {} })
        };
        f.respond(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": caps,
                "serverInfo": { "name": name, "version": "1" },
                "instructions": format!("instructions of {name}"),
            }),
        );
        f.respond("tools_list", json!({ "tools": tools }));
        f.respond(
            "tools_call",
            json!({ "content": [{ "type": "text", "text": "ok" }] }),
        );
        f
    }
    /// Answer `file` (method with `/` -> `_`) with this result.
    fn respond(&self, file: &str, result: Value) {
        let msg = json!({ "jsonrpc": "2.0", "id": "@ID@", "result": result });
        std::fs::write(self.dir.join(format!("{file}.json")), format!("{msg}\n")).unwrap();
    }
    /// A shell hook sourced instead of the JSON answer (`$id`, `$line`, `$d` are set).
    fn hook(&self, file: &str, sh: &str) {
        std::fs::write(self.dir.join(file), sh).unwrap();
    }
    fn config(&self, name: &str, exposure: Exposure) -> ServerConfig {
        ServerConfig {
            name: name.to_string(),
            kind: ServerKind::Stdio(StdioConfig {
                command: "sh".into(),
                args: vec![self.script.to_string_lossy().into_owned()],
                env: Vec::new(),
                cwd: None,
            }),
            enabled: true,
            exposure,
            tool_exposure: Vec::new(),
            description: None,
            timeout_s: 10,
            defined_in: ConfigScope::User,
            project_override: false,
            trust_annotations: true,
        }
    }
}

fn tool(name: &str) -> Value {
    json!({ "name": name, "description": format!("tool {name}"), "inputSchema": { "type": "object", "properties": {} } })
}

fn allow_all() -> AccessGate {
    Arc::new(|_: &Access| true)
}

fn opts(env: &TestEnv, servers: Vec<ServerConfig>, gate: AccessGate) -> ManagerOpts {
    let mut servers = servers;
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    ManagerOpts {
        config: McpConfig {
            servers,
            auto_enable_codemode: None,
            diagnostics: Vec::new(),
        },
        cwd: env.work(),
        agent_dir: env.agent_dir(),
        home: env.home(),
        trusted: false,
        gate,
        client_version: "0.0.0-test".into(),
    }
}

fn state(m: &McpManager, name: &str) -> ServerState {
    m.server(name).map(|s| s.state).unwrap_or(ServerState::Disabled)
}

fn wait_until(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    cond()
}

fn wait_connected(m: &McpManager, name: &str) {
    assert!(
        wait_until(10, || state(m, name) == ServerState::Connected),
        "{name} did not connect: {:?}",
        m.server(name)
    );
}

/// A sandbox that is never asked to run anything.
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

fn with_ctx<R>(env: &TestEnv, f: impl FnOnce(&ToolCtx) -> R) -> R {
    let cwd = env.work();
    let spill = env.tmp();
    let cancel = CancelToken::new();
    let cx = ToolCtx {
        cwd: &cwd,
        spill_dir: &spill,
        sandbox: &NoSandbox,
        mode: Mode::Workspace,
        cancel: &cancel,
        env: &[],
        tune: Default::default(),
    };
    f(&cx)
}

fn find_tool(tools: &[Arc<dyn Tool>], name: &str) -> Arc<dyn Tool> {
    tools
        .iter()
        .find(|t| t.spec().name == name)
        .cloned()
        .unwrap_or_else(|| panic!("no tool {name}"))
}

/// startup does not block
#[test]
fn background_connect() {
    let env = TestEnv::new();
    let slow = Fake::new(&env, "slow", json!([tool("t")]), false);
    slow.hook("startup.sh", "sleep 1\n");
    let mut missing = slow.config("missing", Exposure::Codemode);
    missing.kind = ServerKind::Stdio(StdioConfig {
        command: env.root().join("no-such-server").to_string_lossy().into_owned(),
        ..Default::default()
    });
    let refused_fake = Fake::new(&env, "refused", json!([tool("t")]), false);
    let mut refused = refused_fake.config("refused", Exposure::Codemode);
    if let ServerKind::Stdio(s) = &mut refused.kind {
        s.args.push("deny-me".into());
    }
    // A gate that is slow to answer: it runs on the connection threads, never in start().
    let gate: AccessGate = Arc::new(|a: &Access| {
        thread::sleep(Duration::from_millis(200));
        !matches!(a, Access::Exec { command, .. } if command.ends_with("deny-me"))
    });
    let m = McpManager::new(opts(
        &env,
        vec![slow.config("slow", Exposure::Codemode), missing, refused],
        gate,
    ));
    assert_eq!(state(&m, "slow"), ServerState::Pending);
    let t0 = Instant::now();
    m.start();
    m.start(); // idempotent
    assert!(
        t0.elapsed() < Duration::from_millis(150),
        "start() blocked: {:?}",
        t0.elapsed()
    );
    assert!(matches!(
        state(&m, "slow"),
        ServerState::Pending | ServerState::Connecting
    ));
    assert!(
        m.tools().is_empty(),
        "nothing is offered before a server connected"
    );

    wait_connected(&m, "slow");
    assert!(wait_until(10, || matches!(
        state(&m, "missing"),
        ServerState::Failed(_)
    )));
    assert!(wait_until(10, || state(&m, "refused") == ServerState::Denied));

    // Failures are data, reported once.
    let report = m.take_report();
    assert_eq!(report.len(), 2, "{report:?}");
    assert!(report
        .iter()
        .any(|l| l.contains("missing") && l.contains("failed")));
    assert!(report
        .iter()
        .any(|l| l.contains("refused") && l.contains("permission denied")));
    assert!(m.take_report().is_empty());

    // `/mcp` order: servers needing attention first, then by name.
    let order: Vec<String> = m.servers().into_iter().map(|s| s.config.name).collect();
    assert_eq!(order, ["missing", "refused", "slow"]);
    let slow_status = m.server("slow").unwrap();
    assert_eq!(slow_status.instructions.as_deref(), Some("instructions of slow"));
    assert_eq!(slow_status.tools.len(), 1);
    assert_eq!(slow_status.saves_to, user_config_path(&env.agent_dir()));
    m.stop();
    m.stop(); // idempotent
}

/// bounded wait
///
/// The core calls `wait_for_direct(FIRST_PROMPT_WAIT_S)`; here the same wait runs with a short
/// timeout against a server that never answers.
#[test]
fn first_prompt_waits_10s() {
    assert_eq!(FIRST_PROMPT_WAIT_S, 10);
    let env = TestEnv::new();
    let silent = |name: &str, x: Exposure| ServerConfig {
        name: name.to_string(),
        kind: ServerKind::Stdio(StdioConfig {
            command: "sh".into(),
            args: vec!["-c".into(), "cat >/dev/null".into()],
            ..Default::default()
        }),
        enabled: true,
        exposure: x,
        tool_exposure: Vec::new(),
        description: None,
        timeout_s: 30,
        defined_in: ConfigScope::User,
        project_override: false,
        trust_annotations: true,
    };

    // A silent server with direct tools: the wait gives up at the timeout.
    let m = McpManager::new(opts(&env, vec![silent("hang", Exposure::Direct)], allow_all()));
    m.start();
    let t0 = Instant::now();
    assert!(!m.wait_for_direct(Duration::from_millis(300)));
    let took = t0.elapsed();
    assert!(
        took >= Duration::from_millis(300) && took < Duration::from_secs(3),
        "{took:?}"
    );
    m.stop();

    // Only servers with direct tools are waited for.
    let mut cm = silent("quiet", Exposure::Codemode);
    cm.tool_exposure = vec![("x*".into(), Exposure::Deferred)];
    let m = McpManager::new(opts(&env, vec![cm], allow_all()));
    m.start();
    let t0 = Instant::now();
    assert!(m.wait_for_direct(Duration::from_secs(5)));
    assert!(t0.elapsed() < Duration::from_secs(1));
    m.stop();

    // A direct server that answers ends the wait as soon as it is connected.
    let fake = Fake::new(&env, "quick", json!([tool("t")]), false);
    let m = McpManager::new(opts(
        &env,
        vec![fake.config("quick", Exposure::Direct)],
        allow_all(),
    ));
    m.start();
    assert!(m.wait_for_direct(Duration::from_secs(10)));
    assert_eq!(state(&m, "quick"), ServerState::Connected);
    m.stop();
}

/// codemode / deferred / direct / hidden
#[test]
fn exposure_modes() {
    let env = TestEnv::new();
    let fake = Fake::new(
        &env,
        "srv",
        json!([tool("a"), tool("b"), tool("c"), tool("d")]),
        true,
    );
    let mut cfg = fake.config("srv", Exposure::Codemode);
    cfg.tool_exposure = vec![
        ("a".into(), Exposure::Direct),
        ("b".into(), Exposure::Deferred),
        ("c".into(), Exposure::Hidden),
    ];
    let m = McpManager::new(opts(&env, vec![cfg.clone()], allow_all()));
    assert!(m.wants_codemode());
    assert!(m.wants_tool_search());
    m.start();
    wait_connected(&m, "srv");

    let tools = m.tools();
    let listed: Vec<(String, Exposure)> = tools
        .iter()
        .map(|t| (t.spec().name.clone(), t.spec().exposure))
        .collect();
    assert_eq!(
        listed,
        vec![
            ("list_mcp_resource_templates".to_string(), Exposure::Codemode),
            ("list_mcp_resources".to_string(), Exposure::Codemode),
            ("mcp__srv__a".to_string(), Exposure::Direct),
            ("mcp__srv__b".to_string(), Exposure::Deferred),
            ("mcp__srv__d".to_string(), Exposure::Codemode),
            ("read_mcp_resource".to_string(), Exposure::Codemode),
        ],
        "hidden tool c is not offered; sorted by name"
    );
    let a = find_tool(&tools, "mcp__srv__a");
    assert_eq!(a.spec().source, oxa_proto::Source::Mcp("srv".into()));
    assert_eq!(
        a.spec().schema_json,
        r#"{"properties":{},"type":"object"}"#,
        "canonical key order"
    );
    let catalog: Vec<String> = m.catalog().into_iter().map(|s| s.name).collect();
    assert_eq!(catalog, listed.iter().map(|l| l.0.clone()).collect::<Vec<_>>());

    // Per-tool status for /mcp.
    let st = m.server("srv").unwrap();
    let flags: Vec<(String, Exposure, bool)> = st
        .tools
        .iter()
        .map(|t| (t.name.clone(), t.exposure, t.overridden))
        .collect();
    assert_eq!(
        flags,
        vec![
            ("a".into(), Exposure::Direct, true),
            ("b".into(), Exposure::Deferred, true),
            ("c".into(), Exposure::Hidden, true),
            ("d".into(), Exposure::Codemode, false),
        ]
    );

    // Every non-hidden tool is callable by its full name; hidden is not found.
    let cancel = CancelToken::new();
    for name in ["mcp__srv__a", "mcp__srv__b", "mcp__srv__d"] {
        let r = m.call_tool(name, &json!({}), &cancel, &mut |_| {}).expect(name);
        assert_eq!(result_text("srv", &r), "ok");
    }
    let e = m
        .call_tool("mcp__srv__c", &json!({}), &cancel, &mut |_| {})
        .unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND);
    let e = m
        .call_tool("mcp__nope__x", &json!({}), &cancel, &mut |_| {})
        .unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND);

    // A toolExposure change applies at once to codemode / tool search and is saved.
    m.set_tool_exposure("srv", "d", Some(Exposure::Hidden)).unwrap();
    assert!(m.catalog().iter().all(|s| s.name != "mcp__srv__d"));
    m.set_exposure("srv", Exposure::Deferred).unwrap();
    let saved = std::fs::read_to_string(user_config_path(&env.agent_dir())).unwrap();
    let saved: Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(saved["mcpServers"]["srv"]["exposure"], "deferred");
    assert_eq!(saved["mcpServers"]["srv"]["toolExposure"]["d"], "hidden");
    assert_eq!(saved["mcpServers"]["srv"]["toolExposure"]["a"], "direct");
    m.set_tool_exposure("srv", "d", None).unwrap();
    assert_eq!(
        find_tool(&m.tools(), "mcp__srv__d").spec().exposure,
        Exposure::Deferred,
        "cleared entry falls back to the server exposure"
    );
    // A project file is not written for an untrusted project.
    let e = m.set_enabled("srv", false, ConfigScope::Project).unwrap_err();
    assert_eq!(e.code, codes::UNTRUSTED);
    // Disabling disconnects; the server stays listed.
    m.set_enabled("srv", false, ConfigScope::User).unwrap();
    assert_eq!(state(&m, "srv"), ServerState::Disabled);
    assert!(m.tools().is_empty());
    assert!(m.namespaces().is_empty());
    m.set_enabled("srv", true, ConfigScope::User).unwrap();
    wait_connected(&m, "srv");
    assert_eq!(
        m.namespaces(),
        vec![("srv".to_string(), None, Some("instructions of srv".to_string()))]
    );
    m.stop();

    // `autoEnableCodemode: false` turns the codemode wish off.
    let mut o = opts(&env, vec![cfg], allow_all());
    o.config.auto_enable_codemode = Some(false);
    let m = McpManager::new(o);
    assert!(!m.wants_codemode());
}

/// prompt section text
#[test]
fn mcp_servers_section() {
    let env = TestEnv::new();
    let fake = Fake::new(&env, "x", json!([]), false);
    let mut alpha = fake.config("alpha", Exposure::Codemode);
    alpha.description = Some("Alpha tools\nsecond line is not shown".into());
    let beta = fake.config("beta", Exposure::Deferred);
    let gamma = fake.config("gamma", Exposure::Direct);
    let mut delta = fake.config("delta", Exposure::Codemode);
    delta.enabled = false;
    let mut eps = fake.config("eps", Exposure::Hidden);
    eps.tool_exposure = vec![("one".into(), Exposure::Deferred)];
    let config = McpConfig {
        servers: vec![alpha, beta, delta, eps, gamma],
        auto_enable_codemode: None,
        diagnostics: Vec::new(),
    };
    let expected = "MCP servers whose tools are not declared to you. Call the tools of `codemode` servers from codemode scripts. Find the tools of `tool_search` servers with `tool_search` and call them with `call_tool`.\n- alpha (codemode): Alpha tools\n- beta (tool_search)\n- eps (tool_search)";
    assert_eq!(servers_section(&config), expected);

    // Only direct servers (or none): no section.
    let only_direct = McpConfig {
        servers: vec![fake.config("gamma", Exposure::Direct)],
        ..Default::default()
    };
    assert_eq!(servers_section(&only_direct), "");
    assert_eq!(servers_section(&McpConfig::default()), "");

    // Descriptions are cut to 250 characters.
    let mut long = fake.config("long", Exposure::Codemode);
    long.description = Some("y".repeat(400));
    let text = servers_section(&McpConfig {
        servers: vec![long],
        ..Default::default()
    });
    let line = text.lines().nth(1).unwrap();
    assert_eq!(
        line.strip_prefix("- long (codemode): ").unwrap().chars().count(),
        250
    );
    assert!(line.ends_with('…'));

    // The manager's section comes from configuration only: identical before and after the
    // server connected (W3).
    let mut srv = fake.config("srv", Exposure::Codemode);
    srv.description = Some("Fake server".into());
    let m = McpManager::new(opts(&env, vec![srv], allow_all()));
    let before = m.prompt_sections();
    assert_eq!(
        before,
        vec![(
            "mcp_servers".to_string(),
            "MCP servers whose tools are not declared to you. Call the tools of `codemode` servers from codemode scripts.\n- srv (codemode): Fake server".to_string()
        )]
    );
    m.start();
    wait_connected(&m, "srv");
    assert_eq!(m.prompt_sections(), before);
    m.stop();
}

/// size bound
#[test]
fn mcp_log_rotation() {
    assert_eq!(
        McpLog::format_line(0, "s", "info", "lg", "hi\nthere"),
        "1970-01-01T00:00:00Z [s] info lg: hi\\nthere"
    );
    assert_eq!(
        McpLog::format_line(1_700_000_000, "srv", "error", "db", "boom"),
        "2023-11-14T22:13:20Z [srv] error db: boom"
    );
    assert_eq!(
        McpLog::format_line(951_782_400, "s", "debug", "", "leap"),
        "2000-02-29T00:00:00Z [s] debug: leap"
    );

    let env = TestEnv::new();
    let path = env.agent_dir().join(LOG_FILE);
    let rotated = env.agent_dir().join(format!("{LOG_FILE}.1"));
    std::fs::write(&rotated, "older").unwrap();
    std::fs::write(&path, vec![b'x'; LOG_ROTATE_BYTES as usize + 1]).unwrap();
    let mut log = McpLog::open(&path);
    log.append(0, "s", "info", "lg", "first");
    assert_eq!(
        std::fs::metadata(&rotated).unwrap().len(),
        LOG_ROTATE_BYTES + 1,
        "replaced .1"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "1970-01-01T00:00:00Z [s] info lg: first\n"
    );
    log.append(1, "s", "info", "lg", "second");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap().lines().count(),
        2,
        "no rotation below the bound"
    );

    // End to end: a server's logging notification lands in mcp.log.
    std::fs::remove_file(&path).unwrap();
    let fake = Fake::new(&env, "srv", json!([tool("t")]), false);
    fake.hook(
        "tools_call.sh",
        r#"printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"warning","logger":"lg","data":"hello log"}}'
sed "s/\"@ID@\"/$id/" "$d/tools_call.json"
"#,
    );
    let m = McpManager::new(opts(
        &env,
        vec![fake.config("srv", Exposure::Codemode)],
        allow_all(),
    ));
    m.start();
    wait_connected(&m, "srv");
    m.call_tool("mcp__srv__t", &json!({}), &CancelToken::new(), &mut |_| {})
        .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.ends_with(" [srv] warning lg: hello log\n"), "{text}");
    m.stop();
}

/// direct tools offered only at the next epoch (W3)
///
/// The epoch itself belongs to the core (it reads `Hooks::tools()` once per epoch and never
/// re-reads mid-epoch); this tests the manager side: a late server's tools are absent from the
/// list read before it connected and present in the next read, and nothing about the prefix
/// section changes.
#[test]
fn late_server_next_epoch() {
    let env = TestEnv::new();
    let fake = Fake::new(&env, "late", json!([tool("t")]), false);
    fake.hook("startup.sh", "sleep 0.5\n");
    let m = McpManager::new(opts(
        &env,
        vec![fake.config("late", Exposure::Direct)],
        allow_all(),
    ));
    let sections = m.prompt_sections();
    m.start();
    let epoch1 = m.tools();
    assert!(epoch1.is_empty(), "late server not yet connected");
    wait_connected(&m, "late");
    let epoch2 = m.tools();
    assert_eq!(epoch2.len(), 1);
    assert_eq!(epoch2[0].spec().name, "mcp__late__t");
    assert_eq!(epoch2[0].spec().exposure, Exposure::Direct);
    assert!(
        epoch1.is_empty(),
        "the list read for epoch 1 is not changed afterwards"
    );
    assert_eq!(m.prompt_sections(), sections);
    assert!(
        sections.is_empty(),
        "direct servers are not in the mcp_servers section"
    );
    m.stop();
}

/// W4; image / audio / binary content blocks become the fixed placeholder line (O-144)
#[test]
fn tool_results_shaped() {
    let env = TestEnv::new();
    let fake = Fake::new(&env, "srv", json!([tool("mixed"), tool("broken")]), false);
    fake.respond(
        "call_mixed",
        json!({ "content": [
            { "type": "text", "text": "alpha" },
            { "type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png" },
            { "type": "audio", "data": "UklGRg==", "mimeType": "audio/wav" },
            { "type": "resource", "resource": { "uri": "file:///b.bin", "blob": "AAEC" } },
            { "type": "text", "text": "omega" },
        ] }),
    );
    fake.respond(
        "call_broken",
        json!({ "content": [{ "type": "text", "text": "it broke" }], "isError": true }),
    );
    fake.hook(
        "tools_call.sh",
        r#"case "$line" in *'"name":"broken"'*) r=broken;; *) r=mixed;; esac
sed "s/\"@ID@\"/$id/" "$d/call_$r.json"
"#,
    );
    let m = McpManager::new(opts(
        &env,
        vec![fake.config("srv", Exposure::Direct)],
        allow_all(),
    ));
    m.start();
    wait_connected(&m, "srv");
    let tools = m.tools();
    let mixed = find_tool(&tools, "mcp__srv__mixed");
    let placeholder = image_placeholder("image", 0, 0);
    let expected =
        format!("alpha\n{placeholder}\n{placeholder}\n[binary resource file:///b.bin omitted]\nomega");
    let out1 = with_ctx(&env, |cx| mixed.run(&json!({}), cx, &mut |_| {}));
    let out2 = with_ctx(&env, |cx| mixed.run(&json!({}), cx, &mut |_| {}));
    assert_eq!(out1.text, expected);
    assert!(
        !out1.text.contains("iVBORw0KGgo"),
        "no image bytes reach the wire"
    );
    assert!(!out1.is_error);
    assert_eq!(out1, out2, "deterministic");
    assert_eq!(out1.ui["content"].as_array().unwrap().len(), 5);
    assert_eq!(out1.ui["isError"], false);

    let broken = find_tool(&tools, "mcp__srv__broken");
    let out = with_ctx(&env, |cx| broken.run(&json!({}), cx, &mut |_| {}));
    assert!(out.is_error);
    assert_eq!(out.text, "it broke");

    // Not connected: the fixed text.
    m.stop();
    let out = with_ctx(&env, |cx| mixed.run(&json!({}), cx, &mut |_| {}));
    assert!(out.is_error);
    assert_eq!(out.text, "MCP server srv is not connected");
}

/// Access::Mcp class
///
/// The decision itself (workspace mode asks for write-class MCP tools) is the core `Policy`'s;
/// the manager's part is the access each tool reports.
#[test]
fn write_tools_ask_in_workspace() {
    let env = TestEnv::new();
    let tools = json!([
        { "name": "r", "annotations": { "readOnlyHint": true } },
        { "name": "w" },
        { "name": "o", "annotations": { "openWorldHint": true } },
        { "name": "d", "annotations": { "readOnlyHint": false, "destructiveHint": true } },
    ]);
    let fake = Fake::new(&env, "srv", tools, true);
    let m = McpManager::new(opts(
        &env,
        vec![fake.config("srv", Exposure::Codemode)],
        allow_all(),
    ));
    m.start();
    wait_connected(&m, "srv");
    let tools = m.tools();
    let class_of = |full: &str, own: &str| {
        let t = find_tool(&tools, full);
        let acc = with_ctx(&env, |cx| t.access(&json!({ "k": 1 }), cx)).unwrap();
        assert_eq!(acc.len(), 1);
        match &acc[0] {
            Access::Mcp { server, tool, class } => {
                assert_eq!(server, "srv");
                assert_eq!(tool, own);
                assert_eq!(*class, t.spec().class);
                *class
            }
            other => panic!("unexpected access {other:?}"),
        }
    };
    assert_eq!(class_of("mcp__srv__r", "r"), ToolClass::Read);
    assert_eq!(class_of("mcp__srv__w", "w"), ToolClass::Write);
    assert_eq!(
        class_of("mcp__srv__o", "o"),
        ToolClass::Write,
        "openWorldHint alone is not read"
    );
    assert_eq!(class_of("mcp__srv__d", "d"), ToolClass::Write);
    let w = find_tool(&tools, "mcp__srv__w");
    assert!(with_ctx(&env, |cx| w.access(&json!([1]), cx)).is_err());

    // The resource tools are read-class.
    let read = find_tool(&tools, "read_mcp_resource");
    let acc = with_ctx(&env, |cx| {
        read.access(&json!({ "server": "srv", "uri": "file:///x" }), cx)
    })
    .unwrap();
    assert_eq!(
        acc,
        vec![Access::Mcp {
            server: "srv".into(),
            tool: "read_mcp_resource".into(),
            class: ToolClass::Read
        }]
    );
    assert!(with_ctx(&env, |cx| read.access(&json!({ "server": "srv" }), cx)).is_err());
    m.stop();
}

/// nothing connects
///
/// `--no-mcp` is a CLI flag (the CLI then never calls `start()`); this tests the manager half:
/// without `start()`, and with every server disabled, no process starts and nothing is asked.
#[test]
fn no_mcp_flag() {
    let env = TestEnv::new();
    let fake = Fake::new(&env, "srv", json!([tool("t")]), false);
    fake.hook("startup.sh", "touch \"$d/started\"\n");
    let asked = Arc::new(AtomicUsize::new(0));
    let counter = asked.clone();
    let gate: AccessGate = Arc::new(move |_: &Access| {
        counter.fetch_add(1, Ordering::SeqCst);
        true
    });
    let marker = fake.dir.join("started");

    // Never started.
    let m = McpManager::new(opts(
        &env,
        vec![fake.config("srv", Exposure::Direct)],
        gate.clone(),
    ));
    let t0 = Instant::now();
    assert!(
        m.wait_for_direct(Duration::from_secs(10)),
        "nothing is connecting"
    );
    assert!(t0.elapsed() < Duration::from_secs(1));
    thread::sleep(Duration::from_millis(200));
    assert_eq!(state(&m, "srv"), ServerState::Pending);
    assert!(m.tools().is_empty());
    assert!(m.catalog().is_empty());
    drop(m);

    // Started, but every server disabled.
    let mut off = fake.config("srv", Exposure::Direct);
    off.enabled = false;
    let m = McpManager::new(opts(&env, vec![off], gate));
    m.start();
    thread::sleep(Duration::from_millis(200));
    assert_eq!(state(&m, "srv"), ServerState::Disabled);
    assert!(m.tools().is_empty());
    assert!(m.wait_for_direct(Duration::from_secs(10)));
    assert!(!m.wants_codemode());
    assert!(m.prompt_sections().is_empty());
    m.stop();

    assert_eq!(asked.load(Ordering::SeqCst), 0, "the gate was never asked");
    assert!(!marker.exists(), "no server process ran");
}

/// O-144: a server's sampling/createMessage gets JSON-RPC -32601; the manager never calls the engine (one slot)
///
/// The manager has no engine handle at all; the test proves the protocol half end to end: the
/// fake server sends `sampling/createMessage` while a tool call is running, reads the client's
/// answer and reports it as the tool result. It also records the `initialize` request to show
/// that no `sampling` capability is advertised.
#[test]
fn sampling_request_refused() {
    let env = TestEnv::new();
    let fake = Fake::new(&env, "srv", json!([tool("ask")]), false);
    fake.hook(
        "initialize.sh",
        r#"printf '%s\n' "$line" > "$d/init_line"
sed "s/\"@ID@\"/$id/" "$d/initialize.json"
"#,
    );
    fake.hook(
        "tools_call.sh",
        r#"printf '%s\n' '{"jsonrpc":"2.0","id":"s1","method":"sampling/createMessage","params":{"messages":[{"role":"user","content":{"type":"text","text":"hi"}}],"maxTokens":5}}'
IFS= read -r reply
rid=$(printf '%s\n' "$reply" | sed -n 's/^{"jsonrpc":"2.0","id":"\([^"]*\)".*/\1/p')
code=$(printf '%s\n' "$reply" | sed -n 's/.*"code":\(-\{0,1\}[0-9]*\).*/\1/p')
case "$reply" in *'"result"'*) res=yes;; *) res=no;; esac
printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"reply id=$rid code=$code result=$res\"}]}}"
"#,
    );
    let m = McpManager::new(opts(
        &env,
        vec![fake.config("srv", Exposure::Direct)],
        allow_all(),
    ));
    m.start();
    wait_connected(&m, "srv");
    let t = find_tool(&m.tools(), "mcp__srv__ask");
    let out = with_ctx(&env, |cx| t.run(&json!({}), cx, &mut |_| {}));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.text, "reply id=s1 code=-32601 result=no");

    let init: Value = serde_json::from_str(
        std::fs::read_to_string(fake.dir.join("init_line"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(
        init["params"]["capabilities"],
        json!({}),
        "no sampling capability"
    );
    m.stop();
}

/// The resource tools: list and read through the manager.
#[test]
fn resource_tools_list_and_read() {
    let env = TestEnv::new();
    let fake = Fake::new(&env, "srv", json!([]), true);
    fake.respond(
        "resources_list",
        json!({ "resources": [
            { "uri": "file:///a.txt", "name": "a", "mimeType": "text/plain" },
            { "uri": "ui://app", "name": "app" },
        ], "nextCursor": "c2" }),
    );
    fake.respond(
        "resources_read",
        json!({ "contents": [
            { "uri": "file:///a.txt", "mimeType": "text/plain", "text": "hello" },
            { "uri": "file:///a.bin", "mimeType": "application/octet-stream", "blob": "AAEC" },
            { "uri": "file:///a.png", "mimeType": "image/png", "blob": "iVBORw0KGgo=" },
        ] }),
    );
    let m = McpManager::new(opts(
        &env,
        vec![fake.config("srv", Exposure::Deferred)],
        allow_all(),
    ));
    m.start();
    wait_connected(&m, "srv");
    let tools = m.tools();
    let list = find_tool(&tools, "list_mcp_resources");
    assert_eq!(list.spec().exposure, Exposure::Deferred);
    let out = with_ctx(&env, |cx| list.run(&json!({ "server": "srv" }), cx, &mut |_| {}));
    assert!(!out.is_error, "{}", out.text);
    let v: Value = serde_json::from_str(&out.text).unwrap();
    assert_eq!(v["server"], "srv");
    assert_eq!(v["resources"].as_array().unwrap().len(), 1, "ui:// omitted");
    assert_eq!(v["resources"][0]["uri"], "file:///a.txt");
    assert_eq!(v["nextCursor"], "c2");

    let read = find_tool(&tools, "read_mcp_resource");
    let out = with_ctx(&env, |cx| {
        read.run(
            &json!({ "server": "srv", "uri": "file:///a.txt" }),
            cx,
            &mut |_| {},
        )
    });
    assert!(!out.is_error, "{}", out.text);
    let lines: Vec<&str> = out.text.lines().collect();
    assert_eq!(lines[0], "hello");
    assert!(
        lines[1].starts_with("[binary resource file:///a.bin (application/octet-stream, 3 bytes) saved to ")
    );
    let saved = lines[1].rsplit("saved to ").next().unwrap().trim_end_matches(']');
    assert_eq!(std::fs::read(Path::new(saved)).unwrap(), vec![0u8, 1, 2]);
    assert_eq!(lines[2], image_placeholder("image", 0, 0));

    let out = with_ctx(&env, |cx| {
        read.run(&json!({ "server": "nope", "uri": "x" }), cx, &mut |_| {})
    });
    assert!(out.is_error);
    m.stop();
}

/// Wave H lane `harden` (pre-flight A1/A2): a panic on a server's connection thread (injected
/// in the access gate, which runs there) fails that server with the panic named; the other
/// server connects and the manager goes on.
#[test]
fn a_panic_while_connecting_fails_that_server_only() {
    oxa_proto::panic::install_hook();
    let env = TestEnv::new();
    let good = Fake::new(&env, "good", json!([tool("t")]), false);
    let bad = Fake::new(&env, "bad", json!([tool("t")]), false);
    let mut bad_cfg = bad.config("bad", Exposure::Direct);
    if let ServerKind::Stdio(s) = &mut bad_cfg.kind {
        s.args.push("panic-me".into());
    }
    let gate: AccessGate = Arc::new(|a: &Access| {
        if matches!(a, Access::Exec { command, .. } if command.ends_with("panic-me")) {
            panic!("injected gate panic");
        }
        true
    });
    let m = McpManager::new(opts(
        &env,
        vec![good.config("good", Exposure::Direct), bad_cfg],
        gate,
    ));
    m.start();
    wait_connected(&m, "good");
    assert!(
        wait_until(
            10,
            || matches!(state(&m, "bad"), ServerState::Failed(ref why) if why.contains("injected gate panic"))
        ),
        "bad: {:?}",
        m.server("bad")
    );
    assert!(!m.tools().is_empty(), "the good server's tool is offered");
    m.stop();
}
