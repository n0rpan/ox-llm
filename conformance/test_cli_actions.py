"""CLI dispatch against bounded loopback metadata/control peers, without a model."""
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from harness import ROOT, PRIMARY, SECONDARY, isolated_env
from test_cli_runtime import CANONICAL_HOME, invoke_case

GOLDENS = Path(__file__).with_name('goldens') / 'cli-actions.json'
FLASH = 'qwen3.8-flash-next-local'
CASES = ('chat', 'chat-no-desktop', 'chat-start-fails', 'images-busy', 'images-start-fails',
         'ornith-unavailable', 'flash-duplicate', 'flash-malformed', 'flash-oversized',
         'flash-select', 'select-refused', 'select-error-bounded', 'plan-ready', 'plan-no-agent',
         'plan-directory')


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def answer(self, code, value):
        raw = json.dumps(value).encode()
        self.send_response(code)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        self.server.requests.append({'method': 'GET', 'path': self.path})
        aliases = [PRIMARY, SECONDARY, FLASH]
        if self.server.case == 'ornith-unavailable':
            aliases.remove(PRIMARY)
        if self.server.case == 'flash-duplicate':
            aliases.append(FLASH)
        rows = [{'id': alias, 'object': 'model'} for alias in aliases]
        if self.server.case == 'flash-malformed':
            rows.append('invalid row')
        value = {'object': 'list', 'data': rows}
        if self.server.case == 'flash-oversized':
            value['padding'] = 'x' * 66000
        self.answer(200, value)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        self.server.requests.append({'method': 'POST', 'path': self.path, 'body': body})
        if self.server.case in ('images-busy', 'select-refused'):
            self.answer(409, {'error': 'Synthetic request is running.'})
        elif self.server.case == 'select-error-bounded':
            self.answer(503, {'error': {'message': '  ' + 'synthetic ' * 60 + '  '}})
        else:
            self.answer(200, {'state': 'unloaded' if self.path.endswith('unload') else 'ready'})


def capture(name):
    home = CANONICAL_HOME
    environment = isolated_env(home)
    binary_dir = home / 'bin'
    binary_dir.mkdir()
    peer = (ROOT / 'conformance/fixtures/host_command.py').read_text().split('\n', 1)[1]
    for command in ('systemctl', 'notify-send', 'omarchy', 'oxa'):
        if name == 'chat-no-desktop' and command == 'omarchy' or name == 'plan-no-agent' and command == 'oxa':
            continue
        path = binary_dir / command
        path.write_text('#!' + sys.executable + '\n' + peer)
        path.chmod(0o700)
    environment.update({'PATH': str(binary_dir), 'OXC_LAUNCH_METADATA': '1',
                        'OXC_STATEFUL_SYSTEMCTL': '1'})
    if name.endswith('start-fails'):
        (home / 'mock-fail-once').write_text('start')
    if name == 'plan-directory':
        environment['LOCAL_AI_CODE_DIRECTORY'] = str(home / 'synthetic project')
    executable = home / 'oxc'
    executable.symlink_to(ROOT / 'lab/src/snapdragon_ai_lab/runtime/local-ai')
    command = shlex.split(os.environ['OXC_COMMAND']) if 'OXC_COMMAND' in os.environ else [sys.executable, str(executable)]
    action = ('images' if name.startswith('images') else 'plan' if name.startswith('plan')
              else 'flash' if name.startswith('flash') else 'ornith' if name.startswith('ornith')
              else 'moe' if name.startswith('select') else 'chat')
    server = ThreadingHTTPServer(('127.0.0.1', 8092), Handler)
    server.case, server.requests = name, []
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    try:
        result = subprocess.run([*command, action], cwd=home, env=environment,
                                stdin=subprocess.DEVNULL, capture_output=True, timeout=10, umask=0o027)
        commands = home / 'host-commands.jsonl'
        detached = name in ('chat', 'images-busy', 'plan-ready', 'plan-directory')
        deadline = time.monotonic() + 3
        while True:
            rows = [json.loads(line) for line in commands.read_text().splitlines()] if commands.exists() else []
            if not detached or any(row['command'] == 'omarchy' for row in rows):
                break
            if time.monotonic() >= deadline:
                raise AssertionError('Desktop peer was not invoked')
            time.sleep(.02)
        output = {'exit': result.returncode, 'stdout': result.stdout.decode(), 'stderr': result.stderr.decode(),
                  'host_commands': rows, 'requests': server.requests}
        return json.loads(json.dumps(output).replace(str(home), '<HOME>'))
    finally:
        server.shutdown()
        server.server_close()
        worker.join()


class CliActionsContract(unittest.TestCase):
    def test_cli_action_goldens(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(invoke_case(name, __file__), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_COMMAND' in os.environ:
            raise SystemExit('Goldens must be recorded from the Python reference')
        GOLDENS.write_text(json.dumps({name: invoke_case(name, __file__) for name in CASES}, indent=2, sort_keys=True) + '\n')
    elif sys.argv[1:2] == ['--case']:
        print(json.dumps(capture(sys.argv[2])))
    else:
        unittest.main()
