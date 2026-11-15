"""Safe refusal stages against the unmodified Python gateway and owned mock."""
import http.client
import json
import os
from pathlib import Path
import subprocess
import sys
import time
from types import SimpleNamespace
import unittest

import source_peer as peer

GOLDENS = Path(__file__).with_name('policy.json')
CASES = ('no-key', 'no-key-no-budget', 'offline', 'no-price', 'no-budget',
         'bad-budget', 'exhausted', 'bad-ledger', 'disabled-ledger', 'guest', 'accepted')


def inputs(name):
    return {'payload': {'model': 'synthetic-cloud', 'messages': [{'role': 'user', 'content': 'Synthetic prompt.'}],
                        'max_tokens': 32, 'stream': False},
            'shared': name == 'guest', 'offline': name == 'offline', 'pricing': name != 'no-price',
            'credentials': name not in ('no-key', 'no-key-no-budget'),
            'cap': None if name in ('no-budget', 'no-key-no-budget') else False if name == 'bad-budget' else 1,
            'ledger_enabled': name != 'disabled-ledger',
            'spent': None if name == 'bad-ledger' else .999 if name == 'exhausted' else 0,
            'reserved': 0, 'required': .08008}


def capture(name):
    home = Path('/home/oxc-cloud-contract')
    spec = inputs(name)
    original = peer.config
    value = original('router')
    model = value['cloud']['models']['synthetic-cloud']
    if spec['pricing']:
        model['pricing'] = {'input_per_million': 1, 'output_per_million': 2}
    value['cloud']['daily_budget_usd'] = spec['cap']
    secret = home / 'provider.json'
    value['cloud']['secrets_file'] = str(secret)
    if spec['credentials']:
        descriptor = os.open(secret, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, 'w') as out:
            json.dump({'openrouter': {'api_key': 'synthetic-policy-key'}}, out)
    ledger = home / '.local/state/oxc/ledger'
    ledger.mkdir(parents=True, mode=0o700)
    if name in ('exhausted', 'bad-ledger'):
        text = '{"remote":true,"cost_usd":0.999}\n' if name == 'exhausted' else '["synthetic-invalid-row"]\n'
        (ledger / ('requests-' + time.strftime('%Y%m%d') + '.jsonl')).write_text(text)
    if name == 'guest':
        # Exercise the reference's already-authenticated guest context. The live
        # LAN listener/authentication is a separate integration contract.
        sys.path.insert(0, str(peer.ROOT / 'oxc/src'))
        from oxc.runtime import ctl_cloud
        state = SimpleNamespace(cloud_entry={'provider': 'openrouter'}, api_value=value,
                                led=None, remote='synthetic-authorized-guest')
        handler = SimpleNamespace(json_reply=lambda status, body: {'status': status, 'body': body})
        return ctl_cloud.forward(handler, state)
    peer.config = lambda _: value

    def inspect(port, upstream):
        peer = http.client.HTTPConnection('127.0.0.1', port, timeout=5)
        try:
            peer.request('POST', '/v1/chat/completions', json.dumps(spec['payload']),
                         {'Content-Type': 'application/json'})
            response = peer.getresponse()
            body = json.loads(response.read())
            result = {'status': response.status, 'body': body}
        finally:
            peer.close()
        if name == 'accepted':
            if response.status != 200 or len(upstream.requests) != 1:
                raise AssertionError('Valid remote request did not reach exactly one owned mock')
            return None
        if response.status < 400 or upstream.requests:
            raise AssertionError('Policy refusal forwarded a request')
        if 'synthetic-policy-key' in json.dumps(result) or str(secret) in json.dumps(result):
            raise AssertionError('Policy error exposed credentials')
        return result

    try:
        return peer.source(name, home, inspect=inspect,
                              environment={'OXC_LEDGER': '0' if name == 'disabled-ledger' else '1'})
    finally:
        peer.config = original


class PolicyContract(unittest.TestCase):
    def test_reference_or_opt_in_policy(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        binary = os.environ.get('OXC_POLICY_CONTRACT')
        for name in CASES:
            with self.subTest(case=name):
                if binary:
                    self.assertTrue(Path(binary).is_absolute())
                    done = subprocess.run([binary], input=json.dumps(inputs(name)), capture_output=True,
                                          text=True, env={'PATH': '/usr/bin:/bin'}, timeout=5)
                    self.assertEqual(done.returncode, 0, done.stderr)
                    actual = json.loads(done.stdout)
                else:
                    actual = peer.reference(name, __file__)
                self.assertEqual(actual, expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_POLICY_CONTRACT' in os.environ:
            raise SystemExit('Record only Python policy')
        GOLDENS.write_text(json.dumps({name: peer.reference(name, __file__) for name in CASES}, indent=2, sort_keys=True) + '\n')
    elif sys.argv[1:2] == ['--case']:
        print(json.dumps(capture(sys.argv[2])))
    else:
        unittest.main()
