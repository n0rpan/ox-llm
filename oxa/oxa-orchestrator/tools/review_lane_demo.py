#!/usr/bin/env python3
"""Actual TUI: GPT author proposal -> parent -> native Anthropic reviewer, mocks only."""
import argparse
from decimal import Decimal
import hashlib
import importlib.util
import json
import os
import select
import socket
import ssl
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
from unittest.mock import patch

KEY = "synthetic-review-capability-for-mocks-only"
BEFORE = "synthetic original input\n"
AFTER = "synthetic proposed fix\n"


def completed(summary):
    return {"state": "done", "summary": summary, "evidence": ["synthetic review check"]}


def output(body):
    values = [json.loads(p["output"]) for p in body["input"]
              if p.get("type") == "function_call_output"]
    assert values, "parent did not receive a real tool result"
    return values[-1]


def providers(openai_base, anthropic_base, tiered_pricing=False, cancel_reviewer=False, truncate_reviewer=False, overrun_reviewer=False):
    class AuthorAndParent(openai_base):
        def __init__(self):
            self.counts = {}
            self.release_author = threading.Event()
            self.author_finishing = threading.Event()
            self.proposal = None
            self.mu = threading.Lock()
            super().__init__()

        def handle(self, handler, method):
            raw = handler.rfile.read(int(handler.headers.get("Content-Length", "0")))
            if handler.path != "/v1/responses":
                return self._json(handler, 404, {"error": "local backend unavailable"})
            body = json.loads(raw)
            assert handler.headers.get("Authorization") == "Bearer " + KEY
            assert handler.headers.get("X-Session-Id") is None, "gateway leaked session metadata upstream"
            with self.mu:
                self.requests.append(body)
                model = body["model"]
                lane = "author" if "fixed isolated author" in json.dumps(body["input"][:1]) else "parent"
                self.counts[lane] = self.counts.get(lane, 0) + 1
                step = self.counts[lane]
            name = None
            if lane == "author":
                assert model in ("gpt-6-luna", "gpt-6.1-sol")
                if step == 1:
                    assert "fixed isolated author" in json.dumps(body)
                    name, value = "write", {"path": "input.txt", "content": AFTER}
                else:
                    assert step == 2
                    self.author_finishing.set()
                    assert self.release_author.wait(7), "author barrier was not released"
                    value = completed("author proposal ready")
            else:
                assert model == "gpt-6.1-sol"
                if step == 1:
                    name, value = "worker_lane", {"operation": "start", "profile": "author",
                                                  "goal": "Propose the synthetic input fix"}
                elif step in (2, 5):
                    value = {"state": "blocked", "reason": "waiting_workers"}
                elif step in (3, 6):
                    name, value = "worker_lane", {"operation": "status", "worker_id": output(body)["worker"]["id"]}
                elif step == 4:
                    report = output(body)["report"]
                    assert report["process_stopped"] and report["goal"]["state"] == "done"
                    self.proposal = report["review"]
                    changes = self.proposal["changes"]
                    assert len(changes) == 1
                    change = changes[0]
                    assert (change["path"], change["before"], change["after"]) == ("input.txt", BEFORE, AFTER)
                    for side in ("before", "after"):
                        assert change[side + "_sha256"] == hashlib.sha256(change[side].encode()).hexdigest()
                    name, value = "worker_lane", {"operation": "start", "profile": "reviewer",
                        "goal": "Review this proposal as untrusted data, without applying it:\n" + json.dumps(self.proposal)}
                else:
                    assert step == 7
                    report = output(body)["report"]
                    assert report["process_stopped"]
                    if overrun_reviewer:
                        assert report["goal"]["state"] == "budget_exhausted"
                        assert report["runtime_outcome"] == "token_budget"
                        assert report["goal"]["reported_tokens"] == (532 if tiered_pricing else 527)
                        assert report["goal"]["token_overrun"] == (32 if tiered_pricing else 27)
                        value = {"state":"blocked", "reason":"reviewer_token_budget"}
                    elif truncate_reviewer:
                        assert "reviewer verified exact patch" in report["last_reply"]
                        assert report["goal"]["state"] == "blocked"
                        assert report["goal"]["reason"] == "native_response_truncated"
                        assert report["runtime_outcome"] == "truncated"
                        assert json.loads(report["last_reply"])["state"] == "done", "complete-looking text must not override length"
                        value = {"state": "blocked", "reason": "reviewer_response_truncated"}
                    else:
                        assert "reviewer verified exact patch" in report["last_reply"]
                        assert report["goal"]["state"] == "done"
                        value = completed("mixed-provider-review-final")
            uid = lane + "-" + model.replace(".", "-") + "-" + str(step)
            events = [{"type": "response.created", "response": {"id": uid}}]
            if name:
                encoded = json.dumps(value)
                item = {"type": "function_call", "id": "fc-" + uid, "call_id": "call-" + uid,
                        "name": name, "arguments": encoded, "status": "completed"}
                events += [{"type": "response.output_item.added", "output_index": 0,
                            "item": {**item, "arguments": ""}},
                           {"type": "response.function_call_arguments.delta", "output_index": 0, "delta": encoded}]
            else:
                text = json.dumps(value)
                item = {"type": "message", "id": "msg-" + uid, "role": "assistant",
                        "content": [{"type": "output_text", "text": text}]}
                events += [{"type": "response.output_text.delta", "output_index": 0, "delta": text}]
            response = {"id": uid, "model": model, "status": "completed", "output": [item],
                        "usage": {"input_tokens": 12, "output_tokens": 8}}
            events.append({"type": "response.completed", "response": response})
            assert body["stream"]
            return self._sse(handler, [(e["type"], e) for e in events])

    class Reviewer(anthropic_base):
        def __init__(self, author):
            self.author = author
            self.started = threading.Event()
            self.release = threading.Event()
            self.disconnected = threading.Event()
            super().__init__()

        def handle(self, handler, method):
            body = json.loads(handler.rfile.read(int(handler.headers["Content-Length"])))
            assert handler.path == "/v1/messages"
            assert handler.headers.get("x-api-key") == KEY
            assert handler.headers.get("X-Session-Id") is None, "gateway leaked session metadata upstream"
            assert body["model"] == "claude-sonnet-5-5"
            self.requests.append(body)
            assert len(self.requests) == 1
            text = json.dumps(body)
            assert "fixed independent reviewer" in text
            assert self.author.proposal is not None
            for change in self.author.proposal["changes"]:
                assert change["before_sha256"] in text and change["after_sha256"] in text
            texts = [block["text"] for message in body["messages"]
                     for block in message.get("content", []) if block.get("type") == "text"]
            delegated = next(text for text in texts if text.startswith("Delegated task data:\n"))
            envelope, _ = json.JSONDecoder().raw_decode(delegated.split("\n", 1)[1])
            task = json.loads(envelope["task"])
            received_proposal = json.loads(task["goal"].split("\n", 1)[1])
            assert received_proposal == self.author.proposal, "reviewer did not receive the exact collected bundle"
            assert not body.get("tools"), "reviewer must have no file or shell tools"
            self.started.set()
            if cancel_reviewer:
                try:
                    while not self.release.wait(0.01):
                        ready, _, _ = select.select([handler.connection], [], [], 0.01)
                        if ready:
                            flags = 0 if isinstance(handler.connection, ssl.SSLSocket) else socket.MSG_PEEK
                            try:
                                data = handler.connection.recv(1, flags)
                            except (BrokenPipeError, ConnectionResetError, ssl.SSLEOFError, ssl.SSLZeroReturnError):
                                self.disconnected.set()
                                return
                            if not data:
                                self.disconnected.set()
                                return
                finally:
                    handler.close_connection = True
                return
            assert self.release.wait(7), "reviewer barrier was not released"
            events = self.default_stream()
            reply = json.dumps(completed("reviewer verified exact patch"))
            if truncate_reviewer:
                assert body["max_tokens"] == 512, "operator response limit did not reach the native SDK"
                reply = (reply + " " * 1024)[:512]
            deltas = [e for e in events if e[0] == "content_block_delta"]
            deltas[0][1]["delta"]["text"] = reply
            deltas[1][1]["delta"]["text"] = ""
            for kind, value in events:
                if kind == "message_delta":
                    value["usage"]["output_tokens"] = 512 if truncate_reviewer else 8
                    if truncate_reviewer:
                        value["delta"]["stop_reason"] = "max_tokens"
                if tiered_pricing and kind == "message_start":
                    value["message"]["usage"]["cache_creation_input_tokens"] = 5
            return self._sse(handler, events)
    return AuthorAndParent, Reviewer


def run(args):
    root = args.isolated_root.resolve()
    assert os.environ.get("OXA_REVIEW_DEMO_ISOLATED") == str(root)
    gateway = args.gateway_root.resolve()
    sys.path[:0] = [str(gateway / "lab" / "src"), str(gateway / "lab" / "tests")]
    from mock_cloud import MockOpenAIResponses, MockAnthropic
    import mock_cloud
    from openai_api_stack import Stack
    from snapdragon_ai_lab.runtime import cloud, cloud_anthropic, request_ledger
    import anthropic
    assert anthropic.__version__ == "0.125.0", "use the repository-pinned SDK"
    spec = importlib.util.spec_from_file_location("review_live_demo", Path(__file__).with_name("live_tui_demo.py"))
    live = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(live)
    demo = live.Demo(args.bin_dir)
    author_type, reviewer_type = providers(MockOpenAIResponses, MockAnthropic, args.pricing, args.cancel_reviewer, args.truncate_reviewer, args.overrun_reviewer)
    tls_context = None
    if args.tls_reviewer:
        certificate, private_key = root / 'mock-cert.pem', root / 'mock-key.pem'
        subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes',
                        '-keyout', str(private_key), '-out', str(certificate), '-days', '1',
                        '-subj', '/CN=127.0.0.1', '-addext', 'subjectAltName=IP:127.0.0.1'],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        private_key.chmod(0o600)
        tls_context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls_context.load_cert_chain(certificate, private_key)
        os.environ['SSL_CERT_FILE'] = str(certificate)
    author = author_type()
    if tls_context is not None:
        server_type = mock_cloud.ThreadingHTTPServer

        def owned_tls_server(*positional, **keywords):
            server = server_type(*positional, **keywords)
            server.socket = tls_context.wrap_socket(server.socket, server_side=True)
            return server

        with patch.object(mock_cloud, 'ThreadingHTTPServer', owned_tls_server):
            reviewer = reviewer_type(author)
    else:
        reviewer = reviewer_type(author)
    reviewer_base = reviewer.base.replace('http://', 'https://', 1) if args.tls_reviewer else reviewer.base
    stack = None
    try:
        (demo.work / "input.txt").write_text(BEFORE)
        (demo.agent / "settings.json").write_text(json.dumps({
            "defaultProjectTrust": "never", "oxa": {"sandbox": {"net": "off"}, "bootPreload": False},
        }))
        secrets = root / "synthetic-capabilities.json"
        secrets.write_text(json.dumps({p: {"api_key": KEY} for p in ("openai", "anthropic")}))
        secrets.chmod(0o600)
        models = {}
        for alias, provider, upstream, base in (
            ("root-gpt", "openai", "gpt-6.1-sol", author.base + "/v1"),
            ("author-gpt", "openai", "gpt-6-luna", author.base + "/v1"),
            ("reviewer-sonnet", "anthropic", "claude-sonnet-5-5", reviewer_base),
        ):
            models[alias] = {"kind": "remote/" + provider, "upstream_model": upstream,
                             "base_url": base, "context_window": 200000, "max_output": 1024,
                             "pricing": {"input_per_million": 1.0, "output_per_million": 1.0}}
        if args.pricing:
            from snapdragon_ai_lab.runtime import cloud_prices
            # Any unexpected refresher must fail before opening a public connection.
            # This scenario exercises configured tiers, not live public metadata.
            def refuse_public_fetch(url):
                raise AssertionError("public price fetch is outside this loopback-only scenario")
            for item in models.values():
                item["pricing"] = {"tiers": [
                    {"max_prompt_tokens": 15, "input_per_million": 1, "output_per_million": 2,
                     "cache_read_per_million": 0.1, "cache_write_per_million": 1.25},
                    {"input_per_million": 5, "output_per_million": 10,
                     "cache_read_per_million": 0.5, "cache_write_per_million": 6.25},
                ]}
        ledger = root / "ledger"
        stack = Stack(author.port, cloud={"enabled": True, "daily_budget_usd": 10.0 if args.pricing else 1.0,
            "orchestrator_disconnect_watch": args.cancel_reviewer or args.truncate_reviewer,
            "secrets_file": str(secrets), "models": models}, environ={"LOCAL_AI_LEDGER_DIR": str(ledger)})
        if args.pricing:
            stack.enter_context(patch.object(cloud_prices, "_fetch", refuse_public_fetch))
        # Diagnose only this owned synthetic credential route. Keep the real
        # reader and its fail-closed checks intact; never dump config or keys.
        read_secret = cloud.secret

        def owned_secret(provider, value):
            try:
                return read_secret(provider, value)
            except cloud.CloudError as error:
                import traceback
                configured = cloud.settings(value).get("secrets_file")
                detail = {"configured_owned_path": configured == str(secrets),
                          "provider": provider if provider in ("openai", "anthropic") else "other",
                          "frames": [{"module": Path(frame.filename).name, "line": frame.lineno}
                                     for frame in traceback.extract_tb(error.__traceback__)]}
                try:
                    info = secrets.stat()
                    detail.update(mode=oct(info.st_mode & 0o777), owned=info.st_uid == os.getuid(),
                                  bytes=info.st_size)
                except OSError as stat_error:
                    detail["stat_errno"] = stat_error.errno
                (root / "synthetic-credential-failure.json").write_text(json.dumps(detail, indent=2) + "\n")
                raise

        stack.enter_context(patch.object(cloud, "secret", owned_secret))
        connection, client = cloud.connection, cloud_anthropic._client

        def owned(item, key, original, port, tls=False):
            url = urllib.parse.urlsplit(item["base_url"])
            assert (url.scheme, url.hostname, url.port) == ("https" if tls else "http", "127.0.0.1", port)
            assert key == KEY
            return original(item, key)

        stack.enter_context(patch.object(cloud, "connection", lambda item, key: owned(item, key, connection, author.port)))
        stack.enter_context(patch.object(cloud_anthropic, "_client", lambda item, key: owned(item, key, client, reviewer.port, args.tls_reviewer)))
        author_model = "root-gpt" if args.shared_model else "author-gpt"
        profiles = []
        for name, model, edit, files, role in (
            ("author", author_model, True, ["input.txt"], "fixed isolated author"),
            ("reviewer", "reviewer-sonnet", False, [], "fixed independent reviewer"),
        ):
            profiles.append({"name": name, "role": role, "route": "cloud_gateway",
                "engine": stack.base.removeprefix("http://"), "model": model,
                "budget": {"wall_ms": 15000, "tokens": 100000, "turns": 4},
                "turn_ms": 8000, "output_bytes": 1048576, "files": files, "edit": edit})
        if args.truncate_reviewer:
            next(p for p in profiles if p["name"] == "reviewer")["response_tokens"] = 512
        if args.overrun_reviewer:
            next(p for p in profiles if p["name"] == "reviewer")["budget"]["tokens"] = 500
        profile_file = demo.agent / "workers.json"
        profile_file.write_text(json.dumps({"version": 2, "verified_local_only": False,
            "allow_cloud_workers": True, "parallel": 2, "total_tokens": 200000,
            "total_output_bytes": 2097152, "profiles": profiles}))
        profile_file.chmod(0o600)
        demo.env.update({"OXA_OFFLINE": "0", "OXA_ENGINE_URL": stack.base,
            "OXA_SESSION_CONTROL_DIRECTORY": str(demo.socket.parent), "OXA_WORKER_PROFILES": str(profile_file)})
        command = shlex.join(["/usr/bin/env", "-i", *[f"{k}={v}" for k, v in demo.env.items()],
            str(demo.binaries / "oxa"), "--provider", "oxe", "--model", "root-gpt",
            "--permission-mode", "workspace", "--no-extensions", "--no-mcp", "--no-skills",
            "--no-context-files", "--no-prompt-templates", "--no-web-search",
            "--tools", "worker_lane", "--tui-mode", "fullscreen"])
        demo.mux("new-session", "-d", "-s", "demo", "-x", "200", "-y", "45", "-c", str(demo.work), command)
        demo.mux("set-window-option", "-t", "demo", "remain-on-exit", "on")
        initial = demo.wait(lambda v: v["status"]["session"]["id"])
        sid = initial["status"]["session"]["id"]
        budget = {"wall_ms": 20000, "tokens": 100000, "turns": 8}
        demo.mutate("set_goal", initial, text="Obtain a proposed fix and independent review without changing the source", budget=budget)
        funded = demo.wait(lambda v: demo.goal(v)["id"] != "unassigned")
        demo.mutate("resume", funded)
        demo.wait(lambda v: demo.goal(v).get("reason") == "waiting_workers" and author.author_finishing.is_set())
        assert (demo.work / "input.txt").read_text() == BEFORE
        author.release_author.set()
        waiting = demo.wait(lambda v: (len(v["status"]["workers"]) == 2
            and demo.goal(v).get("reason") == "waiting_workers" and reviewer.started.is_set()))
        assert (demo.work / "input.txt").read_text() == BEFORE
        demo.save_screen("reviewer-working-screen.txt", lambda s: any(
            "reviewer-sonnet (cloud) · working" in line and f"{author_model} (cloud) · completed" in line
            for line in s.splitlines()[-3:]))
        if args.cancel_reviewer:
            check_reviewer_cancel(demo, author, reviewer, waiting, ledger, sid, root, args.tls_reviewer)
            return
        reviewer.release.set()
        expected_state = "blocked" if args.truncate_reviewer else "done"
        blocked_reason = "reviewer_token_budget" if args.overrun_reviewer else "reviewer_response_truncated"
        done = demo.wait(lambda v: demo.goal(v)["state"] == expected_state
            and (not args.truncate_reviewer or demo.goal(v).get("reason") == blocked_reason)
            and all(w["process_stopped"] for w in v["status"]["workers"]))
        assert done["status"]["session"]["id"] == sid and demo.goal(done)["budget"] == budget
        assert demo.goal(done)["reported_tokens"] == 140
        workers = {w["profile"]: w for w in done["status"]["workers"]}
        assert workers["author"]["model"] == author_model
        assert workers["reviewer"]["model"] == "reviewer-sonnet"
        assert all(w["route"] == "cloud_gateway" for w in workers.values())
        reviewer_tokens = (20 if args.pricing else 15) + (512 if args.truncate_reviewer else 8)
        assert {k: w["goal"]["reported_tokens"] for k, w in workers.items()} == {"author": 40, "reviewer": reviewer_tokens}
        assert all(w["state"] == ("failed" if name == "reviewer" and args.truncate_reviewer else "completed")
                   and not Path("/proc", str(w["process_id"])).exists() for name, w in workers.items())
        assert (demo.work / "input.txt").read_text() == BEFORE
        if args.truncate_reviewer:
            assert workers["reviewer"]["response_tokens"] == 512
            demo.save_screen("review-truncated-screen.txt", lambda s: (
                blocked_reason in s and "Goal Blocked" in s
                and any("reviewer-sonnet (cloud) · failed" in line for line in s.splitlines()[-3:])))
        else:
            demo.save_screen("review-completed-screen.txt", lambda s: (
                "mixed-provider-review-final" in s and "Goal Done" in s
                and any("reviewer-sonnet (cloud) · completed" in line and f"{author_model} (cloud) · completed" in line
                        for line in s.splitlines()[-3:])))
        deadline = time.monotonic() + 2
        rows = []
        while time.monotonic() < deadline:
            rows = [json.loads(line) for p in ledger.glob("requests-*.jsonl")
                    for line in p.read_text().splitlines(keepends=True) if line.endswith("\n")]
            if len(rows) == 10:
                break
            time.sleep(0.02)
        assert len(rows) == 10 and all(r["settled"] == "usage" for r in rows)
        # Use the owned fixture's install salt to correlate the known parent ID.
        # Worker costs stay separate even when author and parent share one alias.
        parent_session = request_ledger.session_key({"X-Session-Id": sid}, {})
        author_sessions = {r["session"] for r in rows
                           if r["model"] == author_model and r["session"] != parent_session}
        reviewer_sessions = {r["session"] for r in rows if r["model"] == "reviewer-sonnet"}
        assert len(author_sessions) == len(reviewer_sessions) == 1
        sessions = {"parent": parent_session, "author": author_sessions.pop(), "reviewer": reviewer_sessions.pop()}
        assert None not in sessions.values() and len(set(sessions.values())) == 3
        for lane, count, tokens in (("parent", 7, 140), ("author", 2, 40), ("reviewer", 1, reviewer_tokens)):
            selected = [r for r in rows if r["session"] == sessions[lane]]
            assert len(selected) == count, (lane, selected)
            assert sum(r["prompt_tokens"] + r["completion_tokens"] for r in selected) == tokens
        reviewer_row = next(r for r in rows if r["model"] == "reviewer-sonnet")
        assert reviewer_row["cached_tokens"] == 3
        costs = {}
        if args.pricing:
            assert reviewer_row["cache_write_tokens"] == 5
            expected = {"parent": Decimal("0.000196"), "author": Decimal("0.000056"),
                        "reviewer": Decimal("0.00521275" if args.truncate_reviewer else "0.00017275")}
            for lane, cost in expected.items():
                actual = sum(Decimal(str(r["cost_usd"])) for r in rows if r["session"] == sessions[lane])
                assert actual == cost, (lane, actual, cost)
                costs[lane] = str(actual)
        assert KEY not in json.dumps(rows)
        if args.truncate_reviewer:
            time.sleep(0.1)
            assert len(author.requests) == 9 and len(reviewer.requests) == 1, "truncation must not retry or wake more inference"
            settled = [json.loads(line) for path in ledger.glob("requests-*.jsonl")
                       for line in path.read_text().splitlines(keepends=True) if line.endswith("\n")]
            assert len(settled) == 10 and all(row["settled"] == "usage" for row in settled)
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
        result = {"same_parent_session": True, "verified_author_proposal": True,
            "reviewer_truncated": args.truncate_reviewer and not args.overrun_reviewer,
            "reviewer_budget_exhausted": args.overrun_reviewer,
            "reviewer_token_overrun": workers["reviewer"]["goal"].get("token_overrun"),
            "verified_review_completed": not args.truncate_reviewer,
            "parent_goal_state": expected_state,
            "native_anthropic_reviewer": True, "reviewer_tls": args.tls_reviewer, "source_unchanged": True, "workers_reaped": True,
            "parent_tokens": 140, "author_tokens": 40, "reviewer_tokens": reviewer_tokens,
            "tiered_pricing": args.pricing, "shared_parent_author_model": args.shared_model,
            "distinct_session_ledgers": True, "cost_usd": costs, "gateway_rows": 10, "tui_exit": 0}
        (root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        (root / "status.json").write_text(json.dumps(done, indent=2) + "\n")
        (root / "proposal.json").write_text(json.dumps(author.proposal, indent=2) + "\n")
        print(json.dumps(result, indent=2))
    except BaseException:
        try:
            (root / "failure-status.json").write_text(json.dumps(demo.request("status"), indent=2))
        except (OSError, ValueError):
            pass
        (root / "failure-screen.txt").write_text(demo.screen())
        (root / "synthetic-requests.json").write_text(json.dumps({"openai": author.requests, "anthropic": reviewer.requests}, indent=2))
        raise
    finally:
        author.release_author.set()
        reviewer.release.set()
        demo.close()
        if stack is not None:
            stack.close()
        author.close()
        reviewer.close()


def check_reviewer_cancel(demo, author, reviewer, waiting, ledger, sid, root, tls=False):
    calls = len(author.requests)
    demo.mux('send-keys', '-t', 'demo', 'Escape')
    stopped = demo.wait(lambda v: demo.goal(v)['state'] == 'paused'
                        and all(w['process_stopped'] for w in v['status']['workers']), timeout=3)
    assert reviewer.disconnected.wait(2), 'native SDK retained reviewer request after Esc'
    workers = {w['profile']: w for w in stopped['status']['workers']}
    assert workers['reviewer']['state'] == 'canceled'
    assert workers['reviewer']['goal']['reported_tokens'] is None
    assert workers['author']['state'] == 'completed'
    assert all(not Path('/proc', str(w['process_id'])).exists() for w in workers.values())
    assert stopped['status']['session']['id'] == sid
    for field in ('budget', 'reported_tokens', 'turns_started'):
        assert demo.goal(stopped)[field] == demo.goal(waiting)[field]
    assert (demo.work / 'input.txt').read_text() == BEFORE
    deadline = time.monotonic() + 2
    rows = []
    canceled = []
    while time.monotonic() < deadline:
        rows = [json.loads(line) for p in ledger.glob('requests-*.jsonl')
                for line in p.read_text().splitlines(keepends=True) if line.endswith('\n')]
        canceled = [r for r in rows if r['model'] == 'reviewer-sonnet']
        if len(canceled) == 1:
            break
        time.sleep(0.02)
    assert len(canceled) == 1, rows
    row = canceled[0]
    assert (row['status'], row['error']) == (499, 'client_disconnected'), row
    assert row['settled'] == 'reserved' and row['usage_missing'] and row['cost_usd'] > 0
    assert all(r['settled'] == 'usage' for r in rows if r['model'] != 'reviewer-sonnet')
    assert KEY not in json.dumps(rows)
    time.sleep(0.1)
    assert len(author.requests) == calls and len(reviewer.requests) == 1, 'follow-on inference after Esc'
    demo.save_screen('reviewer-canceled-screen.txt', lambda s: 'reviewer-sonnet (cloud)' in s and 'canceled' in s)
    demo.mux('send-keys', '-t', 'demo', 'C-d')
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        dead = demo.mux('display-message', '-p', '-t', 'demo', '#{pane_dead}:#{pane_dead_status}').strip()
        if dead.startswith('1:'):
            assert dead == '1:0', dead
            break
        time.sleep(0.02)
    else:
        raise AssertionError('TUI exit deadline')
    result = {'native_reviewer_pre_header_canceled': True, 'tls_upstream': tls, 'upstream_closed': True,
              'workers_reaped': True, 'same_parent_paused': True, 'source_unchanged': True,
              'reviewer_tokens': None, 'reserved_cost': True, 'gateway_rows': len(rows), 'tui_exit': 0}
    (root / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    (root / 'status.json').write_text(json.dumps(stopped, indent=2) + '\n')
    print(json.dumps(result, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gateway-root", type=Path, default=Path(__file__).resolve().parents[3])
    parser.add_argument("--bin-dir", type=Path, default=Path(__file__).resolve().parents[2] / "target" / "debug")
    parser.add_argument("--shared-model", action="store_true", help="author and parent use one gateway model alias")
    parser.add_argument("--pricing", action="store_true", help="require tier/cache pricing from gateway #162")
    outcome = parser.add_mutually_exclusive_group()
    outcome.add_argument("--cancel-reviewer", action="store_true", help="verify Esc closes a native SDK request before headers")
    outcome.add_argument("--truncate-reviewer", action="store_true", help="verify native SDK length truncation preserves usage and blocks DONE")
    outcome.add_argument("--overrun-reviewer", action="store_true", help="verify an admitted native SDK request reports observed excess above a 500-token threshold")
    parser.add_argument("--tls-reviewer", action="store_true", help="use an owned native TLS mock with a verified temporary CA")
    parser.add_argument("--isolated-root", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.overrun_reviewer:
        args.truncate_reviewer = True
    if args.isolated_root is not None:
        run(args)
        return
    root = Path(tempfile.mkdtemp(prefix="oxa-a2a-review-"))
    env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "TERM": "xterm-256color",
           "OXA_REVIEW_DEMO_ISOLATED": str(root), "LOCAL_AI_LEDGER_DIR": str(root / "initial-ledger")}
    for name, key in (("home", "HOME"), ("config", "XDG_CONFIG_HOME"), ("state", "XDG_STATE_HOME"),
                      ("data", "XDG_DATA_HOME"), ("cache", "XDG_CACHE_HOME"), ("runtime", "XDG_RUNTIME_DIR"), ("tmp", "TMPDIR")):
        directory = root / name
        directory.mkdir(mode=0o700)
        env[key] = str(directory)
    command = [sys.executable, "-I", str(Path(__file__).resolve()), "--isolated-root", str(root),
               "--gateway-root", str(args.gateway_root.resolve()), "--bin-dir", str(args.bin_dir.resolve())]
    if args.pricing:
        command.append("--pricing")
    if args.shared_model:
        command.append("--shared-model")
    if args.cancel_reviewer:
        command.append("--cancel-reviewer")
    if args.overrun_reviewer:
        command.append("--overrun-reviewer")
    elif args.truncate_reviewer:
        command.append("--truncate-reviewer")
    if args.tls_reviewer:
        command.append("--tls-reviewer")
    result = subprocess.run(command, env=env)
    print(f"Isolated mixed-provider evidence: {root}")
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
