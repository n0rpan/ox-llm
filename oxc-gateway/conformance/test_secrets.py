"""Request-time private file and incremental redaction contracts. Synthetic only."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
PYTHON = '/usr/bin/python3'
HOME = Path('/home/oxc-secret-contract')
KEY = 'synthetic-private-key'
GOLDENS = Path(__file__).with_name('secrets.json')
CASES = ('object', 'flat', 'missing', 'relative', 'directory', 'symlink', 'dangling',
         'parent-symlink', 'symlink-loop', 'git-directory', 'git-empty', 'git-file', 'source-tree',
         'mode', 'owner', 'oversized', 'size-boundary', 'bad-json', 'not-object',
         'missing-provider', 'empty-key', 'space-key', 'newline-key', 'unicode-key',
         'long-key', 'max-key', 'utf8-bom', 'utf16', 'utf32', 'nonfinite-extra',
         'rotation', 'json-redaction', 'stream-splits', 'stream-partial', 'stream-unicode')


def write(path, content, mode=0o600):
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode), 'wb') as out:
        out.write(content)


def fixture(name):
    path = HOME / 'provider.json'
    expected = KEY
    if name == 'max-key':
        expected = 'p' * 8192
    value = {'openrouter': {'api_key': expected}}
    raw = json.dumps(value).encode()
    if name == 'flat':
        raw = json.dumps({'openrouter': KEY}).encode()
    elif name == 'bad-json':
        raw = b'{broken'
    elif name == 'not-object':
        raw = b'[]'
    elif name == 'missing-provider':
        raw = b'{}'
    elif name in ('empty-key', 'space-key', 'newline-key', 'unicode-key', 'long-key'):
        key = {'empty-key': '', 'space-key': KEY + ' ', 'newline-key': KEY + '\n',
               'unicode-key': 'synthetic-\u03bb', 'long-key': 'p' * 8193}[name]
        raw = json.dumps({'openrouter': key}).encode()
    elif name == 'oversized':
        raw = raw + b' ' * (65537 - len(raw))
    elif name == 'size-boundary':
        raw = raw + b' ' * (65536 - len(raw))
    elif name in ('utf8-bom', 'utf16', 'utf32'):
        raw = json.dumps(value).encode({'utf8-bom': 'utf-8-sig', 'utf16': 'utf-16', 'utf32': 'utf-32'}[name])
    elif name == 'nonfinite-extra':
        raw = b'{"unused":[NaN,Infinity,-Infinity],"openrouter":{"api_key":"' + KEY.encode() + b'"}}'
    if name == 'directory':
        path.mkdir()
    elif name == 'owner':
        path = Path(os.environ['OXC_SECRET_OWNER_PATH'])
    elif name != 'missing':
        write(path, raw)
    if name in ('symlink', 'dangling'):
        if name == 'symlink':
            target = HOME / 'target.json'
            path.rename(target)
        else:
            path.unlink()
            target = HOME / 'missing-target.json'
        path.symlink_to(target)
    elif name == 'symlink-loop':
        path.unlink()
        path.symlink_to(path.name)
    elif name == 'parent-symlink':
        parent = HOME / 'actual'
        parent.mkdir()
        path.rename(parent / path.name)
        (HOME / 'link').symlink_to(parent, target_is_directory=True)
        path = HOME / 'link' / path.name
    elif name in ('git-directory', 'git-empty', 'git-file', 'source-tree'):
        parent = HOME / 'repository'
        parent.mkdir()
        path.rename(parent / path.name)
        path = parent / path.name
        if name == 'source-tree':
            (parent / 'AGENTS.md').write_text('Synthetic source tree.\n')
            (parent / 'engine').mkdir()
        elif name == 'git-file':
            (parent / '.git').write_text('gitdir: synthetic\n')
        elif name == 'git-empty':
            (parent / '.git').mkdir()  # a mount placeholder still counts: the check is not loosened
        else:
            (parent / '.git').mkdir()
            (parent / '.git' / 'HEAD').write_text('ref: refs/heads/main\n')
    elif name == 'mode':
        path.chmod(0o640)
    spec = {'path': 'provider.json' if name == 'relative' else str(path), 'expected': expected}
    if name == 'rotation':
        second = 'synthetic-rotated-key'
        replacement = HOME / 'replacement.json'
        write(replacement, json.dumps({'openrouter': second}).encode())
        spec.update(replace=str(replacement), second=second)
    elif name == 'json-redaction':
        spec['json'] = {KEY: ['before ' + KEY + ' after', {'error': KEY}],
                        'escaped': json.loads('"synthetic-\\u0070rivate-key"')}
    elif name == 'stream-splits':
        spec['runs'] = [['prefix ' + KEY[:index], KEY[index:] + ' suffix'] for index in range(1, len(KEY))]
    elif name == 'stream-partial':
        spec['runs'] = [['tail ' + KEY[:8]], [KEY + KEY, ':', KEY[:3]], ['ordinary text', '']]
    elif name == 'stream-unicode':
        spec['runs'] = [['\u03bb ' + KEY[:5], KEY[5:] + ' \U0001f600']]
    return spec


def capture(name):
    spec = fixture(name)
    native = os.environ.get('OXC_SECRETS_CONTRACT')
    if native:
        done = subprocess.run([native], input=json.dumps(spec), text=True, capture_output=True,
                              env={'PATH': '/usr/bin:/bin', 'HOME': str(HOME)}, timeout=5)
        if done.returncode:
            raise AssertionError(done.stderr)
        return json.loads(done.stdout)
    sys.path.insert(0, str(ROOT / 'oxc/src'))
    from oxc.runtime import cloud
    config = {'cloud': {'secrets_file': spec['path']}}
    try:
        key = cloud.secret('openrouter', config)
        if 'runs' in spec:
            outputs = []
            for run in spec['runs']:
                stream = cloud.StreamRedactor(key)
                outputs.append([stream.feed(part.encode()).decode() for part in run] + [stream.finish().decode()])
            return {'outputs': outputs}
        if 'json' in spec:
            return {'redacted': cloud.redact_value(spec['json'], key)}
        if 'replace' in spec:
            os.replace(spec['replace'], spec['path'])
            second = cloud.secret('openrouter', config)
            return {'first_ok': key == spec['expected'], 'second_ok': second == spec['second']}
        return {'ok': key == spec['expected']}
    except cloud.CloudError as error:
        message = str(error)
        if KEY in message or str(HOME) in message:
            raise AssertionError('Credential error exposed a value or path')
        return {'error': message}


def invoke(name):
    with tempfile.TemporaryDirectory(prefix='oxc-secret-contract-') as directory:
        env = {'HOME': str(HOME), 'PATH': '/usr/bin:/bin', 'LANG': 'C.UTF-8', 'LC_ALL': 'C.UTF-8'}
        native = os.environ.get('OXC_SECRETS_CONTRACT')
        if native:
            if not Path(native).is_absolute():
                raise AssertionError('Native contract binary must be absolute')
            env['OXC_SECRETS_CONTRACT'] = native
        if name == 'owner':
            if os.getuid() == 0:
                # Setup only, before entering the single-ID user namespace.
                target = Path(directory) / 'foreign.json'
                write(target, b'{"openrouter":"synthetic-unused"}', 0o644)
                os.chown(target, 65534, -1)
                env['OXC_SECRET_OWNER_PATH'] = str(HOME / target.name)
            else:
                # Public metadata only: owner is unmapped inside the user
                # namespace, and both readers must refuse before reading bytes.
                env['OXC_SECRET_OWNER_PATH'] = '/proc/uptime'
        done = subprocess.run(['bwrap', '--die-with-parent', '--unshare-net', '--unshare-user',
                               '--uid', '0', '--gid', '0', '--ro-bind', '/', '/',
                               '--dev-bind', '/dev', '/dev', '--tmpfs', '/home', '--tmpfs', '/tmp',
                               '--ro-bind', str(ROOT), str(ROOT), '--bind', directory, str(HOME),
                               '--chdir', str(HOME), '--', PYTHON, str(Path(__file__).resolve()), '--case', name],
                              env=env, capture_output=True, text=True, timeout=15)
        if done.returncode:
            raise AssertionError(done.stderr)
        result = json.loads(done.stdout)
        if name == 'symlink-loop':
            # Python 3.12 resolve() raises before open; 3.13's non-strict
            # resolve lets O_NOFOLLOW refuse the loop. Both are real reference
            # variants. Validate the full safe refusal, never mask success or
            # an arbitrary error, before normalizing interpreter wording.
            allowed = ('Cloud credentials are not configured.',
                       'Cloud credentials are not configured: the configured path is a symbolic link.')
            if result.get('error') not in allowed:
                raise AssertionError('Unexpected symlink-loop result: ' + repr(result))
            result = {'error': '<validated:interpreter-specific-loop-refusal>'}
        return result


class SecretsContract(unittest.TestCase):
    def test_reference_or_opt_in_private_reader_and_redaction(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(invoke(name), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_SECRETS_CONTRACT' in os.environ:
            raise SystemExit('Record only the Python reference')
        GOLDENS.write_text(json.dumps({name: invoke(name) for name in CASES}, indent=2, sort_keys=True) + '\n')
    elif sys.argv[1:2] == ['--case']:
        print(json.dumps(capture(sys.argv[2])))
    else:
        unittest.main()
