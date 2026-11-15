"""Engine-free HTTP foundations, before the cloud-only reference has landed.

These contracts are useful on any Linux x86_64/arm64 host. They are deliberately
not a cloud-only parity claim: this reference still has no remote catalog. No
model, engine executable, credentials or hardware fixture is installed.
"""
import fcntl
import gzip
import json
import os
from pathlib import Path
import sys
import unittest

from harness import Daemon
from test_http_gates import CASES as GATES, SYNTHETIC_KEY

GOLDENS = Path(__file__).with_name('goldens') / 'engine-free.json'
# The successful gzip case in the full gate suite starts its synthetic engine.
# Every selected case here must finish while a different process owns the lease.
CASES = {name: value[:4] for name, value in GATES.items() if name != 'valid-gzip'}
CASES.update({
    'unknown-route-body-is-not-json-parsed': ('POST', '/v1/not-a-route', '{', {}),
    'unknown-route-body-is-not-gzip-decoded': ('POST', '/v1/not-a-route', b'invalid', {'Content-Encoding': 'gzip'}),
    'gzip-invalid-second-member': ('POST', '/v1/chat/completions',
        gzip.compress(b'{"messages": [{"role": "user", "content": "Synthetic prompt."}]}', mtime=0)
        + gzip.compress(b'{invalid', mtime=0), {'Content-Encoding': 'gzip'}),
    'gzip-invalid-trailing-bytes': ('POST', '/v1/chat/completions',
        gzip.compress(b'{"messages": [{"role": "user", "content": "Synthetic prompt."}]}', mtime=0)
        + b'invalid', {'Content-Encoding': 'gzip'}),
})


def capture(name):
    method, path, body, headers = CASES[name]
    auth = name.startswith(('auth-', 'cors-'))
    config = {'openai_api': {'cors_origins': ['https://example.invalid']}} if name.startswith('cors-') else {}
    with Daemon(engine_free=True, config_changes=config,
                environ={'OXC_API_KEY': SYNTHETIC_KEY} if auth else {}) as daemon:
        lease = Path(daemon.env['OXC_LEASE'])
        lease.parent.mkdir(parents=True, mode=0o700)
        with lease.open('xb') as owner:
            owner.write(b'synthetic-other-owner\n')
            owner.flush()
            fcntl.flock(owner, fcntl.LOCK_EX | fcntl.LOCK_NB)
            before = lease.stat()
            if name == 'chunked-body-refused':
                raw = (f'POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{daemon.port}\r\n'
                       'Transfer-Encoding: chunked\r\nContent-Type: application/json\r\n'
                       'Connection: close\r\n\r\n').encode() + body
                reply = daemon.call_wire(raw, method, path)
            else:
                reply = daemon.call(method, path, body, headers=headers)
            status = daemon.call('GET', '/_local/status')['body']
            if (status['state'] != 'unloaded' or status['loaded_model'] is not None
                    or status['resident_bytes'] is not None or status['active_requests'] != 0):
                raise AssertionError('Engine-free HTTP unexpectedly acquired local inference')
            after = lease.stat()
            if (before.st_dev, before.st_ino, before.st_mtime_ns) != (after.st_dev, after.st_ino, after.st_mtime_ns):
                raise AssertionError('Engine-free HTTP mutated another inference lease')
            if lease.read_bytes() != b'synthetic-other-owner\n':
                raise AssertionError('Engine-free HTTP changed the lease contents')
            if daemon.events() or (daemon.home / 'fake-engine').exists() or (daemon.home / 'synthetic.gguf').exists():
                raise AssertionError('Engine-free HTTP installed or ran a local engine fixture')
            children = []
            for process in Path('/proc').iterdir():
                if not process.name.isdecimal():
                    continue
                try:
                    parent = process.joinpath('stat').read_text().rpartition(')')[2].split()[1]
                except FileNotFoundError:
                    continue  # An unrelated process exited during enumeration.
                if parent == str(daemon.process.pid):
                    children.append(process.name)
            if children:
                raise AssertionError('Engine-free HTTP left a child process')
            if json.loads(daemon.config_path.read_text()) != config:
                raise AssertionError('HTTP gate changed the runtime config')
            return reply


class EngineFreeHttpContract(unittest.TestCase):
    def test_gate_goldens_without_a_local_engine(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(capture(name), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_DAEMON_COMMAND' in os.environ:
            raise SystemExit('Record goldens only from the Python reference')
        GOLDENS.write_text(json.dumps({name: capture(name) for name in CASES}, indent=2, sort_keys=True) + '\n')
    else:
        unittest.main()
