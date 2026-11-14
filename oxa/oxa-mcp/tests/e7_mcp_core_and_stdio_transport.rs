//! E7 acceptance tests: MCP core and stdio transport. Owner: task E7.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_fake::TestEnv;
use oxa_mcp::*;
use oxa_proto::{codes, CancelToken, ToolClass, Value};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Messages a scripted server sends in answer to one client message, each after a delay.
type Out = Vec<(Duration, Value)>;

fn now(v: Value) -> (Duration, Value) {
    (Duration::ZERO, v)
}

fn ok(id: &Value, result: Value) -> (Duration, Value) {
    now(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn err(id: &Value, code: i64, message: &str) -> (Duration, Value) {
    now(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
}

fn method_of(m: &Value) -> &str {
    m.get("method").and_then(Value::as_str).unwrap_or("")
}

fn init_result(resources: bool) -> Value {
    let caps = if resources {
        json!({ "tools": {}, "resources": {} })
    } else {
        json!({ "tools": {} })
    };
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": caps,
        "serverInfo": { "name": "fake", "version": "1" },
        "instructions": "use the fake tools",
    })
}

/// Serve one end of a pipe: every client message goes to `handler`, whose output is sent back in
/// order. Ends when the client side is dropped (or after 10 s of silence).
fn serve(mut end: PipeTransport, mut handler: impl FnMut(&Value) -> Out + Send + 'static) -> JoinHandle<()> {
    thread::spawn(move || loop {
        let msg = match end.recv(Duration::from_secs(10)) {
            Ok(Some(m)) => m,
            _ => return,
        };
        for (delay, out) in handler(&msg) {
            if !delay.is_zero() {
                thread::sleep(delay);
            }
            if end.send(&out).is_err() {
                return;
            }
        }
    })
}

/// A stdio server that runs a shell script.
fn sh_server(env: &TestEnv, script: &str) -> StdioTransport {
    let cfg = StdioConfig {
        command: "sh".into(),
        args: vec!["-c".into(), script.into()],
        ..Default::default()
    };
    StdioTransport::spawn(&cfg, &[], &env.work(), &env.home()).expect("spawn sh server")
}

/// Process `pid` exists and is not a zombie.
fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => false,
        Ok(s) => {
            let state = s.rsplit(')').next().unwrap_or("").trim().chars().next();
            !matches!(state, Some('Z') | Some('X') | None)
        }
    }
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

/// capabilities and version
#[test]
fn initialize_handshake() {
    let (client_end, server_end) = PipeTransport::pair();
    let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let srv = serve(server_end, move |m| {
        seen2.lock().unwrap().push(m.clone());
        match method_of(m) {
            "initialize" => vec![ok(&m["id"], init_result(true))],
            "tools/list" => vec![ok(&m["id"], json!({ "tools": [] }))],
            _ => vec![],
        }
    });
    let mut c = McpClient::connect(Box::new(client_end), "9.9.9", Duration::from_secs(5)).expect("connect");
    assert_eq!(c.instructions(), Some("use the fake tools"));
    assert!(c.has_resources());
    // The list request is served after the initialized notification on the same pipe, so the
    // answer is also the point where the notification is known to have arrived.
    assert!(c.list_tools().expect("list").is_empty());
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0]["method"], "initialize");
    assert_eq!(seen[0]["id"], 1);
    assert_eq!(seen[0]["params"]["protocolVersion"], PROTOCOL_VERSION);
    assert_eq!(seen[0]["params"]["clientInfo"]["name"], "oxa");
    assert_eq!(seen[0]["params"]["clientInfo"]["version"], "9.9.9");
    assert_eq!(seen[1]["method"], "notifications/initialized");
    assert!(seen[1].get("id").is_none());
    c.close();
    drop(c);
    srv.join().unwrap();
}

#[test]
fn initialize_wrong_protocol_version_is_error() {
    let (client_end, server_end) = PipeTransport::pair();
    let srv = serve(server_end, move |m| match method_of(m) {
        "initialize" => vec![ok(
            &m["id"],
            json!({ "protocolVersion": "1999-01-01", "capabilities": {} }),
        )],
        _ => vec![],
    });
    let e = McpClient::connect(Box::new(client_end), "1", Duration::from_secs(5))
        .err()
        .expect("a protocol version oxa does not speak is an error");
    assert_eq!(e.code, codes::MCP);
    srv.join().unwrap();
}

/// with pagination
#[test]
fn tools_list_and_call() {
    let (client_end, server_end) = PipeTransport::pair();
    let srv = serve(server_end, move |m| {
        let id = &m["id"];
        match method_of(m) {
            "initialize" => vec![ok(id, init_result(false))],
            "tools/list" => match m["params"].get("cursor").and_then(Value::as_str) {
                None => vec![ok(
                    id,
                    json!({
                        "tools": [{
                            "name": "a",
                            "description": "first",
                            "inputSchema": { "type": "object" },
                            "annotations": { "readOnlyHint": false },
                        }],
                        "nextCursor": "page-2",
                    }),
                )],
                Some("page-2") => vec![ok(
                    id,
                    json!({
                        "tools": [{
                            "name": "b",
                            "inputSchema": { "type": "object" },
                            "annotations": { "readOnlyHint": true },
                        }],
                    }),
                )],
                Some(other) => vec![err(id, -32602, &format!("bad cursor {other}"))],
            },
            "tools/call" => {
                let name = m["params"]["name"].as_str().unwrap_or("");
                match name {
                    "a" => {
                        let x = m["params"]["arguments"]["x"].clone();
                        vec![ok(
                            id,
                            json!({ "content": [{ "type": "text", "text": format!("got {x}") }] }),
                        )]
                    }
                    "b" => vec![ok(
                        id,
                        json!({ "content": [{ "type": "text", "text": "bad" }], "isError": true }),
                    )],
                    _ => vec![err(id, -32602, "no such tool")],
                }
            }
            _ => vec![],
        }
    });
    let mut c = McpClient::connect(Box::new(client_end), "1", Duration::from_secs(5)).unwrap();
    let tools = c.list_tools().expect("list");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["a", "b"], "all pages, in server order");
    assert_eq!(tools[0].description, "first");
    assert_eq!(tools[0].annotations.read_only_hint, Some(false));
    assert_eq!(tools[1].annotations.class(true), ToolClass::Read);
    assert_eq!(tools[1].description, "");

    let cancel = CancelToken::new();
    let r = c
        .call_tool("a", &json!({ "x": 1 }), &cancel, &mut |_, _, _| {})
        .expect("call");
    assert!(!r.is_error);
    assert_eq!(result_text("srv", &r), "got 1");

    let r = c
        .call_tool("b", &json!({}), &cancel, &mut |_, _, _| {})
        .expect("isError is still Ok");
    assert!(r.is_error);
    assert_eq!(result_text("srv", &r), "bad");

    let e = c
        .call_tool("boom", &json!({}), &cancel, &mut |_, _, _| {})
        .expect_err("JSON-RPC error");
    assert_eq!(e.code, codes::MCP);
    assert!(e.message.contains("no such tool"), "{}", e.message);
    c.close();
    drop(c);
    srv.join().unwrap();
}

/// long call with progress stays alive
#[test]
fn progress_resets_timeout() {
    let (client_end, server_end) = PipeTransport::pair();
    let srv = serve(server_end, move |m| {
        let id = &m["id"];
        match method_of(m) {
            "initialize" => vec![ok(id, init_result(false))],
            "tools/call" => {
                // Six gaps of 150 ms: 900 ms in all, more than the 400 ms timeout. Each progress
                // notification must restart the timer for the call to survive.
                let token = m["params"]["_meta"]["progressToken"].clone();
                let mut out: Out = (1..=5)
                    .map(|i| {
                        (
                            Duration::from_millis(150),
                            json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/progress",
                                "params": { "progressToken": token, "progress": i, "total": 5, "message": "step" },
                            }),
                        )
                    })
                    .collect();
                out.push((
                    Duration::from_millis(150),
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [{ "type": "text", "text": "done" }] } }),
                ));
                out
            }
            _ => vec![],
        }
    });
    let mut c = McpClient::connect(Box::new(client_end), "1", Duration::from_millis(400)).unwrap();
    let mut seen: Vec<(f64, Option<f64>, Option<String>)> = Vec::new();
    let cancel = CancelToken::new();
    let r = c
        .call_tool("slow", &json!({}), &cancel, &mut |p, t, msg| {
            seen.push((p, t, msg.map(str::to_string)))
        })
        .expect("a call with progress outlives the timeout");
    assert_eq!(result_text("srv", &r), "done");
    assert_eq!(seen.len(), 5);
    assert_eq!(seen[0], (1.0, Some(5.0), Some("step".to_string())));
    c.close();
    drop(c);
    srv.join().unwrap();
}

#[test]
fn call_without_progress_times_out() {
    let (client_end, server_end) = PipeTransport::pair();
    let srv = serve(server_end, move |m| {
        let id = &m["id"];
        match method_of(m) {
            "initialize" => vec![ok(id, init_result(false))],
            "tools/call" => vec![(
                Duration::from_millis(1200),
                json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [] } }),
            )],
            _ => vec![],
        }
    });
    let mut c = McpClient::connect(Box::new(client_end), "1", Duration::from_millis(300)).unwrap();
    let e = c
        .call_tool("slow", &json!({}), &CancelToken::new(), &mut |_, _, _| {})
        .expect_err("no progress: the timeout fires");
    assert_eq!(e.code, codes::TIMEOUT);
    // The late answer is ignored and the client stays usable.
    thread::sleep(Duration::from_millis(1000));
    c.close();
    drop(c);
    srv.join().unwrap();
}

#[test]
fn cancel_aborts_call() {
    let (client_end, server_end) = PipeTransport::pair();
    let srv = serve(server_end, move |m| {
        let id = &m["id"];
        match method_of(m) {
            "initialize" => vec![ok(id, init_result(false))],
            "tools/call" => vec![(
                Duration::from_secs(3),
                json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [] } }),
            )],
            _ => vec![],
        }
    });
    let mut c = McpClient::connect(Box::new(client_end), "1", Duration::from_secs(10)).unwrap();
    let cancel = CancelToken::new();
    let flag = cancel.clone();
    let t0 = Instant::now();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        flag.cancel();
    });
    let e = c
        .call_tool("slow", &json!({}), &cancel, &mut |_, _, _| {})
        .expect_err("cancelled");
    assert_eq!(e.code, codes::ABORTED);
    assert!(t0.elapsed() < Duration::from_secs(2), "cancel returns promptly");
    c.close();
    drop(c);
    srv.join().unwrap();
}

/// resources tools
#[test]
fn resources_list_and_read() {
    let (client_end, server_end) = PipeTransport::pair();
    let srv = serve(server_end, move |m| {
        let id = &m["id"];
        match method_of(m) {
            "initialize" => vec![ok(id, init_result(true))],
            "resources/list" => vec![ok(
                id,
                json!({
                    "resources": [
                        { "uri": "file:///a", "name": "a", "mimeType": "text/plain" },
                        { "uri": "ui://app/main", "name": "app" },
                        { "uri": "mem://panel", "name": "panel", "mimeType": "text/html;profile=mcp-app" },
                    ],
                }),
            )],
            "resources/templates/list" => vec![ok(
                id,
                json!({ "resourceTemplates": [{ "uriTemplate": "file:///{name}", "name": "files" }] }),
            )],
            "resources/read" => {
                let uri = m["params"]["uri"].as_str().unwrap_or("").to_string();
                vec![ok(id, json!({ "contents": [{ "uri": uri, "text": "hello" }] }))]
            }
            _ => vec![],
        }
    });
    let mut c = McpClient::connect(Box::new(client_end), "1", Duration::from_secs(5)).unwrap();
    assert!(c.has_resources());
    let (list, next) = c.list_resources(None).expect("list");
    assert_eq!(next, None);
    assert_eq!(list.len(), 1, "MCP Apps resources are omitted");
    assert_eq!(list[0].uri, "file:///a");
    assert_eq!(list[0].name, "a");
    assert_eq!(list[0].extra["mimeType"], "text/plain");
    let (templates, _) = c.list_resource_templates(None).expect("templates");
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].uri, "file:///{name}");
    assert_eq!(templates[0].name, "files");
    let contents = c.read_resource("file:///a").expect("read");
    assert_eq!(contents[0]["text"], "hello");
    assert_eq!(contents[0]["uri"], "file:///a");
    c.close();
    drop(c);
    srv.join().unwrap();
}

/// stdin close, SIGTERM, SIGKILL of the group
#[test]
fn stop_sequence() {
    let env = TestEnv::new();

    // Stage 0: a server that exits on stdin EOF stops at once.
    let mut t = sh_server(&env, "cat >/dev/null");
    let pid = t.pid().expect("pid");
    let t0 = Instant::now();
    t.close();
    assert!(t0.elapsed() < Duration::from_secs(2), "stdin EOF is enough");
    assert!(!alive(pid));

    // Stages 1 and 2: a server that ignores SIGTERM dies only after SIGKILL, 2 s after each
    // signal.
    let mut t = sh_server(&env, "trap '' TERM; while :; do sleep 0.05; done");
    let pid = t.pid().expect("pid");
    let t0 = Instant::now();
    t.close();
    let took = t0.elapsed();
    assert!(
        took >= Duration::from_secs(3),
        "SIGTERM then a 2 s wait: {took:?}"
    );
    assert!(took < Duration::from_secs(12), "bounded: {took:?}");
    assert!(!alive(pid));
}

#[test]
fn stop_sequence_kills_the_group() {
    let env = TestEnv::new();
    let pidfile = env.root().join("grandchild.pid");
    let script = format!(
        "trap '' TERM; sleep 60 & echo $! > '{}'; while :; do sleep 0.05; done",
        pidfile.display()
    );
    let mut t = sh_server(&env, &script);
    assert!(
        wait_until(5, || pidfile.exists()
            && std::fs::read_to_string(&pidfile)
                .map(|s| s.trim().parse::<u32>().is_ok())
                .unwrap_or(false)),
        "the server started its background process"
    );
    let grandchild: u32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    assert!(alive(grandchild));
    t.close();
    assert!(
        wait_until(3, || !alive(grandchild)),
        "SIGKILL of the group also ends what the server started"
    );
}

/// not to the terminal
#[test]
fn server_stderr_to_log() {
    let env = TestEnv::new();
    let mut t = sh_server(&env, "echo boom-one 1>&2; echo boom-two 1>&2; cat >/dev/null");
    assert!(wait_until(5, || t.stderr_tail().contains("boom-two")));
    assert_eq!(t.stderr_tail(), "boom-one\nboom-two");
    t.close();
}

#[test]
fn stderr_tail_keeps_last_20_lines() {
    let env = TestEnv::new();
    let mut t = sh_server(
        &env,
        "i=1; while [ $i -le 30 ]; do echo line$i 1>&2; i=$((i+1)); done; cat >/dev/null",
    );
    assert!(wait_until(5, || t.stderr_tail().contains("line30")));
    let tail = t.stderr_tail();
    let lines: Vec<&str> = tail.lines().collect();
    assert_eq!(lines.len(), 20);
    assert_eq!(lines[0], "line11");
    assert_eq!(lines[19], "line30");
    t.close();
}

/// no panic
#[test]
fn malformed_message_is_mcp_error() {
    // Not JSON-RPC 2.0 at all.
    for bad in [
        json!({ "hello": 1 }),
        json!([1, 2]),
        json!({ "jsonrpc": "1.0", "id": 1, "result": {} }),
        json!({ "jsonrpc": "2.0", "id": "x", "result": {} }),
        json!({ "jsonrpc": "2.0", "id": 1 }),
        json!({ "jsonrpc": "2.0", "method": 7 }),
    ] {
        let e = rpc_decode(&bad).err().unwrap_or_else(|| panic!("accepted {bad}"));
        assert_eq!(e.code, codes::MCP, "{bad}");
    }

    // A server answering initialize with garbage: connect fails with an mcp error.
    let (client_end, server_end) = PipeTransport::pair();
    let srv = serve(server_end, move |m| match method_of(m) {
        "initialize" => vec![now(json!({ "nonsense": true }))],
        _ => vec![],
    });
    let e = McpClient::connect(Box::new(client_end), "1", Duration::from_secs(5))
        .err()
        .expect("garbage answer");
    assert_eq!(e.code, codes::MCP);
    srv.join().unwrap();

    // A malformed tools list fails the listing, not the process.
    let (client_end, server_end) = PipeTransport::pair();
    let srv = serve(server_end, move |m| {
        let id = &m["id"];
        match method_of(m) {
            "initialize" => vec![ok(id, init_result(false))],
            "tools/list" => vec![ok(id, json!({ "tools": 5 }))],
            _ => vec![],
        }
    });
    let mut c = McpClient::connect(Box::new(client_end), "1", Duration::from_secs(5)).unwrap();
    let e = c.list_tools().expect_err("tools is not an array");
    assert_eq!(e.code, codes::MCP);
    c.close();
    drop(c);
    srv.join().unwrap();
}

/// A stray non-JSON line on stdout is dropped; the session goes on.
#[test]
fn non_json_stdout_lines_are_dropped() {
    let env = TestEnv::new();
    let init = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{}}}"#;
    let tools = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#;
    let script = format!(
        "read a; echo 'Starting up...'; echo '{init}'; read b; read c; echo 'debug noise'; echo '{tools}'; cat >/dev/null"
    );
    let t = sh_server(&env, &script);
    let mut c = McpClient::connect(Box::new(t), "1", Duration::from_secs(5)).expect("connect");
    assert!(c.list_tools().expect("list").is_empty());
    c.close();
}

/// Exec access asked per plan sec. 2
///
/// The permission question is asked by the caller (the manager, F6) before `spawn`, with
/// `Access::Exec { command, net: true }`; `spawn` takes no gate. This test checks what spawn
/// does with the decision it was given: the command runs with the expanded `~/` paths, the
/// working directory, and exactly the expanded environment.
#[test]
fn spawn_passes_policy() {
    let env = TestEnv::new();
    let sub = env.home().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    env.write(
        "home/probe.sh",
        "pwd -P > out.txt\necho \"$OXA_E7_PROBE\" >> out.txt\ncat >/dev/null\n",
    );
    let cfg = StdioConfig {
        command: "sh".into(),
        args: vec!["~/probe.sh".into()],
        cwd: Some("~/sub".into()),
        ..Default::default()
    };
    let env_pairs = vec![("OXA_E7_PROBE".to_string(), "probe-value".to_string())];
    let mut t = StdioTransport::spawn(&cfg, &env_pairs, &env.work(), &env.home()).expect("spawn");
    let out = sub.join("out.txt");
    assert!(wait_until(5, || std::fs::read_to_string(&out)
        .map(|s| s.lines().count() == 2)
        .unwrap_or(false)));
    let text = std::fs::read_to_string(&out).unwrap();
    let mut lines = text.lines();
    assert_eq!(
        lines.next().unwrap(),
        sub.canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(lines.next().unwrap(), "probe-value");
    t.close();

    // A missing executable is an mcp error, not a panic.
    let cfg = StdioConfig {
        command: "/nonexistent/oxa-e7-no-such-server".into(),
        ..Default::default()
    };
    let e = StdioTransport::spawn(&cfg, &[], &env.work(), &env.home())
        .err()
        .expect("cannot start");
    assert_eq!(e.code, codes::MCP);
}

/// Extra: the wire shapes and the W4 text of a result.
#[test]
fn rpc_shapes_and_result_text() {
    assert_eq!(
        rpc_request(3, "tools/list", &Value::Null),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" })
    );
    assert_eq!(
        rpc_request(4, "tools/call", &json!({ "name": "a" })),
        json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "a" } })
    );
    assert_eq!(
        rpc_notification("notifications/initialized", &Value::Null),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
    );
    assert!(matches!(
        rpc_decode(&json!({ "jsonrpc": "2.0", "method": "x" })),
        Ok(Incoming::Notification { .. })
    ));
    assert!(matches!(
        rpc_decode(&json!({ "jsonrpc": "2.0", "id": "s", "method": "ping" })),
        Ok(Incoming::Request { .. })
    ));
    assert_eq!(
        rpc_decode(&json!({ "jsonrpc": "2.0", "id": 7, "error": { "code": -1, "message": "m" } })),
        Ok(Incoming::Response {
            id: 7,
            result: Err((-1, "m".to_string()))
        })
    );

    let r = CallToolResult {
        content: vec![
            json!({ "type": "text", "text": "line one" }),
            json!({ "type": "image", "data": "AAAA", "mimeType": "image/png" }),
            json!({ "type": "resource_link", "uri": "file:///x.txt", "name": "x" }),
            json!({ "type": "resource", "resource": { "uri": "mem://t", "mimeType": "text/plain", "text": "embedded" } }),
            json!({ "type": "resource", "resource": { "uri": "mem://b", "blob": "QQ==" } }),
        ],
        structured_content: None,
        is_error: false,
    };
    assert_eq!(
        result_text("srv", &r),
        [
            "line one",
            "[image image 0x0: not visible to this model]",
            "[resource link: read_mcp_resource(server: \"srv\", uri: \"file:///x.txt\")]",
            "embedded",
            "[binary resource mem://b omitted]",
        ]
        .join("\n")
    );
    let structured = CallToolResult {
        content: vec![],
        structured_content: Some(json!({ "a": 1 })),
        is_error: false,
    };
    assert_eq!(
        result_text("srv", &structured),
        serde_json::to_string_pretty(&json!({ "a": 1 })).unwrap()
    );
    assert_eq!(result_text("srv", &CallToolResult::default()), "");
}

/// Extra: server logging reaches the sink; a server request is answered while the client is idle.
#[test]
fn log_sink_and_pump_answer_server_requests() {
    let (client_end, server_end) = PipeTransport::pair();
    let replies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let replies2 = replies.clone();
    let srv = serve(server_end, move |m| {
        let id = &m["id"];
        if m.get("method").is_none() {
            // A reply from the client (to our ping): remember it.
            replies2.lock().unwrap().push(m.clone());
            return vec![];
        }
        match method_of(m) {
            "initialize" => vec![ok(id, init_result(false))],
            "tools/list" => vec![
                now(json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/message",
                    "params": { "level": "warning", "logger": "srv", "data": "hello" },
                })),
                ok(id, json!({ "tools": [] })),
                // Sent after the response: it waits until the client is idle and pumps.
                now(json!({ "jsonrpc": "2.0", "id": "p1", "method": "ping" })),
            ],
            _ => vec![],
        }
    });
    let mut c = McpClient::connect(Box::new(client_end), "1", Duration::from_secs(5)).unwrap();
    let logged: Arc<Mutex<Vec<(String, String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let logged2 = logged.clone();
    c.set_log_sink(Box::new(move |l, g, m| {
        logged2
            .lock()
            .unwrap()
            .push((l.to_string(), g.to_string(), m.to_string()))
    }));
    c.list_tools().unwrap();
    assert_eq!(
        logged.lock().unwrap().clone(),
        vec![("warning".into(), "srv".into(), "hello".into())]
    );
    assert!(wait_until(3, || {
        c.pump().unwrap();
        replies
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "p1" && r["result"] == json!({}))
    }));
    c.close();
    drop(c);
    srv.join().unwrap();
}

/// Extra: a whole session over a real process: initialize, list, then the server exits.
#[test]
fn stdio_round_trip() {
    let env = TestEnv::new();
    let init = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"instructions":"sh server"}}"#;
    let tools = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"echoes","inputSchema":{"type":"object"}}]}}"#;
    let script = format!("read a; echo '{init}'; read b; read c; echo '{tools}'; cat >/dev/null");
    let t = sh_server(&env, &script);
    let mut c = McpClient::connect(Box::new(t), "1", Duration::from_secs(5)).expect("connect");
    assert_eq!(c.instructions(), Some("sh server"));
    let tools = c.list_tools().expect("list");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    c.close();
}
