"""Public CLI staged update/rollback against loopback and stateful systemd peers."""
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import unittest

from harness import ROOT, isolated_env

GOLDENS = Path(__file__).with_name('goldens') / 'update.json'
RECORD = False
RECORDED = {}


class UpdateContract(unittest.TestCase):
    def setUp(self):
        self.cli_index = 0
        self.expected = {} if RECORD else json.loads(GOLDENS.read_text())
        self.temp = tempfile.TemporaryDirectory(prefix='oxc-update-conformance-')
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        self.env = isolated_env(self.home)
        binary = self.home / 'bin'
        binary.mkdir()
        systemctl = binary / 'systemctl'
        source = (ROOT / 'conformance/fixtures/host_command.py').read_text().split('\n', 1)[1]
        systemctl.write_text('#!' + sys.executable + '\n' + source)
        systemctl.chmod(0o700)
        self.env.update({'PATH': str(binary) + ':/usr/bin:/bin', 'OXC_STATEFUL_SYSTEMCTL': '1',
                         'LOCAL_AI_UPDATE_READY_SECONDS': '0.1', 'LOCAL_AI_UPDATE_CHAT_SECONDS': '2',
                         'LOCAL_AI_STAGE_ROOTS': str(self.home / 'staged')})
        self.calls = []
        self.chat_status = 200
        test = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def reply(self, status, body):
                raw = json.dumps(body).encode()
                self.send_response(status)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            def do_GET(self):
                test.calls.append({'method': 'GET', 'path': self.path})
                self.reply(200, {'state': 'unloaded', 'model': 'synthetic-local'})

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers.get('Content-Length', '0'))))
                test.calls.append({'method': 'POST', 'path': self.path, 'body': body,
                                   'origin': self.headers.get('X-Local-AI-Origin')})
                if self.path == '/v1/chat/completions':
                    self.reply(test.chat_status, {'choices': [{'message': {'content': 'ready'}, 'finish_reason': 'stop'}]})
                else:
                    self.reply(200, {})

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.close_server)
        self.env['LOCAL_AI_CHAT_URL'] = 'http://127.0.0.1:' + str(self.server.server_port)
        executable = self.home / 'oxc'
        executable.symlink_to(ROOT / 'lab/src/snapdragon_ai_lab/runtime/local-ai')
        self.command = shlex.split(os.environ['OXC_COMMAND']) if 'OXC_COMMAND' in os.environ else [sys.executable, str(executable)]
        self.stage = self.home / 'staged/synthetic-release'
        (self.stage / 'rollback').mkdir(parents=True)
        (self.stage / 'tools').mkdir()
        live = self.home / 'live'
        live.mkdir()
        self.paths = {'runtime': live / 'runtime.json', 'unit': live / 'unit'}
        self.old = {'runtime': b'{"version":1}\n', 'unit': b'synthetic previous unit\n'}
        self.new = {'runtime': b'{"version":2}\n', 'unit': b'synthetic next unit\n'}
        staged_names = {'runtime': 'runtime.json', 'unit': 'unit'}
        for key, path in self.paths.items():
            path.write_bytes(self.old[key])
            path.chmod(0o600)
            (self.stage / staged_names[key]).write_bytes(self.new[key])
            (self.stage / 'rollback' / (key + '.original')).write_bytes(self.old[key])
        manifest = {'release': 'synthetic-release', 'paths': {key: str(path) for key, path in self.paths.items()},
                    'staged_files': staged_names,
                    'preimage_sha256': {key: hashlib.sha256(value).hexdigest() for key, value in self.old.items()},
                    'preimage_mode': {key: '0600' for key in self.paths}}
        (self.stage / 'manifest.json').write_text(json.dumps(manifest))
        for tool in ('install_staged_release.py', 'stage_rollback_guard.py'):
            shutil.copyfile(ROOT / 'lab/tools' / tool, self.stage / 'tools' / tool)
        binding = [sys.executable, str(self.stage / 'tools/stage_rollback_guard.py'), 'bind', str(self.stage)]
        for key, path in self.paths.items():
            binding += ['--path', key + '=' + str(path)]
        result = subprocess.run(binding, env=self.env, cwd=self.home, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        files = {path.relative_to(self.stage).as_posix(): hashlib.sha256(path.read_bytes()).hexdigest()
                 for path in self.stage.rglob('*') if path.is_file()}
        (self.stage / 'SEAL.json').write_text(json.dumps({'files': files}))

    def close_server(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)

    def cli(self, args, expected):
        result = subprocess.run(self.command + [str(arg) for arg in args], cwd=self.home, env=self.env,
                                capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
        key = self._testMethodName + '/' + str(self.cli_index)
        self.cli_index += 1
        # Only the measured chat duration and temporary HOME vary. All progress
        # messages, refusal reasons and stdout/stderr placement remain exact.
        stdout = re.sub(r'Chat answered in [0-9]+(?:\.[0-9]+)? s\.',
                        'Chat answered in <seconds> s.', result.stdout)
        snapshot = {'exit': result.returncode, 'stdout': stdout.replace(str(self.home), '<HOME>'),
                    'stderr': result.stderr.replace(str(self.home), '<HOME>')}
        if RECORD:
            RECORDED[key] = snapshot
        else:
            self.assertIn(key, self.expected)
            self.assertEqual(snapshot, self.expected[key])
        return result

    def contents(self):
        return {key: path.read_bytes() for key, path in self.paths.items()}

    def commands(self):
        path = self.home / 'host-commands.jsonl'
        return [json.loads(line)['args'] for line in path.read_text().splitlines()] if path.exists() else []

    def test_check_then_install_then_explicit_rollback(self):
        self.cli(['update', self.stage, '--check'], 0)
        self.assertEqual(self.contents(), self.old)
        self.assertFalse(any('stop' in args for args in self.commands()))
        result = self.cli(['update', self.stage], 0)
        self.assertIn('Updated to synthetic-release.', result.stdout)
        self.assertEqual(self.contents(), self.new)
        self.assertIn(['--user', 'stop', 'local-ai-chat.service', 'prompt-studio-app.service'], self.commands())
        for path in self.paths.values():
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        chats = [call for call in self.calls if call['path'] == '/v1/chat/completions']
        self.assertEqual(len(chats), 1)
        self.assertEqual(chats[0]['origin'], 'lab')
        self.assertEqual(chats[0]['body']['max_tokens'], 8)
        self.assertTrue(any(call['path'] == '/_local/unload' for call in self.calls))
        state = self.home / 'state/local-ai/update/state.json'
        self.assertEqual(stat.S_IMODE(state.stat().st_mode), 0o600)
        self.cli(['rollback', '--check'], 0)
        self.assertEqual(self.contents(), self.new)
        result = self.cli(['rollback'], 0)
        self.assertIn('The previous version is back and running.', result.stdout)
        self.assertEqual(self.contents(), self.old)

    def test_changed_seal_is_refused_without_stopping_services(self):
        (self.stage / 'runtime.json').write_bytes(b'synthetic changed payload\n')
        self.cli(['update', self.stage], 2)
        self.assertEqual(self.contents(), self.old)
        self.assertFalse(any('stop' in args for args in self.commands()))

    def test_live_drift_is_refused_without_overwriting_owner_data(self):
        self.paths['runtime'].write_bytes(b'synthetic owner change\n')
        self.cli(['update', self.stage], 2)
        self.assertEqual(self.paths['runtime'].read_bytes(), b'synthetic owner change\n')
        self.assertEqual(self.paths['unit'].read_bytes(), self.old['unit'])
        self.assertFalse(any('stop' in args for args in self.commands()))

    def test_unproven_service_stop_refuses_install(self):
        (self.home / 'mock-stuck-stop').touch()
        self.cli(['update', self.stage], 2)
        self.assertEqual(self.contents(), self.old)
        self.assertFalse((self.stage / 'service-state.json').exists())

    def test_failed_chat_automatically_restores_previous_files(self):
        self.chat_status = 503
        result = self.cli(['update', self.stage], 1)
        self.assertIn('previous version', result.stdout)
        self.assertEqual(self.contents(), self.old)
        for path in self.paths.values():
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)

    def test_failed_start_automatically_restores_previous_files(self):
        (self.home / 'mock-fail-once').write_text('start\n')
        self.cli(['update', self.stage, '--no-chat'], 1)
        self.assertEqual(self.contents(), self.old)


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_COMMAND' in os.environ:
            raise SystemExit('Goldens must be recorded from the Python reference')
        RECORD = True
        program = unittest.main(argv=[sys.argv[0]], exit=False)
        if not program.result.wasSuccessful():
            raise SystemExit(1)
        GOLDENS.write_text(json.dumps(RECORDED, indent=2, sort_keys=True) + '\n')
    else:
        unittest.main()
