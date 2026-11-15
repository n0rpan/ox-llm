#!/usr/bin/env python3
"""Real gateway + feature-built TUI/native worker against an owned Responses mock."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import shlex
import select
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
import uuid
from unittest.mock import patch

KEY = "synthetic-cloud-key-for-mock-only"


def mock_type(base, case):
    compacting = case == "compaction"
    class Upstream(base):
        def __init__(self):
            self.counts = {}
            self.normal_counts = {}
            self.local_requests = []
            self.summaries = 0
            self.mu = threading.Lock()
            self.release_worker = threading.Event()
            self.streaming = threading.Event()
            self.disconnected = threading.Event()
            super().__init__()

        def unfinished_stream(self, handler):
            handler.send_response(200)
            handler.send_header("Content-Type", "text/event-stream")
            handler.send_header("Connection", "close")
            handler.end_headers()
            event = {"type": "response.output_text.delta", "output_index": 0,
                     "delta": "synthetic streaming work "}
            chunk = ("event: response.output_text.delta\ndata: " + json.dumps(event) + "\n\n").encode()
            handler.wfile.write(chunk)
            handler.wfile.flush()
            self.streaming.set()
            try:
                while not self.release_worker.is_set():
                    ready, _, _ = select.select([handler.connection], [], [], 0.02)
                    if ready and not handler.connection.recv(1, socket.MSG_PEEK):
                        self.disconnected.set()
                        break
                    if case == "cancel-stream":
                        handler.wfile.write(chunk)
                        handler.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                self.disconnected.set()
            finally:
                handler.close_connection = True

        def handle(self, handler, method):
            raw = handler.rfile.read(int(handler.headers.get("Content-Length", "0")))
            body = json.loads(raw) if raw else None
            if handler.path != "/v1/responses":
                # Stack also uses this owned server as its unavailable local backend.
                # Remote token counting must tolerate local-only routes returning 404.
                self.local_requests.append({"method": method, "path": handler.path})
                return self._json(handler, 404, {"error": "local backend unavailable"})
            with self.mu:
                summarizing = "Stop working on the task now." in json.dumps(body.get("input", [])[-1:])
                self.requests.append({"body": body, "path": handler.path, "summary": summarizing})
                model = body["model"]
                self.counts[model] = self.counts.get(model, 0) + 1
                if summarizing:
                    assert compacting and model == "gpt-6.1-sol"
                    self.summaries += 1
                    step = self.summaries
                else:
                    self.normal_counts[model] = self.normal_counts.get(model, 0) + 1
                    step = self.normal_counts[model]
            assert handler.path == "/v1/responses"
            assert handler.headers.get("Authorization") == "Bearer " + KEY
            if summarizing:
                name, args = None, "gateway-summary-marker: preserve verified worker identity and file evidence"
            elif model == "gpt-6.1-sol":
                if step == 1:
                    name, args = "worker_lane", {
                        "operation": "start", "profile": "author",
                        "goal": "Read the approved input and report evidence",
                    }
                elif step == 2:
                    name, args = None, {"state": "blocked", "reason": "waiting_workers"}
                elif step == 3:
                    name, args = None, {
                        "state": "done", "summary": "gateway-parent-final",
                        "evidence": ["worker result verified"],
                    }
                else:
                    raise AssertionError("unexpected parent inference")
            else:
                assert model == "gpt-6-luna"
                if step == 1:
                    name, args = "read", {"path": "input.txt"}
                elif step == 2:
                    if case.startswith("cancel-"):
                        return self.unfinished_stream(handler)
                    # A barrier makes cache expiry deterministic under load.
                    assert self.release_worker.wait(7), "parent did not reach the expiry barrier"
                    name, args = None, {
                        "state": "done", "summary": "gateway-worker-final",
                        "evidence": ["read input.txt"],
                    }
                else:
                    raise AssertionError("unexpected worker inference")
            uid = ("summary" if summarizing else "root" if model == "gpt-6.1-sol" else "worker") + str(step)
            output = []
            events = [{"type": "response.created", "response": {"id": "resp_" + uid}}]
            if name:
                output.append({"type": "reasoning", "id": "rs_" + uid, "summary": [],
                               "encrypted_content": "synthetic-opaque-" + uid})
                encoded = json.dumps(args)
                call = {"type": "function_call", "id": "fc_" + uid,
                        "call_id": "call_" + uid, "name": name,
                        "arguments": encoded, "status": "completed"}
                output.append(call)
                events.append({"type": "response.output_item.added", "output_index": 1,
                               "item": {**call, "arguments": ""}})
                cut = len(encoded) // 2
                for piece in (encoded[:cut], encoded[cut:]):
                    events.append({"type": "response.function_call_arguments.delta",
                                   "output_index": 1, "delta": piece})
            else:
                text = args if summarizing else json.dumps(args)
                output.append({"type": "message", "id": "msg_" + uid, "role": "assistant",
                               "content": [{"type": "output_text", "text": text}]})
                events.append({"type": "response.output_text.delta", "output_index": 0,
                               "delta": text})
            response = {"id": "resp_" + uid, "model": model, "status": "completed",
                        "output": output, "usage": {"input_tokens": 12, "output_tokens": 8}}
            events.append({"type": "response.completed", "response": response})
            if body.get("stream"):
                return self._sse(handler, [(e["type"], e) for e in events])
            return self._json(handler, 200, response)
    return Upstream


def run_isolated(args):
    root = args.isolated_root.resolve()
    assert os.environ.get("OXA_MOCK_GATEWAY_ISOLATED") == str(root)
    gateway = args.gateway_root.resolve()
    # Import gateway code only after re-exec with the empty HOME/environment.
    sys.path[:0] = [str(gateway / "lab" / "src"), str(gateway / "lab" / "tests")]
    from mock_cloud import MockOpenAIResponses
    from openai_api_stack import Stack
    from snapdragon_ai_lab.runtime import cloud, cloud_openai, request_ledger
    demo_file = Path(__file__).with_name("live_tui_demo.py")
    spec = importlib.util.spec_from_file_location("a2a_live_demo", demo_file)
    live = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(live)
    demo = live.Demo(args.bin_dir)
    compacting = args.case == "compaction"
    mutation_ids = []
    if args.control_client:
        def cli_request(method, **fields):
            command = [str(demo.binaries / "oxa"), "orchestrate-control", "--socket", str(demo.socket)]
            data = None
            if method == "status":
                command.append("status")
            elif method == "events":
                command.extend(["events", "--after", str(fields["after"])])
            else:
                request_id = "gateway-client-" + uuid.uuid4().hex
                mutation_ids.append(request_id)
                data = json.dumps({"id": request_id, "method": method, **fields})
                command.append("request")
            result = subprocess.run(command, input=data, capture_output=True, text=True,
                                    env=demo.env, timeout=5)
            assert result.returncode == 0, result.stderr or result.stdout
            return json.loads(result.stdout)
        demo.request = cli_request
    upstream = mock_type(MockOpenAIResponses, args.case)()
    stack = None
    try:
        (demo.work / "input.txt").write_text("synthetic approved input\n")
        settings = json.loads((demo.agent / "settings.json").read_text())
        # SDK inference stays online, but shell network is off. In particular,
        # the parent must not ask the host keyring for a GitHub capability.
        settings.setdefault("oxa", {}).setdefault("sandbox", {})["net"] = "off"
        if compacting:
            settings["oxa"]["compaction"] = {
                "softTokens": 15, "turnStart": False, "midRun": False, "idleSummary": False,
            }
            settings["compaction"] = {"keepRecentTokens": 0}
            demo.env["OXA_GOAL_AUTO_COMPACTION"] = "1"
        (demo.agent / "settings.json").write_text(json.dumps(settings))
        keyfile = root / "provider-capability.json"
        keyfile.write_text(json.dumps({"openai": {"api_key": KEY}}))
        keyfile.chmod(0o600)
        ledger = root / "gateway-ledger"
        models = {
            alias: {
                "kind": "remote/openai", "upstream_model": model,
                "base_url": upstream.base + "/v1", "context_window": 200000,
                "max_output": 1024,
                "pricing": {"input_per_million": 1.0, "output_per_million": 1.0},
            }
            for alias, model in (("root-gpt", "gpt-6.1-sol"), ("worker-gpt", "gpt-6-luna"))
        }
        stack = Stack(upstream.port, cloud={
            "enabled": True, "daily_budget_usd": 1.0,
            "secrets_file": str(keyfile), "models": models,
        }, environ={"LOCAL_AI_LEDGER_DIR": str(ledger)})
        connect = cloud.connection

        def owned_connection(item, key):
            url = urllib.parse.urlsplit(item["base_url"])
            assert (url.scheme, url.hostname, url.port) == ("http", "127.0.0.1", upstream.port)
            assert key == KEY
            return connect(item, key)

        stack.enter_context(patch.object(cloud, "connection", owned_connection))
        profiles = demo.agent / "workers.json"
        profiles.write_text(json.dumps({
            "version": 2, "verified_local_only": False, "allow_cloud_workers": True,
            "parallel": 1, "total_tokens": 100000, "total_output_bytes": 1048576,
            "profiles": [{
                "name": "author", "role": "fixed gateway author", "route": "cloud_gateway",
                "engine": stack.base.removeprefix("http://"), "model": "worker-gpt",
                "budget": {"wall_ms": 15000, "tokens": 100000, "turns": 4},
                "turn_ms": 8000, "output_bytes": 1048576,
                "files": ["input.txt"], "edit": False,
            }],
        }))
        profiles.chmod(0o600)
        demo.env.update({
            "OXA_OFFLINE": "0", "OXA_ENGINE_URL": stack.base,
            "OXA_SESSION_CONTROL_DIRECTORY": str(demo.socket.parent),
            "OXA_WORKER_PROFILES": str(profiles),
        })
        cli = [
            str(demo.binaries / "oxa"), "--provider", "oxe", "--model", "root-gpt",
            "--permission-mode", "workspace", "--no-extensions", "--no-mcp", "--no-skills",
            "--no-context-files", "--no-prompt-templates", "--no-web-search",
            "--tools", "worker_lane", "--tui-mode", "fullscreen",
        ]
        command = shlex.join([
            "/usr/bin/env", "-i", *[f"{k}={v}" for k, v in demo.env.items()], *cli,
        ])
        demo.mux("new-session", "-d", "-s", "demo", "-x", "180", "-y", "45",
                 "-c", str(demo.work), command)
        demo.mux("set-window-option", "-t", "demo", "remain-on-exit", "on")
        status = demo.wait(lambda v: v["status"]["session"]["id"])
        sid = status["status"]["session"]["id"]
        budget = {"wall_ms": 20000, "tokens": 100000, "turns": 8}
        demo.mutate("set_goal", status, text="Drive a gateway worker and verify its result", budget=budget)
        funded = demo.wait(lambda v: demo.goal(v)["id"] != "unassigned")
        demo.mutate("resume", funded)
        waiting = demo.wait(lambda v: (
            demo.goal(v)["state"] == "blocked" and demo.goal(v).get("reason") == "waiting_workers"
            and upstream.counts.get("gpt-6-luna", 0) == 2
        ))
        if args.case.startswith("cancel-"):
            check_cancellation(demo, upstream, ledger, waiting, sid, root, args.case)
            return
        with cloud_openai._HISTORY_LOCK:
            evicted = len(cloud_openai._HISTORY)
            cloud_openai._HISTORY.clear()
        assert evicted > 0
        upstream.release_worker.set()
        done = demo.wait(lambda v: (
            demo.goal(v)["state"] == "done" and v["status"]["workers"][0]["process_stopped"]
        ), timeout=15)
        assert done["status"]["session"]["id"] == sid
        assert demo.goal(done)["budget"] == budget
        expected_parent_tokens = 100 if compacting else 60
        assert demo.goal(done)["reported_tokens"] == expected_parent_tokens, demo.goal(done)
        assert upstream.summaries == (2 if compacting else 0)
        child = done["status"]["workers"][0]
        assert child["state"] == "completed" and child["goal"]["reported_tokens"] == 40
        assert child["route"] == "cloud_gateway" and child["model"] == "worker-gpt"
        assert not Path("/proc", str(child["process_id"])).exists()
        parent_requests = [r["body"] for r in upstream.requests if r["body"]["model"] == "gpt-6.1-sol" and not r["summary"]]
        worker_requests = [r["body"] for r in upstream.requests if r["body"]["model"] == "gpt-6-luna"]
        assert (len(parent_requests), len(worker_requests)) == (3, 2)
        assert "synthetic approved input" in json.dumps(worker_requests[1])
        assert "fixed gateway author" in json.dumps(worker_requests[0])
        assert "synthetic-opaque-worker1" in json.dumps(worker_requests[1])
        assert "synthetic-opaque-" not in json.dumps(parent_requests[-1])
        assert "previous_response_id" not in parent_requests[-1]
        if not compacting:
            assert any(p.get("type") == "function_call" and p.get("name") == "worker_lane"
                       for p in parent_requests[-1]["input"])
        assert "gateway-worker-final" in json.dumps(parent_requests[-1])
        if compacting:
            assert "gateway-summary-marker" in json.dumps(parent_requests[-1])
        if args.control_client:
            cursor = 0
            admitted = set()
            while True:
                page = demo.request("events", after=cursor)
                assert not page["gap"], "lost admission evidence"
                for row in page["events"]:
                    event = row["event"]
                    if event["kind"] == "control_admitted":
                        admitted.add(event["request_id"])
                cursor = page["next"]
                if cursor == page["latest"]:
                    break
            assert set(mutation_ids) <= admitted, (mutation_ids, admitted)
        demo.save_screen("gateway-screen.txt", lambda s: (
            "gateway-parent-final" in s and "Goal Done" in s
            and any("cloud · openai" in line for line in s.splitlines() if " OXA " in line)
            and "worker-gpt (cloud)" in s
        ))
        expected_rows = 7 if compacting else 5
        deadline = time.monotonic() + 3
        rows = []
        while time.monotonic() < deadline:
            rows = [json.loads(line) for p in ledger.glob("requests-*.jsonl")
                    for line in p.read_text().splitlines() if line]
            if len(rows) == expected_rows:
                break
            time.sleep(0.02)
        assert len(rows) == expected_rows
        assert sum(r["prompt_tokens"] + r["completion_tokens"] for r in rows
                   if r["model"] == "root-gpt") == demo.goal(done)["reported_tokens"]
        assert sum(r["prompt_tokens"] + r["completion_tokens"] for r in rows
                   if r["model"] == "worker-gpt") == child["goal"]["reported_tokens"]
        parent_session = request_ledger.session_key({"X-Session-Id": sid}, {})
        assert {r["session"] for r in rows if r["model"] == "root-gpt"} == {parent_session}
        child_sessions = {r["session"] for r in rows if r["model"] == "worker-gpt"}
        assert len(child_sessions) == 1 and None not in child_sessions and parent_session not in child_sessions
        assert all(r.get("remote_provider") == "openai" and r.get("settled") == "usage" for r in rows)
        assert KEY not in json.dumps(rows) and "synthetic-opaque-" not in json.dumps(rows)
        demo.mux("send-keys", "-t", "demo", "C-d")
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            dead = demo.mux("display-message", "-p", "-t", "demo", "#{pane_dead}:#{pane_dead_status}").strip()
            if dead.startswith("1:"):
                assert dead == "1:0", dead
                break
            time.sleep(0.02)
        else:
            raise AssertionError("TUI exit deadline")
        result = {
            "real_gateway": True, "control_client_verified": args.control_client, "root_model": "root-gpt", "worker_model": "worker-gpt",
            "evicted_native_entries": evicted, "root_tokens": expected_parent_tokens, "worker_tokens": 40,
            "summary_calls": upstream.summaries,
            "gateway_rows": len(rows), "continuation_recovered": True,
            "case": args.case,
            "worker_file_read": True, "cloud_footer_verified": True, "tui_exit": 0,
        }
        (root / "status.json").write_text(json.dumps(done, indent=2) + "\n")
        (root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result, indent=2))
    except BaseException:
        try:
            (root / "failure-status.json").write_text(json.dumps(demo.request("status"), indent=2))
        except (OSError, ValueError):
            pass
        (demo.root / "failure-screen.txt").write_text(demo.screen())
        (root / "upstream-debug.json").write_text(json.dumps(upstream.requests, indent=2))
        raise
    finally:
        upstream.release_worker.set()
        demo.close()
        if stack is not None:
            stack.close()
        upstream.close()


def check_cancellation(demo, upstream, ledger, waiting, sid, root, case):
    assert upstream.streaming.wait(2), "worker never entered upstream streaming"
    demo.save_screen("gateway-working-screen.txt", lambda s: (
        "worker-gpt (cloud)" in s and "working" in s
    ))
    before = time.monotonic()
    demo.mux("send-keys", "-t", "demo", "Escape")
    stopped = demo.wait(lambda v: (
        demo.goal(v)["state"] == "paused"
        and v["status"]["workers"][0]["process_stopped"]
    ), timeout=3)
    child = stopped["status"]["workers"][0]
    assert child["state"] == "canceled", child
    assert not Path("/proc", str(child["process_id"])).exists()
    assert stopped["status"]["session"]["id"] == sid
    for field in ("budget", "turns_started", "reported_tokens"):
        assert demo.goal(stopped)[field] == demo.goal(waiting)[field]
    assert demo.goal(stopped)["reported_tokens"] == 40
    assert child["goal"]["reported_tokens"] is None, child
    closed = upstream.disconnected.wait(max(0, 3 - (time.monotonic() - before)))
    rows = [json.loads(line) for p in ledger.glob("requests-*.jsonl")
            for line in p.read_text().splitlines() if line]
    evidence = {"case": case, "same_parent_paused": True, "worker_reaped": True,
                "parent_tokens": 40, "worker_tokens": None, "upstream_closed": closed,
                "upstream_calls": len(upstream.requests), "gateway_rows": len(rows)}
    (root / "cancellation.json").write_text(json.dumps(evidence, indent=2) + "\n")
    (root / "status.json").write_text(json.dumps(stopped, indent=2) + "\n")
    assert closed, "Esc reaped worker, but gateway retained quiet upstream beyond 3 seconds"
    deadline = time.monotonic() + 2
    while time.monotonic() < deadline:
        rows = [json.loads(line) for p in ledger.glob("requests-*.jsonl")
                for line in p.read_text().splitlines() if line]
        if len(rows) == 4:
            break
        time.sleep(0.02)
    assert len(rows) == 4, rows
    canceled = [r for r in rows if r.get("settled") == "reserved"]
    assert len(canceled) == 1, rows
    assert canceled[0]["model"] == "worker-gpt" and canceled[0]["usage_missing"]
    assert canceled[0]["cost_usd"] > 0
    assert KEY not in json.dumps(rows)
    assert len(upstream.requests) == 4, "cancellation triggered follow-on inference"
    demo.save_screen("gateway-canceled-screen.txt", lambda s: (
        "worker-gpt (cloud)" in s and "canceled" in s
    ))
    demo.mux("send-keys", "-t", "demo", "C-d")
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        dead = demo.mux("display-message", "-p", "-t", "demo", "#{pane_dead}:#{pane_dead_status}").strip()
        if dead.startswith("1:"):
            assert dead == "1:0", dead
            break
        time.sleep(0.02)
    else:
        raise AssertionError("TUI exit deadline")
    evidence.update(gateway_rows=len(rows), reserved_cost=True, tui_exit=0)
    (root / "result.json").write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps(evidence, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gateway-root", type=Path, default=Path(__file__).resolve().parents[3])
    parser.add_argument("--bin-dir", type=Path, default=Path(__file__).resolve().parents[2] / "target" / "debug")
    parser.add_argument("--case", choices=("expiry", "compaction", "cancel-stream", "cancel-quiet"), default="expiry")
    parser.add_argument("--control-client", action="store_true", help="use the actual CLI control client")
    parser.add_argument("--isolated-root", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.isolated_root is not None:
        run_isolated(args)
        return
    root = Path(tempfile.mkdtemp(prefix="oxa-a2a-cloud-"))
    env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "TERM": "xterm-256color",
           "OXA_MOCK_GATEWAY_ISOLATED": str(root)}
    for name, variable in (
        ("home", "HOME"), ("config", "XDG_CONFIG_HOME"), ("state", "XDG_STATE_HOME"),
        ("data", "XDG_DATA_HOME"), ("cache", "XDG_CACHE_HOME"),
        ("runtime", "XDG_RUNTIME_DIR"), ("tmp", "TMPDIR"),
    ):
        directory = root / name
        directory.mkdir(mode=0o700)
        env[variable] = str(directory)
    env["LOCAL_AI_LEDGER_DIR"] = str(root / "initial-ledger")
    command = [sys.executable, "-I", str(Path(__file__).resolve()), "--isolated-root", str(root),
               "--gateway-root", str(args.gateway_root.resolve()), "--bin-dir", str(args.bin_dir.resolve()),
               "--case", args.case]
    if args.control_client:
        command.append("--control-client")
    result = subprocess.run(command, env=env)
    print(f"Isolated gateway evidence: {root}")
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
