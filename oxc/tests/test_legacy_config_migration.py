"""Upgrade from a pre-rename install: config and key file are copied from ~/.config/local-ai/llm."""
import io
import json
import os
import stat
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from oxc import legacy_config

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
            {'cloud_only': True, 'cloud': {'enabled': True, 'daily_budget_usd': 7, 'models': {'a/b': {}},
                                           'secrets_file': str(self.legacy / 'cloud-secrets.json')}, 'x': 1}))
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

    def test_custom_secrets_name_keeps_original_path(self):
        custom = self.legacy / 'custom-key.json'
        custom.write_text('{}')
        value = json.loads((self.legacy / 'runtime.json').read_text())
        value['cloud']['secrets_file'] = str(custom)
        (self.legacy / 'runtime.json').write_text(json.dumps(value))
        legacy_config.migrate()
        self.assertEqual(json.loads((self.new / 'runtime.json').read_text())['cloud']['secrets_file'], str(custom))

    def test_failed_copy_leaves_no_half_directory(self):
        with patch('oxc.legacy_config._copy', side_effect=OSError('boom')):
            with self.assertRaises(OSError):
                legacy_config.migrate()
        self.assertFalse(self.new.exists())
        self.assertEqual(legacy_config.migrate(), ['runtime.json', 'cloud-secrets.json'])
