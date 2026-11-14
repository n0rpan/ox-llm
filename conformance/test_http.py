"""HTTP contracts against an unmodified daemon process in an empty installation."""
import http.client
import json
import os
import re
from pathlib import Path
import shlex
import socket
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
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


class Daemon:
    def __enter__(self):
        self.temp = tempfile.TemporaryDirectory(prefix='oxc-http-conformance-')
        home = Path(self.temp.name)
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            self.port = listener.getsockname()[1]
        env = {'HOME': str(home), 'PATH': '/usr/bin:/bin', 'LANG': 'C.UTF-8',
               'PYTHONPATH': str(ROOT / 'lab/src'), 'LOCAL_LLM_PORT': str(self.port),
               'LOCAL_LLM_CONFIG': str(home / 'runtime.json'),
               'LOCAL_AI_LEASE': str(home / 'inference.lock')}
        for key, subdir in (('XDG_CONFIG_HOME', 'config'), ('XDG_STATE_HOME', 'state'),
                            ('XDG_DATA_HOME', 'data'), ('XDG_CACHE_HOME', 'cache')):
            env[key] = str(home / subdir)
        command = shlex.split(os.environ['OXC_DAEMON_COMMAND']) if 'OXC_DAEMON_COMMAND' in os.environ else [
            sys.executable, '-m', 'snapdragon_ai_lab.runtime.controller']
        self.log = (home / 'daemon.log').open('w+')
        self.process = subprocess.Popen(command, cwd=home, env=env,
                                        stdout=self.log, stderr=self.log)
        try:
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if self.process.poll() is not None:
                    break
                try:
                    self.call('GET', '/v1/models', None)
                    return self
                except (OSError, http.client.HTTPException):
                    time.sleep(0.02)
            self.log.seek(0)
            raise AssertionError('Daemon failed to start: ' + self.log.read())
        except BaseException:
            self.__exit__(None, None, None)
            raise

    def __exit__(self, *_):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.log.close()
        self.temp.cleanup()

    def call(self, method, path, body):
        connection = http.client.HTTPConnection('127.0.0.1', self.port, timeout=3)
        try:
            connection.request(method, path, body=body,
                               headers={'Content-Type': 'application/json',
                                        'X-Request-ID': 'conformance-request'})
            response = connection.getresponse()
            payload = response.read().decode()
            request_id = response.getheader('X-Request-ID')
            if not request_id or not re.fullmatch(r'req_[0-9a-f]{32}', request_id):
                raise AssertionError('Invalid generated request ID: ' + repr(request_id))
            return {'status': response.status,
                    'content-type': response.getheader('Content-Type'),
                    'x-request-id': '<generated:req_32hex>',
                    'body': json.loads(payload)}
        finally:
            connection.close()


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
