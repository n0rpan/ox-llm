"""Loopback sharing metadata and the off switch; active listeners stay a separate contract."""
import base64
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import stat
import sys
import unittest

from harness import Daemon
from test_api import chat

GOLDENS = Path(__file__).with_name('goldens') / 'share-control.json'
CASES = ('off', 'unconfigured', 'paused', 'options', 'invalid-options', 'key-private', 'key-read-only',
         'key-public', 'key-symlink', 'key-hardlink', 'key-invalid', 'config-symlink',
         'validation', 'busy')


def run_case(name):
    changes = {'lan_share': 'off', 'synthetic_share_setting': {'keep': True}}
    if name == 'paused':
        changes.update(lan_share_pin={'interface': 'synthetic0', 'subnet': '10.0.0.0/24',
                                     'gateway': '10.0.0.1', 'gateway_mac': None},
                       lan_share_paused={'reason': 'network_changed', 't': 1})
    if name == 'options':
        changes['lan_share_options'] = {'rate_per_min': 6000, 'burst': 1000, 'max_body_mb': 16,
                                       'max_tokens': 1000000, 'tls_cert': '/synthetic/cert', 'tls_key': '/synthetic/key'}
    if name == 'invalid-options':
        changes['lan_share_options'] = {'rate_per_min': True, 'burst': 1001, 'max_body_mb': 17, 'max_tokens': 1.5}
        changes['lan_share'] = {'invalid': True}
    with Daemon(configured=name != 'unconfigured', config_changes=changes) as daemon:
        key_path = daemon.config_path.resolve().parent.parent / 'lan-share-key.json'
        credential = 'lai-' + base64.urlsafe_b64encode(os.urandom(32)).decode().rstrip('=')
        if name.startswith('key-'):
            key_path.write_text('invalid JSON' if name == 'key-invalid' else json.dumps(
                {'key': credential, 'created': 1, 'shown': name == 'key-private'}))
            key_path.chmod(0o644 if name == 'key-public' else 0o400 if name == 'key-read-only' else 0o600)
            if name == 'key-symlink':
                key_path.rename(daemon.home / 'synthetic-key-target')
                key_path.symlink_to(daemon.home / 'synthetic-key-target')
            elif name == 'key-hardlink':
                os.link(key_path, daemon.home / 'synthetic-key-target')
        if name == 'config-symlink':
            directory = daemon.home / 'config-target'
            directory.mkdir()
            target = directory / 'runtime.json'
            daemon.config_path.rename(target)
            target.chmod(0o640)
            daemon.config_path.symlink_to(target)
        def key_snapshot():
            if not key_path.exists() and not key_path.is_symlink():
                return None
            info = key_path.lstat()
            return (info.st_mode, info.st_nlink, key_path.is_symlink(), key_path.read_bytes())
        before_key = key_snapshot()
        output = [daemon.call('GET', '/_local/share')]
        if name == 'off':
            output.append(daemon.call('HEAD', '/_local/share'))
        if name == 'validation':
            for body in ({}, {'mode': 'invalid'}, {'mode': 1}, {'mode': 'off', 'extra': True}, []):
                output.append(daemon.call('POST', '/_local/share', body))
        if name == 'busy':
            with ThreadPoolExecutor(max_workers=1) as executor:
                held = executor.submit(daemon.call, 'POST', '/v1/chat/completions',
                                       chat(messages=[{'role': 'user', 'content': '[hold]'}]))
                try:
                    daemon.wait_event(lambda row: row['event'] == 'held')
                    output.append(daemon.call('POST', '/_local/share', {'mode': 'off'}))
                finally:
                    Path(daemon.env['FAKE_ENGINE_GATE']).touch()
                if held.result(timeout=5)['status'] != 200:
                    raise AssertionError('Sharing off interfered with local inference')
        else:
            output.append(daemon.call('POST', '/_local/share', {'mode': 'off'}))
        output.append(daemon.call('GET', '/_local/share'))
        if key_snapshot() != before_key:
            raise AssertionError('Sharing status/off changed its key file')
        after = json.loads(daemon.config_path.read_text()) if daemon.config_path.exists() else {}
        output.append({'settings': {key: after.get(key) for key in
                       ('lan_share', 'lan_share_pin', 'lan_share_paused', 'synthetic_share_setting')},
                       'config_symlink': daemon.config_path.is_symlink(),
                       'config_mode': stat.S_IMODE(daemon.config_path.stat().st_mode) if daemon.config_path.exists() else None,
                       'key_created': key_path.exists() if not name.startswith('key-') else None})
        expected_fingerprint = 'sha256:' + hashlib.sha256(credential.encode()).hexdigest()[:12]
        for row in output:
            body = row.get('body')
            if not isinstance(body, dict) or 'port' not in body:
                continue
            if body['port'] != daemon.port:
                raise AssertionError('Share metadata published the wrong port')
            body['port'] = '<validated:proxy-port>'
            if body['key_fingerprint'] is not None:
                if body['key_fingerprint'] != expected_fingerprint:
                    raise AssertionError('Share key fingerprint does not identify the private key')
                body['key_fingerprint'] = '<validated:key-fingerprint>'
        if credential in json.dumps(output) or credential in (daemon.home / 'daemon.log').read_text():
            raise AssertionError('A share key escaped into status or logs')
        if name != 'busy' and any(row['event'] == 'started' for row in daemon.events()):
            raise AssertionError('Sharing metadata started an engine')
        return output


class ShareControlContract(unittest.TestCase):
    def test_share_control_goldens(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(run_case(name), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_DAEMON_COMMAND' in os.environ:
            raise SystemExit('Goldens must be recorded from the Python reference')
        GOLDENS.write_text(json.dumps({name: run_case(name) for name in CASES}, indent=2, sort_keys=True) + '\n')
    else:
        unittest.main()
