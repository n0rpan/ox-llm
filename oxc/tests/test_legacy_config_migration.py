"""Upgrade from a pre-rename install: config and key file are copied from ~/.config/local-ai/llm."""
import io
import json
import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from oxc import cloud_install, legacy_config

FAKE_KEY = 'not-a-real-key'


class LegacyMigration(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(dir='/tmp')
        self.addCleanup(tmp.cleanup)
        self.home = Path(tmp.name)
        env = patch.dict(os.environ, {'HOME': str(self.home)})
        env.start()
        self.addCleanup(env.stop)
        os.environ.pop('XDG_CONFIG_HOME', None)
        self.legacy = self.home / '.config/local-ai/llm'
        self.new = self.home / '.config/oxc/llm'
        self.legacy.mkdir(parents=True)
        (self.legacy / 'runtime.json').write_text(json.dumps(
            {'cloud': {'enabled': True, 'secrets_file': str(self.legacy / 'cloud-secrets.json')}, 'x': 1}))
        (self.legacy / 'cloud-secrets.json').write_text(json.dumps({'openrouter': FAKE_KEY}))
        for name in legacy_config.FILES:
            os.chmod(self.legacy / name, 0o600)

    def test_copy_keeps_mode_rewrites_path_and_leaves_legacy(self):
        before = (self.legacy / 'runtime.json').read_text()
        with patch('sys.stderr', new=io.StringIO()) as err:
            copied = legacy_config.migrate()
        self.assertEqual(copied, ['runtime.json', 'cloud-secrets.json'])
        self.assertNotIn(FAKE_KEY, err.getvalue())
        for name in copied:
            self.assertEqual(stat.S_IMODE((self.new / name).stat().st_mode), 0o600)
        value = json.loads((self.new / 'runtime.json').read_text())
        self.assertEqual(value['cloud']['secrets_file'], str(self.new / 'cloud-secrets.json'))
        self.assertEqual(value['x'], 1)
        self.assertEqual((self.legacy / 'runtime.json').read_text(), before)
        self.assertTrue((self.legacy / 'cloud-secrets.json').exists())

    def test_existing_new_dir_and_symlinks_are_left_alone(self):
        self.new.mkdir(parents=True)
        self.assertEqual(legacy_config.migrate(), [])
        self.assertEqual(list(self.new.iterdir()), [])
        other = self.home / 'other'
        other.mkdir()
        (other / 'target').write_text(FAKE_KEY)
        (self.legacy / 'cloud-secrets.json').unlink()
        (self.legacy / 'cloud-secrets.json').symlink_to(other / 'target')
        new = self.home / 'fresh'
        self.assertEqual(legacy_config.migrate(new), ['runtime.json'])
        self.assertFalse((new / 'cloud-secrets.json').exists())

    def test_cloud_only_install_keeps_upgraded_config(self):
        models = self.home / 'models.json'
        models.write_text(json.dumps({'openrouter/m': {
            'kind': 'remote', 'provider': 'openrouter', 'base_url': 'https://openrouter.example.invalid/api/v1',
            'upstream_model': 'v/u', 'context_window': 64000, 'max_output': 4096,
            'pricing': {'input_per_million': 1.0, 'output_per_million': 1.0}}}))
        args = ['--cloud-only', '--prefix', str(self.home / 'prefix'), '--config-dir', str(self.new),
                '--no-systemd', '--models-file', str(models)]
        with patch('sys.stdout', new=io.StringIO()), patch('sys.stderr', new=io.StringIO()):
            code = cloud_install.main(args, runner=lambda c, **k: subprocess.CompletedProcess(c, 0, '', ''))
        self.assertEqual(code, 0)
        value = json.loads((self.new / 'runtime.json').read_text())
        self.assertEqual(value['x'], 1)
        self.assertEqual(value['cloud']['secrets_file'], str(self.new / 'cloud-secrets.json'))
        self.assertEqual(json.loads((self.new / 'cloud-secrets.json').read_text()), {'openrouter': FAKE_KEY})
