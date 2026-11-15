"""Chooser facts use metadata/receipts and exact model+server prefix identities."""
import hashlib
import json
import os
from pathlib import Path
import sys
import unittest
from concurrent.futures import ThreadPoolExecutor

from harness import Daemon, PRIMARY, SECONDARY
from test_api import chat
from test_lifecycle import status, wait_until

GOLDENS = Path(__file__).with_name('goldens') / 'catalog.json'
CASES = ('empty', 'selected', 'custom', 'prefix-cold', 'prefix-warm', 'prefix-mismatch',
         'prefix-symlink', 'qualification-stale', 'qualification-missing', 'qualification-good',
         'qualification-changed', 'states')


def run_case(name):
    with Daemon(configured=True) as daemon:
        config = json.loads(daemon.config_path.read_text())
        if name == 'selected':
            config['selected_model'] = SECONDARY
        elif name == 'custom':
            config['profiles']['synthetic-custom'] = {**config['profiles'][PRIMARY],
                                                       'display_name': 'Synthetic custom model'}
        if name.startswith('prefix'):
            directory = daemon.home / '.cache/local-ai/snapdragon-engine/prefix'
            args = [*config['oxe_args'], '--prefix-cache', 'default']
            config['oxe_args'] = args
            config['profiles'][PRIMARY]['oxe_args'] = args
            if name != 'prefix-cold':
                directory.mkdir(parents=True)
                model = 'f' * 64 if name == 'prefix-mismatch' else config['sha256']
                identity = ('model=sha256:' + model + ' server=sha256:' + config['server_sha256']).encode()
                (directory / ('a' * 64 + '.bin')).write_bytes(
                    b'OXEPFX01' + len(identity).to_bytes(4, 'little') + identity + b'synthetic state')
            if name == 'prefix-symlink':
                directory.rename(daemon.home / 'prefix-target')
                directory.symlink_to(daemon.home / 'prefix-target', target_is_directory=True)
        if name.startswith('qualification'):
            receipt = daemon.home / 'synthetic-receipt.json'
            receipt.write_text('{"synthetic":true}\n')
            validation = {'qualified': True, 'server_sha256': config['server_sha256'],
                          'report': str(receipt), 'receipt_sha256': hashlib.sha256(receipt.read_bytes()).hexdigest()}
            if name == 'qualification-stale':
                validation['server_sha256'] = 'e' * 64
            if name == 'qualification-missing':
                receipt.unlink()
            config['validation'] = validation
            config['profiles'][PRIMARY]['validation'] = validation
        daemon.config_path.write_text(json.dumps(config))
        output = [daemon.call('GET', '/_local/models')]
        if name == 'qualification-changed':
            previous = receipt.stat()
            receipt.write_text('{"synthetic":false}\n')
            os.utime(receipt, ns=(previous.st_atime_ns, previous.st_mtime_ns + 1))
            output.append(daemon.call('GET', '/_local/models'))
        if name == 'states':
            output.append(daemon.call('POST', '/_local/load', {}))
            output.append(daemon.call('GET', '/_local/models'))
            output.append(daemon.call('POST', '/_local/unload', {}))
            output.append(daemon.call('GET', '/_local/models'))
        elif any(row['event'] == 'started' for row in daemon.events()):
            raise AssertionError('Catalog poll started an engine')
        if str(daemon.home) in json.dumps(output):
            raise AssertionError('Catalog exposed a private path')
        return output


class CatalogContract(unittest.TestCase):
    def test_status_unload_idle_and_harness_facts(self):
        for idle in (None, 2.5):
            with self.subTest(idle=idle), Daemon(configured=True, config_changes={
                    'keep_warm': {'idle_unload_seconds': idle} if idle is not None else {}}) as daemon:
                before = status(daemon)
                expected_idle = 7200.0 if idle is None else idle
                self.assertEqual(before['idle_unload_seconds'], expected_idle)
                self.assertIsNone(before['loaded_model'])
                self.assertIsNone(before['resident_bytes'])
                self.assertIsNone(before['idle_unload_remaining_seconds'])
                self.assertEqual(before['unload'], {'method': 'POST', 'path': '/_local/unload', 'available': False})
                self.assertIn('storage', before['capabilities'])
                self.assertEqual(before['harnesses'], {})
                self.assertIsNone(before['error_at'])
                self.assertIsNone(before['error_origin'])
                self.assertEqual(daemon.call('POST', '/_local/load', {})['status'], 200)
                loaded = status(daemon)
                self.assertEqual(loaded['loaded_model'], PRIMARY)
                self.assertTrue(0 < loaded['resident_bytes'] < 16 * 1024**3)
                self.assertTrue(0 <= loaded['idle_unload_remaining_seconds'] <= expected_idle)
                self.assertTrue(loaded['unload']['available'])
                with ThreadPoolExecutor(max_workers=1) as executor:
                    held = executor.submit(daemon.call, 'POST', '/v1/chat/completions',
                                           chat(messages=[{'role': 'user', 'content': '[hold]'}]))
                    try:
                        daemon.wait_event(lambda row: row['event'] == 'held')
                        answering = status(daemon)
                        self.assertFalse(answering['unload']['available'])
                        self.assertIsNone(answering['idle_unload_remaining_seconds'])
                    finally:
                        Path(daemon.env['FAKE_ENGINE_GATE']).touch()
                    self.assertEqual(held.result(timeout=5)['status'], 200)
                wait_until(lambda: status(daemon)['active_requests'] == 0)
                self.assertEqual(status(daemon)['harnesses'], daemon.call('GET', '/_local/harnesses')['body']['harnesses'])
                self.assertEqual(daemon.call('POST', '/_local/unload', {})['status'], 200)
                self.assertIsNone(status(daemon)['resident_bytes'])

    def test_catalog_goldens(self):
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
