"""Legacy conversation flags follow disk-off and private root-override policy."""
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest

from harness import Daemon, PRIMARY

GOLDENS = Path(__file__).with_name('goldens') / 'conversation-stores.json'
CASES = ('default', 'equals', 'override', 'environment', 'off', 'off-override', 'bad-root')


def normalize(value, home):
    if isinstance(value, str):
        return value.replace(str(home), '<home>')
    if isinstance(value, list):
        return [normalize(item, home) for item in value]
    if isinstance(value, dict):
        return {key: normalize(item, home) for key, item in value.items()}
    return value


def run_case(name):
    with tempfile.TemporaryDirectory(prefix='oxc-conversation-conformance-') as temporary:
        home = Path(temporary)
        environment = {'FAKE_ENGINE_LEGACY_STORES': '1'}
        if name == 'environment':
            environment['OXC_STATE_CACHE_ROOT'] = str(home / 'cache-env-root')
        with Daemon(configured=True, home=home, environ=environment) as daemon:
            config = json.loads(daemon.config_path.read_text())
            cache = {'disk_budget_bytes': 0 if name in ('off', 'off-override') else 64_000_000_000}
            if name in ('override', 'off-override'):
                cache['root'] = str(home / 'cache-root')
            elif name in ('environment', 'bad-root'):
                cache['root'] = '/outside-synthetic-home'
            config['cache'] = cache
            prefix, sessions = str(home / 'original prefix'), str(home / 'original sessions')
            legacy = ['--prefix-cache=' + prefix, '--session-cache=' + sessions,
                      '--prefix-cache-max-bytes=4096', '--prefix-cache-min-tokens=64', '--prefix-cache-ttl-h=24'] if name == 'equals' else [
                      '--prefix-cache', prefix, '--session-cache', sessions,
                      '--prefix-cache-max-bytes', '4096', '--prefix-cache-min-tokens', '64', '--prefix-cache-ttl-h', '24']
            args = [*config['oxe_args'], *legacy, '--ctx-size', '{context}']
            config['oxe_args'] = args
            config['profiles'][PRIMARY]['oxe_args'] = args
            daemon.config_path.write_text(json.dumps(config))
            output = [daemon.call('POST', '/_local/load', {})]
            expected = 503 if name == 'bad-root' else 200
            if output[0]['status'] != expected:
                raise AssertionError(output[0])
            starts = [event for event in daemon.events() if event['event'] == 'started']
            output.append({'launches': [event['legacy'] for event in starts],
                           'override_exists': (home / 'cache-root').exists(),
                           'environment_exists': (home / 'cache-env-root').exists()})
            if name == 'bad-root' and starts:
                raise AssertionError('Invalid root reached the engine')
            return normalize(output, home)


class ConversationStoreContract(unittest.TestCase):
    def test_conversation_store_goldens(self):
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
