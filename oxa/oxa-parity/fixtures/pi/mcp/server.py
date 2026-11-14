# G11 fixture: a minimal MCP stdio server (JSON-RPC 2.0, one message per line).
# Not a recording from pi: it is the test double the mcp parity rows drive. Run as
#   python3 -I server.py <record dir> [mode]
# It writes what it observed into <record dir>: caps.json (the client's initialize
# capabilities), init.json (cwd and the environment variables the tests inspect), eof (stdin
# closed), pid (its process id). Modes: "plain" (default), "slow_init" (waits 2 s before
# answering initialize), "stubborn" (ignores stdin close and SIGTERM until SIGKILL).
# Tools: echo (readOnlyHint), slow {seconds, progress} (sleeps, optional progress
# notifications), write_thing (no annotations: a write-class tool), "a.b", "a_b" (readOnly),
# log_big {bytes} (a logging notification of that size, then a result), sample (asks the
# client for sampling and reports the JSON-RPC code it got back).
# Resources: res://one (text), res://pic (image blob), and a template.
import json, os, signal, sys, time

rec = sys.argv[1]
mode = sys.argv[2] if len(sys.argv) > 2 else "plain"


def put(name, value):
    with open(os.path.join(rec, name), "w") as f:
        if isinstance(value, str):
            f.write(value)
        else:
            json.dump(value, f)


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def result(mid, value):
    send({"jsonrpc": "2.0", "id": mid, "result": value})


def text(t):
    return {"content": [{"type": "text", "text": t}]}


put("pid", str(os.getpid()))
if mode == "stubborn":
    signal.signal(signal.SIGTERM, signal.SIG_IGN)

TOOLS = [
    {"name": "echo", "description": "Echo the text.",
     "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]},
     "annotations": {"readOnlyHint": True}},
    {"name": "slow", "description": "Sleep, optionally sending progress.",
     "inputSchema": {"type": "object", "properties": {"seconds": {"type": "number"}, "progress": {"type": "boolean"}}},
     "annotations": {"readOnlyHint": True}},
    {"name": "write_thing", "description": "Write something.",
     "inputSchema": {"type": "object", "properties": {}}},
    {"name": "a.b", "description": "Dotted name.", "inputSchema": {"type": "object", "properties": {}},
     "annotations": {"readOnlyHint": True}},
    {"name": "a_b", "description": "Underscored name.", "inputSchema": {"type": "object", "properties": {}},
     "annotations": {"readOnlyHint": True}},
    {"name": "log_big", "description": "Log a big message.",
     "inputSchema": {"type": "object", "properties": {"bytes": {"type": "integer"}}},
     "annotations": {"readOnlyHint": True}},
    {"name": "sample", "description": "Ask the client for sampling.", "inputSchema": {"type": "object", "properties": {}},
     "annotations": {"readOnlyHint": True}},
]

for line in sys.stdin:
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    method, mid, params = msg.get("method"), msg.get("id"), msg.get("params") or {}
    if method == "initialize":
        if mode == "slow_init":
            time.sleep(2)
        put("caps.json", params.get("capabilities", {}))
        put("init.json", {"cwd": os.getcwd(), "lang": os.environ.get("LANG"), "term": os.environ.get("TERM"),
                          "oxa_test_var": os.environ.get("OXA_TEST_VAR"), "oxa_test_cmd": os.environ.get("OXA_TEST_CMD"), "pwd_env": os.environ.get("PWD")})
        result(mid, {"protocolVersion": params.get("protocolVersion"), "capabilities": {"tools": {}, "resources": {}},
                     "serverInfo": {"name": "fake", "version": "1"},
                     "instructions": "Fixture server instructions."})
    elif method == "tools/list":
        result(mid, {"tools": TOOLS})
    elif method == "tools/call":
        name, args = params.get("name"), params.get("arguments") or {}
        if name == "echo":
            result(mid, text("echo:" + str(args.get("text", ""))))
        elif name == "slow":
            total = float(args.get("seconds", 0))
            steps = int(total / 0.5)
            for i in range(steps):
                time.sleep(0.5)
                if args.get("progress"):
                    tok = (params.get("_meta") or {}).get("progressToken")
                    send({"jsonrpc": "2.0", "method": "notifications/progress",
                          "params": {"progressToken": tok, "progress": i + 1}})
            result(mid, text("slept"))
        elif name == "log_big":
            send({"jsonrpc": "2.0", "method": "notifications/message",
                  "params": {"level": "info", "logger": "fixture", "data": "x" * int(args.get("bytes", 0))}})
            result(mid, text("logged"))
        elif name == "sample":
            send({"jsonrpc": "2.0", "id": "s1", "method": "sampling/createMessage",
                  "params": {"messages": [], "maxTokens": 1}})
            reply = json.loads(sys.stdin.readline())
            code = reply.get("error", {}).get("code")
            result(mid, text("sampling:" + str(code)))
        else:
            result(mid, text(name + ":" + json.dumps(args, sort_keys=True)))
    elif method == "resources/list":
        result(mid, {"resources": [{"uri": "res://one", "name": "one", "mimeType": "text/plain"},
                                   {"uri": "res://pic", "name": "pic", "mimeType": "image/png"}]})
    elif method == "resources/templates/list":
        result(mid, {"resourceTemplates": [{"uriTemplate": "res://item/{id}", "name": "item"}]})
    elif method == "resources/read":
        uri = params.get("uri")
        if uri == "res://pic":
            result(mid, {"contents": [{"uri": uri, "mimeType": "image/png", "blob": "iVBORw0KGgo="}]})
        else:
            result(mid, {"contents": [{"uri": uri, "mimeType": "text/plain", "text": "resource body"}]})
    elif method == "ping":
        result(mid, {})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "method not found"}})

# stdin closed (the client stopped the server)
if mode == "stubborn":
    put("eof", "1")
    while True:
        time.sleep(1)
put("eof", "1")
