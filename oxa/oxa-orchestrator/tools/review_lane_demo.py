#!/usr/bin/env python3
"""Actual TUI: GPT author proposal -> parent -> native Anthropic reviewer, mocks only."""
import argparse
import hashlib
import importlib.util
import json
import os
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


def providers(openai_base, anthropic_base):
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
            with self.mu:
                self.requests.append(body)
                model = body["model"]
                self.counts[model] = self.counts.get(model, 0) + 1
                step = self.counts[model]
            name = None
            if model == "gpt-6-luna":
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
                    assert report["process_stopped"] and report["goal"]["state"] == "done"
                    assert "reviewer verified exact patch" in report["last_reply"]
                    value = completed("mixed-provider-review-final")
            uid = model.replace(".", "-") + "-" + str(step)
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
            super().__init__()

        def handle(self, handler, method):
            body = json.loads(handler.rfile.read(int(handler.headers["Content-Length"])))
            assert handler.path == "/v1/messages"
            assert handler.headers.get("x-api-key") == KEY
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
            assert self.release.wait(7), "reviewer barrier was not released"
            events = self.default_stream()
            reply = json.dumps(completed("reviewer verified exact patch"))
            deltas = [e for e in events if e[0] == "content_block_delta"]
            deltas[0][1]["delta"]["text"] = reply
            deltas[1][1]["delta"]["text"] = ""
            for kind, value in events:
                if kind == "message_delta":
                    value["usage"]["output_tokens"] = 8
            return self._sse(handler, events)
    return AuthorAndParent, Reviewer


def run(args):
    root = args.isolated_root.resolve()
    assert os.environ.get("OXA_REVIEW_DEMO_ISOLATED") == str(root)
    gateway = args.gateway_root.resolve()
    sys.path[:0] = [str(gateway / "lab" / "src"), str(gateway / "lab" / "tests")]
    from mock_cloud import MockOpenAIResponses, MockAnthropic
    from openai_api_stack import Stack
    from snapdragon_ai_lab.runtime import cloud, cloud_anthropic
    import anthropic
    assert anthropic.__version__ == "0.125.0", "use the repository-pinned SDK"
    spec = importlib.util.spec_from_file_location("review_live_demo", Path(__file__).with_name("live_tui_demo.py"))
    live = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(live)
    demo = live.Demo(args.bin_dir)
    author_type, reviewer_type = providers(MockOpenAIResponses, MockAnthropic)
    author = author_type()
    reviewer = reviewer_type(author)
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
            ("reviewer-sonnet", "anthropic", "claude-sonnet-5-5", reviewer.base),
        ):
            models[alias] = {"kind": "remote/" + provider, "upstream_model": upstream,
                             "base_url": base, "context_window": 200000, "max_output": 1024,
                             "pricing": {"input_per_million": 1.0, "output_per_million": 1.0}}
        ledger = root / "ledger"
        stack = Stack(author.port, cloud={"enabled": True, "daily_budget_usd": 1.0,
            "secrets_file": str(secrets), "models": models}, environ={"LOCAL_AI_LEDGER_DIR": str(ledger)})
        connection, client = cloud.connection, cloud_anthropic._client

        def owned(item, key, original, port):
            url = urllib.parse.urlsplit(item["base_url"])
            assert (url.scheme, url.hostname, url.port) == ("http", "127.0.0.1", port)
            assert key == KEY
            return original(item, key)

        stack.enter_context(patch.object(cloud, "connection", lambda item, key: owned(item, key, connection, author.port)))
        stack.enter_context(patch.object(cloud_anthropic, "_client", lambda item, key: owned(item, key, client, reviewer.port)))
        profiles = []
        for name, model, edit, files, role in (
            ("author", "author-gpt", True, ["input.txt"], "fixed isolated author"),
            ("reviewer", "reviewer-sonnet", False, [], "fixed independent reviewer"),
        ):
            profiles.append({"name": name, "role": role, "route": "cloud_gateway",
                "engine": stack.base.removeprefix("http://"), "model": model,
                "budget": {"wall_ms": 15000, "tokens": 100000, "turns": 4},
                "turn_ms": 8000, "output_bytes": 1048576, "files": files, "edit": edit})
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
        demo.wait(lambda v: (len(v["status"]["workers"]) == 2
            and demo.goal(v).get("reason") == "waiting_workers" and reviewer.started.is_set()))
        assert (demo.work / "input.txt").read_text() == BEFORE
        demo.save_screen("reviewer-working-screen.txt", lambda s: any(
            "reviewer-sonnet (cloud) · working" in line and "author-gpt (cloud) · completed" in line
            for line in s.splitlines()[-3:]))
        reviewer.release.set()
        done = demo.wait(lambda v: demo.goal(v)["state"] == "done" and all(w["process_stopped"] for w in v["status"]["workers"]))
        assert done["status"]["session"]["id"] == sid and demo.goal(done)["budget"] == budget
        assert demo.goal(done)["reported_tokens"] == 140
        workers = {w["profile"]: w for w in done["status"]["workers"]}
        assert workers["author"]["model"] == "author-gpt"
        assert workers["reviewer"]["model"] == "reviewer-sonnet"
        assert all(w["route"] == "cloud_gateway" for w in workers.values())
        assert {k: w["goal"]["reported_tokens"] for k, w in workers.items()} == {"author": 40, "reviewer": 23}
        assert all(w["state"] == "completed" and not Path("/proc", str(w["process_id"])).exists() for w in workers.values())
        assert (demo.work / "input.txt").read_text() == BEFORE
        demo.save_screen("review-completed-screen.txt", lambda s: (
            "mixed-provider-review-final" in s and "Goal Done" in s
            and any("reviewer-sonnet (cloud) · completed" in line and "author-gpt (cloud) · completed" in line
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
        for model, tokens in (("root-gpt", 140), ("author-gpt", 40), ("reviewer-sonnet", 23)):
            assert sum(r["prompt_tokens"] + r["completion_tokens"] for r in rows if r["model"] == model) == tokens
        assert next(r for r in rows if r["model"] == "reviewer-sonnet")["cached_tokens"] == 3
        assert KEY not in json.dumps(rows)
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
            "native_anthropic_reviewer": True, "source_unchanged": True, "workers_reaped": True,
            "parent_tokens": 140, "author_tokens": 40, "reviewer_tokens": 23, "gateway_rows": 10, "tui_exit": 0}
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gateway-root", type=Path, default=Path(__file__).resolve().parents[3])
    parser.add_argument("--bin-dir", type=Path, default=Path(__file__).resolve().parents[2] / "target" / "debug")
    parser.add_argument("--isolated-root", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
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
    result = subprocess.run([sys.executable, "-I", str(Path(__file__).resolve()), "--isolated-root", str(root),
        "--gateway-root", str(args.gateway_root.resolve()), "--bin-dir", str(args.bin_dir.resolve())], env=env)
    print(f"Isolated mixed-provider evidence: {root}")
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
