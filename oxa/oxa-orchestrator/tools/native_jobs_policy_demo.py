#!/usr/bin/env python3
"""Verify the release-mode A2A native-job refusal through the real TUI, fakes only."""
import importlib.util
import json
from pathlib import Path
import shlex

source = Path(__file__).with_name('live_tui_demo.py')
spec = importlib.util.spec_from_file_location('a2a_live_demo', source)
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)


def run(demo):
    def call(identity, args):
        return {'tool_calls': [{'id': identity, 'name': 'bash', 'args': json.dumps(args)}],
                'finish': 'tool_calls'}

    url, record = demo.fake('policy', [
        call('start', {'command': 'echo escaped > escaped', 'background': True}),
        call('read', {'job': 'job-1', 'wait_s': 1}),
        call('foreground', {'command': 'echo foreground-ok > foreground'}),
        live.text_turn('done', 'Policy checked', ['Foreground command completed']),
    ])
    demo.env.update({
        'OXA_ENGINE_URL': url,
        'OXA_SESSION_CONTROL_DIRECTORY': str(demo.socket.parent),
        'OXA_BACKGROUND_TASKS': '1',
    })
    args = [str(demo.binaries / 'oxa'), '--provider', 'oxe', '--model', 'fake-model',
            '--permission-mode', 'workspace', '--offline', '--no-extensions',
            '--no-mcp', '--no-skills', '--no-context-files', '--no-prompt-templates',
            '--no-web-search', '--tools', 'bash,background_command', '--tui-mode', 'fullscreen']
    launch = shlex.join(['/usr/bin/env', '-i', *[f'{k}={v}' for k, v in demo.env.items()], *args])
    demo.mux('new-session', '-d', '-s', 'demo', '-x', '180', '-y', '45',
             '-c', str(demo.work), launch)
    status = demo.wait(lambda value: value['status']['session']['id'])
    session = status['status']['session']['id']
    demo.mutate('set_goal', status, text='Verify release background policy',
                budget={'wall_ms': 15000, 'tokens': 100000, 'turns': 4})
    status = demo.wait(lambda value: demo.goal(value)['id'] != 'unassigned')
    demo.mutate('resume', status)
    status = demo.wait(lambda value: demo.goal(value)['state'] == 'done')
    assert status['status']['session']['id'] == session
    assert not (demo.work / 'escaped').exists(), 'native detached start escaped A2A ownership'
    assert (demo.work / 'foreground').read_text().strip() == 'foreground-ok'
    requests = demo.records(record)
    tools = [message for message in json.loads(requests[-1]['body'])['messages']
             if message.get('role') == 'tool']
    assert sum('Native Bash background jobs are disabled' in message.get('content', '')
               for message in tools) == 2, 'native starts and reads must both be refused'
    result = {'native_start_refused': True, 'native_read_refused': True,
              'foreground_preserved': True, 'goal_state': demo.goal(status)['state']}
    (demo.root / 'policy-result.json').write_text(json.dumps(result, indent=2) + '\n')
    (demo.root / 'policy-screen.txt').write_text(demo.screen())
    print(json.dumps(result, indent=2))


def main():
    demo = live.Demo(source.parents[2] / 'target' / 'debug')
    try:
        run(demo)
    finally:
        demo.close()


if __name__ == '__main__':
    main()
