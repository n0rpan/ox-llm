#!/usr/bin/env python3
"""Real CLI/TUI regression for owned background work at authority loss; mocks only."""
import argparse
import json
import importlib.util
from pathlib import Path
import shlex
import time

spec = importlib.util.spec_from_file_location(
    "a2a_live_demo", Path(__file__).with_name("live_tui_demo.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
Demo = module.Demo


def run(binaries, mode):
    demo = Demo(binaries)
    try:
        command = "sleep 1; printf leaked > delayed-write.txt"
        reply = ({"state": "continue", "summary": "task still running"}
                 if mode == "settlement" else
                 {"state": "blocked", "reason": "waiting_background_tasks"})
        url, record = demo.fake("main", [
            {"tool_calls": [{"id": "owned-task", "name": "background_command",
                             "args": json.dumps({"operation": "start", "command": command,
                                                 "timeout_s": 5})}], "finish": "tool_calls"},
            {"text": json.dumps(reply)},
        ])
        demo.env.update({"OXA_ENGINE_URL": url, "OXA_BACKGROUND_TASKS": "1",
                         "OXA_SESSION_CONTROL_DIRECTORY": str(demo.socket.parent)})
        args = [str(demo.binaries / "oxa"), "--model", "fake-model", "--offline",
                "--permission-mode", "workspace", "--no-extensions", "--no-mcp",
                "--no-skills", "--no-context-files", "--no-prompt-templates",
                "--no-web-search", "--tools", "background_command",
                "--tui-mode", "fullscreen"]
        launch = shlex.join(["/usr/bin/env", "-i",
                             *[f"{k}={v}" for k, v in demo.env.items()], *args])
        demo.mux("new-session", "-d", "-s", "demo", "-x", "180", "-y", "45",
                 "-c", str(demo.work), launch)
        demo.mux("set-window-option", "-t", "demo", "remain-on-exit", "on")
        status = demo.wait(lambda v: v["status"]["session"]["id"])
        demo.mutate("set_goal", status, text="Await owned background work", budget={
            "wall_ms": 8000, "tokens": None, "turns": 4 if mode == "escape" else 1})
        status = demo.wait(lambda v: demo.goal(v)["id"] != "unassigned")
        demo.mutate("resume", status)
        status = demo.wait(lambda v: v["status"].get("tasks")
                           and v["status"]["session"]["busy"] is False)
        task = status["status"]["tasks"][0]
        pid = task["process_id"]
        if mode != "settlement":
            assert demo.goal(status)["state"] == "blocked", status
            assert demo.goal(status)["reason"] == "waiting_background_tasks", status
            assert task["state"] == "running" and not task["process_stopped"], status
            if mode == "admission":
                demo.mutate("resume", status)
            else:
                demo.save_screen("waiting-screen.txt", lambda screen:
                                 "waiting_background_tasks" in screen
                                 and "● READY" in "\n".join(screen.splitlines()[-3:]))
                demo.mux("send-keys", "-t", "demo", "Escape")
        expected = "paused" if mode == "escape" else "budget_exhausted"
        status = demo.wait(lambda v: demo.goal(v)["state"] == expected
                           and v["status"]["tasks"][0]["process_stopped"] is True)
        assert status["status"]["tasks"][0]["state"] == "canceled", status
        assert not Path(f"/proc/{pid}").exists(), "owned process must be reaped"
        time.sleep(1.1)
        assert not (demo.work / "delayed-write.txt").exists(), "late descendant write"
        chats = [r for r in demo.records(record) if json.loads(r["body"]).get("max_tokens", 0) > 0]
        assert len(chats) == 2, "revocation must not wake follow-on inference"
        demo.mux("send-keys", "-t", "demo", "C-d")
        deadline = time.monotonic() + 3
        while demo.mux("display-message", "-p", "-t", "demo", "#{pane_dead}").strip() != "1":
            assert time.monotonic() < deadline, "TUI exit deadline"
            time.sleep(0.02)
        assert demo.mux("display-message", "-p", "-t", "demo", "#{pane_dead_status}").strip() == "0"
        result = {"mode": mode, "goal": expected, "task": "canceled",
                  "process_stopped": True, "late_write": False, "requests": len(chats)}
        (demo.root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result))
    except BaseException:
        (demo.root / "failure-screen.txt").write_text(demo.screen())
        raise
    finally:
        demo.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path,
                        default=Path(__file__).resolve().parents[2] / "target" / "debug")
    args = parser.parse_args()
    for mode in ("settlement", "admission", "escape"):
        run(args.bin_dir, mode)


if __name__ == "__main__":
    main()
