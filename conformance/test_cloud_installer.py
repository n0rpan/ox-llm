"""Cloud-only install process/file contracts; synthetic HOME and private network."""
import http.client
import hashlib
import json
import os
from pathlib import Path
import shlex
import socket
import stat
import subprocess
import sys
import time
import unittest

from cli_harness import CANONICAL_HOME, invoke_case
from environment import PYTHON, isolated_env
from test_cloud_catalog import normalize

GOLDENS = Path(__file__).with_name('goldens') / 'cloud-installer.json'
CASES = ('foreground', 'systemd', 'existing-config', 'force', 'bad-catalog', 'bad-json',
         'not-object', 'bad-budget', 'bad-port', 'bad-unit', 'config-symlink',
         'launcher-symlink', 'default-catalog', 'quoted-paths', 'installed-startup',
         'legacy-copy', 'legacy-default-budget', 'legacy-secrets-symlink', 'legacy-keep-new', 'legacy-force')


def call(port, method, path, body=None):
    peer = http.client.HTTPConnection('127.0.0.1', port, timeout=3)
    try:
        peer.request(method, path, json.dumps(body) if body is not None else None,
                     {'Content-Type': 'application/json'})
        response = peer.getresponse()
        return {'status': response.status, 'body': json.loads(response.read())}
    finally:
        peer.close()


def capture(name):
    home = CANONICAL_HOME
    prefix = home / ('prefix with spaces $ %' if name == 'quoted-paths' else 'prefix')
    legacy = home / 'config/local-ai/llm' if name.startswith('legacy-') else None
    config = home / 'config/oxc/llm' if legacy else home / 'config'
    if not legacy or name == 'legacy-keep-new':
        config.mkdir(parents=True, exist_ok=True, mode=0o700)
    if legacy:
        legacy.mkdir(parents=True, mode=0o700)
    unit_dir = home / 'units'
    secret = (legacy or config) / 'cloud-secrets.json'
    secret.write_text('{"synthetic-unread":"synthetic-must-stay-private"}\n')
    secret.chmod(0o600)
    original_secret = secret.read_bytes()
    catalog = home / 'models.json'
    item = {'kind': 'remote/openrouter', 'upstream_model': 'synthetic/upstream',
            'base_url': 'http://127.0.0.1:1/v1', 'context_window': 64000, 'max_output': 512,
            'pricing': {'input_per_million': 1.0, 'output_per_million': 2.0}}
    models = {'synthetic-cloud': item}
    if legacy:
        (legacy / 'runtime.json').write_text(json.dumps({'cloud_only': True, 'cloud': {
            'enabled': True, 'daily_budget_usd': 7.0, 'secrets_file': str(secret),
            'models': {'synthetic-legacy': item}}}) + '\n')
        (legacy / 'runtime.json').chmod(0o600)
        original_runtime = (legacy / 'runtime.json').read_bytes()
    catalog.write_text(json.dumps(models))
    if name == 'bad-catalog':
        catalog.write_text(json.dumps({'bad id!': item}))
    elif name == 'bad-json':
        catalog.write_text('{broken')
    elif name == 'not-object':
        catalog.write_text('[]')
    if name in ('existing-config', 'force', 'legacy-keep-new'):
        (config / 'runtime.json').write_text('{"owner":"synthetic-keep"}\n')
    target = home / 'untouched-target'
    target.write_text('synthetic-owned-target\n')
    if name == 'legacy-secrets-symlink':
        secret.unlink()
        secret.symlink_to(target)
        original_secret = target.read_bytes()
    if name == 'config-symlink':
        (config / 'runtime.json').symlink_to(target)
    if name == 'launcher-symlink':
        (prefix / 'bin').mkdir(parents=True)
        (prefix / 'bin/oxc-controller').symlink_to(target)
    bin_dir = home / 'peers'
    bin_dir.mkdir()
    systemctl = bin_dir / 'systemctl'
    systemctl.write_text('#!' + PYTHON + '\nimport json,os,sys\nfrom pathlib import Path\n'
                         'with (Path(os.environ["HOME"])/"systemctl.jsonl").open("a") as out:\n'
                         ' out.write(json.dumps(sys.argv[1:])+"\\n")\n')
    systemctl.chmod(0o700)
    env = isolated_env(home)
    env['PATH'] = str(bin_dir) + ':/usr/bin:/bin'
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        port = probe.getsockname()[1]
    args = ['--cloud-only', '--prefix', str(prefix), '--config-dir', str(config),
            '--unit-dir', str(unit_dir), '--port', str(port), '--unit-name', 'oxc-synthetic',
            '--daily-budget', '2.5']
    if name != 'default-catalog':
        args += ['--models-file', str(catalog)]
    if name not in ('systemd', 'quoted-paths'):
        args += ['--no-systemd']
    if name in ('force', 'legacy-force'):
        args += ['--force']
    if name == 'legacy-default-budget':
        index = args.index('--daily-budget')
        del args[index:index + 2]
    if name in ('bad-budget', 'bad-port', 'bad-unit'):
        field, bad = {'bad-budget': ('--daily-budget', '0'), 'bad-port': ('--port', '0'),
                      'bad-unit': ('--unit-name', 'bad/name')}[name]
        args[args.index(field) + 1] = bad
    native = os.environ.get('OXC_CLOUD_INSTALL_COMMAND')
    command = shlex.split(native) if native else [PYTHON, '-m', 'oxc.cloud_install']
    done = subprocess.run([*command, *args], env=env, cwd=home, capture_output=True, text=True, timeout=15)
    output = json.loads(done.stdout) if done.returncode == 0 else done.stdout
    result = {'exit': done.returncode, 'stdout': output, 'stderr': done.stderr,
              'systemctl': [json.loads(line) for line in (home / 'systemctl.jsonl').read_text().splitlines()]
                           if (home / 'systemctl.jsonl').exists() else []}
    runtime = config / 'runtime.json'
    if runtime.is_file() and not runtime.is_symlink():
        result['config'] = json.loads(runtime.read_text())
        result['config_mode'] = stat.S_IMODE(runtime.stat().st_mode)
    launcher = prefix / 'bin/oxc-controller'
    if done.returncode == 0:
        if 'synthetic-must-stay-private' in json.dumps(result):
            raise AssertionError('Installer read or exposed the secrets file')
        result['launcher_mode'] = stat.S_IMODE(launcher.stat().st_mode)
        text = launcher.read_text()
        subprocess.run(['/bin/sh', '-n', str(launcher)], check=True)
        expected = {'LOCAL_LLM_CONFIG': str(runtime), 'LOCAL_LLM_PORT': str(port),
                    'OXC_LEASE': str(prefix / 'state/inference.lock'),
                    'OXC_LEDGER_DIR': str(prefix / 'state/ledger'), 'XDG_STATE_HOME': str(prefix / 'state')}
        exports = dict(line[len('export '):].split('=', 1) for line in text.splitlines() if line.startswith('export '))
        if any(shlex.split(exports.get(key, '')) != [value] for key, value in expected.items()):
            raise AssertionError('Installed launcher lost its isolated config/state environment')
        if native:
            if '.py' in text or 'python' in text.lower() or list(prefix.rglob('*.py')):
                raise AssertionError('Native cloud install depends on Python')
        elif 'exec ' + PYTHON + ' ' not in text:
            raise AssertionError('Reference launcher changed interpreter')
        result['launcher_environment'] = expected
        unit = unit_dir / 'oxc-synthetic.service'
        result['unit'] = unit.read_text() if unit.exists() else None
        if name == 'installed-startup':
            with (home / 'installed.log').open('w+') as log:
                process = subprocess.Popen([str(launcher)], env=env, cwd=home, stdout=log, stderr=log)
                try:
                    deadline = time.monotonic() + 8
                    while True:
                        try:
                            health = call(port, 'GET', '/health')
                            if health['status'] == 200:
                                break
                        except OSError:
                            pass
                        if process.poll() is not None or time.monotonic() >= deadline:
                            log.seek(0)
                            raise AssertionError('Installed startup failed: ' + log.read())
                        time.sleep(.02)
                    status = call(port, 'GET', '/_local/status')['body']
                    result['installed'] = {'health': health, 'models': call(port, 'GET', '/v1/models'),
                                           'engine_mode': status['engine_mode'], 'loaded_model': status['loaded_model'],
                                           'keyless': call(port, 'POST', '/v1/chat/completions',
                                            {'model': 'synthetic-cloud', 'messages': [{'role': 'user', 'content': 'Synthetic prompt.'}]})}
                finally:
                    process.terminate()
                    process.wait(timeout=5)
        names = [path.name for path in prefix.rglob('*')]
        if any(name.endswith('.gguf') or 'oxe-server' in name or name == 'cloud-secrets.json' for name in names):
            raise AssertionError('Cloud install copied a local model/engine or credentials')
    if secret.read_bytes() != original_secret or target.read_text() != 'synthetic-owned-target\n':
        raise AssertionError('Installer overwrote a protected owner file')
    if legacy:
        if (legacy / 'runtime.json').read_bytes() != original_runtime:
            raise AssertionError('Migration changed the original runtime config')
        copied = config / 'cloud-secrets.json'
        if copied.exists():
            if copied.is_symlink() or copied.read_bytes() != original_secret:
                raise AssertionError('Migration did not preserve credential bytes privately')
        result['migration'] = {'original_runtime_sha256': hashlib.sha256(original_runtime).hexdigest(),
                               'original_secret_sha256': hashlib.sha256(original_secret).hexdigest(),
                               'copied_secret': copied.exists(),
                               'copied_secret_mode': stat.S_IMODE(copied.stat().st_mode) if copied.exists() else None}
    result = normalize(result, home, port)
    # The install summary carries its port as an export string, rather than a
    # numeric `port` field or URL. Validate against the requested port first.
    if isinstance(result.get('launcher_environment'), dict):
        result['launcher_environment']['LOCAL_LLM_PORT'] = '<PORT>'
    return result


class CloudInstallerContract(unittest.TestCase):
    def test_cloud_only_install_process_files_and_startup(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(invoke_case(name, __file__, timeout=40), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_CLOUD_INSTALL_COMMAND' in os.environ:
            raise SystemExit('Record only the Python installer')
        GOLDENS.write_text(json.dumps({name: invoke_case(name, __file__, timeout=40) for name in CASES}, indent=2, sort_keys=True) + '\n')
    elif sys.argv[1:2] == ['--case']:
        print(json.dumps(capture(sys.argv[2])))
    else:
        unittest.main()
