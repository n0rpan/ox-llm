"""Real 60-second shared-socket limits, observed concurrently in isolated networking."""
from environment import PYTHON
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import socket
import sys
import time
import unittest

from harness import Daemon, ROOT
from test_cli_runtime import CANONICAL_HOME, invoke_case
from test_lifecycle import status, wait_until
from test_share_live import prepare_network
from test_share_network import ADDRESS, generate_certificate

GOLDENS = Path(__file__).with_name('goldens') / 'share-io.json'


def capture():
    home = CANONICAL_HOME
    binary_dir = home / 'bin'
    binary_dir.mkdir()
    executable = binary_dir / 'ip'
    source = (ROOT / 'conformance/fixtures/network_ip.py').read_text().split('\n', 1)[1]
    executable.write_text('#!' + PYTHON + '\n' + source)
    executable.chmod(0o700)
    network = home / 'network.json'
    network.write_text(json.dumps({'addresses': [{'ifname': 'enp-fixture', 'link_type': 'ether',
                         'addr_info': [{'local': ADDRESS, 'prefixlen': 24}]}],
                         'routes': [{'dev': 'enp-fixture'}]}))
    environment = {'PATH': str(binary_dir) + ':/usr/bin:/bin', 'FAKE_SHARE_NETWORK': str(network)}
    plain_home, tls_home = home / 'plain', home / 'encrypted'
    plain_home.mkdir(); tls_home.mkdir()
    cert, key = generate_certificate(tls_home, 'sharing', ADDRESS)
    with Daemon(configured=True, home=plain_home, port=8092, environ=environment,
                config_changes={'lan_share': 'lan'}) as plain, \
         Daemon(configured=True, home=tls_home, port=8093, environ=environment,
                config_changes={'lan_share': 'lan', 'lan_share_options': {'tls_cert': str(cert), 'tls_key': str(key)}}) as tls:
        credential = json.loads((plain.config_path.resolve().parent.parent / 'lan-share-key.json').read_text())['key']
        def stalled(port, request):
            with socket.create_connection((ADDRESS, port), timeout=65) as client:
                start = time.monotonic()
                client.sendall(request)
                try:
                    closed = client.recv(1) == b''
                except ConnectionError:
                    closed = True
                elapsed = time.monotonic() - start
                if not 58 <= elapsed <= 65:
                    raise AssertionError('Shared I/O did not retain its 60-second limit: ' + str(elapsed))
                return {'connection_closed': closed, 'timeout': '<validated:60-second-io-timeout>'}
        with ThreadPoolExecutor(max_workers=3) as workers:
            pending = {
                'headers': workers.submit(stalled, plain.port, b'GET /v1/models HTTP/1.1\r\nHost: synthetic\r\n'),
                'body': workers.submit(stalled, plain.port, ('POST /v1/chat/completions HTTP/1.1\r\n'
                          'Host: synthetic\r\nAuthorization: Bearer ' + credential + '\r\n'
                          'Content-Type: application/json\r\nContent-Length: 100\r\n\r\n{').encode()),
                'tls_handshake': workers.submit(stalled, tls.port, b''),
            }
            wait_until(lambda: plain.call('GET', '/_local/share')['body']['remote_active'] == 1)
            output = {name: result.result(timeout=66) for name, result in pending.items()}
        wait_until(lambda: plain.call('GET', '/_local/share')['body']['remote_active'] == 0)
        for name, daemon in (('plain', plain), ('tls', tls)):
            sharing = daemon.call('GET', '/_local/share')['body']
            output[name] = {key: sharing[key] for key in ('remote_active', 'remote_requests', 'remote_refused')}
            output[name]['model_state'] = status(daemon)['state']
            output[name]['engine_launches'] = len([r for r in daemon.events() if r['event'] == 'started'])
        return output


class ShareIOContract(unittest.TestCase):
    def test_stalled_headers_body_and_tls_handshake(self):
        self.assertEqual(invoke_case('stalled', __file__, network_admin=True, timeout=75), json.loads(GOLDENS.read_text()))


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_DAEMON_COMMAND' in os.environ or 'OXC_COMMAND' in os.environ:
            raise SystemExit('Goldens must be recorded from the Python reference')
        GOLDENS.write_text(json.dumps(invoke_case('stalled', __file__, network_admin=True, timeout=75), indent=2, sort_keys=True) + '\n')
    elif sys.argv[1:2] == ['--case']:
        prepare_network(sys.argv[2], __file__, ADDRESS)
    elif sys.argv[1:2] == ['--case-ready']:
        print(json.dumps(capture()))
    else:
        unittest.main()
