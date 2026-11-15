"""Check the policy library's frozen contracts against the unmodified controller.

Only engine-free fixtures are needed here. Library tests check the same gate
decisions; admitted requests end at a test handler rather than a model router.
"""
import fcntl
import http.client
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest

from environment import ROOT, isolated_env
from http_policy_cases import CASES

GOLDENS = Path(__file__).with_name('goldens') / 'http-policy.json'


def write_private(path, data):
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), 'wb') as stream:
        stream.write(data)


def free_port():
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        return listener.getsockname()[1]


def read_reply(port, wire, method):
    with socket.create_connection(('127.0.0.1', port), timeout=5) as peer:
        peer.sendall(wire)
        with http.client.HTTPResponse(peer) as response:
            response.begin()
            raw = response.read()
            request_id = response.getheader('X-Request-ID')
            if not (method == 'OPTIONS' and response.status == 204 and request_id is None):
                if not request_id or not re.fullmatch(r'req_[0-9a-f]{32}', request_id):
                    raise AssertionError('Invalid generated request ID')
            result = {'status': response.status, 'content-type': response.getheader('Content-Type'),
                      'x-request-id': '<generated:req_32hex>' if request_id else None,
                      'body': None if method == 'HEAD' or response.status == 204 else json.loads(raw)}
            for name in ('Access-Control-Allow-Origin', 'Access-Control-Allow-Headers',
                         'Access-Control-Allow-Methods', 'Access-Control-Allow-Private-Network',
                         'Access-Control-Max-Age', 'Vary'):
                if response.getheader(name) is not None:
                    result[name.lower()] = response.getheader(name)
            return result


def capture(case):
    with tempfile.TemporaryDirectory(prefix='oxc-policy-reference-') as directory:
        home = Path(directory)
        ports = set()
        while len(ports) < 3:
            ports.add(free_port())
        port, backend, flash = sorted(ports)
        config = {'openai_api': dict(case['api'])} if 'api' in case else {}
        key = home / 'synthetic-key'
        if 'keyfile' in case:
            if case['keyfile'] is not None:
                target = home / 'synthetic-key-target' if case.get('symlink') else key
                write_private(target, case['keyfile'])
                if case.get('symlink'):
                    key.symlink_to(target.name)
            config.setdefault('openai_api', {})['api_key_file'] = str(key)
        path = home / 'runtime.json'
        write_private(path, json.dumps(config).encode())
        original_config = path.read_bytes()
        environment = isolated_env(home)
        environment.update({'LOCAL_LLM_PORT': str(port), 'LOCAL_LLM_BACKEND_PORT': str(backend),
                            'LOCAL_FLASH_BACKEND_PORT': str(flash), 'LOCAL_LLM_CONFIG': str(path),
                            'LOCAL_AI_LEASE': str(home / 'inference.lock'), 'LOCAL_AI_ENABLE_OXE': '1'})
        environment.update(case.get('env', {}))
        with (home / 'daemon.log').open('wb') as log:
            process = subprocess.Popen([sys.executable, '-m', 'snapdragon_ai_lab.runtime.controller'],
                                       cwd=home, env=environment, stdout=log, stderr=log,
                                       start_new_session=True)
            try:
                deadline = time.monotonic() + 10
                while True:
                    if process.poll() is not None:
                        raise AssertionError('Reference controller exited')
                    try:
                        with socket.create_connection(('127.0.0.1', port), timeout=.1):
                            break
                    except OSError:
                        if time.monotonic() >= deadline:
                            raise AssertionError('Reference controller did not start')
                        time.sleep(.02)
                replace = lambda value: value.replace('<port>', str(port)).replace(
                    '<other-port>', str(port + 1 if port < 65535 else 1024))
                method = case.get('method', 'GET')
                target = replace(case.get('path', '/v1/models'))
                headers = [('Host', replace(host)) for host in case.get('hosts', ['127.0.0.1:<port>'])]
                headers += [(name, replace(value)) for name, value in case.get('headers', [])]
                wire = (f'{method} {target} HTTP/1.1\r\nConnection: close\r\n' +
                        ''.join(f'{name}: {value}\r\n' for name, value in headers) + '\r\n').encode('latin-1')
                with (home / 'inference.lock').open('xb') as lease:
                    lease.write(b'synthetic-other-owner\n')
                    lease.flush()
                    fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    before = os.fstat(lease.fileno())
                    result = read_reply(port, wire, method)
                    after = (home / 'inference.lock').stat()
                    if (before.st_ino, before.st_mtime_ns) != (after.st_ino, after.st_mtime_ns):
                        raise AssertionError('Reference changed a foreign inference lease')
                    if (home / 'inference.lock').read_bytes() != b'synthetic-other-owner\n':
                        raise AssertionError('Reference changed foreign lease content')
                if path.read_bytes() != original_config:
                    raise AssertionError('Reference rewrote configuration')
                if case.get('keyfile') is not None and key.read_bytes() != case['keyfile']:
                    raise AssertionError('Reference rewrote its key file')
                return result
            finally:
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=5)
                except ProcessLookupError:
                    process.wait(timeout=5)


class PolicyReferenceContract(unittest.TestCase):
    def test_frozen_engine_free_policy(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name, case in CASES.items():
            with self.subTest(case=name):
                self.assertEqual(capture(case), expected[name])


if __name__ == '__main__':
    unittest.main()
