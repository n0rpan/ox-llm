"""LAN pinning/discovery through real controllers, using isolated loopback aliases."""
from environment import PYTHON
import copy
import hashlib
import http.client
import json
import os
from pathlib import Path
import ssl
import subprocess
import sys
import time
import unittest

from harness import Daemon, ROOT
from test_api import chat
from test_cli_runtime import CANONICAL_HOME, invoke_case
from test_lifecycle import status, wait_until
from test_share_live import prepare_network

GOLDENS = Path(__file__).with_name('goldens') / 'share-network.json'
ADDRESS = '192.168.50.2'
PIN = {'interface': 'enp-fixture', 'subnet': '192.168.50.0/24',
       'gateway': '192.168.50.1', 'gateway_mac': '02:00:00:00:00:01'}
CASES = ('lan-start', 'lan-enable', 'lan-matching', 'lan-invalid-pin', 'lan-absent',
         'lan-unpinned-absent', 'lan-interface-changed', 'lan-subnet-changed',
         'lan-gateway-changed', 'lan-mac-changed', 'lan-unverified', 'lan-no-mac-pin',
         'lan-vpn-default', 'lan-unavailable', 'lan-default-second', 'lan-watch-resume',
         'ts-no-interface', 'ts-invalid-address-json', 'ts-invalid-route-json',
         'ts-tool-error', 'tls-invalid', 'ts-bind-failure', 'lan-no-gateway',
         'lan-public-address', 'lan-failed-neighbor', 'tls-active', 'tls-1.2',
         'tls-invalid-pem', 'tls-mismatch', 'tls-relative', 'tls-disconnect')


def generate_certificate(home, stem, address):
    cert, key = home / (stem + '-cert.pem'), home / (stem + '-key.pem')
    subprocess.run(['/usr/bin/openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes',
                    '-keyout', str(key), '-out', str(cert), '-days', '1',
                    '-subj', '/CN=synthetic.invalid', '-addext', 'subjectAltName=IP:' + address],
                   stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                   check=True, timeout=10)
    return cert, key


def capture(name):
    home = CANONICAL_HOME
    binary_dir = home / 'bin'
    binary_dir.mkdir()
    executable = binary_dir / 'ip'
    source = (ROOT / 'conformance/fixtures/network_ip.py').read_text().split('\n', 1)[1]
    executable.write_text('#!' + PYTHON + '\n' + source)
    executable.chmod(0o700)
    fixture = {'addresses': [{'ifname': PIN['interface'], 'link_type': 'ether',
                             'addr_info': [{'local': ADDRESS, 'prefixlen': 24}]}],
               'routes': [{'dev': PIN['interface'], 'gateway': PIN['gateway']}],
               'neighbors': [{'lladdr': PIN['gateway_mac'].upper(), 'state': ['REACHABLE']}]}
    settings = {'lan_share': 'lan', 'share_contract_unknown': {'preserve': True}}
    if name in ('lan-enable', 'lan-unavailable', 'lan-vpn-default'):
        settings['lan_share'] = 'off'
    if name in ('lan-matching', 'lan-absent', 'lan-interface-changed', 'lan-subnet-changed',
                'lan-gateway-changed', 'lan-mac-changed', 'lan-unverified', 'lan-no-mac-pin'):
        settings['lan_share_pin'] = copy.deepcopy(PIN)
    if name == 'lan-invalid-pin':
        settings['lan_share_pin'] = {**PIN, 'subnet': 'not-a-network'}
    if name in ('lan-absent', 'lan-unpinned-absent', 'lan-unavailable'):
        fixture['addresses'] = []
    if name == 'lan-interface-changed':
        settings['lan_share_pin']['interface'] = 'other-fixture'
    if name == 'lan-subnet-changed':
        settings['lan_share_pin']['subnet'] = '192.168.51.0/24'
    if name == 'lan-gateway-changed':
        settings['lan_share_pin']['gateway'] = '192.168.50.254'
    if name == 'lan-mac-changed':
        settings['lan_share_pin']['gateway_mac'] = '02:00:00:00:00:02'
    if name in ('lan-unverified', 'lan-no-mac-pin'):
        fixture['neighbors'] = []
    if name == 'lan-no-mac-pin':
        settings['lan_share_pin']['gateway_mac'] = None
    if name == 'lan-vpn-default':
        fixture['addresses'][0]['link_type'] = 'none'
    if name == 'lan-default-second':
        fixture['addresses'].insert(0, {'ifname': 'tun-fixture', 'link_type': 'none',
                                       'addr_info': [{'local': '10.0.0.2', 'prefixlen': 8}]})
        fixture['routes'].insert(0, {'dev': 'tun-fixture'})
    if name == 'lan-no-gateway':
        fixture['routes'][0].pop('gateway')
    if name == 'lan-public-address':
        fixture['addresses'][0]['addr_info'][0]['local'] = '203.0.113.1'
    if name == 'lan-failed-neighbor':
        settings['lan_share_pin'] = copy.deepcopy(PIN)
        fixture['neighbors'][0]['state'] = ['FAILED']
    if name.startswith('ts-') or name == 'tls-invalid':
        settings['lan_share'] = 'tailscale'
        fixture = {'addresses': [{'ifname': 'tailscale-fixture', 'link_type': 'none',
                                  'addr_info': [{'local': '100.64.0.2', 'prefixlen': 10}]}], 'routes': []}
    if name == 'ts-no-interface':
        fixture['addresses'][0]['ifname'] = 'other-fixture'
    if name.startswith('ts-invalid-'):
        fixture['invalid_json'] = 'addresses' if name == 'ts-invalid-address-json' else 'routes'
    if name == 'ts-tool-error':
        fixture['exit'] = 7
    if name == 'tls-invalid':
        settings['lan_share_options'] = {'tls_cert': str(home / 'absent-cert.pem'),
                                         'tls_key': str(home / 'absent-key.pem')}
    elif name.startswith('tls-'):
        cert, key = generate_certificate(home, 'sharing', ADDRESS)
        if name == 'tls-invalid-pem':
            cert.write_text('synthetic invalid certificate\n')
        if name == 'tls-mismatch':
            _, key = generate_certificate(home, 'mismatched', ADDRESS)
        settings['lan_share_options'] = {'tls_cert': str(cert) if name != 'tls-relative' else cert.name,
                                         'tls_key': str(key)}
    network = home / 'network.json'
    network.write_text(json.dumps(fixture))
    environment = {'PATH': str(binary_dir) + ':/usr/bin:/bin', 'FAKE_SHARE_NETWORK': str(network)}
    with Daemon(configured=True, home=home, port=8092, environ=environment, config_changes=settings) as daemon:
        output = [daemon.call('GET', '/_local/share')]
        if name in ('lan-enable', 'lan-unavailable', 'lan-vpn-default'):
            output.append(daemon.call('POST', '/_local/share', {'mode': 'lan'}))
        if name == 'lan-watch-resume':
            fixture['neighbors'][0]['lladdr'] = '02:00:00:00:00:02'
            network.write_text(json.dumps(fixture))
            wait_until(lambda: daemon.call('GET', '/_local/share')['body'].get('mode') == 'off', timeout=14)
            output.append(daemon.call('GET', '/_local/share'))
            output.append({'auto_off_config': saved(daemon)})
            output.append(daemon.call('POST', '/_local/share', {'mode': 'lan'}))
        final = daemon.call('GET', '/_local/share')['body']
        if name == 'tls-disconnect' and final.get('active'):
            output.append(daemon.call('POST', '/_local/load', {}))
            credential = json.loads((daemon.config_path.resolve().parent.parent / 'lan-share-key.json').read_text())['key']
            client = http.client.HTTPSConnection(ADDRESS, daemon.port,
                         context=ssl.create_default_context(cafile=str(cert)), timeout=3)
            try:
                client.request('POST', '/v1/chat/completions', body=json.dumps(chat(messages=[
                               {'role': 'user', 'content': '[hold]'}])),
                               headers={'Authorization': 'Bearer ' + credential, 'Content-Type': 'application/json'})
                daemon.wait_event(lambda row: row['event'] == 'held')
                client.close()
                # The reference omits its socket-peek monitor for TLS. Closing
                # the client must leave this held backend request admitted.
                time.sleep(.25)
                output.append({'active_after_disconnect': status(daemon)['active_requests'],
                               'backend_cancelled': any(row['event'] == 'cancelled' for row in daemon.events())})
            finally:
                client.close()
                Path(daemon.env['FAKE_ENGINE_GATE']).touch()
            wait_until(lambda: status(daemon)['active_requests'] == 0)
        if final.get('active'):
            key_file = daemon.config_path.resolve().parent.parent / 'lan-share-key.json'
            credential = json.loads(key_file.read_text())['key']
            encrypted = final['encryption'] == 'tls'
            if encrypted:
                context = ssl.create_default_context(cafile=str(cert))
                if name == 'tls-1.2':
                    context.minimum_version = context.maximum_version = ssl.TLSVersion.TLSv1_2
                client = http.client.HTTPSConnection(ADDRESS, daemon.port, context=context, timeout=3)
            else:
                client = http.client.HTTPConnection(ADDRESS, daemon.port, timeout=3)
            try:
                client.request('GET', '/v1/models', headers={'Authorization': 'Bearer ' + credential})
                response = client.getresponse()
                output.append({'shared_models_status': response.status, 'object': json.loads(response.read())['object']})
                if encrypted:
                    output.append({'protocol': client.sock.version()})
            finally:
                client.close()
        output.append({'config': saved(daemon)})
        def normalize(value):
            if isinstance(value, list):
                for item in value: normalize(item)
            elif isinstance(value, dict):
                if value.get('key_fingerprint') is not None:
                    key_file = daemon.config_path.resolve().parent.parent / 'lan-share-key.json'
                    credential = json.loads(key_file.read_text())['key']
                    expected = 'sha256:' + hashlib.sha256(credential.encode()).hexdigest()[:12]
                    if value['key_fingerprint'] != expected:
                        raise AssertionError('Invalid sharing key metadata')
                    value['key_fingerprint'] = '<validated:key-fingerprint>'
                for item in value.values(): normalize(item)
        normalize(output)
        if json.loads(daemon.config_path.read_text()).get('share_contract_unknown') != {'preserve': True}:
            raise AssertionError('Sharing mutation discarded an unknown config field')
        return output


def saved(daemon):
    config = json.loads(daemon.config_path.read_text())
    result = {key: value for key, value in config.items() if key in ('lan_share', 'lan_share_pin', 'lan_share_paused')}
    if isinstance(result.get('lan_share_paused'), dict):
        at = result['lan_share_paused'].get('t')
        if type(at) is not int or abs(time.time() - at) > 30:
            raise AssertionError('Invalid network-pause timestamp')
        result['lan_share_paused']['t'] = '<validated:recent-unix-seconds>'
    return result


class ShareNetworkContract(unittest.TestCase):
    def test_share_network_goldens(self):
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
        prepare_network(sys.argv[2], __file__, ADDRESS)
    elif sys.argv[1:2] == ['--case-ready']:
        print(json.dumps(capture(sys.argv[2])))
    else:
        unittest.main()
