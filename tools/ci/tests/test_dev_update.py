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
import time
import unittest

ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / 'tools/lab/dev-update.sh'


class Fixture(unittest.TestCase):
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
                    'GITHUB_TOKEN': 'synthetic-unused',
                    'FIXTURE_CANARY': 'http://127.0.0.1:8743/chat',
                    'FIXTURE_UNIT': 'oxa-lead.service',
                    'FIXTURE_BOARD_UNIT': 'oxa-board-sample.service'}
        self.stub('getent', '''import os, sys
uid = os.geteuid()
assert sys.argv[1:] == ['passwd', str(uid)]
print(f"fixture:x:{uid}:{os.getegid()}::{os.environ['FIXTURE_ACCOUNT_HOME']}:/bin/sh")
''')
        self.stub('curl', '''import json, os, pathlib, subprocess, sys
args = sys.argv[1:]
assert args[0] == '-q'
url = next(arg for arg in args if arg.startswith('http'))
with pathlib.Path(os.environ['FIXTURES'], 'requests.log').open('a') as log:
    log.write(url + '\\n')
if url.startswith('http://'):
    # The promotion canary: a loopback denial probe, never a real request.
    assert url == os.environ['FIXTURE_CANARY']
    assert args[args.index('--noproxy') + 1] == '*'
    with pathlib.Path(os.environ['FIXTURES'], 'canary.log').open('a') as log:
        log.write(url + '\\n')
    sys.stdout.write(os.environ.get('FIXTURE_DENY', '403'))
    sys.exit(0)
assert url.startswith(('https://api.github.com/repos/oxstack/ox-llm/',
                       'https://github.com/oxstack/ox-llm/releases/download/dev/'))
target = pathlib.Path(args[args.index('-o')+1])
if '/compare/' in url:
    if url.rsplit('/', 1)[1] == os.environ.get('FIXTURE_COMPARE_FAIL'):
        sys.exit(22)
    statuses = json.loads(pathlib.Path(os.environ['FIXTURES'], 'compare.json').read_text())
    target.write_text(json.dumps({'status': statuses[url.rsplit('/', 1)[1]]}))
elif url.endswith('/tags/dev'):
    target.write_bytes(pathlib.Path(os.environ['FIXTURES'], 'release.json').read_bytes())
else:
    target.write_bytes(pathlib.Path(os.environ['FIXTURES'], url.rsplit('/', 1)[1]).read_bytes())
    # The download itself must hold neither activation lock.
    for name, lock in [('session', os.environ.get('FIXTURE_SESSION', '')),
                       ('install', os.environ.get('FIXTURE_INSTALL_LOCK', ''))]:
        if not lock:
            continue
        probe = subprocess.run(['flock', '-n', lock + '.lock' if name == 'session' else lock,
                                'true'], capture_output=True)
        with pathlib.Path(os.environ['FIXTURES'], 'requests.log').open('a') as log:
            log.write(f'{name}-lock-{probe.returncode}\\n')
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

        # A user-unit double: the promotion only ever restarts the configured unit
        # and reads its MainPID, so the fixture records both and starts the exact
        # binary the queue names ('current' resolves through the install symlink).
        self.stub('systemctl', '''import json, os, pathlib, subprocess, sys
root = pathlib.Path(os.environ['FIXTURES'])
args = sys.argv[1:]
assert args[0] == '--user'
if args[1:4] == ['show', '--property=MainPID', '--value']:
    assert args[4] == os.environ['FIXTURE_UNIT']
    pid = root / 'service.pid'
    print(pid.read_text().strip() if pid.exists() else '0')
    sys.exit(0)
if args[1:4] == ['show', '--property=ActiveState', '--value']:
    assert args[4] == os.environ['FIXTURE_BOARD_UNIT']
    state = root / 'board.state'
    print(state.read_text().strip() if state.exists() else 'inactive')
    sys.exit(0)
assert args[1] == 'restart' and args[2] == os.environ['FIXTURE_UNIT']
# The activation must still hold the artifact-install lock here, so a sampler
# taking it shared cannot observe the window between the two renames.
probe = subprocess.run(['flock', '-n', os.environ['FIXTURE_INSTALL_LOCK'], 'true'],
                       capture_output=True)
with (root / 'requests.log').open('a') as log:
    log.write('install-lock-held\\n' if probe.returncode else 'install-lock-free\\n')
queue = json.loads((root / 'restarts.json').read_text())
target = queue.pop(0) if queue else 'lead'
(root / 'restarts.json').write_text(json.dumps(queue))
with (root / 'restarts.log').open('a') as log:
    log.write(target + '\\n')
# 'lead' runs whatever the configured regular executable now is.
path = os.environ['FIXTURE_BINARY'] if target == 'lead' else target
child = subprocess.Popen([path, '120'], stdin=subprocess.DEVNULL,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                         start_new_session=True)
(root / 'service.pid').write_text(str(child.pid))
with (root / 'service.pids').open('a') as spawned:
    spawned.write(str(child.pid) + '\\n')
''')

    def stub(self, name, body):
        path = self.bin / name
        path.write_text('#!' + sys.executable + '\n' + body)
        path.chmod(0o755)

    def build(self, sha='a' * 40, payload=None):
        version = 'v0.2.0-dev.' + sha[:7]
        name = 'ox-llm-' + version + '-linux-' + platform.machine() + '.tar.gz'
        if payload is None:
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

class DevUpdate(Fixture):
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

    def test_reusing_a_legacy_release_adds_its_executable_manifest(self):
        sha, _, payload = self.build()
        first = self.run_update()
        self.assertEqual(first.returncode, 0, first.stderr)
        release = self.prefix / os.readlink(self.prefix / 'current')
        (release / 'binary.sha256').unlink()

        repeated = self.run_update()
        self.assertEqual(repeated.returncode, 0, repeated.stderr)
        expected = hashlib.sha256(payload).hexdigest() + '  usr/bin/oxa\n'
        self.assertEqual((release / 'binary.sha256').read_text(), expected)
        self.assertEqual((release / 'binary.sha256').stat().st_mode & 0o777, 0o600)

    def test_checksum_or_provenance_refusal_preserves_previous_activation(self):
        self.build()
        self.assertEqual(self.run_update().returncode, 0)
        before = os.readlink(self.prefix / 'current')
        for refusal in ['checksum', 'provenance']:
            self.build('b' * 40)
            self.env['REJECT_VERIFY'] = '1' if refusal == 'provenance' else '0'
            if refusal == 'checksum':
                sums = self.remote / ('SHA256SUMS-v0.2.0-dev.' + 'b' * 7)
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


class DevPromote(Fixture):
    """The optional promotion mode over the operator's existing N0 units.

    The units run their own regular executable with a separate checksum
    manifest; the stock dev install only supplies the verified bytes.
    """

    def setUp(self):
        super().setUp()
        self.operator = self.root / 'operator'
        self.operator.mkdir(mode=0o700)
        self.units = self.operator / 'board-bin'
        self.units.mkdir(mode=0o700)
        self.boards = self.operator / 'board'
        self.boards.mkdir(mode=0o700)
        self.binary = self.units / 'oxa'
        # The manifest lives beside the board, not beside the executable, and
        # names it relatively, exactly as the N0 layout does.
        self.manifest = self.boards / 'binary.sha256'
        self.install_lock = self.operator / 'install.lock'
        self.install_lock.write_text('')
        self.install_lock.chmod(0o600)
        self.sessions = self.operator / 'sessions'
        self.sessions.mkdir(mode=0o700)
        self.session = self.sessions / 'lead.jsonl'
        self.session.write_text('')
        self.session.chmod(0o600)
        self.rollout = self.operator / 'rollout.json'
        self.env.update({'FIXTURE_BINARY': str(self.binary),
                         'FIXTURE_SESSION': str(self.session),
                         'FIXTURE_INSTALL_LOCK': str(self.install_lock)})
        (self.remote / 'restarts.json').write_text('[]')
        self.addCleanup(self.stop_unit)

    def stop_unit(self):
        pids = self.remote / 'service.pids'
        if not pids.exists():
            return
        for pid in pids.read_text().split():
            try:
                os.kill(int(pid), 15)
            except (ProcessLookupError, ValueError):
                pass

    def elf(self, sha):
        # A real ELF that stays alive so /proc/<pid>/exe is inspectable, with
        # per-build trailing bytes so each build's executable digest differs.
        return Path('/bin/sleep').read_bytes() + sha.encode()

    def install(self, sha):
        self.build(sha, payload=self.elf(sha))
        result = self.run_update()
        self.assertEqual(result.returncode, 0, result.stderr)
        return sha

    def deploy_binary(self, sha):
        self.binary.write_bytes(self.elf(sha))
        self.binary.chmod(0o755)
        digest = hashlib.sha256(self.elf(sha)).hexdigest()
        self.manifest.write_text(digest + '  ../board-bin/oxa\n')
        self.manifest.chmod(0o600)
        return digest

    def replace_binary(self, sha):
        # Replace the path while a process runs it, so the live inode differs
        # from what the path now holds.
        staged = self.units / 'oxa.replacement'
        staged.write_bytes(self.elf(sha))
        staged.chmod(0o755)
        os.replace(staged, self.binary)
        digest = hashlib.sha256(self.elf(sha)).hexdigest()
        self.manifest.write_text(digest + '  ../board-bin/oxa\n')
        self.manifest.chmod(0o600)
        return digest

    def start_unit(self):
        child = subprocess.Popen([str(self.binary), '120'], stdin=subprocess.DEVNULL,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                 start_new_session=True)
        (self.remote / 'service.pid').write_text(str(child.pid))
        with (self.remote / 'service.pids').open('a') as spawned:
            spawned.write(str(child.pid) + '\n')

    def running_digest(self):
        pid = int((self.remote / 'service.pid').read_text().strip())
        return hashlib.sha256(Path(f'/proc/{pid}/exe').read_bytes()).hexdigest()

    def compare(self, mapping):
        (self.remote / 'compare.json').write_text(json.dumps(mapping))

    def restarts(self):
        path = self.remote / 'restarts.log'
        return path.read_text().split() if path.exists() else []

    def requests(self):
        path = self.remote / 'requests.log'
        return path.read_text().split() if path.exists() else []

    def assets(self):
        return [line for line in self.requests() if line.endswith('.tar.gz')]

    def canary(self):
        path = self.remote / 'canary.log'
        return path.read_text().split() if path.exists() else []

    def promote(self, **overrides):
        args = ['bash', str(SCRIPT)]
        if overrides.get('prefix'):
            args.append(str(overrides['prefix']))
        args += ['--promote',
                 '--unit', overrides.get('unit', 'oxa-lead.service'),
                 '--board-unit', overrides.get('board_unit', 'oxa-board-sample.service'),
                 '--binary', str(overrides.get('binary', self.binary)),
                 '--manifest', str(overrides.get('manifest', self.manifest)),
                 '--install-lock', str(overrides.get('install_lock', self.install_lock)),
                 '--session', str(overrides.get('session', self.session)),
                 '--receipt', str(overrides.get('receipt', self.rollout))]
        if 'lead_url' in overrides:
            args += ['--lead-url', overrides['lead_url']]
        return subprocess.run(args, env=self.env, text=True, capture_output=True, timeout=90)

    def prepare(self, old='a' * 40, new='b' * 40):
        self.install(old)
        old_digest = self.deploy_binary(old)
        self.start_unit()
        self.build(new, payload=self.elf(new))
        self.compare({f'{old}...{new}': 'ahead', f'{new}...main': 'ahead'})
        return old, old_digest

    def test_activates_the_regular_executable_with_its_matching_manifest(self):
        old, old_digest = self.prepare()
        new = 'b' * 40
        result = self.promote()
        self.assertEqual(result.returncode, 0, result.stderr)
        new_digest = hashlib.sha256(self.elf(new)).hexdigest()
        self.assertEqual(self.binary.read_bytes(), self.elf(new))
        self.assertEqual(self.manifest.read_text(), new_digest + '  ../board-bin/oxa\n')
        self.assertEqual(self.binary.stat().st_mode & 0o777, 0o755)
        self.assertEqual(self.manifest.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.restarts(), ['lead'])
        self.assertEqual(self.canary(), ['http://127.0.0.1:8743/chat'])
        self.assertEqual(self.running_digest(), new_digest)
        self.assertIn('Promoted dev v0.2.0-dev.' + new[:7], result.stdout)
        receipt = json.loads(self.rollout.read_text())
        self.assertEqual((receipt['action'], receipt['verification']), ('promoted', 'verified'))
        self.assertEqual((receipt['source_sha'], receipt['binary_sha256']), (new, new_digest))
        self.assertEqual((receipt['previous_source_sha'], receipt['previous_binary_sha256']),
                         (old, old_digest))
        # The stock receipt keeps the archive digest; the executable digest is its own.
        stock = json.loads((self.prefix / 'current/receipt.json').read_text())
        self.assertNotEqual(stock['sha256'], new_digest)
        self.assertEqual((self.prefix / 'current/binary.sha256').read_text(),
                         new_digest + '  usr/bin/oxa\n')
        self.assertEqual((self.prefix / 'current/usr/bin/oxa').read_bytes(), self.elf(new))
        stage = self.prefix / 'promote' / new
        self.assertEqual((stage / 'previous/oxa').read_bytes(), self.elf(old))
        self.assertEqual((stage / 'previous.sha256').read_text().strip(), old_digest)
        # Neither activation lock is held while the artifact downloads.
        self.assertIn('session-lock-0', self.requests())
        self.assertIn('install-lock-0', self.requests())

    def test_unchanged_source_does_not_deploy_or_download(self):
        self.prepare()
        self.assertEqual(self.promote().returncode, 0)
        before, downloads = list(self.restarts()), self.assets()
        self.compare({'b' * 40 + '...' + 'b' * 40: 'identical'})
        result = self.promote()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('No deploy: unchanged source', result.stdout + result.stderr)
        self.assertEqual(self.restarts(), before)
        self.assertEqual(self.assets(), downloads)

    def test_older_source_does_not_deploy(self):
        self.prepare()
        self.assertEqual(self.promote().returncode, 0)
        before, downloads = list(self.restarts()), self.assets()
        # A release that is behind the build the units actually run.
        self.build('c' * 40, payload=self.elf('c' * 40))
        self.compare({'b' * 40 + '...' + 'c' * 40: 'behind'})
        result = self.promote()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('No deploy: older source', result.stdout + result.stderr)
        self.assertEqual(self.restarts(), before)
        self.assertEqual(self.assets(), downloads)

    def test_release_source_off_main_is_refused(self):
        old, old_digest = self.prepare()
        previous_downloads = self.assets()
        self.compare({f'{old}...{"b" * 40}': 'ahead', f'{"b" * 40}...main': 'diverged'})
        result = self.promote()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('the release source is not on main', result.stderr)
        self.assertEqual(self.restarts(), [])
        self.assertEqual(self.assets(), previous_downloads)
        self.assertEqual(hashlib.sha256(self.binary.read_bytes()).hexdigest(), old_digest)

    def test_failed_second_comparison_cannot_reuse_the_first_response(self):
        old, old_digest = self.prepare()
        new = 'b' * 40
        self.env['FIXTURE_COMPARE_FAIL'] = f'{new}...main'
        result = self.promote()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('source ancestry could not be checked', result.stderr)
        self.assertEqual(self.restarts(), [])
        self.assertEqual(hashlib.sha256(self.binary.read_bytes()).hexdigest(), old_digest)

    def test_busy_session_defers_without_replacing_the_pair(self):
        _, old_digest = self.prepare()
        previous_downloads = self.assets()
        holder = subprocess.Popen(['flock', str(self.session) + '.lock', 'sleep', '60'])
        self.addCleanup(holder.kill)
        for _ in range(100):
            if subprocess.run(['flock', '-n', str(self.session) + '.lock', 'true'],
                              capture_output=True).returncode:
                break
            time.sleep(0.02)
        result = self.promote()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('an owner chat turn is in flight', result.stdout + result.stderr)
        self.assertEqual(hashlib.sha256(self.binary.read_bytes()).hexdigest(), old_digest)
        self.assertEqual(self.restarts(), [])
        self.assertEqual(self.assets(), previous_downloads)

    def test_active_board_sampler_defers_without_replacing_the_pair(self):
        _, old_digest = self.prepare()
        previous_downloads = self.assets()
        (self.remote / 'board.state').write_text('activating')
        result = self.promote()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('the board sampler is not idle', result.stdout + result.stderr)
        self.assertEqual(hashlib.sha256(self.binary.read_bytes()).hexdigest(), old_digest)
        self.assertEqual(self.restarts(), [])
        self.assertEqual(self.assets(), previous_downloads)

    def test_operator_install_lock_blocks_only_the_activation(self):
        _, old_digest = self.prepare()
        holder = subprocess.Popen(['flock', str(self.install_lock), 'sleep', '60'])
        self.addCleanup(holder.kill)
        for _ in range(100):
            if subprocess.run(['flock', '-n', str(self.install_lock), 'true'],
                              capture_output=True).returncode:
                break
            time.sleep(0.02)
        downloads = len(self.assets())
        result = self.promote()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('another artifact install is in progress', result.stdout + result.stderr)
        self.assertEqual(hashlib.sha256(self.binary.read_bytes()).hexdigest(), old_digest)
        self.assertEqual(self.restarts(), [])
        # The verified bytes were downloaded and imported; only activation waited.
        self.assertEqual(len(self.assets()), downloads + 1)

        holder.terminate()
        holder.wait(timeout=5)
        retried = self.promote()
        self.assertEqual(retried.returncode, 0, retried.stderr)
        self.assertEqual(self.running_digest(), hashlib.sha256(self.elf('b' * 40)).hexdigest())

    def test_shared_sampler_lock_defers_the_activation(self):
        _, old_digest = self.prepare()
        holder = subprocess.Popen(['flock', '--shared', str(self.install_lock), 'sleep', '60'])
        self.addCleanup(holder.kill)
        for _ in range(100):
            if subprocess.run(['flock', '-n', str(self.install_lock), 'true'],
                              capture_output=True).returncode:
                break
            time.sleep(0.02)
        result = self.promote()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('another artifact install is in progress', result.stdout + result.stderr)
        self.assertEqual(hashlib.sha256(self.binary.read_bytes()).hexdigest(), old_digest)
        self.assertEqual(self.restarts(), [])

    def test_activation_holds_the_artifact_lock_across_the_two_renames(self):
        self.prepare()
        result = self.promote()
        self.assertEqual(result.returncode, 0, result.stderr)
        # The unit double probes the lock while restarting: the activation must
        # still hold it there, so a sampler cannot observe the split window.
        self.assertIn('install-lock-held', self.requests())

    def test_failed_running_digest_restores_the_preserved_pair(self):
        old, old_digest = self.prepare()
        stale = self.operator / 'stale'
        stale.mkdir(mode=0o700)
        (stale / 'oxa').write_bytes(self.elf(old))
        (stale / 'oxa').chmod(0o755)
        (self.remote / 'restarts.json').write_text(json.dumps([str(stale / 'oxa')]))
        result = self.promote()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f'rolled back to the previous binary ({old_digest})',
                      result.stdout + result.stderr)
        self.assertEqual(self.binary.read_bytes(), self.elf(old))
        self.assertEqual(self.manifest.read_text(), old_digest + '  ../board-bin/oxa\n')
        self.assertEqual(self.running_digest(), old_digest)
        self.assertEqual(self.restarts(), [str(stale / 'oxa'), 'lead'])
        receipt = json.loads(self.rollout.read_text())
        self.assertEqual((receipt['action'], receipt['verification']), ('failed', 'verified'))
        self.assertEqual(receipt['source_sha'], old)
        self.assertEqual(receipt['binary_sha256'], old_digest)
        stock = json.loads((self.prefix / 'releases' /
                            next(path.name for path in (self.prefix / 'releases').iterdir()
                                 if path.name.startswith(old))).joinpath('receipt.json').read_text())
        self.assertEqual(receipt['archive_sha256'], stock['sha256'])

    def test_denied_direct_get_failure_also_rolls_back(self):
        old, old_digest = self.prepare()
        self.env['FIXTURE_DENY'] = '200'
        result = self.promote()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('rolled back to the previous binary', result.stdout + result.stderr)
        self.assertEqual(self.binary.read_bytes(), self.elf(old))
        self.assertEqual(self.running_digest(), old_digest)
        self.assertEqual(json.loads(self.rollout.read_text())['action'], 'failed')

    def test_replaced_executable_is_not_taken_for_the_running_binary(self):
        self.install('a' * 40)
        self.deploy_binary('a' * 40)
        self.start_unit()
        # Replace the executable at its path: the process keeps the old inode,
        # so the live /proc digest is not what the path now holds.
        self.replace_binary('d' * 40)
        self.build('b' * 40, payload=self.elf('b' * 40))
        self.compare({f'{"a" * 40}...{"b" * 40}': 'ahead', f'{"b" * 40}...main': 'ahead'})
        result = self.promote()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('does not match its executable path', result.stderr)
        self.assertEqual(self.restarts(), [])
        self.assertEqual(self.binary.read_bytes(), self.elf('d' * 40))

    def test_source_baseline_requires_a_matching_executable_digest(self):
        self.install('a' * 40)
        # The units run a linked binary the stock install did not produce.
        deployed = self.deploy_binary('d' * 40)
        self.start_unit()
        self.build('b' * 40, payload=self.elf('b' * 40))
        self.compare({f'{"a" * 40}...{"b" * 40}': 'ahead', f'{"b" * 40}...main': 'ahead'})
        downloads = self.assets()
        result = self.promote()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('no verified source baseline', result.stderr)
        self.assertEqual(self.restarts(), [])
        self.assertEqual(self.assets(), downloads)
        self.assertEqual(hashlib.sha256(self.binary.read_bytes()).hexdigest(), deployed)

    def test_promotion_refuses_a_prefix_that_was_never_installed(self):
        self.build()
        result = self.promote()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('the dev prefix is not installed', result.stderr)
        self.assertEqual(self.requests(), [])

    def test_promotion_refuses_inputs_it_cannot_activate_safely(self):
        self.prepare()
        downloads, restarts = len(self.assets()), list(self.restarts())
        missing = subprocess.run(['bash', str(SCRIPT), '--promote', '--unit', 'oxa-lead.service'],
                                 env=self.env, text=True, capture_output=True, timeout=30)
        self.assertEqual(missing.returncode, 2, missing.stderr)
        remote = self.promote(lead_url='http://198.51.100.1:8743/chat')
        self.assertNotEqual(remote.returncode, 0)
        self.assertIn('canary outside loopback', remote.stderr)
        self.boards.chmod(0o770)
        unsafe = self.promote()
        self.boards.chmod(0o700)
        self.assertNotEqual(unsafe.returncode, 0)
        self.assertIn('manifest directory that is not trusted', unsafe.stderr)
        self.sessions.chmod(0o777)
        open_directory = self.promote()
        self.sessions.chmod(0o700)
        self.assertNotEqual(open_directory.returncode, 0)
        self.assertIn('untrusted promotion input directory', open_directory.stderr)
        self.assertEqual(self.restarts(), restarts)
        self.assertEqual(len(self.assets()), downloads)


if __name__ == '__main__':
    unittest.main()
