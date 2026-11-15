"""Control CLI contracts against owned HTTP/system peers, with no engine fixture."""
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

from cli_harness import CANONICAL_HOME, invoke_case
from environment import ROOT, isolated_env, PYTHON

GOLDENS = Path(__file__).with_name('goldens') / 'cli-control.json'
CASES = {
    'load': (['load'], 'ok', 0),
    'load-start-fails': (['load'], 'ok', 1),
    'load-refused': (['load'], 'refused', 1),
    'unload': (['unload'], 'ok', 0),
    'unload-refused': (['unload'], 'refused', 1),
    'unload-reason-trimmed': (['unload'], 'reason', 1),
    'unload-reason-bounded': (['unload'], 'long-reason', 1),
    'unload-invalid-json': (['unload'], 'invalid', 1),
    'unload-nonobject-json': (['unload'], 'list', 1),
    'unload-oversized': (['unload'], 'large', 1),
    'unload-bounded-error-prefix': (['unload'], 'large-error', 1),
    'unload-nonfinite-json': (['unload'], 'nonfinite', 0),
    'unload-nonfinite-error-json': (['unload'], 'nonfinite-error', 1),
    'unload-trickled-body': (['unload'], 'trickle', 0),
    'unload-no-notifier': (['unload'], 'refused', 1),
    'forget-all': (['harness-forget'], 'ok', 0),
    'forget-one': (['harness-forget', '--harness', 'synthetic'], 'ok', 0),
    'forget-invalid': (['harness-forget', '--harness', '../invalid'], 'ok', 2),
    'forget-refused': (['harness-forget'], 'refused', 1),
    'prepare': (['harness-prepare', '--harness', 'synthetic'], 'ok', 0),
    'prepare-missing': (['harness-prepare'], 'ok', 2),
    'prepare-invalid': (['harness-prepare', '--harness', '../invalid'], 'ok', 2),
    'prepare-start-fails': (['harness-prepare', '--harness', 'synthetic'], 'ok', 1),
    'prepare-refused': (['harness-prepare', '--harness', 'synthetic'], 'refused', 1),
    'flash-cache': (['flash-cache', '--pref', '18'], 'ok', 0),
    'flash-cache-missing': (['flash-cache'], 'ok', 2),
    'flash-cache-refused': (['flash-cache', '--pref', 'auto'], 'refused', 1),
    'clear-cache': (['clear-cache'], 'ok', 0),
    'clear-cache-refused': (['clear-cache'], 'refused', 1),
    'storage-delete': (['storage-delete', '--id', 'rust-prefix-' + 'a' * 16], 'ok', 0),
    'storage-delete-invalid': (['storage-delete', '--id', 'invalid'], 'ok', 2),
    'storage-delete-start-fails': (['storage-delete', '--id', 'rust-prefix-' + 'a' * 16], 'ok', 1),
    'storage-delete-refused': (['storage-delete', '--id', 'rust-prefix-' + 'a' * 16], 'refused', 1),
}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        self.server.requests.append({'method': 'POST', 'path': self.path, 'body': body})
        kind = self.server.kind
        code, raw = 200, b'{}'
        if kind == 'refused':
            code, raw = 409, json.dumps({'error': 'Synthetic job is running.'}).encode()
        elif kind == 'reason':
            code, raw = 503, json.dumps({'error': {'message': '  Synthetic refusal.  '}}).encode()
        elif kind == 'long-reason':
            code, raw = 503, json.dumps({'error': '  ' + '\u00e9' * 350 + '  '}).encode()
        elif kind == 'invalid':
            raw = b'{invalid'
        elif kind == 'list':
            raw = b'[]'
        elif kind == 'large':
            raw = json.dumps({'padding': 'x' * 65537}).encode()
        elif kind == 'large-error':
            code, raw = 503, json.dumps({'error': 'Synthetic bounded refusal.'}).encode().ljust(65536, b' ') + b'extra'
        elif kind == 'nonfinite':
            raw = b'{"count": NaN}'
        elif kind == 'nonfinite-error':
            code, raw = 503, b'{"error": "Synthetic nonfinite refusal.", "count": Infinity}'
        elif kind == 'trickle':
            raw = b'{ }'
        self.send_response(code)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(raw)))
        self.end_headers()
        try:
            if kind == 'trickle':
                # The unchanged control deadline is 15 s per stalled read, not
                # a 15 s total response deadline. Each byte arrives within it.
                for index, byte in enumerate(raw):
                    if index:
                        time.sleep(8)
                    self.wfile.write(bytes([byte]))
                    self.wfile.flush()
            else:
                self.wfile.write(raw)
        except OSError:
            pass  # The refusal/timeout caller may already have closed its socket.


def capture(name):
    arguments, kind, expected_exit = CASES[name]
    home = CANONICAL_HOME
    env = isolated_env(home)
    binary_dir = home / 'bin'
    binary_dir.mkdir()
    peer = (ROOT / 'conformance/fixtures/host_command.py').read_text().split('\n', 1)[1]
    for command in ('systemctl', 'notify-send'):
        if name == 'unload-no-notifier' and command == 'notify-send':
            continue
        path = binary_dir / command
        path.write_text('#!' + PYTHON + '\n' + peer)
        path.chmod(0o700)
    env.update(PATH=str(binary_dir), OXC_STATEFUL_SYSTEMCTL='1')
    if name.endswith('start-fails'):
        (home / 'mock-fail-once').write_text('start')
    executable = home / 'oxc'
    executable.symlink_to(ROOT / 'oxc/src/oxc/runtime/oxc')
    command = shlex.split(os.environ['OXC_COMMAND']) if 'OXC_COMMAND' in os.environ else [PYTHON, str(executable)]
    server = ThreadingHTTPServer(('127.0.0.1', 8092), Handler)
    server.daemon_threads = True
    server.kind, server.requests = kind, []
    thread = threading.Thread(target=server.serve_forever)
    thread.start()
    try:
        result = subprocess.run([*command, *arguments], env=env, cwd=home,
                                capture_output=True, text=True, timeout=22)
        if result.returncode != expected_exit:
            raise AssertionError(f'{name}: exit {result.returncode}, expected {expected_exit}: {result.stderr}')
        calls = home / 'host-commands.jsonl'
        return {'exit': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr,
                'requests': server.requests,
                'host_commands': [json.loads(row) for row in calls.read_text().splitlines()] if calls.exists() else []}
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


class CliControlContract(unittest.TestCase):
    def test_control_peers_without_a_local_engine(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(invoke_case(name, script=__file__, timeout=27), expected[name])


if __name__ == '__main__':
    if len(sys.argv) == 3 and sys.argv[1] == '--case':
        print(json.dumps(capture(sys.argv[2])))
    elif sys.argv[1:] == ['--record-python']:
        if 'OXC_COMMAND' in os.environ:
            raise SystemExit('Record goldens only from the Python reference')
        GOLDENS.write_text(json.dumps({name: invoke_case(name, script=__file__, timeout=27) for name in CASES},
                                     indent=2, sort_keys=True) + '\n')
    else:
        unittest.main()
