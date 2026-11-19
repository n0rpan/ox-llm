"""Exercise the actual lab installer with public-API/attestation doubles; no network or binaries run."""
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / 'tools/lab/dev-update.sh'


class DevUpdate(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.remote = self.root / 'remote'
        self.remote.mkdir()
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.home = self.root / 'home'
        self.protected = self.home / '.local/state/oxc/dev-builds'
        self.prefix = self.protected / 'default'
        self.stable = self.protected / 'stable/bin/oxa'
        self.stable.parent.mkdir(parents=True)
        self.stable.write_bytes(b'stable sentinel')
        self.env = {'PATH': str(self.bin) + ':' + os.defpath, 'HOME': str(self.home),
                    'FIXTURE_ACCOUNT_HOME': str(self.home),
                    'FIXTURES': str(self.remote), 'GH_TOKEN': 'synthetic-unused',
                    'GITHUB_TOKEN': 'synthetic-unused'}
        self.stub('getent', '''import os, sys
uid = os.geteuid()
assert sys.argv[1:] == ['passwd', str(uid)]
print(f"fixture:x:{uid}:{os.getegid()}::{os.environ['FIXTURE_ACCOUNT_HOME']}:/bin/sh")
''')
        self.stub('curl', '''import os, pathlib, shutil, sys
args = sys.argv[1:]
assert args[0] == '-q'
url = next(arg for arg in args if arg.startswith('https://'))
assert url.startswith(('https://api.github.com/repos/oxstack/ox-llm/',
                       'https://github.com/oxstack/ox-llm/releases/download/dev/'))
name = 'release.json' if url.endswith('/tags/dev') else url.rsplit('/', 1)[1]
shutil.copyfile(pathlib.Path(os.environ['FIXTURES']) / name, args[args.index('-o')+1])
''')
        self.stub('gh', '''import os, pathlib, sys
args = sys.argv[1:]
assert not any(key in os.environ for key in ['GH_TOKEN', 'GITHUB_TOKEN'])
assert args[:2] == ['attestation', 'verify']
assert args[args.index('--source-ref')+1] == 'refs/heads/main'
assert args[args.index('--signer-workflow')+1] == 'oxstack/ox-llm/.github/workflows/dev-build.yml'
assert '--deny-self-hosted-runners' in args
assert pathlib.Path(os.environ['GH_CONFIG_DIR']).parent.name.startswith('.download.')
pathlib.Path(os.environ['FIXTURES'], 'verified').write_text(args[args.index('--source-digest')+1])
sys.exit(int(os.environ.get('REJECT_VERIFY', '0')))
''')

    def stub(self, name, body):
        path = self.bin / name
        path.write_text('#!' + sys.executable + '\n' + body)
        path.chmod(0o755)

    def build(self, sha='a' * 40, base='0.2.1'):
        version = 'v' + base + '-dev.' + sha[:7]
        name = 'ox-llm-' + version + '-linux-' + platform.machine() + '.tar.gz'
        payload = b'\x7fELF synthetic; never executable in a test'
        with tarfile.open(self.remote / name, 'w:gz') as archive:
            entry = tarfile.TarInfo('usr/bin/oxa')
            entry.size = len(payload)
            entry.mode = 0o755
            archive.addfile(entry, io.BytesIO(payload))
        digest = hashlib.sha256((self.remote / name).read_bytes()).hexdigest()
        sums = 'SHA256SUMS-' + version
        (self.remote / sums).write_text(digest + '  ' + name + '\n')
        bundle = 'ox-llm-' + version + '.provenance.jsonl'
        (self.remote / bundle).write_text('{}\n')
        assets = [{'name': n, 'state': 'uploaded',
                   'browser_download_url': 'https://github.com/oxstack/ox-llm/releases/download/dev/' + n}
                  for n in [name, sums, bundle]]
        (self.remote / 'release.json').write_text(json.dumps(
            {'tag_name': 'dev', 'prerelease': True, 'draft': False,
             'target_commitish': sha, 'assets': assets}))
        return sha, digest, payload

    def run_update(self, prefix=None):
        args = ['bash', str(SCRIPT)]
        if prefix is not None:
            args.append(str(prefix))
        return subprocess.run(args, env=self.env,
                              text=True, capture_output=True, timeout=20)

    def test_installs_receipt_and_reuses_verified_build_without_touching_stable(self):
        sha, digest, payload = self.build()
        result = self.run_update()
        self.assertEqual(result.returncode, 0, result.stderr)
        receipt = json.loads((self.prefix / 'current/receipt.json').read_text())
        self.assertEqual((receipt['channel'], receipt['source_sha'], receipt['sha256']), ('dev', sha, digest))
        self.assertTrue(receipt['installed_at'].endswith('Z'))
        self.assertEqual((self.prefix / 'bin/oxa').read_bytes(), payload)
        self.assertEqual((self.remote / 'verified').read_text(), sha)
        self.assertEqual(self.run_update().returncode, 0)
        self.assertEqual(self.stable.read_bytes(), b'stable sentinel')

    def test_old_name_installs_and_newest_semver_wins(self):
        self.build(base='0.2.0')
        self.assertEqual(self.run_update().returncode, 0)
        self.build(base='0.2.1')
        release = json.loads((self.remote / 'release.json').read_text())
        old = [dict(a, name=a['name'].replace('v0.2.1', 'v0.2.0')) for a in release['assets']]
        release['assets'] = old + release['assets']
        (self.remote / 'release.json').write_text(json.dumps(release))
        self.assertEqual(self.run_update().returncode, 0)
        receipt = json.loads((self.prefix / 'current/receipt.json').read_text())
        self.assertEqual(receipt['version'], 'v0.2.1-dev.' + 'a' * 7)

    def test_checksum_or_provenance_refusal_preserves_previous_activation(self):
        self.build()
        self.assertEqual(self.run_update().returncode, 0)
        before = os.readlink(self.prefix / 'current')
        for refusal in ['checksum', 'provenance']:
            self.build('b' * 40)
            self.env['REJECT_VERIFY'] = '1' if refusal == 'provenance' else '0'
            if refusal == 'checksum':
                sums = self.remote / ('SHA256SUMS-v0.2.1-dev.' + 'b' * 7)
                sums.write_text('0' * 64 + sums.read_text()[64:])
            self.assertNotEqual(self.run_update().returncode, 0)
            self.assertEqual(os.readlink(self.prefix / 'current'), before)
            self.assertEqual(self.stable.read_bytes(), b'stable sentinel')

    def test_reinstalling_current_build_keeps_previous_distinct_build(self):
        self.build('a' * 40)
        first = self.run_update()
        self.assertEqual(first.returncode, 0, first.stderr)
        previous = self.prefix / os.readlink(self.prefix / 'current')
        receipt = (previous / 'receipt.json').read_bytes()

        self.build('b' * 40)
        second = self.run_update()
        self.assertEqual(second.returncode, 0, second.stderr)
        current = os.readlink(self.prefix / 'current')
        self.assertNotEqual(self.prefix / current, previous)

        repeated = self.run_update()
        self.assertEqual(repeated.returncode, 0, repeated.stderr)
        self.assertEqual(os.readlink(self.prefix / 'current'), current)
        self.assertEqual((previous / 'receipt.json').read_bytes(), receipt)
        self.assertTrue((previous / 'usr/bin/oxa').is_file())

        self.build('c' * 40)
        third = self.run_update()
        self.assertEqual(third.returncode, 0, third.stderr)
        self.assertFalse(previous.exists())
        self.assertTrue((self.prefix / current / 'receipt.json').is_file())

    def test_unmarked_stable_prefix_is_refused_without_creating_a_lock(self):
        self.build()
        self.stable.parent.parent.chmod(0o700)
        result = self.run_update(self.stable.parent.parent)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(sorted(p.relative_to(self.stable.parent.parent).as_posix()
                                for p in self.stable.parent.parent.rglob('*')), ['bin', 'bin/oxa'])
        self.assertEqual(self.stable.read_bytes(), b'stable sentinel')

    def test_shared_prefix_is_refused_before_writing_state(self):
        self.build()
        for mode in [0o770, 0o707]:
            with self.subTest(mode=oct(mode)):
                prefix = self.protected / f'dev-{mode:o}'
                prefix.mkdir()
                prefix.chmod(mode)
                result = self.run_update(prefix)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(list(prefix.iterdir()), [])
                self.assertFalse((self.remote / 'verified').exists())

    def test_shared_nonsticky_ancestor_is_refused_with_a_private_prefix(self):
        self.build()
        parent = self.home / '.local/state'
        prefix = self.prefix
        prefix.mkdir(mode=0o700, parents=True)
        parent.chmod(0o777)
        result = self.run_update(prefix)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(list(prefix.iterdir()), [])
        self.assertFalse((self.remote / 'verified').exists())

    def test_trusted_sticky_ancestor_allows_a_private_prefix(self):
        sha, _, _ = self.build()
        parent = self.home / '.local/state'
        parent.chmod(0o1777)
        prefix = self.prefix
        result = self.run_update(prefix)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads((prefix / 'current/receipt.json').read_text())['source_sha'], sha)

    def test_aliased_or_nonregular_lock_cannot_touch_another_install(self):
        self.build()
        for kind in ['symlink', 'hardlink', 'fifo']:
            prefix = self.protected / kind
            prefix.mkdir(mode=0o700)
            (prefix / '.dev-channel').write_text('ox-llm dev v1\n')
            lock = prefix / '.update.lock'
            if kind == 'symlink':
                lock.symlink_to(self.stable)
            elif kind == 'hardlink':
                os.link(self.stable, lock)
            else:
                os.mkfifo(lock)
            result = self.run_update(prefix)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(self.stable.read_bytes(), b'stable sentinel')
            self.assertFalse((prefix / 'current').exists())

    def test_workspace_prefix_is_refused_even_with_redirected_home(self):
        self.build()
        workspace = self.root / 'project'
        prefix = workspace / '.local/state/oxc/dev-builds/preview'
        prefix.mkdir(mode=0o700, parents=True)
        self.env['HOME'] = str(workspace)
        result = self.run_update(prefix)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(list(prefix.iterdir()), [])
        self.assertFalse((self.remote / 'verified').exists())


if __name__ == '__main__':
    unittest.main()
