"""Runtime CLI contracts in a fresh network namespace with synthetic system peers."""
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time
import unittest

from harness import Daemon, ROOT, isolated_env

GOLDENS = Path(__file__).with_name('goldens') / 'cli-runtime.json'
CANONICAL_HOME = Path('/home/oxc-conformance')  # synthetic namespace mount, never a real account
CASES = {
    'panel': (['panel'], 0),
    'panel-output': (['panel'], 37),
    'status': (['status'], 0),
    'status-json': (['status', '--json'], 0),
    'ledger': (['ledger'], 0),
    'ledger-json': (['ledger', '--json'], 0),
    'storage': (['storage'], 0),
    'storage-json': (['storage', '--json'], 0),
    'chat': (['chat'], 0),
    'code': (['code'], 0),
    'plan-unavailable': (['plan'], 1),
    'images': (['images'], 0),
    'load': (['load'], 0),
    'unload': (['unload'], 0),
    'moe': (['moe'], 0),
    'ornith': (['ornith'], 0),
    'flash-unavailable': (['flash'], 1),
    'clear-cache': (['clear-cache'], 0),
    'storage-delete-invalid': (['storage-delete', '--id', 'invalid'], 2),
    'harness-prepare-invalid': (['harness-prepare', '--harness', '../invalid'], 2),
    'harness-prepare-missing': (['harness-prepare', '--harness', 'synthetic'], 1),
    'harness-forget': (['harness-forget'], 0),
    'harness-forget-one': (['harness-forget', '--harness', 'synthetic'], 0),
    'flash-cache-invalid': (['flash-cache'], 2),
    'flash-cache-unavailable': (['flash-cache', '--pref', 'auto'], 1),
    'cache': (['cache'], 0),
    'cache-json': (['cache', '--json'], 0),
    'cache-budget-off': (['cache', 'budget', 'off'], 0),
    'cache-budget-four': (['cache', 'budget', '4'], 0),
    'cache-budget-fractional': (['cache', 'budget', '4.1234567'], 0),
    'cache-budget-invalid': (['cache', 'budget', '3'], 2),
    'cache-clear': (['cache', 'clear', '--all'], 0),
    'cache-clear-session': (['cache', 'clear', '--session', 'synthetic-session'], 0),
    'mode': (['mode', '--mode', 'deep'], 0),
    'mode-missing': (['mode'], 2),
    'share': (['share'], 0),
    'share-json': (['share', '--json'], 0),
    'share-off': (['share', 'off'], 0),
    'share-invalid': (['share', 'on', 'invalid'], 2),
    'digest-invalid-day': (['digest', '--day', 'invalid'], 2),
    'digest': (['digest', '--day', '2026-11-01'], 0),
    'digest-status': (['digest', '--status'], 0),
    'digest-soak': (['digest', '--soak', '--day', '2026-11-01'], 0),
    'update-check-no-stage': (['update', '--check'], 2),
    'rollback-check-no-stage': (['rollback', '--check'], 2),
    'retired-dense': (['dense'], 2),
    'uninstall-dry-run': (['uninstall'], 0),
    'uninstall-apply': (['uninstall', '--apply'], 0),
}


def capture_case(name):
    args, expected_exit = CASES[name]
    with Daemon(configured=True, port=8092, home=CANONICAL_HOME) as daemon:
        binary_dir = daemon.home / 'bin'
        binary_dir.mkdir()
        source = (ROOT / 'conformance/fixtures/host_command.py').read_text().split('\n', 1)[1]
        for command in ('systemctl', 'notify-send', 'omarchy', 'omarchy-shell', 'oxa', 'pgrep'):
            path = binary_dir / command
            path.write_text('#!' + sys.executable + '\n' + source)
            path.chmod(0o700)
        executable = daemon.home / 'oxc'
        executable.symlink_to(ROOT / 'lab/src/snapdragon_ai_lab/runtime/local-ai')
        command = shlex.split(os.environ['OXC_COMMAND']) if 'OXC_COMMAND' in os.environ else [sys.executable, str(executable)]
        env = {**daemon.env, 'PATH': str(binary_dir) + ':/usr/bin:/bin'}
        if name == 'panel-output':
            env['OXC_PANEL_OUTPUT'] = '1'
        result = subprocess.run([*daemon.namespace, *command, *args], cwd=daemon.home,
                                env=env, capture_output=True, timeout=15)
        if result.returncode != expected_exit:
            raise AssertionError(name + ': exit ' + str(result.returncode) + '\n' + result.stdout.decode() + result.stderr.decode())
        # Desktop launch is intentionally detached. Wait for its synthetic peer
        # to record the dispatch; do not mistake CLI process exit for launch.
        command_file = daemon.home / 'host-commands.jsonl'
        detached = 'omarchy' if name in ('chat', 'code', 'images') else None
        if detached:
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                rows = [json.loads(line) for line in command_file.read_text().splitlines()] if command_file.exists() else []
                if any(row['command'] == detached for row in rows):
                    break
                time.sleep(0.02)
            else:
                raise AssertionError('Desktop peer was never invoked')
        rows = [json.loads(line) for line in command_file.read_text().splitlines()] if command_file.exists() else []
        value = {'exit': result.returncode, 'stdout': result.stdout.decode(), 'stderr': result.stderr.decode(),
                 'host_commands': rows}
        # Prefix replacement is the only text normalization: storage IDs still
        # have to match exactly, using the same synthetic mount path each run.
        return json.loads(json.dumps(value).replace(str(daemon.home), '<HOME>'))


def invoke_case(name, script=None):
    # A new network namespace per case gives the CLI its hardcoded 8092 without
    # ever reaching a service outside this synthetic installation.
    with tempfile.TemporaryDirectory(prefix='oxc-cli-runtime-') as directory:
        home = Path(directory)
        env = isolated_env(home)
        for key in ('OXC_COMMAND', 'OXC_DAEMON_COMMAND'):
            if key in os.environ:
                env[key] = os.environ[key]
        command = ['bwrap', '--die-with-parent', '--unshare-net', '--ro-bind', '/', '/',
                   '--dev-bind', '/dev', '/dev', '--bind', '/proc', '/proc',
                   '--tmpfs', '/home', '--tmpfs', '/tmp',
                   '--bind', str(home), str(CANONICAL_HOME),
                   '--setenv', 'HOME', str(CANONICAL_HOME), '--',
                   sys.executable, str(Path(script or __file__).resolve()), '--case', name]
        result = subprocess.run(command, env=env, capture_output=True, timeout=25)
        if result.returncode:
            raise AssertionError(name + ': ' + result.stderr.decode())
        return json.loads(result.stdout)


class RuntimeCliContract(unittest.TestCase):
    def test_runtime_goldens(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(invoke_case(name), expected[name])


if __name__ == '__main__':
    if len(sys.argv) == 3 and sys.argv[1] == '--case':
        print(json.dumps(capture_case(sys.argv[2])))
    elif sys.argv[1:] == ['--record-python']:
        if 'OXC_COMMAND' in os.environ or 'OXC_DAEMON_COMMAND' in os.environ:
            raise SystemExit('Goldens must be recorded from the Python reference')
        GOLDENS.write_text(json.dumps({name: invoke_case(name) for name in CASES},
                                     indent=2, sort_keys=True) + '\n')
    else:
        unittest.main()
