//! E-INT (architect): everything of wave E reached through the real program path.
//!
//! Each test calls `oxa_cli::run` (what `main` calls) in print or JSON mode against `oxa-fake`
//! over HTTP, with real hook sets: JavaScript extensions in QuickJS, exec hooks in bwrap, a
//! stdio MCP server process and codemode. Every scenario runs under a hard time limit: a
//! session that does not serve its host (E-INT 1 / 2) fails fast instead of hanging.
//! Isolation: `oxa_fake::TestEnv` for every file and directory; never the real home.

#[path = "common/http.rs"]
mod http;
use oxa_cli::{run, Stdio};
use oxa_config::Env;
use oxa_fake::{FakeOpts, FakeServer, Recorded, ScriptTurn, TestEnv};
use oxa_proto::Role;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The longest any scenario may take. A missing pump would otherwise wait for a handler
/// deadline (30 s) or for ever.
const LIMIT: Duration = Duration::from_secs(60);

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Sink {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

struct Captured {
    code: i32,
    out: String,
    err: String,
}

/// Run the program in-process (stdin and stdout are not terminals, so the mode is print unless
/// the words say otherwise), on its own thread, within [`LIMIT`].
fn go(env: &TestEnv, engine_url: &str, words: &[&str]) -> Captured {
    // Hook transformations below assert exact synthetic prompt bytes. Preserve all hook
    // settings while selecting the supported note-off configuration for these fixtures.
    let settings_path = env.agent_dir().join("settings.json");
    let mut settings: serde_json::Value = std::fs::read_to_string(&settings_path)
        .map(|s| serde_json::from_str(&s).expect("fixture settings"))
        .unwrap_or_else(|_| serde_json::json!({}));
    settings["oxa"]["envNote"] = serde_json::json!(false);
    write(&settings_path, &serde_json::to_string(&settings).unwrap());
    let owned = http::envs(env, engine_url);
    let cwd = env.work();
    let argv: Vec<String> = words.iter().map(|s| s.to_string()).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let pairs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let env = Env::from_pairs(&pairs);
        let out = Sink::default();
        let err = Sink::default();
        let io = Stdio {
            stdin: Box::new(Cursor::new(Vec::new())),
            stdout: Box::new(out.clone()),
            stderr: Box::new(err.clone()),
            stdin_is_tty: false,
            stdout_is_tty: false,
            cwd,
        };
        let code = run(&argv, &env, io);
        let _ = tx.send(Captured {
            code,
            out: out.text(),
            err: err.text(),
        });
    });
    rx.recv_timeout(LIMIT)
        .unwrap_or_else(|_| panic!("oxa {words:?} did not finish in {LIMIT:?}: a host call was not served"))
}

fn fake(script: Vec<ScriptTurn>) -> FakeServer {
    FakeServer::start(script, FakeOpts::default()).expect("fake engine")
}

fn write(path: &Path, content: &str) -> PathBuf {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(path, content).expect("write test file");
    path.to_path_buf()
}

/// `true` when bwrap can build a sandbox here (the exec parts of the tests need it; without it
/// they check the refusal instead).
fn bwrap_usable(env: &TestEnv) -> bool {
    use oxa_proto::Sandbox as _;
    let path = std::env::var("PATH").unwrap_or_default();
    match oxa_sandbox::Bwrap::detect(&path, &env.home()) {
        Ok(b) => b.check().is_ok(),
        Err(_) => false,
    }
}

/// Text of the user and tool messages of a request, as `role:content`.
fn shape(r: &Recorded) -> Vec<String> {
    r.messages
        .iter()
        .map(|m| format!("{}:{}", m.role.as_str(), m.content.clone().unwrap_or_default()))
        .collect()
}

fn session_files(env: &TestEnv) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(&env.agent_dir().join("sessions"), &mut out);
    out
}

const TOOLS_EXTENSION: &str = r#"
export default function (oxa) {
  function sh(ctx, word) {
    try {
      var r = ctx.exec("echo", [word]);
      return r.exit === 0 ? r.output.trim() : "exit:" + r.exit;
    } catch (e) {
      return "ERR:" + e.code;
    }
  }
  oxa.registerTool({
    name: "greet",
    description: "Greets someone.",
    class: "read",
    parameters: { type: "object", properties: { name: { type: "string" } }, required: ["name"] },
    execute: function (toolCallId, params, signal, onUpdate, ctx) {
      ctx.ui.notify("greet tool ran", "info");
      var file = ctx.fs.readFile("note.txt").trim();
      var state = ctx.session.getState();
      return {
        content: [{
          type: "text",
          text: "hello " + params.name + " id=" + toolCallId + " file=" + file +
            " exec=" + sh(ctx, "from-tool") + " streaming=" + state.isStreaming
        }]
      };
    }
  });
  oxa.registerCommand("hello", {
    description: "Queue a note for the next prompt",
    handler: function (args, ctx) {
      ctx.ui.notify("hello command: " + args + " cwd-ok=" + (ctx.cwd.length > 0));
      oxa.sendMessage(
        { customType: "hello", content: "note from /hello " + args, display: false },
        { deliverAs: "nextTurn" }
      );
    }
  });
  oxa.on("tool_result", function (event, ctx) {
    ctx.ui.notify("handler saw " + event.toolName);
    return { content: event.content + " | handler=" + sh(ctx, "from-handler") };
  });
}
"#;

/// An extension that registers a tool, a command and a handler runs in a session. All three
/// get a real `ctx`: `ctx.ui` reaches the session's UI, `ctx.fs` / `ctx.exec` / `ctx.session`
/// are blocking host calls made while the session thread waits in `Hooks::fire`,
/// `Hooks::run_command` and `Tool::run`, and are served there (E-INT 1, 2, 5). The command's
/// `nextTurn` message is appended with the next prompt.
#[test]
fn extension_tool_command_and_handler_run_in_a_session() {
    let env = TestEnv::new();
    write(&env.work().join("note.txt"), "NOTE\n");
    let ext = write(&env.work().join("ext/tools.js"), TOOLS_EXTENSION);
    let server = fake(vec![
        ScriptTurn::tool("call_1", "greet", r#"{"name": "x"}"#),
        ScriptTurn::text("done"),
    ]);
    let ran = go(
        &env,
        &server.url(),
        &["-e", ext.to_str().unwrap(), "/hello there", "use the tool"],
    );
    assert_eq!(ran.code, 0, "stderr: {}", ran.err);
    assert_eq!(ran.out, "done\n");

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "one tool turn and one answer");
    // The tool is declared to the model (a direct tool of the frozen prefix).
    assert!(
        requests[0].tools_json.contains("\"greet\""),
        "{}",
        requests[0].tools_json
    );
    // The command ran first and queued its note; the prompt carried it.
    assert_eq!(
        shape(&requests[0]),
        ["user:note from /hello there", "user:use the tool"]
    );
    let exec = |word: &str| {
        if bwrap_usable(&env) {
            word.to_string()
        } else {
            "ERR:no_sandbox".to_string()
        }
    };
    let tool = requests[1].messages.last().expect("the tool result");
    assert_eq!(tool.role, Role::Tool);
    assert_eq!(
        tool.content.as_deref(),
        Some(
            format!(
                "hello x id=call_1 file=NOTE exec={} streaming=true | handler={}",
                exec("from-tool"),
                exec("from-handler")
            )
            .as_str()
        )
    );
    for line in [
        "hello command: there cwd-ok=true",
        "greet tool ran",
        "handler saw greet",
    ] {
        assert!(
            ran.err.contains(line),
            "ctx.ui.notify line {line:?} in stderr: {}",
            ran.err
        );
    }
}

/// `-ne` disables discovered extensions, `-e` still loads the named one; a long flag that an
/// extension registers is accepted, one that nobody registers is a usage error.
#[test]
fn extension_flags_and_no_extensions() {
    let env = TestEnv::new();
    // Discovered from the agent directory.
    write(
        &env.agent_dir().join("extensions/auto.js"),
        r#"export default function (oxa) { oxa.on("input", function (e) { return { action: "transform", text: e.text + " +auto" }; }); }"#,
    );
    let named = write(
        &env.work().join("named.js"),
        r#"export default function (oxa) {
             oxa.registerFlag("shout", { description: "Upper-case the prompt", type: "boolean", default: false });
             oxa.on("input", function (e) {
               return { action: "transform", text: (oxa.getFlag("shout") ? e.text.toUpperCase() : e.text) + " +named" };
             });
           }"#,
    );
    let named = named.to_str().unwrap();

    let server = fake(vec![
        ScriptTurn::text("a"),
        ScriptTurn::text("b"),
        ScriptTurn::text("c"),
    ]);
    let both = go(&env, &server.url(), &["-e", named, "hi"]);
    assert_eq!(both.code, 0, "stderr: {}", both.err);
    let only_named = go(&env, &server.url(), &["-ne", "-e", named, "--shout", "hi"]);
    assert_eq!(only_named.code, 0, "stderr: {}", only_named.err);
    let none = go(&env, &server.url(), &["-ne", "hi"]);
    assert_eq!(none.code, 0, "stderr: {}", none.err);

    let requests = server.requests();
    assert_eq!(
        shape(&requests[0]),
        ["user:hi +auto +named"],
        "load order: discovered, then -e"
    );
    assert_eq!(
        shape(&requests[1]),
        ["user:HI +named"],
        "-ne keeps -e; --shout is its flag"
    );
    assert_eq!(shape(&requests[2]), ["user:hi"], "-ne alone loads nothing");

    let unknown = go(&env, &server.url(), &["-e", named, "--no-such-flag", "hi"]);
    assert_eq!(unknown.code, 2, "stderr: {}", unknown.err);
    assert!(
        unknown.err.contains("Unknown option: --no-such-flag"),
        "{}",
        unknown.err
    );
}

fn rewriting_extension(declared: bool) -> String {
    let manifest = if declared {
        "export const manifest = { rebase: true };\n"
    } else {
        ""
    };
    format!(
        "{manifest}{}",
        r#"export default function (oxa) {
             oxa.on("context", function (event) {
               // The first request passes unchanged (tool calls and all); later ones are rewritten.
               if (event.messages.length < 3) { return { messages: event.messages }; }
               return { messages: [{ role: "user", content: "REWRITTEN" }] };
             });
           }"#
    )
}

/// A declared-rebase extension is accepted and logged (E-INT 3): the wire is replaced, the
/// cache miss that follows has the cause `extension`, a notice names the extension, the
/// session file has an `extension_rebase` entry and `extensions.log` has a line.
#[test]
fn declared_rebase_is_accepted_and_logged() {
    let env = TestEnv::new();
    let ext = write(&env.work().join("rewrite.js"), &rewriting_extension(true));
    let server = fake(vec![ScriptTurn::text("one"), ScriptTurn::text("two")]);
    let ran = go(
        &env,
        &server.url(),
        &["--mode", "json", "-e", ext.to_str().unwrap(), "first", "second"],
    );
    assert_eq!(ran.code, 0, "stderr: {}", ran.err);

    let requests = server.requests();
    assert_eq!(
        shape(&requests[0]),
        ["user:first"],
        "an unchanged list is not a rebase"
    );
    assert_eq!(
        shape(&requests[1]),
        ["user:REWRITTEN"],
        "the declared rebase is on the wire"
    );

    let miss = ran
        .out
        .lines()
        .find(|l| l.contains("cache_miss"))
        .unwrap_or_else(|| panic!("a cache_miss record: {}", ran.out));
    assert!(miss.contains("\"extension\""), "miss cause extension: {miss}");
    assert!(
        ran.out.contains("declared rebase") && ran.out.contains("rewrite.js"),
        "a notice names the extension: {}",
        ran.out
    );
    assert!(!ran.out.contains("guard_refused"), "{}", ran.out);

    let files = session_files(&env);
    assert_eq!(files.len(), 1);
    let stored = std::fs::read_to_string(&files[0]).unwrap();
    assert!(
        stored.contains("extension_rebase"),
        "the rebase is recorded in the session file"
    );
    let log = std::fs::read_to_string(env.agent_dir().join(oxa_cli::EXTENSIONS_LOG)).expect("extensions.log");
    assert!(
        log.contains("declared rebase") && log.contains("cause: extension"),
        "{log}"
    );
}

/// The same handler without the declaration is refused (W1): the request goes out unchanged,
/// the refusal is reported, nothing is recorded as a rebase.
#[test]
fn undeclared_rebase_is_refused() {
    let env = TestEnv::new();
    let ext = write(&env.work().join("rewrite.js"), &rewriting_extension(false));
    let server = fake(vec![ScriptTurn::text("one"), ScriptTurn::text("two")]);
    let ran = go(
        &env,
        &server.url(),
        &["--mode", "json", "-e", ext.to_str().unwrap(), "first", "second"],
    );
    assert_eq!(ran.code, 0, "stderr: {}", ran.err);

    let requests = server.requests();
    assert_eq!(
        shape(&requests[1]),
        ["user:first", "assistant:one", "user:second"],
        "the wire is untouched"
    );
    assert!(!ran.out.contains("cache_miss"), "no miss: W1 held: {}", ran.out);
    // JSON mode reports a failed handler as pi's `extension_error` record (JSON-025).
    let refusal = ran
        .out
        .lines()
        .find(|l| l.contains("\"type\":\"extension_error\"") && l.contains("guard_refused"));
    assert!(
        refusal.is_some(),
        "the refusal is reported as an extension_error record: out {} err {}",
        ran.out,
        ran.err
    );
    let stored = std::fs::read_to_string(&session_files(&env)[0]).unwrap();
    assert!(!stored.contains("extension_rebase"));
    let log = std::fs::read_to_string(env.agent_dir().join(oxa_cli::EXTENSIONS_LOG)).expect("extensions.log");
    assert!(log.contains("guard_refused"), "{log}");
}

/// A `context` handler that returns the list it was given, on a wire that holds tool calls,
/// tool results and reasoning, is not a rewrite: the payload carries every wire field (E2's
/// checkpoint problem 3), so the reply is the wire itself and nothing is refused.
#[test]
fn unchanged_context_reply_with_tool_calls_is_not_refused() {
    let env = TestEnv::new();
    write(&env.work().join("note.txt"), "NOTE\n");
    let ext = write(
        &env.work().join("echo.js"),
        r#"export default function (oxa) {
             oxa.on("context", function (event) { return { messages: event.messages }; });
           }"#,
    );
    let mut with_reasoning = ScriptTurn::tool("call_1", "read", r#"{"path":   "note.txt"}"#);
    with_reasoning.reasoning = Some("look first".to_string());
    let server = fake(vec![with_reasoning, ScriptTurn::text("done")]);
    let ran = go(&env, &server.url(), &["-e", ext.to_str().unwrap(), "read it"]);
    assert_eq!(ran.code, 0, "stderr: {}", ran.err);
    assert!(!ran.err.contains("guard_refused"), "{}", ran.err);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let assistant = &requests[1].messages[1];
    assert_eq!(assistant.role, Role::Assistant);
    assert_eq!(
        assistant.tool_calls[0].args_raw, r#"{"path":   "note.txt"}"#,
        "the argument bytes are the model's (W2)"
    );
    // W1 held across the hook: the second request extends the first one's cache.
    let usage = requests[1].usage.expect("usage");
    assert!(usage.cached > 0, "the prefix was reused: {usage:?}");
}

/// Exec hooks fire in a session (E-INT 4): a `tool_call` hook from the `hooks` setting gets the
/// event JSON on stdin and blocks the call with exit code 2; its stdout is the reason.
#[test]
fn exec_hooks_fire() {
    let env = TestEnv::new();
    if !bwrap_usable(&env) {
        eprintln!("skipped: bwrap is not usable on this machine (exec hooks run sandboxed)");
        return;
    }
    write(&env.work().join("note.txt"), "NOTE\n");
    write(
        &env.agent_dir().join("settings.json"),
        r#"{"hooks": {"tool_call": "grep -q '\"toolName\":\"read\"' && { echo 'read is blocked by the hook'; exit 2; }"}}"#,
    );
    let server = fake(vec![
        ScriptTurn::tool("call_1", "read", r#"{"path": "note.txt"}"#),
        ScriptTurn::text("done"),
    ]);
    let ran = go(&env, &server.url(), &["read it"]);
    assert_eq!(ran.code, 0, "stderr: {}", ran.err);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let tool = requests[1].messages.last().expect("the tool result");
    assert_eq!(tool.role, Role::Tool);
    assert_eq!(tool.content.as_deref(), Some("read is blocked by the hook"));
}

/// O-144: an extension cannot call the model. `ctx.fetch` to the session's engine endpoint is
/// denied before the policy is asked.
#[test]
fn fetch_to_the_engine_is_denied() {
    let env = TestEnv::new();
    let server = fake(vec![ScriptTurn::text("done")]);
    let ext = write(
        &env.work().join("fetch.js"),
        &r#"export default function (oxa) {
             oxa.on("input", function (event, ctx) {
               var got;
               try { ctx.fetch("ENGINE/v1/chat/completions", { method: "POST", body: "{}" }); got = "fetched"; }
               catch (e) { got = e.code + ": " + e.message; }
               return { action: "transform", text: event.text + " [" + got + "]" };
             });
           }"#
        .replace("ENGINE", &server.url()),
    );
    let ran = go(&env, &server.url(), &["-e", ext.to_str().unwrap(), "hi"]);
    assert_eq!(ran.code, 0, "stderr: {}", ran.err);
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "the engine saw the session's own request only");
    let text = requests[0].messages[0].content.clone().unwrap_or_default();
    assert!(
        text.starts_with("hi [denied: fetch to the engine endpoint is not allowed"),
        "{text}"
    );
}

/// A stdio MCP server in Python: `echo` (read-only) answers with its argument and, to prove
/// O-144, first asks the client for `sampling/createMessage` and reports what came back.
const MCP_SERVER: &str = r#"
import json, sys

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    method, mid = msg.get("method"), msg.get("id")
    if method == "initialize":
        caps = msg["params"].get("capabilities", {})
        with open(sys.argv[1], "w") as f:
            json.dump(caps, f)
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": msg["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fake", "version": "1"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [{
            "name": "echo", "description": "Echo the text.",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]},
            "annotations": {"readOnlyHint": True}}]}})
    elif method == "tools/call":
        send({"jsonrpc": "2.0", "id": "s1", "method": "sampling/createMessage",
              "params": {"messages": [], "maxTokens": 1}})
        reply = json.loads(sys.stdin.readline())
        code = reply.get("error", {}).get("code")
        text = msg["params"]["arguments"].get("text", "")
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "content": [{"type": "text", "text": "echo:" + text + " sampling:" + str(code)}]}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
"#;

/// codemode calls an MCP tool through `HostAction::CallTool` (E-INT 2, 4): the script runs on a
/// codemode worker, its `tools.<name>()` call blocks on the host, and the session thread, which
/// waits in `Tool::run` of `codemode`, serves it by running the MCP tool of a real stdio server
/// process. O-144: the client advertised no `sampling` capability and the server's
/// `sampling/createMessage` was refused with -32601; the engine saw only the session's requests.
#[test]
fn codemode_calls_an_mcp_tool_through_the_host() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipped: python3 is needed for the fake MCP server");
        return;
    }
    let env = TestEnv::new();
    let script = write(&env.root().join("mcp/server.py"), MCP_SERVER);
    let caps_file = env.root().join("mcp/client-capabilities.json");
    write(
        &env.agent_dir().join("mcp.json"),
        &serde_json::json!({
            "mcpServers": {
                "fake": {
                    "command": "python3",
                    "args": [script.to_str().unwrap(), caps_file.to_str().unwrap()],
                    "description": "A fake server."
                }
            }
        })
        .to_string(),
    );
    let code = r#"const r = await tools.mcp__fake__echo({ text: "hi" }); text("got " + r);"#;
    let server = fake(vec![
        ScriptTurn::tool(
            "call_1",
            "codemode",
            &serde_json::json!({ "code": code }).to_string(),
        ),
        ScriptTurn::text("done"),
    ]);
    let ran = go(&env, &server.url(), &["run the script"]);
    assert_eq!(ran.code, 0, "stderr: {}", ran.err);
    assert_eq!(ran.out, "done\n");

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        2,
        "the engine saw the session's own two requests only"
    );
    // The default exposure of an MCP server is codemode: its tool is not declared, codemode is
    // (enabled by the configuration), and the prompt names the server.
    assert!(
        requests[0].tools_json.contains("\"codemode\""),
        "{}",
        requests[0].tools_json
    );
    assert!(
        !requests[0].tools_json.contains("mcp__fake__echo"),
        "{}",
        requests[0].tools_json
    );
    assert!(
        requests[0].system.contains("fake"),
        "the mcp_servers section names the server"
    );
    let tool = requests[1].messages.last().expect("the tool result");
    assert_eq!(tool.role, Role::Tool);
    let text = tool.content.clone().unwrap_or_default();
    assert!(text.contains("got echo:hi sampling:-32601"), "{text}");

    let caps: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&caps_file).expect("the server saw initialize"))
            .unwrap();
    assert!(
        caps.get("sampling").is_none(),
        "sampling must not be advertised: {caps}"
    );

    // `--no-mcp`: nothing connects and codemode is not offered.
    let server = fake(vec![ScriptTurn::text("plain")]);
    std::fs::remove_file(&caps_file).unwrap();
    let ran = go(&env, &server.url(), &["--no-mcp", "hi"]);
    assert_eq!(ran.code, 0, "stderr: {}", ran.err);
    assert!(!server.requests()[0].tools_json.contains("\"codemode\""));
    assert!(!caps_file.exists(), "no server was started");
}

/// Exec-hook payloads above the size of one command-line argument (Linux: 128 KiB) reach the
/// hook on its real standard input (`Sandbox::run_with_stdin`), inside the same sandbox.
#[test]
fn exec_hook_payload_over_the_argument_limit() {
    use oxa_proto::{HookPoint, HookReply, Hooks as _};
    let env = TestEnv::new();
    if !bwrap_usable(&env) {
        eprintln!("skipped: bwrap is not usable on this machine (exec hooks run sandboxed)");
        return;
    }
    let path = std::env::var("PATH").unwrap_or_default();
    let sandbox: Arc<dyn oxa_proto::Sandbox> =
        Arc::new(oxa_sandbox::Bwrap::detect(&path, &env.home()).expect("bwrap"));
    let (decls, warnings) = oxa_ext::parse_exec_hooks(&serde_json::json!({
        "tool_call": "n=$(wc -c); if [ \"$n\" -gt 300000 ]; then echo \"got $n bytes\"; exit 2; fi"
    }));
    assert!(warnings.is_empty(), "{warnings:?}");
    let hooks = oxa_ext::ExecHooks::new(decls, sandbox, env.work());
    let big = "x".repeat(300_000);
    assert!(big.len() > oxa_ext::EXEC_HOOK_INLINE_STDIN_MAX);
    let payload = serde_json::json!({ "toolName": "write", "toolCallId": "c1", "input": { "content": big } });
    let mut ui = oxa_proto::HeadlessUi::default();
    match hooks.fire(HookPoint::ToolCall, &payload, &mut ui) {
        HookReply::Block(reason) => {
            let n: usize = reason
                .trim_start_matches("got ")
                .trim_end_matches(" bytes")
                .parse()
                .unwrap_or_else(|_| panic!("a byte count: {reason}"));
            assert!(n > 300_000, "the whole payload arrived: {n}");
        }
        other => panic!("the hook must block: {other:?}; notes: {:?}", ui.notes),
    }
}
