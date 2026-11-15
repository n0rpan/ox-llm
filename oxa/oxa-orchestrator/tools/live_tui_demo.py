#!/usr/bin/env python3
"""Drive the feature-built CLI in an isolated tmux server against owned fakes."""
import argparse
import json
import os
from pathlib import Path
import selectors
import shlex
import socket
import subprocess
import tempfile
import time
import uuid


def text_turn(state, summary, evidence=None):
    value = {"state": state, "summary": summary}
    if evidence is not None:
        value["evidence"] = evidence
    return {"text": json.dumps(value)}


class Demo:
    def __init__(self, binaries):
        self.binaries = binaries.resolve()
        for name in ("oxa", "oxa-fake"):
            if not os.access(self.binaries / name, os.X_OK):
                raise RuntimeError(f"build {name} first; see LIVE_TUI_DEMO.md")
        self.root = Path(tempfile.mkdtemp(prefix="oxa-a2a-live-"))
        self.home = self.root / "home"
        self.agent = self.home / ".oxa" / "agent"
        self.work = self.root / "work"
        self.agent.mkdir(parents=True, mode=0o700)
        self.work.mkdir()
        self.env = {
            "HOME": str(self.home), "PATH": "/usr/bin:/bin",
            "TERM": "xterm-256color", "LANG": "C.UTF-8",
            "OXA_TEST_ISOLATION": "1", "OXA_OFFLINE": "1",
            "OXA_SYSTEM_SKILLS_DIR": "",
        }
        (self.agent / "settings.json").write_text(json.dumps({
            "defaultProjectTrust": "never", "oxa.bootPreload": False,
        }))
        self.processes = []
        self.tmux = ["tmux", "-L", "oxa-a2a-demo-" + uuid.uuid4().hex, "-f", "/dev/null"]
        self.socket = self.agent / "control" / "session.sock"

    def mux(self, *args, check=True):
        return subprocess.run(
            [*self.tmux, *args], env=self.env, capture_output=True,
            text=True, check=check,
        ).stdout

    def screen(self):
        return self.mux("capture-pane", "-p", "-S", "-100", "-t", "demo", check=False)

    def save_screen(self, name, predicate):
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            screen = self.screen()
            if predicate(screen):
                (self.root / name).write_text(screen)
                return
            time.sleep(0.02)
        raise AssertionError(f"terminal frame did not show {name}")

    def fake(self, name, turns, *extra):
        script = self.root / (name + ".jsonl")
        script.write_text("".join(json.dumps(turn) + "\n" for turn in turns))
        record = self.root / (name + "-requests.jsonl")
        with (self.root / (name + "-stderr.txt")).open("w") as err:
            process = subprocess.Popen(
                [str(self.binaries / "oxa-fake"), "--script", str(script),
                 "--port", "0", "--record", str(record), *extra],
                env=self.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=err, text=True,
            )
        self.processes.append(process)
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            if not selector.select(5):
                raise AssertionError("owned fake startup deadline")
            line = process.stdout.readline().strip()
        if not line.startswith("listening http://127.0.0.1:"):
            raise AssertionError(f"unexpected fake endpoint: {line}")
        return line.split(" ", 1)[1], record

    def request(self, method, **fields):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(2)
            client.connect(str(self.socket))
            wire = json.dumps({"id": "live-demo", "method": method, **fields}) + "\n"
            client.sendall(wire.encode())
            client.shutdown(socket.SHUT_WR)
            chunks = []
            while chunk := client.recv(65536):
                chunks.append(chunk)
            return json.loads(b"".join(chunks))

    def wait(self, predicate, timeout=12):
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            if self.socket.exists():
                last = self.request("status")
                if predicate(last):
                    return last
            time.sleep(0.02)
        if last is not None:
            (self.root / "failure-status.json").write_text(json.dumps(last, indent=2))
        raise AssertionError("live control status deadline")

    @staticmethod
    def goal(status):
        return status["status"]["goal"]

    def mutate(self, method, status, **fields):
        result = self.request(method, revision=self.goal(status)["revision"], **fields)
        assert result.get("queued") is True, result

    @staticmethod
    def records(path):
        if not path.exists():
            return []
        # The fake appends complete JSONL records, but its writer can be between writes.
        lines = path.read_text().splitlines(keepends=True)
        return [json.loads(line) for line in lines if line.endswith("\n")]

    def run(self):
        worker_url, worker_record = self.fake(
            "worker", [text_turn("done", "child result", ["synthetic check"]),
                       text_turn("done", "child result", ["synthetic check"])],
            "--model", "worker-model", "--hold-headers", "2200",
        )
        profiles = self.agent / "workers.json"
        profiles.write_text(json.dumps({
            "version": 1, "verified_local_only": True, "parallel": 1,
            "total_tokens": 200000, "total_output_bytes": 2097152,
            "profiles": [{
                "name": "author", "role": "fixed demo author",
                "engine": worker_url.removeprefix("http://"), "model": "worker-model",
                "budget": {"wall_ms": 10000, "tokens": 100000, "turns": 4},
                "turn_ms": 8000, "output_bytes": 1048576, "response_tokens": 2048,
            }],
        }))
        profiles.chmod(0o600)
        tool = {"id": "demo-worker", "name": "worker_lane", "args": json.dumps({
            "operation": "start", "profile": "author", "goal": "Check synthetic work",
        })}
        url, record = self.fake("main", [
            text_turn("continue", "first shared step"),
            text_turn("done", "same-session-demo", ["one shared session"]),
            {"tool_calls": [tool], "finish": "tool_calls"},
            text_turn("done", "waiting-for-worker", ["worker started"]),
            {"tool_calls": [{**tool, "id": "collected-worker"}], "finish": "tool_calls"},
            text_turn("done", "waiting-for-second-worker", ["worker started"]),
            text_turn("done", "collected-worker-result", ["worker reply verified"]),
            text_turn("continue", "long step one"),
            text_turn("continue", "long step two"),
            {"text": "live-summary-marker: previous steps checked"},
            text_turn("done", "live-compaction-final", ["retained summary context"]),
            {"text": "live-summary-marker: final steps checked"},
        ], "--hold-headers", "400", "--n-ctx", "50000",
            "--model-row", "fake-model:50000:1024:moe")
        self.env.update({
            "OXA_ENGINE_URL": url, "OXA_SESSION_CONTROL_DIRECTORY": str(self.socket.parent),
            "OXA_WORKER_PROFILES": str(profiles), "OXA_GOAL_AUTO_COMPACTION": "1",
        })
        args = [
            str(self.binaries / "oxa"), "--provider", "oxe", "--model", "fake-model",
            "--permission-mode", "workspace", "--offline", "--no-extensions",
            "--no-mcp", "--no-skills", "--no-context-files", "--no-prompt-templates",
            "--no-web-search", "--tools", "worker_lane", "--tui-mode", "fullscreen",
        ]
        command = shlex.join([
            "/usr/bin/env", "-i", *[f"{k}={v}" for k, v in self.env.items()], *args,
        ])
        self.mux("new-session", "-d", "-s", "demo", "-x", "180", "-y", "45",
                 "-c", str(self.work), command)
        self.mux("set-window-option", "-t", "demo", "remain-on-exit", "on")
        status = self.wait(lambda v: v["status"]["session"]["id"])
        session = status["status"]["session"]["id"]
        budget = {"wall_ms": 15000, "tokens": 100000, "turns": 6}
        self.mutate("set_goal", status, text="Demonstrate shared control", budget=budget)
        status = self.wait(lambda v: self.goal(v)["id"] != "unassigned")
        self.mutate("resume", status)
        running = self.wait(lambda v: v["status"]["session"]["busy"])
        clarification = "Record that external steering reached this same session"
        self.mutate("steer", running, text=clarification)
        first = self.wait(lambda v: self.goal(v)["state"] == "done")
        assert first["status"]["session"]["id"] == session
        self.save_screen("shared-screen.txt", lambda s: "same-session-demo" in s and "Goal Done" in s)
        self.wait(lambda _: len(self.records(record)) == 2)
        first_records = self.records(record)
        assert clarification in json.loads(first_records[1]["body"])["messages"][-1]["content"]
        total = sum(r["usage"]["prompt"] + r["usage"]["completion"] for r in first_records)
        assert self.goal(first)["reported_tokens"] == total

        self.mutate("set_goal", first, text="Start a worker and wait for its result", budget=budget)
        status = self.wait(lambda v: self.goal(v)["id"] != self.goal(first)["id"])
        self.mutate("resume", status)
        waiting = self.wait(lambda v: (
            self.goal(v)["state"] == "blocked"
            and self.goal(v).get("reason") == "waiting_workers"
            and len(v["status"]["workers"]) == 1
            and v["status"]["workers"][0].get("goal", {}).get("turns_started") == 1
            and bool(self.records(worker_record))
        ))
        assert waiting["status"]["workers"][0]["response_tokens"] == 2048
        self.save_screen("worker-screen.txt", lambda s: any(
            "author · worker-model · working" in line for line in s.splitlines()[-3:]
        ))
        self.mux("send-keys", "-t", "demo", "Escape")
        stopped = self.wait(lambda v: (
            self.goal(v)["state"] == "paused"
            and v["status"]["workers"][0]["process_stopped"] is True
        ))
        child = stopped["status"]["workers"][0]
        assert child["state"] == "canceled", child
        assert not Path("/proc", str(child["process_id"])).exists()
        assert stopped["status"]["session"]["id"] == session
        for field in ("budget", "turns_started", "reported_tokens"):
            assert self.goal(stopped)[field] == self.goal(waiting)[field]
        self.save_screen("stopped-screen.txt", lambda s: any(
            "author · worker-model · canceled" in line for line in s.splitlines()[-3:]
        ))
        self.wait(lambda _: len(self.records(record)) == 4)
        self.mutate("cancel", self.request("status"))
        canceled = self.wait(lambda v: self.goal(v)["state"] == "canceled")
        self.mutate("set_goal", canceled, text="Start another worker and collect its result", budget=budget)
        funded_worker = self.wait(lambda v: self.goal(v)["id"] != self.goal(canceled)["id"])
        self.mutate("resume", funded_worker)
        collected = self.wait(lambda v: self.goal(v)["state"] == "done")
        assert collected["status"]["session"]["id"] == session
        assert len(collected["status"]["workers"]) == 2
        assert any(w["state"] == "completed" and w["process_stopped"] for w in collected["status"]["workers"])
        self.save_screen("collected-screen.txt", lambda s: "collected-worker-result" in s and "Goal Done" in s)
        self.wait(lambda _: len(self.records(record)) == 7)
        result_prompt = json.loads(self.records(record)[6]["body"])
        assert "child result" in json.dumps(result_prompt["messages"])
        assert "untrusted data" in json.dumps(result_prompt["messages"])

        self.mux("send-keys", "-t", "demo", "-l", "/new")
        self.mux("send-keys", "-t", "demo", "Enter")
        rebound = self.wait(lambda v: (
            v["status"]["session"]["id"] != session and self.goal(v).get("reason") == "no_goal"
        ))
        assert self.goal(rebound)["revision"] > self.goal(stopped)["revision"]
        long_budget = {"wall_ms": 20000, "tokens": 2000000, "turns": 8}
        self.mutate("set_goal", rebound, text="Verify long project work. " + "x" * 14000,
                    budget=long_budget)
        funded = self.wait(lambda v: self.goal(v)["id"] != "unassigned")
        self.mutate("resume", funded)
        compacted = self.wait(lambda v: self.goal(v)["state"] == "done", timeout=18)
        assert self.goal(compacted)["turns_started"] == 3
        assert self.goal(compacted)["budget"] == long_budget
        self.save_screen("compacted-screen.txt", lambda s: (
            "live-compaction-final" in s and "Goal Done; turns 3" in s and "Compacted from" in s
        ))
        self.wait(lambda _: len(self.records(record)) == 14)
        long_records = self.records(record)[7:]
        kinds = [dict(r["headers"])["x-oxe-turn-kind"] for r in long_records]
        assert kinds == ["chat", "chat", "compaction", "prefill", "chat", "compaction", "prefill"], kinds
        total = sum(r["usage"]["prompt"] + r["usage"]["completion"] for r in long_records)
        assert self.goal(compacted)["reported_tokens"] == total
        long_session = compacted["status"]["session"]["id"]
        assert all(dict(r["headers"])["x-oxe-session"] == long_session for r in long_records)
        later = json.loads(long_records[4]["body"])
        assert "live-summary-marker" in json.dumps(later["messages"])
        time.sleep(0.1)
        assert len(self.records(record)) == 14, "stopped work triggered follow-on inference"
        self.mux("send-keys", "-t", "demo", "C-d")
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            dead = self.mux("display-message", "-p", "-t", "demo", "#{pane_dead}:#{pane_dead_status}").strip()
            if dead.startswith("1:"):
                assert dead == "1:0", dead
                break
            time.sleep(0.02)
        else:
            raise AssertionError("TUI did not exit after Ctrl-D")
        worker_requests = [json.loads(r["body"]) for r in self.records(worker_record)]
        assert worker_requests and all(r["max_tokens"] == 2048 for r in worker_requests)
        result = {
            "worker_response_tokens": 2048,
            "shared_session_steer": True, "idle_escape_paused_parent": True,
            "worker_footer_verified": True, "worker_canceled_and_reaped": True,
            "worker_result_collected": True, "tui_session_rebound": True, "compacted_goal_turns": 3,
            "compacted_goal_tokens": total, "sdk_kinds": kinds, "tui_exit": 0,
        }
        (self.root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result, indent=2))

    def close(self):
        self.mux("kill-server", check=False)
        for process in self.processes:
            if process.stdin:
                process.stdin.close()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        print(f"Isolated mock artifacts: {self.root}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path,
                        default=Path(__file__).resolve().parents[2] / "target" / "debug")
    args = parser.parse_args()
    demo = Demo(args.bin_dir)
    try:
        demo.run()
    except BaseException:
        (demo.root / "failure-screen.txt").write_text(demo.screen())
        raise
    finally:
        demo.close()


if __name__ == "__main__":
    main()
