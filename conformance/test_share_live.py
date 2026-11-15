"""Shared-listener contracts in a private network namespace, using loopback aliases only."""
from environment import PYTHON
from concurrent.futures import ThreadPoolExecutor
import fcntl
import hashlib
import hmac
import http.client
import json
import os
from pathlib import Path
import re
import shlex
import socket
import struct
import subprocess
import sys
import unittest

from harness import Daemon, ROOT, PRIMARY, SECONDARY, normalize_completion, sse_frames
from test_api import chat
from test_cli_runtime import CANONICAL_HOME, invoke_case
from test_lifecycle import status, wait_until
from test_ledger import SALT, seed as seed_ledger

GOLDENS = Path(__file__).with_name('goldens') / 'share-live.json'
ADDRESS = '100.64.0.2'
CASES = ('authentication', 'unloaded', 'responses', 'payload', 'routes', 'attachments',
         'preempt', 'rotation', 'rate', 'default-loaded', 'toggle', 'limits',
         'keepalive-off', 'refusal-closes', 'connections', 'preempt-stream')


def prepare_network(name, script=None, address=ADDRESS):
    # CAP_NET_ADMIN is granted only in the wrapper's new user + network namespaces.
    # Add a secondary address on lo; the host interfaces/routing are inaccessible.
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as control:
        request = struct.pack('16sH2s4s8s', b'lo:oxc-share', socket.AF_INET, b'\0' * 2,
                              socket.inet_aton(address), b'\0' * 8)
        fcntl.ioctl(control, 0x8916, request)
    # Drop every setup capability before starting the real controller and its
    # nested host fixture. No product process receives network administration.
    os.execve('/usr/bin/setpriv', ['/usr/bin/setpriv', '--bounding-set=-all', '--inh-caps=-all',
              '--ambient-caps=-all', '--securebits=+noroot,+noroot_locked', '--', PYTHON,
              str(Path(script or __file__).resolve()), '--case-ready', name], dict(os.environ))


def capture(name):
    home = CANONICAL_HOME
    binary_dir = home / 'bin'
    binary_dir.mkdir()
    executable = binary_dir / 'ip'
    source = (ROOT / 'conformance/fixtures/network_ip.py').read_text().split('\n', 1)[1]
    executable.write_text('#!' + PYTHON + '\n' + source)
    executable.chmod(0o700)
    network = home / 'network.json'
    network.write_text(json.dumps({'addresses': [{'ifname': 'tailscale-fixture', 'link_type': 'none',
                                                  'addr_info': [{'local': ADDRESS, 'prefixlen': 10}]}], 'routes': []}))
    environment = {'PATH': str(binary_dir) + ':/usr/bin:/bin', 'FAKE_SHARE_NETWORK': str(network),
                   'FAKE_ENGINE_HEADERS': '1'}
    if name == 'preempt-stream':
        environment['FAKE_ENGINE_STREAM_GATE'] = '1'
    settings = {'lan_share': 'tailscale'}
    if name == 'rate':
        settings['lan_share_options'] = {'rate_per_min': 1, 'burst': 1}
    elif name == 'limits':
        settings['lan_share_options'] = {'max_body_mb': 1, 'max_tokens': 2}
    with Daemon(configured=True, home=home, port=8092, environ=environment, config_changes=settings) as daemon:
        seed_ledger(daemon)
        sharing = daemon.call('GET', '/_local/share')['body']
        if sharing.get('active') is not True:
            raise AssertionError('Shared listener did not activate: ' + repr(sharing))
        key_path = daemon.config_path.resolve().parent.parent / 'lan-share-key.json'
        credential = json.loads(key_path.read_text())['key']
        fingerprint = 'sha256:' + hashlib.sha256(credential.encode()).hexdigest()[:12]
        if sharing['key_fingerprint'] != fingerprint or sharing['key_shown'] is not False:
            raise AssertionError('Startup key metadata is invalid')
        output = []
        credentials = [credential]

        def call(method='GET', path='/v1/models', body=None, auth=True, headers=None):
            supplied = {'Authorization': 'Bearer ' + credential} if auth else {}
            supplied.update(headers or {})
            if isinstance(body, (dict, list)):
                body = json.dumps(body)
                supplied.setdefault('Content-Type', 'application/json')
            connection = http.client.HTTPConnection(ADDRESS, daemon.port, timeout=5)
            try:
                connection.request(method, path, body=body, headers=supplied)
                response = connection.getresponse()
                raw = response.read().decode()
                request_id = response.getheader('X-Request-ID')
                if request_id is not None and not re.fullmatch(r'req_[0-9a-f]{32}', request_id):
                    raise AssertionError('Invalid shared request ID')
                content_type = response.getheader('Content-Type')
                value = None if method == 'HEAD' else sse_frames(raw) if content_type.startswith('text/event-stream') else json.loads(raw)
                row = {'status': response.status, 'content-type': content_type,
                       'x-request-id': '<generated:req_32hex>' if request_id is not None else None,
                       'body': normalize_completion(value)}
                for field in ('Retry-After', 'X-Local-AI-Warning', 'X-Local-AI-Ignored'):
                    if response.getheader(field) is not None:
                        row[field.lower()] = response.getheader(field)
                return row
            finally:
                connection.close()

        if name == 'authentication':
            output += [call(auth=False), call(headers={'Authorization': 'Bearer invalid'}),
                       call(headers={'X-Api-Key': credential}, auth=False),
                       call(headers={'Authorization': 'Bearer invalid', 'X-Api-Key': credential})]
        elif name == 'rate':
            output += [call(auth=False), call()]
        elif name == 'unloaded':
            output.append(call('POST', '/v1/chat/completions', chat()))
            output.append({'state': status(daemon)['state'], 'launches': len([r for r in daemon.events() if r['event'] == 'started'])})
        elif name == 'routes':
            for method, path, body in (('GET', '/_local/status', None), ('POST', '/_local/unload', {}),
                                       ('POST', '/v1/responses', {'input': 'synthetic'}),
                                       ('HEAD', '/v1/models/' + PRIMARY, None)):
                output.append(call(method, path, body))
        elif name in ('keepalive-off', 'refusal-closes'):
            connection = http.client.HTTPConnection(ADDRESS, daemon.port, timeout=2)
            try:
                connection.request('GET', '/v1/models', headers={'Authorization': 'Bearer ' +
                                   (credential if name == 'keepalive-off' else 'wrong')})
                response = connection.getresponse()
                output.append({'status': response.status, 'body': json.loads(response.read())})
                if name == 'keepalive-off':
                    output.append(daemon.call('POST', '/_local/share', {'mode': 'off'}))
                try:
                    closed = connection.sock is None or connection.sock.recv(1) == b''
                except ConnectionError:
                    closed = True
                output.append({'connection_closed': closed})
            finally:
                connection.close()
        elif name == 'connections':
            held = [socket.create_connection((ADDRESS, daemon.port), timeout=2) for _ in range(8)]
            try:
                with socket.create_connection((ADDRESS, daemon.port), timeout=2) as excess:
                    try:
                        closed = excess.recv(1) == b''
                    except ConnectionError:
                        closed = True
                    output.append({'ninth_connection_closed': closed})
                output.append({'mode': status(daemon)['state']})
            finally:
                for connection in held:
                    connection.close()
        else:
            output.append(daemon.call('POST', '/_local/load', {}))
            if name == 'responses':
                output += [call('POST', '/v1/chat/completions', chat()),
                           call('POST', '/v1/chat/completions', chat(stream=True, stream_options={'include_usage': True})),
                           call('POST', '/v1/completions', {'model': PRIMARY, 'prompt': 'synthetic'}),
                           call('POST', '/v1/chat/completions', chat(model=SECONDARY))]
            elif name == 'payload':
                payload = chat(max_tokens=20000, max_completion_tokens=30000, n=3, id_slot=7, cache_prompt=False,
                               n_keep=128, lora={'private': True}, share_token=credential,
                               chat_template_kwargs={'enable_thinking': True, 'preserve_thinking': False, 'private': True})
                output.append(call('POST', '/v1/chat/completions', payload, headers={
                    'X-Oxe-Session': 'c' * 16, 'X-Oxe-Turn-Kind': 'private', 'X-Oxe-Snapshot': 'keep',
                    'X-Oxe-Probe': 'yes', 'X-Session-Id': 'synthetic-remote-session',
                    'X-Local-AI-Origin': 'lab', 'Cookie': 'synthetic=private', 'X-Forwarded-For': '203.0.113.1'}))
            elif name == 'attachments':
                for url in ('http://127.0.0.1/private', 'file:///synthetic/private', 'data:image/png;base64,c3ludGhldGlj'):
                    output.append(call('POST', '/v1/chat/completions', chat(messages=[
                        {'role': 'user', 'content': [{'type': 'image_url', 'image_url': {'url': url}}]}])))
            elif name in ('preempt', 'preempt-stream'):
                with ThreadPoolExecutor(max_workers=1) as executor:
                    pending = executor.submit(call, 'POST', '/v1/chat/completions',
                                              chat(stream=name == 'preempt-stream', messages=[{'role': 'user',
                                                   'content': '[hold-stream]' if name == 'preempt-stream' else '[hold]'}]))
                    try:
                        daemon.wait_event(lambda row: row['event'] == ('stream-held' if name == 'preempt-stream' else 'held'))
                        output.append(daemon.call('POST', '/v1/chat/completions', chat()))
                        output.append(pending.result(timeout=5))
                    finally:
                        Path(daemon.env['FAKE_ENGINE_GATE']).touch()
                daemon.wait_event(lambda row: row['event'] == 'cancelled')
            elif name == 'rotation':
                output.append(call())
                entry = home / 'oxc'
                entry.symlink_to(ROOT / 'oxc/src/oxc/runtime/oxc')
                command = shlex.split(os.environ['OXC_COMMAND']) if 'OXC_COMMAND' in os.environ else [PYTHON, str(entry)]
                rotated = subprocess.run([*command, 'share', 'key', '--rotate'], cwd=home, env=daemon.env,
                                         capture_output=True, timeout=5)
                if rotated.returncode != 0:
                    raise AssertionError('Key rotation failed')
                credentials.append(json.loads(key_path.read_text())['key'])
                output.append(call())  # old key fails at the next request
                credential = credentials[-1]
                output.append(call())
            elif name == 'default-loaded':
                output.append(daemon.call('POST', '/_local/select', {'model': SECONDARY}))
                output.append(call('POST', '/v1/chat/completions', {'messages': [{'role': 'user', 'content': 'synthetic'}]}))
                current = status(daemon)
                output.append({'selected': current['selected_model'], 'loaded': current['loaded_model']})
            elif name == 'toggle':
                output.append(call())
                wait_until(lambda: daemon.call('GET', '/_local/share')['body']['remote_active'] == 0)
                output.append(daemon.call('POST', '/_local/share', {'mode': 'off'}))
                try:
                    connection = socket.create_connection((ADDRESS, daemon.port), timeout=.3)
                except OSError:
                    output.append({'off_listening': False})
                else:
                    connection.close()
                    raise AssertionError('Off left the shared listener open')
                output.append(daemon.call('POST', '/_local/share', {'mode': 'tailscale'}))
                output.append(call())
            elif name == 'limits':
                output.append(call('POST', '/v1/chat/completions', headers={'Content-Length': str(1024**2 + 1)}))
                output.append(call('POST', '/v1/chat/completions', headers={'Transfer-Encoding': 'chunked'}))
                output.append(call('POST', '/v1/chat/completions', chat(max_tokens=100)))
        wait_until(lambda: daemon.call('GET', '/_local/share')['body']['remote_active'] == 0)
        if name == 'payload':
            rows = daemon.call('GET', '/_local/ledger')['body']['rows']
            if len(rows) != 1:
                raise AssertionError('Shared inference did not produce exactly one ledger record')
            row = rows[0]
            expected_client = hmac.new(SALT, b'lan-share-client-v1\0' + ADDRESS.encode(), hashlib.sha256).hexdigest()[:16]
            expected_session = hmac.new(SALT, b'h:synthetic-remote-session', hashlib.sha256).hexdigest()[:16]
            if row.get('origin') != 'remote' or row.get('client_addr') != expected_client or row.get('session') != expected_session:
                raise AssertionError('Shared ledger origin/client/session is not controller-derived')
            output.append({'ledger': {'origin': row['origin'], 'client_addr': '<validated:keyed-client>',
                                      'session': '<validated:keyed-session>'}})
        final = daemon.call('GET', '/_local/share')['body']
        expected = 'sha256:' + hashlib.sha256(credential.encode()).hexdigest()[:12]
        if final['key_fingerprint'] != expected:
            raise AssertionError('Share metadata has a stale key')
        final['key_fingerprint'] = '<validated:key-fingerprint>'
        output.append({'sharing': final})
        def fingerprints(value):
            if isinstance(value, list):
                for item in value: fingerprints(item)
            elif isinstance(value, dict):
                if value.get('key_fingerprint') is not None and value['key_fingerprint'] != '<validated:key-fingerprint>':
                    known = ['sha256:' + hashlib.sha256(key.encode()).hexdigest()[:12] for key in credentials]
                    if value['key_fingerprint'] not in known:
                        raise AssertionError('Control reply published an unknown key')
                    value['key_fingerprint'] = '<validated:key-fingerprint>'
                for item in value.values(): fingerprints(item)
        fingerprints(output)
        events = daemon.events()
        output.append({'inference_requests': [row for row in events if row['event'] == 'request' and row['method'] == 'POST'],
                       'hints': [row['values'] for row in events if row['event'] == 'hints']})
        if name == 'payload' and any(any(value is not None for value in hint.values()) for hint in output[-1]['hints']):
            raise AssertionError('Shared requests forwarded private engine hints')
        # Validate keyed hints before masking their install-specific value.
        for hint in output[-1]['hints']:
            session = hint.get('X-Oxe-Session')
            if session is not None:
                if not re.fullmatch('[0-9a-f]{16}', session):
                    raise AssertionError('Invalid private engine hint')
                hint['X-Oxe-Session'] = '<validated:keyed-session>'
        public = json.dumps(output)
        if any(key in public or key in (home / 'daemon.log').read_text() for key in credentials):
            raise AssertionError('A share key escaped into public data or engine requests')
        return output


class ShareLiveContract(unittest.TestCase):
    def test_share_live_goldens(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(invoke_case(name, __file__, network_admin=True), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_DAEMON_COMMAND' in os.environ or 'OXC_COMMAND' in os.environ:
            raise SystemExit('Goldens must be recorded from the Python reference')
        GOLDENS.write_text(json.dumps({name: invoke_case(name, __file__, network_admin=True) for name in CASES}, indent=2, sort_keys=True) + '\n')
    elif sys.argv[1:2] == ['--case']:
        prepare_network(sys.argv[2])
    elif sys.argv[1:2] == ['--case-ready']:
        print(json.dumps(capture(sys.argv[2])))
    else:
        unittest.main()
