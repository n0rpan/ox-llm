#!/usr/bin/env python3
"""Check native Bash job lifetime through the real shared-session TUI.

Uses only an owned loopback fake, a private HOME and a bounded heartbeat command.
Exits nonzero if a goal declares DONE while its native job still runs. This is a
release-integration diagnostic, not a replacement for the orchestrator suite.
"""
import argparse
import importlib.util
import json
from pathlib import Path
import shlex
import time

spec = importlib.util.spec_from_file_location('a2a_live_demo', Path(__file__).with_name('live_tui_demo.py'))
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)
Demo, text_turn = live.Demo, live.text_turn


def run(demo):
    # Bound the command even if the TUI crashes before its shutdown cleanup.
    command = 'for i in $(seq 1 80); do echo tick >> native-heartbeat; sleep 0.1; done'
    tool = {'id': 'native-job', 'name': 'bash', 'args': json.dumps({
        'command': command, 'background': True,
    })}
    url, record = demo.fake('native-job', [
        {'tool_calls': [tool], 'finish': 'tool_calls'},
        text_turn('done', 'Native job started', ['Bash returned a job ID']),
    ], '--hold-headers', '100')
    demo.env.update({
        'OXA_ENGINE_URL': url,
        'OXA_SESSION_CONTROL_DIRECTORY': str(demo.socket.parent),
    })
    args = [
        str(demo.binaries / 'oxa'), '--provider', 'oxe', '--model', 'fake-model',
        '--permission-mode', 'workspace', '--offline', '--no-extensions',
        '--no-mcp', '--no-skills', '--no-context-files', '--no-prompt-templates',
        '--no-web-search', '--tools', 'bash', '--tui-mode', 'fullscreen',
    ]
    launch = shlex.join([
        '/usr/bin/env', '-i', *[f'{k}={v}' for k, v in demo.env.items()], *args,
    ])
    demo.mux('new-session', '-d', '-s', 'demo', '-x', '180', '-y', '45',
             '-c', str(demo.work), launch)
    status = demo.wait(lambda value: value['status']['session']['id'])
    session = status['status']['session']['id']
    demo.mutate('set_goal', status, text='Start the native background command',
                budget={'wall_ms': 12000, 'tokens': 100000, 'turns': 4})
    status = demo.wait(lambda value: demo.goal(value)['id'] != 'unassigned')
    demo.mutate('resume', status)
    status = demo.wait(lambda value: demo.goal(value)['state'] in ('done', 'blocked'))
    heartbeat = demo.work / 'native-heartbeat'
    before = heartbeat.stat().st_size if heartbeat.exists() else 0
    time.sleep(0.5)
    after = heartbeat.stat().st_size if heartbeat.exists() else 0
    # A refused/failed Bash start must never count as a passing lifecycle check.
    assert before > 0 and len(demo.records(record)) >= 2, 'native job did not run'
    assert status['status']['session']['id'] == session, 'session changed'
    result = {
        'goal_state': demo.goal(status)['state'],
        'heartbeat_before': before, 'heartbeat_after': after,
        'live_job_after_done': demo.goal(status)['state'] == 'done' and after > before,
    }
    (demo.root / 'native-result.json').write_text(json.dumps(result, indent=2) + '\n')
    (demo.root / 'native-status.json').write_text(json.dumps(status, indent=2) + '\n')
    (demo.root / 'native-screen.txt').write_text(demo.screen())
    print(json.dumps(result, indent=2))
    assert not result['live_job_after_done'], 'goal declared DONE while native Bash job kept running'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=Path,
                        default=Path(__file__).resolve().parents[2] / 'target' / 'debug')
    args = parser.parse_args()
    demo = Demo(args.bin_dir)
    try:
        run(demo)
    finally:
        demo.close()


if __name__ == '__main__':
    main()
