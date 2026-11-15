"""Bootstrap/install/rollback contracts through their public process entry points."""
import hashlib
import fcntl
import http.client
import json
import os
from pathlib import Path
import shlex
import stat
import subprocess
import sys
import tempfile
import time
import unittest

from harness import ROOT, free_port, isolated_env

GOLDENS = Path(__file__).with_name('goldens') / 'units'
NATIVE_UNITS = GOLDENS.parent / 'native-units'


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
            'bootstrap': ('OXC_BOOTSTRAP_COMMAND', 'oxc.bootstrap'),
            'install': ('OXC_INSTALL_COMMAND', 'oxc.bootstrap_install'),
        }[operation]
        command = shlex.split(os.environ[variable]) if variable in os.environ else [sys.executable, '-m', module]
        result = subprocess.run(command + [str(arg) for arg in args], cwd=self.home, env=self.env,
                                capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
        return result

    def stage_payload(self):
        self.invoke('bootstrap', ['--project', ROOT / 'oxc', 'stage', self.stage, '--target-home', self.target])
        self.invoke('bootstrap', ['verify-stage', self.stage])
        return digest(self.stage / 'stage-manifest.json')

    def apply_args(self, stage_digest):
        return ['apply', '--stage', self.stage, '--target-home', self.target,
                '--journal', self.journal, '--stage-manifest-sha256', stage_digest]

    def expected_units(self):
        native = json.loads((self.stage / 'stage-manifest.json').read_text()).get('executor') == 'native'
        # This is the explicit executor migration. No other service setting may
        # change, and the original Python service snapshots remain immutable.
        if native:
            for original in GOLDENS.glob('*.service'):
                expected = original.read_text().replace(
                    'ExecStart=/usr/bin/python3 %h/.local/share/ai-stack/runtime/controller.py',
                    'ExecStart=%h/.local/bin/oxc serve').replace(
                    'ExecStart=/usr/bin/python3 %h/.local/share/ai-stack/runtime/studio',
                    'ExecStart=%h/.local/bin/oxc studio')
                self.assertEqual((NATIVE_UNITS / original.name).read_text(), expected)
            payload = self.stage / 'payload'
            self.assertEqual((payload / '.local/bin/oxc').read_bytes()[:4], b'\x7fELF')
            self.assertEqual(list((payload / '.local/share/ai-stack/runtime').glob('*.py')), [])
        return NATIVE_UNITS if native else GOLDENS

    def install_payload(self):
        return json.loads(self.invoke('install', self.apply_args(self.stage_payload())).stdout)

    def undo_args(self, identity):
        return ['rollback', '--journal', self.journal, '--journal-sha256', identity]

    def invoke_reference_install(self, args):
        result = subprocess.run([sys.executable, '-m', 'oxc.bootstrap_install',
                                 *(str(arg) for arg in args)], cwd=self.home, env=self.env,
                                capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return json.loads(result.stdout)

    def test_real_source_payload_unit_text_and_hash_bound_rollback(self):
        stage_digest = self.stage_payload()
        # The installed units are public artifacts; capture their complete text.
        # Native ExecStart is an explicit migration difference; all other service
        # controls remain byte-for-byte parity requirements.
        staged_units = self.stage / 'payload/.config/systemd/user'
        goldens = self.expected_units()
        self.assertEqual({p.name for p in staged_units.glob('*.service')},
                         {p.name for p in goldens.glob('*.service')})
        for golden in goldens.glob('*.service'):
            self.assertEqual((staged_units / golden.name).read_bytes(), golden.read_bytes())
        result = json.loads(self.invoke('install', self.apply_args(stage_digest)).stdout)
        self.assertTrue(result['installed_source_payload'])
        self.assertFalse(result['qualified'])
        self.assertFalse(result['services_started'])
        self.assertFalse(result['panel_layout_changed'])
        self.assertEqual(result['journal_sha256'], digest(self.journal))
        for golden in goldens.glob('*.service'):
            installed = self.target / '.config/systemd/user' / golden.name
            self.assertEqual(installed.read_bytes(), golden.read_bytes())
            self.assertEqual(stat.S_IMODE(installed.stat().st_mode), 0o600)
        config = self.target / '.config/oxc/llm/runtime.json'
        runtime = json.loads(config.read_text())
        self.assertFalse(runtime['ready'])
        self.assertFalse(runtime['profiles']['ornith-35b-a3b-local']['ready'])
        self.assertEqual(runtime['selected_model'], 'ornith-35b-a3b-local')
        self.assertEqual(stat.S_IMODE(config.stat().st_mode), 0o600)
        executable = self.target / '.local/bin/oxc'
        self.assertEqual(stat.S_IMODE(executable.stat().st_mode), 0o700)
        installed_env = {**self.env, 'HOME': str(self.target)}
        installed_env.pop('PYTHONPATH')  # Installed help must work without the checkout package.
        help_result = subprocess.run([str(executable), '--help'], env=installed_env, cwd=self.target,
                                     capture_output=True, timeout=5)
        self.assertEqual(help_result.returncode, 0, help_result.stderr.decode())
        self.assertIn(b'usage: oxc', help_result.stdout)
        lease = self.target / '.local/state/oxc/inference.lock'
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
        executable = self.target / '.local/bin/oxc'
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

    def test_tampered_source_refuses_before_any_target_mutation(self):
        identity = self.stage_payload()
        (self.stage / 'payload/.local/bin/oxc').write_bytes(b'synthetic changed payload\n')
        self.invoke('install', self.apply_args(identity), expected=1)
        self.assertFalse(self.target.exists())
        self.assertFalse(self.journal.exists())

    def test_unlisted_staged_directory_is_refused(self):
        identity = self.stage_payload()
        (self.stage / 'payload/synthetic-extra').mkdir(mode=0o700)
        self.invoke('install', self.apply_args(identity), expected=1)
        self.assertFalse(self.target.exists())
        self.assertFalse(self.journal.exists())

    def test_staged_file_symlink_is_refused(self):
        identity = self.stage_payload()
        path = self.stage / 'payload/.local/bin/oxc'
        outside = self.home / 'unrelated'
        outside.write_bytes(path.read_bytes())
        path.unlink()
        path.symlink_to(outside)
        self.invoke('install', self.apply_args(identity), expected=1)
        self.assertFalse(self.target.exists())
        self.assertFalse(self.journal.exists())
        self.assertTrue(outside.is_file())

    def test_existing_journal_is_preserved(self):
        identity = self.stage_payload()
        self.journal.write_bytes(b'synthetic owner journal\n')
        self.invoke('install', self.apply_args(identity), expected=1)
        self.assertEqual(self.journal.read_bytes(), b'synthetic owner journal\n')
        self.assertFalse(self.target.exists())

    def test_target_must_match_staged_plan(self):
        identity = self.stage_payload()
        self.target = self.home / 'other-target'
        self.invoke('install', self.apply_args(identity), expected=1)
        self.assertFalse(self.target.exists())
        self.assertFalse(self.journal.exists())

    def test_rollback_preserves_owner_edits_and_unrecorded_files(self):
        installed = self.install_payload()
        executable = self.target / '.local/bin/oxc'
        executable.write_bytes(b'synthetic owner edit\n')
        unrecorded = self.target / 'owner-note.txt'
        unrecorded.write_bytes(b'synthetic owner note\n')
        result = json.loads(self.invoke('install', self.undo_args(installed['journal_sha256'])).stdout)
        self.assertIn('.local/bin/oxc', result['preserved'])
        self.assertEqual(executable.read_bytes(), b'synthetic owner edit\n')
        self.assertEqual(unrecorded.read_bytes(), b'synthetic owner note\n')
        self.assertFalse((self.target / '.config/oxc/llm/runtime.json').exists())

    def test_wrong_rollback_hash_preserves_installation(self):
        self.install_payload()
        journal = self.journal.read_bytes()
        executable = self.target / '.local/bin/oxc'
        identity = digest(executable)
        self.invoke('install', self.undo_args('0' * 64), expected=1)
        self.assertEqual(self.journal.read_bytes(), journal)
        self.assertEqual(digest(executable), identity)

    def test_existing_canonical_lease_is_never_replaced_or_truncated(self):
        lease = self.target / '.local/state/oxc/inference.lock'
        lease.parent.mkdir(parents=True, mode=0o700)
        lease.write_bytes(b'synthetic existing lease\n')
        lease.chmod(0o600)
        inode = lease.stat().st_ino
        installed = self.install_payload()
        self.assertEqual(installed['canonical_inode'], inode)
        self.invoke('install', self.undo_args(installed['journal_sha256']))
        self.assertEqual(lease.stat().st_ino, inode)
        self.assertEqual(lease.read_bytes(), b'synthetic existing lease\n')

    def test_busy_canonical_lease_refuses_and_rolls_back_created_files(self):
        identity = self.stage_payload()
        lease = self.target / '.local/state/oxc/inference.lock'
        lease.parent.mkdir(parents=True, mode=0o700)
        lease.write_bytes(b'synthetic held lease\n')
        lease.chmod(0o600)
        inode = lease.stat().st_ino
        with lease.open('r+b') as stream:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.invoke('install', self.apply_args(identity), expected=1)
        state = json.loads(self.journal.read_text())
        self.assertEqual(state['status'], 'rolled_back_after_failure')
        self.assertFalse(state['installed'])
        self.assertEqual(lease.stat().st_ino, inode)
        self.assertEqual(lease.read_bytes(), b'synthetic held lease\n')
        self.assertFalse((self.target / '.local/bin/oxc').exists())

    def test_public_canonical_lease_refuses_before_journal_creation(self):
        identity = self.stage_payload()
        lease = self.target / '.local/state/oxc/inference.lock'
        lease.parent.mkdir(parents=True, mode=0o700)
        lease.write_bytes(b'')
        lease.chmod(0o644)
        self.invoke('install', self.apply_args(identity), expected=1)
        self.assertFalse(self.journal.exists())
        self.assertFalse((self.target / '.config').exists())

    def test_live_home_and_hidden_home_trees_are_refused(self):
        for name in ('', '.synthetic-hidden', '.config/synthetic'):
            with self.subTest(name=name):
                self.target = self.home / name
                if self.stage.exists():
                    import shutil
                    shutil.rmtree(self.stage)
                identity = self.stage_payload()
                self.invoke('install', self.apply_args(identity), expected=1)
                self.assertFalse(self.journal.exists())
                self.assertFalse((self.target / '.local/bin/oxc').exists())

    def test_forged_journal_cannot_name_an_unplanned_file(self):
        self.install_payload()
        outside = self.home / 'unrelated'
        outside.write_bytes(b'synthetic unrelated owner file\n')
        state = json.loads(self.journal.read_text())
        state['files'][-1]['path'] = '../unrelated'
        self.journal.write_text(json.dumps(state))
        self.invoke('install', self.undo_args(digest(self.journal)), expected=1)
        self.assertEqual(outside.read_bytes(), b'synthetic unrelated owner file\n')

    def test_symlinked_journal_is_refused(self):
        identity = self.stage_payload()
        outside = self.home / 'unrelated'
        outside.write_bytes(b'synthetic unrelated owner file\n')
        self.journal.symlink_to(outside)
        self.invoke('install', self.apply_args(identity), expected=1)
        self.assertEqual(outside.read_bytes(), b'synthetic unrelated owner file\n')
        self.assertFalse(self.target.exists())

    def test_installed_journal_can_be_rolled_back_by_reference(self):
        installed = self.install_payload()
        lease = self.target / '.local/state/oxc/inference.lock'
        inode = lease.stat().st_ino
        self.invoke_reference_install(self.undo_args(installed['journal_sha256']))
        self.assertFalse((self.target / '.local/bin/oxc').exists())
        self.assertEqual(lease.stat().st_ino, inode)

    def test_reference_journal_can_be_rolled_back_by_alternate(self):
        installed = self.invoke_reference_install(self.apply_args(self.stage_payload()))
        lease = self.target / '.local/state/oxc/inference.lock'
        inode = lease.stat().st_ino
        self.invoke('install', self.undo_args(installed['journal_sha256']))
        self.assertFalse((self.target / '.local/bin/oxc').exists())
        self.assertEqual(lease.stat().st_ino, inode)

    def test_pinned_asset_plans_match_reference_for_both_image_sets(self):
        for image in ('flux2-klein', 'z-image'):
            with self.subTest(image=image):
                args = ['--project', ROOT / 'oxc', 'plan', '--target-home', self.target,
                        '--image-model', image]
                actual = json.loads(self.invoke('bootstrap', args).stdout)
                reference = subprocess.run([sys.executable, '-m', 'oxc.bootstrap',
                    *(str(arg) for arg in args)], cwd=self.home, env=self.env,
                    capture_output=True, text=True, timeout=20, check=True)
                self.assertEqual(actual, json.loads(reference.stdout))
                self.assertFalse(self.target.exists())

    def test_installed_service_entry_points_start_without_models(self):
        self.install_payload()
        for unit, route in (('oxc-chat.service', '/_local/status'),
                            ('prompt-studio-app.service', '/')):
            with self.subTest(unit=unit):
                text = (self.target / '.config/systemd/user' / unit).read_text()
                fields = dict(line.split('=', 1) for line in text.splitlines()
                              if line.startswith(('ExecStart=', 'WorkingDirectory=')))
                env = isolated_env(self.target)
                for line in text.splitlines():
                    if line.startswith('Environment='):
                        name, value = line.removeprefix('Environment=').split('=', 1)
                        env[name] = value.replace('%h', str(self.target))
                port = free_port()
                env['LOCAL_LLM_PORT' if 'chat' in unit else 'STUDIO_PORT'] = str(port)
                command = shlex.split(fields['ExecStart'].replace('%h', str(self.target)))
                process = subprocess.Popen(command, cwd=fields['WorkingDirectory'].replace('%h', str(self.target)),
                    env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, umask=0o077)
                try:
                    deadline = time.monotonic() + 5
                    body = None
                    while time.monotonic() < deadline and process.poll() is None:
                        connection = http.client.HTTPConnection('127.0.0.1', port, timeout=.5)
                        try:
                            connection.request('GET', route)
                            response = connection.getresponse()
                            body = response.read()
                            self.assertEqual(response.status, 200, body)
                            break
                        except (ConnectionError, OSError):
                            time.sleep(.02)
                        finally:
                            connection.close()
                    self.assertIsNotNone(body, unit + ' did not start')
                    if 'chat' in unit:
                        self.assertIsInstance(json.loads(body), dict)
                    else:
                        self.assertIn(b'<title>Prompt studio</title>', body)
                finally:
                    process.terminate()
                    try:
                        process.communicate(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.communicate(timeout=5)

    def test_bootstrap_and_install_help_match_reference(self):
        for operation, module, actions in (
                ('bootstrap', 'bootstrap', ('', 'plan', 'stage', 'verify-stage')),
                ('install', 'bootstrap_install', ('', 'apply', 'rollback'))):
            for action in actions:
                with self.subTest(operation=operation, action=action):
                    args = ([action] if action else []) + ['--help']
                    actual = self.invoke(operation, args)
                    reference = subprocess.run([sys.executable, '-m', 'oxc.' + module, *args],
                        cwd=self.home, env=self.env, capture_output=True, text=True, timeout=10, check=True)
                    self.assertEqual(actual.stdout, reference.stdout)
                    self.assertEqual(actual.stderr, reference.stderr)

    def test_equal_option_forms_are_preserved(self):
        self.invoke('bootstrap', ['--project=' + str(ROOT / 'oxc'), 'stage', self.stage,
                                 '--target-home=' + str(self.target), '--image-model=z-image'])
        args = ['apply', '--stage=' + str(self.stage), '--target-home=' + str(self.target),
                '--journal=' + str(self.journal), '--stage-manifest-sha256=' + digest(self.stage / 'stage-manifest.json')]
        installed = json.loads(self.invoke('install', args).stdout)
        self.invoke('install', ['rollback', '--journal=' + str(self.journal),
                               '--journal-sha256=' + installed['journal_sha256']])
        self.assertFalse((self.target / '.local/bin/oxc').exists())

    def test_agent_identity_refresh_retains_the_canonical_lease(self):
        source = self.home / 'synthetic-agent.c'
        binary = self.home / 'synthetic-agent'
        source.write_text(r'''
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc != 4 || strcmp(argv[1], "identity") || strcmp(argv[2], "--refresh") || strcmp(argv[3], "--offline")) return 2;
    char path[4096];
    snprintf(path, sizeof(path), "%s/.local/state/oxc/inference.lock", getenv("HOME"));
    int fd = open(path, O_RDWR | O_NOFOLLOW);
    int held = fd >= 0 && flock(fd, LOCK_EX | LOCK_NB) < 0 && (errno == EAGAIN || errno == EWOULDBLOCK);
    if (fd >= 0) close(fd);
    snprintf(path, sizeof(path), "%s/synthetic-agent-proof.json", getenv("HOME"));
    FILE *proof = fopen(path, "w");
    if (!proof) return 3;
    fprintf(proof, "{\"lease_held\":%s}\n", held ? "true" : "false");
    fclose(proof);
    return 0;
}
''')
        subprocess.run(['/usr/bin/cc', '-o', str(binary), str(source)], env=self.env,
                       cwd=self.home, capture_output=True, timeout=20, check=True)
        self.invoke('bootstrap', ['--project', ROOT / 'oxc', 'stage', self.stage,
                                 '--target-home', self.target, '--oxa-binary', binary])
        install_args = self.apply_args(digest(self.stage / 'stage-manifest.json'))
        # argparse stores the last spelling. Refresh must use that validated
        # target, rather than reparsing an earlier duplicate option.
        install_args[1:1] = ['--target-home', self.home / 'unrelated-target']
        installed = json.loads(self.invoke('install', install_args).stdout)
        self.assertEqual(installed['agent_identity'], 'refreshed-offline')
        self.assertEqual(json.loads((self.target / 'synthetic-agent-proof.json').read_text()), {'lease_held': True})


if __name__ == '__main__':
    unittest.main()
