"""Engine-free wire contracts for Host/Origin/key precedence and CORS.

Every key, file, listener and inference lease belongs to this fixture. No model,
engine, real credential or upstream service is installed or contacted.
"""
import fcntl
import json
import os
from pathlib import Path
import sys
import unittest

from harness import Daemon

GOLDENS = Path(__file__).with_name('goldens') / 'http-policy.json'
KEY = 'synthetic-policy-credential'
AUTH = {'LOCAL_AI_API_KEY': KEY}
ORIGIN = 'https://example.invalid'
CORS = {'cors_origins': [ORIGIN]}
WILDCARD = {'cors_origins': ['*']}
NAVIGATION = [('Sec-Fetch-Mode', 'navigate'), ('Sec-Fetch-Dest', 'document'),
              ('Sec-Fetch-User', '?1'), ('Sec-Fetch-Site', 'cross-site')]
CASES = {
    'host-missing': {'hosts': []},
    'host-duplicate-same': {'hosts': ['127.0.0.1:<port>', '127.0.0.1:<port>']},
    'host-duplicate-conflicting': {'hosts': ['127.0.0.1:<port>', 'example.invalid']},
    'host-casefold': {'hosts': ['LOCALHOST:<port>']},
    'host-whitespace': {'hosts': [' localhost:<port> ']},
    'host-wrong-port': {'hosts': ['localhost:<other-port>']},
    'host-bare-loopback': {'hosts': ['127.0.0.1']},
    'absolute-target': {'path': 'http://127.0.0.1:<port>/v1/models'},
    'leading-double-slash': {'path': '//v1/models'},
    'content-length-trailing-space': {'method': 'POST', 'path': '/v1/conformance-missing', 'headers': [('Content-Length', '0 ')]},
    'origin-duplicate': {'headers': [('Origin', 'http://localhost:<port>')] * 2},
    'fetch-site-duplicate': {'headers': [('Sec-Fetch-Site', 'same-origin')] * 2},
    'fetch-site-cross-site': {'headers': [('Sec-Fetch-Site', 'cross-site')]},
    'fetch-site-same-site': {'headers': [('Sec-Fetch-Site', 'same-site')]},
    'origin-loopback': {'headers': [('Origin', 'http://[::1]:<port>')]},
    'origin-wrong-port': {'headers': [('Origin', 'http://localhost:<other-port>')]},
    'origin-trailing-space': {'headers': [('Origin', 'http://localhost:<port> ')]},
    'fetch-site-trailing-space': {'headers': [('Sec-Fetch-Site', 'same-origin ')]},
    'navigation-with-query': {'path': '/?synthetic=1', 'headers': NAVIGATION},
    'navigation-duplicate-mode': {'path': '/', 'headers': [*NAVIGATION, ('Sec-Fetch-Mode', 'navigate')]},
    'auth-empty-bearer-blocks-alternate': {'env': AUTH, 'headers': [('Authorization', 'Bearer '), ('x-api-key', KEY)]},
    'auth-alternate-header-priority': {'env': AUTH, 'headers': [('x-api-key', 'synthetic-wrong'), ('api-key', KEY)]},
    'auth-non-bearer-allows-alternate': {'env': AUTH, 'headers': [('Authorization', 'Basic synthetic'), ('x-api-key', KEY)]},
    'auth-mixed-case-bearer': {'env': AUTH, 'headers': [('Authorization', 'bEaReR  ' + KEY + ' ')]},
    'auth-duplicate-first-wrong': {'env': AUTH, 'headers': [('Authorization', 'Bearer synthetic-wrong'), ('Authorization', 'Bearer ' + KEY)]},
    'auth-local-route-exempt': {'env': AUTH, 'path': '/_local/conformance-missing'},
    'auth-health-exempt': {'env': AUTH, 'path': '/health'},
    'key-file-null': {'api': {'api_key_file': None}},
    'key-file-relative': {'api': {'api_key_file': 'synthetic-key'}},
    'key-file-not-string': {'api': {'api_key_file': 7}},
    'key-file-empty': {'keyfile': b''},
    'key-file-missing': {'keyfile': None},
    'key-file-invalid-utf8': {'keyfile': b'\xff\n'},
    'key-env-precedes-file': {'env': AUTH, 'keyfile': b'synthetic-other\n', 'headers': [('Authorization', 'Bearer ' + KEY)]},
    'key-blank-env-uses-file': {'env': {'LOCAL_AI_API_KEY': '  '}, 'keyfile': (KEY + '\n').encode(), 'headers': [('api-key', KEY)]},
    'key-file-symlink': {'keyfile': (KEY + '\n').encode(), 'symlink': True, 'headers': [('api-key', KEY)]},
    'key-file-unicode': {'keyfile': 'synthetic-\u00e9\n'.encode(), 'headers': [('Authorization', 'Bearer synthetic-\u00e9')]},
    'key-file-cr-line-ending': {'keyfile': (KEY + '\rsynthetic-ignored\n').encode(), 'headers': [('api-key', KEY)]},
    'cors-wildcard-with-key': {'api': WILDCARD, 'env': AUTH, 'headers': [('Origin', ORIGIN), ('api-key', KEY)]},
    'cors-wildcard-without-key': {'api': WILDCARD, 'headers': [('Origin', ORIGIN)]},
    'cors-null-exact-refused': {'api': {'cors_origins': ['null']}, 'headers': [('Origin', 'null')]},
    'cors-null-wildcard-with-key': {'api': WILDCARD, 'env': AUTH, 'headers': [('Origin', 'null'), ('api-key', KEY)]},
    'cors-local-route-refused': {'api': CORS, 'env': AUTH, 'path': '/_local/conformance-missing', 'headers': [('Origin', ORIGIN)]},
    'cors-preflight-empty-request-headers': {'api': CORS, 'method': 'OPTIONS', 'headers': [('Origin', ORIGIN), ('Access-Control-Request-Headers', '')]},
    'cors-preflight-default-request-headers': {'api': CORS, 'method': 'OPTIONS', 'headers': [('Origin', ORIGIN)]},
    'cors-preflight-no-authentication': {'api': CORS, 'env': AUTH, 'method': 'OPTIONS', 'headers': [('Origin', ORIGIN)]},
    'cors-auth-failure-retains-headers': {'api': CORS, 'env': AUTH, 'headers': [('Origin', ORIGIN)]},
    'cors-duplicate-origin-refused': {'api': CORS, 'env': AUTH, 'headers': [('Origin', ORIGIN)] * 2},
    'host-refusal-before-cors-and-auth': {'hosts': ['example.invalid'], 'api': CORS, 'env': AUTH, 'headers': [('Origin', ORIGIN)]},
}


def capture(name):
    case = CASES[name]
    config = {'openai_api': case['api']} if 'api' in case else {}
    with Daemon(engine_free=True, config_changes=config, environ=case.get('env')) as daemon:
        key_path = daemon.home / 'synthetic-key'
        if 'keyfile' in case:
            if case['keyfile'] is not None:
                target = daemon.home / 'synthetic-key-target' if case.get('symlink') else key_path
                target.write_bytes(case['keyfile'])
                target.chmod(0o600)
                if case.get('symlink'):
                    key_path.symlink_to(target.name)
            config.setdefault('openai_api', {})['api_key_file'] = str(key_path)
            daemon.config_path.write_text(json.dumps(config))
        original_config = daemon.config_path.read_bytes()
        original_key = key_path.read_bytes() if key_path.exists() else None
        replace = lambda value: value.replace('<port>', str(daemon.port)).replace(
            '<other-port>', str(daemon.port + 1 if daemon.port < 65535 else 1024))
        method, path = case.get('method', 'GET'), replace(case.get('path', '/v1/models'))
        headers = [('Host', replace(host)) for host in case.get('hosts', ['127.0.0.1:<port>'])]
        headers += [(key, replace(value)) for key, value in case.get('headers', [])]
        wire = (f'{method} {path} HTTP/1.1\r\nConnection: close\r\n' +
                ''.join(f'{key}: {value}\r\n' for key, value in headers) + '\r\n').encode('latin-1')
        lease = Path(daemon.env['LOCAL_AI_LEASE'])
        lease.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
        with lease.open('xb') as owner:
            owner.write(b'synthetic-other-owner\n')
            owner.flush()
            fcntl.flock(owner, fcntl.LOCK_EX | fcntl.LOCK_NB)
            before = lease.stat()
            result = daemon.call_wire(wire, method, path)
            after = lease.stat()
            if (before.st_dev, before.st_ino, before.st_mtime_ns) != (after.st_dev, after.st_ino, after.st_mtime_ns):
                raise AssertionError('HTTP policy mutated a foreign inference lease')
            if lease.read_bytes() != b'synthetic-other-owner\n':
                raise AssertionError('HTTP policy changed foreign lease content')
            status = daemon.call('GET', '/_local/status')['body']
            if status['loaded_model'] is not None or status['active_requests'] or daemon.events():
                raise AssertionError('HTTP policy started local inference')
        if daemon.config_path.read_bytes() != original_config:
            raise AssertionError('HTTP policy rewrote configuration')
        if original_key is not None and key_path.read_bytes() != original_key:
            raise AssertionError('HTTP policy changed the key file')
        if (daemon.home / 'fake-engine').exists() or (daemon.home / 'synthetic.gguf').exists():
            raise AssertionError('HTTP policy installed a local inference fixture')
        return result


class HttpPolicyContract(unittest.TestCase):
    def test_engine_free_wire_policy(self):
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
