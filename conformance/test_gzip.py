"""Gzip framing against the real controller and exact loopback engine work."""
import gzip
import json
import os
from pathlib import Path
import sys
import unittest

from harness import Daemon
from test_api import chat

GOLDENS = Path(__file__).with_name('goldens') / 'http-gzip.json'
CASES = {
    'concatenated-json': 200,
    'empty-member-prefix': 200,
    'empty-member-suffix': 200,
    'trailing-zero-padding': 200,
    'zero-padding-between-members': 200,
    'invalid-second-crc': 400,
    'invalid-trailing-data': 400,
    'truncated-footer': 400,
    'concatenated-decompressed-limit': 400,
}


def capture(name):
    raw = json.dumps(chat()).encode()
    split = len(raw) // 2
    first = gzip.compress(raw, mtime=0)
    if name == 'concatenated-json':
        body = gzip.compress(raw[:split], mtime=0) + gzip.compress(raw[split:], mtime=0)
    elif name == 'empty-member-prefix':
        body = gzip.compress(b'', mtime=0) + first
    elif name == 'empty-member-suffix':
        body = first + gzip.compress(b'', mtime=0)
    elif name == 'trailing-zero-padding':
        body = first + b'\0' * 8
    elif name == 'zero-padding-between-members':
        body = gzip.compress(raw[:split], mtime=0) + b'\0' * 8 + gzip.compress(raw[split:], mtime=0)
    elif name == 'invalid-second-crc':
        second = bytearray(gzip.compress(b' ', mtime=0))
        second[-8] ^= 1
        body = first + second
    elif name == 'invalid-trailing-data':
        body = first + b'invalid'
    elif name == 'truncated-footer':
        body = first[:-4]
    elif name == 'concatenated-decompressed-limit':
        body = gzip.compress(raw.ljust(16 * 1024**2, b' '), mtime=0) + gzip.compress(b' ', mtime=0)
    else:
        raise AssertionError(name)
    with Daemon(configured=True) as daemon:
        reply = daemon.call('POST', '/v1/chat/completions', body, {'Content-Encoding': 'gzip'})
        if reply['status'] != CASES[name]:
            raise AssertionError(name + ': ' + repr(reply))
        requests = [{'path': row['path'], 'body': row['body']} for row in daemon.events()
                    if row['event'] == 'request' and row['method'] == 'POST']
        if len(requests) != (1 if CASES[name] == 200 else 0):
            raise AssertionError('Compressed request performed unexpected inference work')
        if CASES[name] != 200 and daemon.events():
            raise AssertionError('Invalid compressed request started an engine')
        return {'reply': reply, 'backend': requests}


class GzipContract(unittest.TestCase):
    def test_gzip_members_integrity_and_decompressed_limit(self):
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
