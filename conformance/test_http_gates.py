"""Host/origin/authentication gates and HTTP framing, before engine admission."""
import gzip
import json
import os
from pathlib import Path
import sys
import unittest

from harness import Daemon
from test_api import chat

GOLDENS = Path(__file__).with_name('goldens') / 'http-gates.json'
SYNTHETIC_KEY = 'synthetic-conformance-credential'
CASES = {
    'head-models': ('HEAD', '/v1/models', None, {}, 200),
    'host-refused': ('GET', '/v1/models', None, {'Host': 'example.invalid'}, 403),
    'origin-refused': ('GET', '/v1/models', None, {'Origin': 'https://example.invalid'}, 403),
    'auth-missing': ('GET', '/v1/models', None, {}, 401),
    'auth-wrong': ('GET', '/v1/models', None, {'Authorization': 'Bearer synthetic-wrong'}, 401),
    'auth-bearer': ('GET', '/v1/models', None, {'Authorization': 'Bearer ' + SYNTHETIC_KEY}, 200),
    'auth-x-api-key': ('GET', '/v1/models', None, {'x-api-key': SYNTHETIC_KEY}, 200),
    'auth-api-key': ('GET', '/v1/models', None, {'api-key': SYNTHETIC_KEY}, 200),
    'cors-allowed': ('GET', '/v1/models', None, {'Origin': 'https://example.invalid',
                     'Authorization': 'Bearer ' + SYNTHETIC_KEY}, 200),
    # Bug-for-bug: the Python 204 preflight omits x-request-id.
    'cors-preflight-missing-request-id': ('OPTIONS', '/v1/chat/completions', None, {
        'Origin': 'https://example.invalid', 'Access-Control-Request-Method': 'POST',
        'Access-Control-Request-Headers': 'authorization, content-type',
        'Access-Control-Request-Private-Network': 'true'}, 204),
    'chunked-refused': ('POST', '/v1/chat/completions', None,
                         {'Transfer-Encoding': 'chunked', 'Content-Length': '0'}, 411),
    'chunked-body-refused': ('POST', '/v1/chat/completions', b'2\r\n{}\r\n0\r\n\r\n',
                             {'Transfer-Encoding': 'chunked'}, 411),
    'negative-length': ('POST', '/v1/chat/completions', None, {'Content-Length': '-1'}, 413),
    'oversize-length': ('POST', '/v1/chat/completions', None, {'Content-Length': '16777217'}, 413),
    'invalid-length': ('POST', '/v1/chat/completions', None, {'Content-Length': 'invalid'}, 400),
    'invalid-gzip': ('POST', '/v1/chat/completions', b'invalid', {'Content-Encoding': 'gzip'}, 400),
    'valid-gzip': ('POST', '/v1/chat/completions', gzip.compress(json.dumps(chat()).encode(), mtime=0),
                   {'Content-Encoding': 'gzip'}, 200),
    'percent-route-refused': ('POST', '/v1/%63hat/completions', chat(), {}, 404),
}


def run_case(name):
    method, path, body, headers, expected_status = CASES[name]
    auth = name.startswith('auth-') or name.startswith('cors-')
    config = {'openai_api': {'cors_origins': ['https://example.invalid']}} if name.startswith('cors-') else {}
    with Daemon(configured=True, environ={'LOCAL_AI_API_KEY': SYNTHETIC_KEY} if auth else {},
                config_changes=config) as daemon:
        if name == 'chunked-body-refused':
            raw = (f'POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{daemon.port}\r\n'
                   'Transfer-Encoding: chunked\r\nContent-Type: application/json\r\n'
                   'Connection: close\r\n\r\n').encode() + body
            reply = daemon.call_wire(raw, method, path)
        else:
            reply = daemon.call(method, path, body, headers=headers)
        if reply['status'] != expected_status:
            raise AssertionError(name + ': ' + repr(reply))
        if name != 'valid-gzip' and daemon.events():
            raise AssertionError('Gate unexpectedly started an engine: ' + name)
        return reply


class HttpGateContract(unittest.TestCase):
    def test_gate_goldens(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(run_case(name), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_DAEMON_COMMAND' in os.environ:
            raise SystemExit('Goldens must be recorded from the Python reference')
        GOLDENS.write_text(json.dumps({name: run_case(name) for name in CASES},
                                     indent=2, sort_keys=True) + '\n')
    else:
        unittest.main()
