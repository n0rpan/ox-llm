"""Exercise the macOS lab installer with public-API/attestation/platform doubles; runs on Linux and macOS."""
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / 'tools/lab/dev-update-macos.sh'
NEEDED = ['jq', 'shasum', 'tar', 'find', 'mv', 'ln', 'diff', 'mktemp', 'awk', 'id', 'date',
          'cat', 'cp', 'mkdir', 'rm', 'readlink', 'grep', 'env', 'dirname']


@unittest.skipUnless(shutil.which('shasum') and shutil.which('jq'), 'needs shasum and jq')
class DevUpdateMacos(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()  # macOS temp dirs sit behind /var symlinks
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
        self.events = self.remote / 'events'
        self.env = {'PATH': ':'.join([str(self.bin), str(Path(shutil.which('shasum')).parent), os.defpath]), 'HOME': str(self.home),
                    'FIXTURE_ACCOUNT_HOME': str(self.home), 'FIXTURES': str(self.remote),
                    'FIXTURE_UNAME_S': 'Darwin', 'FIXTURE_UNAME_M': 'arm64',
                    'GH_TOKEN': 'synthetic-unused', 'GITHUB_TOKEN': 'synthetic-unused'}
        self.stub('uname', '''import os, sys
print(os.environ['FIXTURE_UNAME_S'] if sys.argv[1:] == ['-s'] else os.environ['FIXTURE_UNAME_M'])
''')
        self.stub('dscl', '''import os, sys
args = sys.argv[1:]
assert args[:2] == ['.', '-read'] and args[2].startswith('/Users/')
if args[3] == 'NFSHomeDirectory':
    print('NFSHomeDirectory: ' + os.environ['FIXTURE_ACCOUNT_HOME'])
else:
    assert args[3] == 'UniqueID'
    print('UniqueID: %d' % os.geteuid())
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
assert '--deny-self-hosted-runners' in args and '--bundle' in args
assert pathlib.Path(os.environ['GH_CONFIG_DIR']).parent.name.startswith('.download.')
fixtures = pathlib.Path(os.environ['FIXTURES'])
fixtures.joinpath('verified').write_text(args[args.index('--source-digest')+1])
with open(fixtures / 'events', 'a') as log:
    log.write('verify\\n')
sys.exit(int(os.environ.get('REJECT_VERIFY', '0')))
''')
        # Every file in a fresh payload "has" the quarantine attribute until it is removed.
        self.stub('xattr', '''import os, sys
args = sys.argv[1:]
assert args[1] == 'com.apple.quarantine'
log = os.path.join(os.environ['FIXTURES'], 'events')
if args[0] == '-p':
    sys.exit(0 if '/.download.' in args[2] else 1)
assert args[0] == '-d'
with open(log, 'a') as out:
    out.write('xattr -d ' + args[2] + '\\n')
''')
        self.stub('codesign', 'import sys\nsys.exit(0)\n')
        for name in ['spctl', 'sudo']:
            self.stub(name, '''import os, sys
with open(os.path.join(os.environ['FIXTURES'], 'events'), 'a') as out:
    out.write('FORBIDDEN ' + sys.argv[0] + '\\n')
sys.exit(1)
''')

    def stub(self, name, body):
        path = self.bin / name
        path.write_text('#!' + sys.executable + '\n' + body)
        path.chmod(0o755)

    def build(self, sha='a' * 40, entries=None, base='0.2.1'):
        version = 'v' + base + '-dev.12.' + sha[:7]
        name = 'ox-llm-' + version + '-macos-arm64.tar.gz'
        payload = b'\xcf\xfa\xed\xfe synthetic; never executable in a test'
        with tarfile.open(self.remote / name, 'w:gz') as archive:
            for path, body in (entries or [('usr/bin/oxa', payload)]):
                entry = tarfile.TarInfo(path)
                entry.size = len(body)
                entry.mode = 0o755
                archive.addfile(entry, io.BytesIO(body))
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

    def run_update(self, prefix=None, env=None):
        args = [shutil.which('bash'), str(SCRIPT)]
        if prefix is not None:
            args.append(str(prefix))
        return subprocess.run(args, env=env or self.env, text=True, capture_output=True, timeout=30)

    def test_installs_receipt_and_reuses_verified_build(self):
        sha, digest, payload = self.build()
        result = self.run_update()
        self.assertEqual(result.returncode, 0, result.stderr)
        receipt = json.loads((self.prefix / 'current/receipt.json').read_text())
        self.assertEqual((receipt['channel'], receipt['platform'], receipt['source_sha'], receipt['sha256']),
                         ('dev', 'macos-arm64', sha, digest))
        self.assertEqual((self.prefix / 'bin/oxa').read_bytes(), payload)
        self.assertEqual((self.remote / 'verified').read_text(), sha)
        self.assertEqual(os.readlink(self.prefix / 'bin'), 'current/usr/bin')
        self.assertEqual(self.run_update().returncode, 0)
        self.assertEqual(self.stable.read_bytes(), b'stable sentinel')
        self.assertFalse((self.prefix / '.update.lock.d').exists())

    def test_new_build_keeps_previous_for_rollback(self):
        self.build('a' * 40)
        self.assertEqual(self.run_update().returncode, 0)
        first = os.readlink(self.prefix / 'current')
        self.build('b' * 40)
        result = self.run_update()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotEqual(os.readlink(self.prefix / 'current'), first)
        self.assertEqual(os.readlink(self.prefix / 'previous'), first)
        self.assertTrue((self.prefix / first / 'usr/bin/oxa').is_file())

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
        self.assertEqual(receipt['version'], 'v0.2.1-dev.12+' + 'a' * 7)

    def test_checksum_or_provenance_refusal_preserves_previous_activation(self):
        self.build()
        self.assertEqual(self.run_update().returncode, 0)
        before = os.readlink(self.prefix / 'current')
        for refusal in ['checksum', 'provenance']:
            self.build('b' * 40)
            self.env['REJECT_VERIFY'] = '1' if refusal == 'provenance' else '0'
            if refusal == 'checksum':
                sums = self.remote / ('SHA256SUMS-v0.2.1-dev.12.' + 'b' * 7)
                sums.write_text('0' * 64 + sums.read_text()[64:])
            self.assertNotEqual(self.run_update().returncode, 0)
            self.assertEqual(os.readlink(self.prefix / 'current'), before)
            self.assertEqual(self.stable.read_bytes(), b'stable sentinel')

    def test_missing_gh_fails_closed_without_checksum_only_fallback(self):
        self.build()
        (self.bin / 'gh').unlink()
        clean = self.root / 'clean-bin'
        clean.mkdir()
        for tool in NEEDED:
            found = shutil.which(tool, path=self.env['PATH'])
            if found:
                (clean / tool).symlink_to(found)
        for tool in ['uname', 'dscl', 'curl', 'xattr', 'codesign']:
            (clean / tool).symlink_to(self.bin / tool)
        env = dict(self.env, PATH=str(clean))
        result = self.run_update(env=env)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('gh is required', result.stderr)
        self.assertFalse((self.remote / 'verified').exists())
        self.assertFalse((self.prefix / 'current').exists())

    def test_no_macos_asset_for_this_commit_keeps_current_and_says_so(self):
        self.build()
        self.assertEqual(self.run_update().returncode, 0)
        before = os.readlink(self.prefix / 'current')
        self.build('b' * 40)
        release = json.loads((self.remote / 'release.json').read_text())
        release['assets'] = [a for a in release['assets'] if 'macos' not in a['name']]
        (self.remote / 'release.json').write_text(json.dumps(release))
        result = self.run_update()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('no macos-arm64 asset', result.stderr)
        self.assertEqual(os.readlink(self.prefix / 'current'), before)

    def test_quarantine_is_removed_only_inside_the_new_release_after_verification(self):
        self.build()
        result = self.run_update()
        self.assertEqual(result.returncode, 0, result.stderr)
        events = self.events.read_text().splitlines()
        self.assertEqual(events[0], 'verify')
        removals = [e.split(' ', 2)[2] for e in events if e.startswith('xattr -d ')]
        self.assertTrue(removals)
        for path in removals:
            self.assertTrue(path.startswith(str(self.prefix) + '/.download.'), path)
        self.assertFalse([e for e in events if e.startswith('FORBIDDEN')])
        # A refused build never reaches the xattr step.
        self.events.unlink()
        self.build('b' * 40)
        self.env['REJECT_VERIFY'] = '1'
        self.assertNotEqual(self.run_update().returncode, 0)
        self.assertEqual(self.events.read_text().splitlines(), ['verify'])

    def test_unsafe_archive_entries_are_refused(self):
        self.build(entries=[('usr/bin/oxa', b'x'), ('usr/../evil', b'x')])
        self.assertNotEqual(self.run_update().returncode, 0)
        self.build(entries=[('usr/bin/oxa', b'x'), ('etc/passwd', b'x')])
        self.assertNotEqual(self.run_update().returncode, 0)
        self.assertFalse((self.prefix / 'current').exists())

    def test_refuses_other_platforms_prefixes_outside_the_protected_directory_and_sudo(self):
        self.build()
        for key, value in [('FIXTURE_UNAME_S', 'Linux'), ('FIXTURE_UNAME_M', 'x86_64'), ('SUDO_USER', 'root')]:
            result = self.run_update(env=dict(self.env, **{key: value}))
            self.assertNotEqual(result.returncode, 0, key)
        outside = self.root / 'elsewhere'
        outside.mkdir(mode=0o700)
        for prefix in [outside, self.protected / 'a/b', self.protected / '..' / 'x']:
            self.assertNotEqual(self.run_update(prefix).returncode, 0, prefix)
        shared = self.protected / 'shared'
        shared.mkdir()
        shared.chmod(0o770)
        self.assertNotEqual(self.run_update(shared).returncode, 0)
        self.assertEqual(list(shared.iterdir()), [])
        self.assertFalse((self.remote / 'verified').exists())
        self.assertEqual(self.stable.read_bytes(), b'stable sentinel')

    def test_unmarked_nonempty_prefix_and_held_lock_are_refused(self):
        self.build()
        self.stable.parent.parent.chmod(0o700)
        self.assertNotEqual(self.run_update(self.stable.parent.parent).returncode, 0)
        self.prefix.mkdir(mode=0o700, parents=True)
        (self.prefix / '.update.lock.d').mkdir()
        result = self.run_update()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.remote / 'verified').exists())


if __name__ == '__main__':
    unittest.main()
