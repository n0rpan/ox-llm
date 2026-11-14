"""Bootstrap/install/rollback contracts through their public process entry points."""
import hashlib
import json
import os
from pathlib import Path
import shlex
import stat
import subprocess
import sys
import tempfile
import unittest

from harness import ROOT, isolated_env

GOLDENS = Path(__file__).with_name('goldens') / 'units'


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


class BootstrapContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='oxc-install-conformance-')
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        self.target = self.home / 'target'
        self.stage = self.home / 'stage'
        self.journal = self.home / 'journal.json'
        self.env = isolated_env(self.home)

    def invoke(self, operation, args, expected=0):
        variable, module = {
            'bootstrap': ('OXC_BOOTSTRAP_COMMAND', 'snapdragon_ai_lab.bootstrap'),
            'install': ('OXC_INSTALL_COMMAND', 'snapdragon_ai_lab.bootstrap_install'),
        }[operation]
        command = shlex.split(os.environ[variable]) if variable in os.environ else [sys.executable, '-m', module]
        result = subprocess.run(command + [str(arg) for arg in args], cwd=self.home, env=self.env,
                                capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
        return result

    def stage_payload(self):
        self.invoke('bootstrap', ['--project', ROOT / 'lab', 'stage', self.stage, '--target-home', self.target])
        self.invoke('bootstrap', ['verify-stage', self.stage])
        return digest(self.stage / 'stage-manifest.json')

    def apply_args(self, stage_digest):
        return ['apply', '--stage', self.stage, '--target-home', self.target,
                '--journal', self.journal, '--stage-manifest-sha256', stage_digest]

    def test_real_source_payload_unit_text_and_hash_bound_rollback(self):
        stage_digest = self.stage_payload()
        # The installed units are public artifacts; capture their complete text.
        # A native ExecStart must eventually be an explicitly reviewed migration
        # difference; all other service controls remain parity requirements.
        staged_units = self.stage / 'payload/.config/systemd/user'
        self.assertEqual({p.name for p in staged_units.glob('*.service')},
                         {p.name for p in GOLDENS.glob('*.service')})
        for golden in GOLDENS.glob('*.service'):
            self.assertEqual((staged_units / golden.name).read_bytes(), golden.read_bytes())
        result = json.loads(self.invoke('install', self.apply_args(stage_digest)).stdout)
        self.assertTrue(result['installed_source_payload'])
        self.assertFalse(result['qualified'])
        self.assertFalse(result['services_started'])
        self.assertFalse(result['panel_layout_changed'])
        self.assertEqual(result['journal_sha256'], digest(self.journal))
        for golden in GOLDENS.glob('*.service'):
            installed = self.target / '.config/systemd/user' / golden.name
            self.assertEqual(installed.read_bytes(), golden.read_bytes())
            self.assertEqual(stat.S_IMODE(installed.stat().st_mode), 0o600)
        config = self.target / '.config/local-ai/llm/runtime.json'
        runtime = json.loads(config.read_text())
        self.assertFalse(runtime['ready'])
        self.assertFalse(runtime['profiles']['ornith-35b-a3b-local']['ready'])
        self.assertEqual(runtime['selected_model'], 'ornith-35b-a3b-local')
        self.assertEqual(stat.S_IMODE(config.stat().st_mode), 0o600)
        executable = self.target / '.local/bin/local-ai'
        self.assertEqual(stat.S_IMODE(executable.stat().st_mode), 0o700)
        installed_env = {**self.env, 'HOME': str(self.target)}
        installed_env.pop('PYTHONPATH')  # Installed help must work without the checkout package.
        help_result = subprocess.run([str(executable), '--help'], env=installed_env, cwd=self.target,
                                     capture_output=True, timeout=5)
        self.assertEqual(help_result.returncode, 0, help_result.stderr.decode())
        self.assertIn(b'usage: local-ai', help_result.stdout)
        lease = self.target / '.local/state/local-ai/inference.lock'
        lease_inode = lease.stat().st_ino
        self.assertEqual(stat.S_IMODE(lease.stat().st_mode), 0o600)
        self.invoke('install', ['rollback', '--journal', self.journal,
                               '--journal-sha256', result['journal_sha256']])
        self.assertFalse(executable.exists())
        self.assertFalse(config.exists())
        self.assertTrue(lease.is_file())
        self.assertEqual(lease.stat().st_ino, lease_inode)

    def test_wrong_manifest_hash_refuses_before_installing(self):
        self.stage_payload()
        result = self.invoke('install', self.apply_args('0' * 64), expected=1)
        self.assertIn('stage_manifest_identity_mismatch', result.stderr)
        self.assertFalse(self.journal.exists())
        self.assertFalse(self.target.exists())

    def test_existing_managed_file_is_preserved(self):
        stage_digest = self.stage_payload()
        executable = self.target / '.local/bin/local-ai'
        executable.parent.mkdir(parents=True, mode=0o700)
        executable.write_bytes(b'synthetic owner file\n')
        executable.chmod(0o700)
        result = self.invoke('install', self.apply_args(stage_digest), expected=1)
        self.assertIn('existing_managed_file_preserved', result.stderr)
        self.assertEqual(executable.read_bytes(), b'synthetic owner file\n')
        self.assertFalse(self.journal.exists())
        self.assertFalse((self.target / '.config').exists())

    def test_symlinked_target_is_refused_without_touching_destination(self):
        stage_digest = self.stage_payload()
        destination = self.home / 'unrelated'
        destination.mkdir(mode=0o700)
        self.target.symlink_to(destination, target_is_directory=True)
        result = self.invoke('install', self.apply_args(stage_digest), expected=1)
        self.assertIn('symlinked_target_path_refused', result.stderr)
        self.assertEqual(list(destination.iterdir()), [])
        self.assertFalse(self.journal.exists())


if __name__ == '__main__':
    unittest.main()
