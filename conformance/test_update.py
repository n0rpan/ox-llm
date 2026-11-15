"""Public CLI staged update/rollback against loopback and stateful systemd peers."""
import hashlib
import importlib.util
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
        previous_umask = os.umask(0o022)
        self.addCleanup(os.umask, previous_umask)
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
                         'OXC_UPDATE_READY_SECONDS': '0.1', 'OXC_UPDATE_CHAT_SECONDS': '2',
                         'OXC_STAGE_ROOTS': str(self.home / 'staged')})
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
                    if getattr(test, 'chat_hook', None):
                        test.chat_hook()
                    self.reply(test.chat_status, {'choices': [{'message': {'content': 'ready'}, 'finish_reason': 'stop'}]})
                else:
                    self.reply(200, {})

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.close_server)
        self.env['OXC_CHAT_URL'] = 'http://127.0.0.1:' + str(self.server.server_port)
        executable = self.home / 'oxc'
        executable.symlink_to(ROOT / 'oxc/src/oxc/runtime/oxc')
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
            shutil.copyfile(ROOT / 'oxc/tools' / tool, self.stage / 'tools' / tool)
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

    def reseal(self):
        files = {path.relative_to(self.stage).as_posix(): hashlib.sha256(path.read_bytes()).hexdigest()
                 for path in self.stage.rglob('*') if path.is_file() and path.name != 'SEAL.json'
                 and '__pycache__' not in path.parts}
        (self.stage / 'SEAL.json').write_text(json.dumps({'files': files}))

    def render_binding(self, paths=None, old=None, new=None, services=None, service_state=None):
        # Fixture generation uses the shipped stager's data renderer. Assertions
        # still exercise the public CLI and its real file/service transaction.
        source = ROOT / 'oxc/tools/stage_rollback_guard.py'
        spec = importlib.util.spec_from_file_location('conformance_stage_guard', source)
        guard = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(guard)
        paths, old, new = paths or self.paths, old or self.old, new or self.new
        names = {key: 'runtime.json' if key == 'runtime' else key for key in paths}
        script = guard.render_rollback(paths, new, old, staged_files=names,
                                       modes={key: 0o600 for key in paths if old[key] is not None},
                                       services=services, service_state=service_state)
        (self.stage / 'rollback/rollback.py').write_bytes(script)
        manifest = json.loads((self.stage / 'manifest.json').read_text())
        manifest.update(guard.manifest_binding(paths, new, old, names,
                        modes={key: 0o600 for key in paths if old[key] is not None}))
        (self.stage / 'manifest.json').write_text(json.dumps(manifest))
        self.reseal()

    def test_check_then_install_then_explicit_rollback(self):
        self.cli(['update', self.stage, '--check'], 0)
        self.assertEqual(self.contents(), self.old)
        self.assertFalse(any('stop' in args for args in self.commands()))
        result = self.cli(['update', self.stage], 0)
        self.assertIn('Updated to synthetic-release.', result.stdout)
        self.assertEqual(self.contents(), self.new)
        saved_services = self.stage / 'service-state.json'
        services = json.loads(saved_services.read_text())
        self.assertEqual(stat.S_IMODE(saved_services.stat().st_mode), 0o400)
        self.assertEqual(services['states'], {'oxc-chat.service': 'active', 'prompt-studio-app.service': 'active'})
        self.assertEqual(services['active'], {'oxc-chat.service': True, 'prompt-studio-app.service': True})
        self.assertIsNone(services['chat_release'])
        self.assertIn(['--user', 'stop', 'oxc-chat.service', 'prompt-studio-app.service'], self.commands())
        for path in self.paths.values():
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        chats = [call for call in self.calls if call['path'] == '/v1/chat/completions']
        self.assertEqual(len(chats), 1)
        self.assertEqual(chats[0]['origin'], 'lab')
        self.assertEqual(chats[0]['body']['max_tokens'], 8)
        self.assertTrue(any(call['path'] == '/_local/unload' for call in self.calls))
        state = self.home / 'state/oxc/update/state.json'
        self.assertEqual(stat.S_IMODE(state.stat().st_mode), 0o600)
        installed = json.loads(state.read_text())
        self.assertEqual(installed['current']['stage'], str(self.stage))
        self.assertEqual(installed['current']['since'], installed['history'][-1]['time'])
        self.assertEqual(installed['history'][-1]['action'], 'update')
        self.assertEqual(installed['history'][-1]['result'], 'installed')
        self.assertEqual(installed['history'][-1]['agent_identity'], 'absent')
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

    def test_install_and_rollback_receipts(self):
        self.cli(['update', self.stage, '--no-chat'], 0)
        receipts = list((self.stage / 'receipts').glob('install-*.json'))
        self.assertEqual(len(receipts), 1)
        installed = json.loads(receipts[0].read_text())
        self.assertEqual(installed['action'], 'install')
        self.assertEqual(installed['stage'], self.stage.name)
        self.assertEqual(installed['installed'], ['runtime', 'unit'])
        self.assertEqual(stat.S_IMODE(receipts[0].stat().st_mode), 0o644)
        for key in self.paths:
            self.assertEqual(installed['sha256'][key], {'preimage': hashlib.sha256(self.old[key]).hexdigest(),
                                                        'installed': hashlib.sha256(self.new[key]).hexdigest()})
        self.cli(['rollback'], 0)
        receipts = list((self.stage / 'rollback/receipts').glob('rollback-*.json'))
        self.assertEqual(len(receipts), 1)
        restored = json.loads(receipts[0].read_text())
        self.assertEqual(restored['action'], 'rollback')
        self.assertEqual(restored['restored'], ['runtime', 'unit'])
        self.assertEqual(restored['started'], ['oxc-chat.service'])
        self.assertEqual(restored['start_basis'], 'all bound services')
        self.assertEqual(stat.S_IMODE(receipts[0].stat().st_mode), 0o644)

    def test_created_and_removed_files_round_trip(self):
        paths = {**self.paths, 'created': self.home / 'live/created', 'removed': self.home / 'live/removed'}
        old = {**self.old, 'created': None, 'removed': b'synthetic retired file\n'}
        new = {**self.new, 'created': b'synthetic new file\n', 'removed': None}
        paths['removed'].write_bytes(old['removed'])
        paths['removed'].chmod(0o600)
        (self.stage / 'rollback/removed.original').write_bytes(old['removed'])
        (self.stage / 'created').write_bytes(new['created'])
        self.render_binding(paths, old, new)
        manifest = json.loads((self.stage / 'manifest.json').read_text())
        manifest['installed_mode'] = {'created': '0700'}
        (self.stage / 'manifest.json').write_text(json.dumps(manifest))
        self.reseal()
        self.cli(['update', self.stage, '--no-chat'], 0)
        self.assertEqual(paths['created'].read_bytes(), new['created'])
        self.assertEqual(stat.S_IMODE(paths['created'].stat().st_mode), 0o700)
        self.assertFalse(paths['removed'].exists())
        self.cli(['rollback'], 0)
        self.assertFalse(paths['created'].exists())
        self.assertEqual(paths['removed'].read_bytes(), old['removed'])
        self.assertEqual(stat.S_IMODE(paths['removed'].stat().st_mode), 0o600)

    def test_owner_edit_after_install_refuses_rollback(self):
        self.cli(['update', self.stage, '--no-chat'], 0)
        self.paths['runtime'].write_bytes(b'synthetic owner edit after install\n')
        calls = len(self.commands())
        self.cli(['rollback', '--check'], 2)
        self.assertEqual(len(self.commands()), calls)
        self.assertEqual(self.paths['runtime'].read_bytes(), b'synthetic owner edit after install\n')

    def test_resealed_changed_preimage_refuses_rollback_binding(self):
        (self.stage / 'rollback/runtime.original').write_bytes(b'synthetic changed saved copy\n')
        self.reseal()
        self.cli(['update', self.stage, '--check'], 2)
        self.assertEqual(self.contents(), self.old)
        self.assertEqual(self.commands(), [])

    def test_bound_service_state_is_validated_before_rollback(self):
        self.render_binding(services=['oxc-chat.service', 'prompt-studio-app.service'], service_state='service-state.json')
        self.cli(['update', self.stage, '--no-chat'], 0)
        service_state = self.stage / 'service-state.json'
        service_state.chmod(0o600)
        service_state.write_text('{"active": {"oxc-chat.service": "invalid"}}')
        service_state.chmod(0o400)
        calls = len(self.commands())
        self.cli(['rollback', '--check'], 2)
        self.assertEqual(len(self.commands()), calls)
        self.assertEqual(self.contents(), self.new)

    def test_wrong_controller_release_is_not_readiness(self):
        manifest = json.loads((self.stage / 'manifest.json').read_text())
        manifest['release_dir'] = str(self.stage / 'expected-controller-release')
        (self.stage / 'manifest.json').write_text(json.dumps(manifest))
        self.reseal()
        pid = self.home / 'controller-pid.txt'
        pid.write_text('MainPID=' + str(os.getpid()) + '\n')
        self.env['OXC_SYSTEMCTL_SHOW_FILE'] = str(pid)
        self.cli(['update', self.stage, '--no-chat'], 1)
        self.assertEqual(self.contents(), self.old)

    def test_unknown_controller_release_keeps_recovery_not_ready(self):
        manifest = json.loads((self.stage / 'manifest.json').read_text())
        manifest['release_dir'] = str(self.stage / 'unknown-controller-release')
        (self.stage / 'manifest.json').write_text(json.dumps(manifest))
        self.reseal()
        self.cli(['update', self.stage, '--no-chat'], 3)
        self.assertEqual(self.contents(), self.old)
        state = json.loads((self.home / 'state/oxc/update/state.json').read_text())
        self.assertFalse(state['history'][-1]['ready'])

    def test_owner_edit_during_failed_chat_refuses_automatic_rollback(self):
        self.chat_status = 503
        self.chat_hook = lambda: self.paths['runtime'].write_bytes(b'synthetic concurrent owner edit\n')
        self.cli(['update', self.stage], 3)
        self.assertEqual(self.paths['runtime'].read_bytes(), b'synthetic concurrent owner edit\n')
        self.assertEqual(self.paths['unit'].read_bytes(), self.new['unit'])

    def test_agent_identity_refresh_uses_the_target_home_and_offline_fallback(self):
        target = self.home / 'identity-home'
        agent = target / '.local/bin/oxa'
        agent.parent.mkdir(parents=True)
        log = self.home / 'identity-calls.jsonl'
        agent.write_text('#!' + sys.executable + '\nimport os,json,sys\n'
                         'with open(os.environ["FAKE_IDENTITY_LOG"],"a") as out:\n'
                         ' out.write(json.dumps({"args":sys.argv[1:],"home":os.environ["HOME"],'
                         '"url":os.environ["OXA_ENGINE_URL"]})+"\\n")\n'
                         'raise SystemExit(0 if "--offline" in sys.argv else 1)\n')
        agent.chmod(0o700)
        self.env['FAKE_IDENTITY_LOG'] = str(log)
        manifest = json.loads((self.stage / 'manifest.json').read_text())
        manifest['home'] = str(target)
        (self.stage / 'manifest.json').write_text(json.dumps(manifest))
        self.reseal()
        self.cli(['update', self.stage, '--no-chat'], 0)
        calls = [json.loads(line) for line in log.read_text().splitlines()]
        self.assertEqual(calls, [{'args': ['identity', '--refresh'], 'home': str(target), 'url': self.env['OXC_CHAT_URL']},
                                 {'args': ['identity', '--refresh', '--offline'], 'home': str(target), 'url': self.env['OXC_CHAT_URL']}])
        state = json.loads((self.home / 'state/oxc/update/state.json').read_text())
        self.assertEqual(state['history'][-1]['agent_identity'], 'refreshed-offline')


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
