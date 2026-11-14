"""HTTP contracts against an unmodified daemon process in an empty installation."""
import json
import os
from pathlib import Path
import sys
import unittest

from harness import Daemon

GOLDENS = Path(__file__).with_name('goldens') / 'http-empty.json'
CASES = {
    'models': ('GET', '/v1/models', None),
    'unknown-model': ('GET', '/v1/models/not-a-model', None),
    'unknown-route': ('GET', '/v1/not-a-route', None),
    'chat-invalid-json': ('POST', '/v1/chat/completions', '{'),
    'completion-invalid-json': ('POST', '/v1/completions', '{'),
    'chat-empty': ('POST', '/v1/chat/completions', '{}'),
    'completion-empty': ('POST', '/v1/completions', '{}'),
}


class EmptyInstallHttpContract(unittest.TestCase):
    def test_http_goldens(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        with Daemon() as daemon:
            for name, args in CASES.items():
                with self.subTest(case=name):
                    self.assertEqual(daemon.call(*args), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_DAEMON_COMMAND' in os.environ:
            raise SystemExit('Goldens must be recorded from the Python reference')
        with Daemon() as daemon:
            GOLDENS.write_text(json.dumps({name: daemon.call(*args) for name, args in CASES.items()},
                                         indent=2, sort_keys=True) + '\n')
    else:
        unittest.main()
