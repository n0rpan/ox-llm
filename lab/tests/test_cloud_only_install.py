"""Cloud-only installer (#70 b): a prefix install with no engine, no model download and no
Snapdragon step, that really starts the installed controller. Loopback only, fake keys."""
import http.client
import io
import json
import os
import socket
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

from snapdragon_ai_lab import cloud_install

MODEL = {'openrouter/mock-model': {
    'kind': 'remote', 'provider': 'openrouter', 'base_url': 'https://openrouter.example.invalid/api/v1',
    'upstream_model': 'vendor/up', 'context_window': 64000, 'max_output': 4096,
    'pricing': {'input_per_million': 1.0, 'output_per_million': 1.0}}}


def free_port():
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        return probe.getsockname()[1]


class CloudOnlyInstall(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(dir='/tmp')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.models = self.root / 'models.json'
        self.models.write_text(json.dumps(MODEL))
        self.args = ['--cloud-only', '--prefix', str(self.root / 'prefix'), '--config-dir', str(self.root / 'config'),
                     '--unit-dir', str(self.root / 'units'), '--port', '18092', '--unit-name', 'oxc-test',
                     '--models-file', str(self.models), '--daily-budget', '2.5']

    def run_install(self, extra=(), machine='x86_64'):
        calls = []
        with patch('platform.machine', return_value=machine), patch('sys.stdout', new=io.StringIO()):
            code = cloud_install.main([*self.args, *extra], runner=lambda cmd, **kw: calls.append(cmd) or
                                      subprocess.CompletedProcess(cmd, 0, '', ''))
        return code, calls

    def test_runtime_json_and_unit_on_x86(self):
        code, calls = self.run_install()
        self.assertEqual(code, 0)
        value = json.loads((self.root / 'config/runtime.json').read_text())
        self.assertIs(value['cloud_only'], True)
        self.assertIs(value['cloud']['enabled'], True)
        self.assertEqual(value['cloud']['daily_budget_usd'], 2.5)
        self.assertEqual(list(value['cloud']['models']), list(MODEL))
        self.assertEqual(value['cloud']['secrets_file'], str(self.root / 'config/cloud-secrets.json'))
        unit = (self.root / 'units/oxc-test.service').read_text()
        launcher = (self.root / 'prefix/bin/oxc-controller').read_text()
        self.assertIn('LOCAL_LLM_PORT=18092', launcher)
        self.assertIn(str(self.root / 'config/runtime.json'), launcher)
        self.assertIn('ExecStart=' + str(self.root / 'prefix/bin/oxc-controller'), unit)
        self.assertNotIn('%h', unit)
        self.assertEqual([c[:3] for c in calls], [['systemctl', '--user', 'daemon-reload'],
                                                  ['systemctl', '--user', 'enable']])
        self.assertIn('oxc-test.service', calls[1])
        # no engine, no model, no Snapdragon artefact
        names = {p.name for p in (self.root / 'prefix').rglob('*')}
        self.assertFalse([n for n in names if n.endswith(('.gguf', '.bin')) or 'oxe-server' in n])

    def test_no_systemd_writes_no_unit_and_runs_no_command(self):
        code, calls = self.run_install(['--no-systemd'], machine='aarch64')
        self.assertEqual(code, 0)
        self.assertEqual(calls, [])
        self.assertFalse((self.root / 'units').exists())
        self.assertTrue((self.root / 'prefix/bin/oxc-controller').is_file())

    def test_existing_config_is_kept_and_bad_catalog_refused(self):
        self.run_install(['--no-systemd'])
        (self.root / 'config/runtime.json').write_text('{"mine": 1}')
        code, _ = self.run_install(['--no-systemd'])
        self.assertEqual(code, 1)
        self.assertEqual(json.loads((self.root / 'config/runtime.json').read_text()), {'mine': 1})
        self.models.write_text(json.dumps({'bad id!': {'kind': 'remote'}}))
        self.args[self.args.index('--config-dir') + 1] = str(self.root / 'config2')
        code, _ = self.run_install(['--no-systemd'])
        self.assertEqual(code, 1)
        self.assertFalse((self.root / 'config2/runtime.json').exists())

    def test_default_catalog_and_next_step(self):
        self.args[self.args.index('--models-file'):self.args.index('--models-file') + 2] = []
        out = io.StringIO()
        with patch('sys.stdout', new=out):
            code = cloud_install.main([*self.args, '--no-systemd'])
        self.assertEqual(code, 0)
        models = json.loads((self.root / 'config/runtime.json').read_text())['cloud']['models']
        providers = {m['provider'] for m in models.values()}
        self.assertEqual(providers, {'anthropic', 'openai', 'openrouter'})
        from snapdragon_ai_lab.runtime import cloud
        self.assertEqual(set(cloud.entries({'cloud': {'enabled': True, 'models': models}})), set(models))
        for item in models.values():
            self.assertEqual(item['kind'], 'remote/' + item['provider'])
            self.assertIsNotNone(cloud.entry('x/y', item)['pricing'])
        expected = {
            'anthropic/claude-haiku-5-5': ('claude-haiku-5-5', 32000, 0.5, 2.5),
            'anthropic/claude-sonnet-5-5': ('claude-sonnet-5-5', 32000, 2.0, 10.0),
            'anthropic/claude-opus-5-5': ('claude-opus-5-5', 32000, 4.0, 20.0),
            'openrouter/deepseek-v4-pro': ('deepseek/deepseek-v4-pro-0813', 32768, 0.111, 4.44),
            'openrouter/deepseek-v4.1-flash': ('deepseek/deepseek-v4.1-flash', 32768, 0.0507, 0.2028),
            'openai/gpt-6-luna': ('gpt-6-luna', 128000, 0.1, 0.5),
            'openai/gpt-6.1-sol': ('gpt-6.1-sol', 128000, 2.0, 10.0)}
        self.assertEqual(set(models), set(expected))
        for alias, (upstream, output, price_in, price_out) in expected.items():
            item = models[alias]
            self.assertEqual((item['upstream_model'], item['context_window'], item['max_output'],
                              item['pricing']['input_per_million'], item['pricing']['output_per_million']),
                             (upstream, 200000, output, price_in, price_out), alias)
        claude = models['anthropic/claude-sonnet-5-5']
        self.assertEqual((claude['provider'], claude['base_url']), ('anthropic', 'https://api.anthropic.com'))
        self.assertEqual(models['openai/gpt-6-luna']['base_url'], 'https://api.openai.com/v1')
        self.assertEqual(models['openrouter/deepseek-v4-pro']['base_url'], 'https://openrouter.ai/api/v1')
        printed = json.loads(out.getvalue())
        self.assertIn(str(self.root / 'config/cloud-secrets.json'), printed['next_step'])
        self.assertFalse((self.root / 'config/cloud-secrets.json').exists())

    def install_output(self, models_file, importable):
        out, err = io.StringIO(), io.StringIO()
        args = list(self.args)
        if models_file is None:
            i = args.index('--models-file')
            del args[i:i + 2]
        with patch.object(cloud_install, 'anthropic_importable', return_value=importable), \
                patch('sys.stdout', new=out), patch('sys.stderr', new=err):
            code = cloud_install.main([*args, '--no-systemd', '--force'])
        self.assertEqual(code, 0)
        return json.loads(out.getvalue()), err.getvalue()

    def test_missing_anthropic_sdk_is_reported_with_the_pip_command(self):
        result, err = self.install_output(None, importable=False)
        self.assertIs(result['anthropic_sdk'], False)
        command = f'{sys.executable} -m pip install anthropic==0.125.0'
        self.assertIn(command, result['sdk_hint'])
        self.assertEqual(len(err.strip().splitlines()), 1)
        self.assertIn(command, err)

    def test_present_sdk_or_no_anthropic_entry_prints_no_hint(self):
        result, err = self.install_output(None, importable=True)
        self.assertIs(result['anthropic_sdk'], True)
        self.assertNotIn('sdk_hint', result)
        self.assertEqual(err, '')
        result, err = self.install_output(self.models, importable=False)
        self.assertNotIn('sdk_hint', result)
        self.assertEqual(err, '')

    def test_sdk_check_uses_the_controller_python_and_installs_nothing(self):
        seen = []

        def fake_run(cmd, **kw):
            seen.append(cmd)
            return subprocess.CompletedProcess(cmd, 1, '', '')
        with patch('subprocess.run', fake_run):
            self.assertFalse(cloud_install.anthropic_importable())
        self.assertEqual(seen, [[sys.executable, '-c', 'import anthropic']])

    def test_installed_controller_serves_only_remote_models(self):
        port = free_port()
        self.args[self.args.index('--port') + 1] = str(port)
        self.assertEqual(self.run_install(['--no-systemd'])[0], 0)
        env = {k: v for k, v in os.environ.items() if not k.startswith(('LOCAL_', 'OXC_', 'PYTHON'))}
        env['HOME'] = str(self.root / 'home')
        process = subprocess.Popen([str(self.root / 'prefix/bin/oxc-controller')], env=env, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, text=True)
        self.addCleanup(lambda: (process.terminate(), process.wait(10), process.stdout.close()))
        body = None
        for _ in range(100):
            try:
                client = http.client.HTTPConnection('127.0.0.1', port, timeout=3)
                client.request('GET', '/v1/models', headers={'Host': f'127.0.0.1:{port}'})
                response = client.getresponse()
                body = (response.status, json.loads(response.read()))
                break
            except OSError:
                if process.poll() is not None:
                    self.fail('controller exited: ' + process.stdout.read())
                time.sleep(.1)
        self.assertEqual(body[0], 200)
        self.assertEqual([r['id'] for r in body[1]['data']], list(MODEL))
        self.assertFalse(list((self.root / 'home').rglob('*')), 'must not write outside the prefix/config')


if __name__ == '__main__':
    unittest.main()
