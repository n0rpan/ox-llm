"""Lifecycle, contention, cancellation, file privacy and host guard contracts."""
from concurrent.futures import ThreadPoolExecutor
import fcntl
import http.client
import json
from pathlib import Path
import stat
import time
import unittest

from harness import Daemon, PRIMARY, SECONDARY
from test_api import chat


def wait_until(predicate, timeout=5):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.02)
    raise AssertionError('Condition did not become true within the deadline')


def status(daemon):
    response = daemon.call('GET', '/_local/status')
    if response['status'] != 200:
        raise AssertionError(response)
    return response['body']


def launches(daemon):
    return [row for row in daemon.events() if row['event'] == 'started']


class LifecycleContract(unittest.TestCase):
    def test_polls_do_not_load_and_requests_reuse_engine(self):
        with Daemon(configured=True) as daemon:
            for path in ('/_local/status', '/_local/models', '/v1/models', '/v1/models/' + PRIMARY,
                         '/_local/cache', '/_local/storage', '/_local/harnesses', '/_local/ledger'):
                with self.subTest(path=path):
                    self.assertEqual(daemon.call('GET', path)['status'], 200)
            self.assertEqual(status(daemon)['state'], 'unloaded')
            self.assertEqual(launches(daemon), [])
            for _ in range(2):
                self.assertEqual(daemon.call('POST', '/v1/chat/completions', chat())['status'], 200)
            self.assertEqual(len(launches(daemon)), 1)
            self.assertEqual(status(daemon)['state'], 'ready')
            self.assertEqual(daemon.call('POST', '/_local/unload', {})['status'], 200)
            self.assertEqual(status(daemon)['state'], 'unloaded')
            self.assertEqual(daemon.call('POST', '/v1/chat/completions', chat())['status'], 200)
            self.assertEqual(len(launches(daemon)), 2)

    def test_backend_tokens_private_rotated_removed_and_not_exposed(self):
        with Daemon(configured=True) as daemon:
            self.assertEqual(daemon.call('POST', '/_local/load', {})['body'], {'state': 'ready'})
            keys = list((daemon.home / 'run/local-ai').glob('oxe-backend-*.key'))
            self.assertEqual(len(keys), 1)
            first = keys[0].read_bytes()
            self.assertGreaterEqual(len(first), 32)
            self.assertEqual(stat.S_IMODE(keys[0].stat().st_mode), 0o600)
            self.assertEqual(stat.S_IMODE(keys[0].parent.stat().st_mode), 0o700)
            for path in ('/_local/status', '/_local/models', '/v1/models', '/_local/ledger'):
                self.assertFalse(first.decode() in json.dumps(daemon.call('GET', path)))
            daemon.log.flush()
            self.assertFalse(first in (daemon.home / 'daemon.log').read_bytes())
            # The engine's private endpoint refuses unauthenticated access.
            connection = http.client.HTTPConnection('127.0.0.1', daemon.backend_port, timeout=2)
            try:
                connection.request('GET', '/health')
                response = connection.getresponse()
                self.assertEqual(response.status, 401)
                response.read()
            finally:
                connection.close()
            self.assertEqual(daemon.call('POST', '/_local/unload', {})['status'], 200)
            self.assertFalse(keys[0].exists())
            self.assertEqual(daemon.call('POST', '/_local/load', {})['status'], 200)
            keys = list((daemon.home / 'run/local-ai').glob('oxe-backend-*.key'))
            self.assertEqual(len(keys), 1)
            self.assertFalse(first == keys[0].read_bytes())

    def test_lease_is_exclusive_until_unload(self):
        with Daemon(configured=True) as daemon:
            self.assertEqual(daemon.call('POST', '/_local/load', {})['status'], 200)
            lease = Path(daemon.env['LOCAL_AI_LEASE'])
            self.assertEqual(stat.S_IMODE(lease.stat().st_mode), 0o600)
            with lease.open('a') as stream:
                with self.assertRaises(BlockingIOError):
                    fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
                self.assertEqual(daemon.call('POST', '/_local/unload', {})['status'], 200)
                fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
                fcntl.flock(stream, fcntl.LOCK_UN)

    def test_external_lease_blocks_start_and_recovers(self):
        with Daemon(configured=True) as daemon:
            lease = Path(daemon.env['LOCAL_AI_LEASE'])
            lease.parent.mkdir(parents=True, exist_ok=True)
            with lease.open('a') as stream:
                fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
                reply = daemon.call('POST', '/_local/load', {})
                self.assertEqual(reply['status'], 503)
                self.assertEqual(reply['body'], {'error': 'Another image, chat or setup job is running. Try again when it finishes.'})
                self.assertEqual(launches(daemon), [])
            self.assertEqual(daemon.call('POST', '/_local/load', {})['status'], 200)

    def test_symlink_lease_is_refused(self):
        with Daemon(configured=True) as daemon:
            lease = Path(daemon.env['LOCAL_AI_LEASE'])
            lease.parent.mkdir(parents=True, exist_ok=True)
            target = daemon.home / 'untouched'
            target.write_text('synthetic sentinel')
            lease.symlink_to(target)
            reply = daemon.call('POST', '/_local/load', {})
            self.assertEqual(reply['status'], 503)
            self.assertIn('symbolic link; refusing to use it', reply['body']['error'])
            self.assertEqual(target.read_text(), 'synthetic sentinel')
            self.assertEqual(launches(daemon), [])

    def test_fifo_and_busy_controls(self):
        with Daemon(configured=True) as daemon, ThreadPoolExecutor(max_workers=3) as pool:
            first = pool.submit(daemon.call, 'POST', '/v1/chat/completions',
                                chat(messages=[{'role': 'user', 'content': '[hold] first'}]))
            try:
                daemon.wait_event(lambda row: row['event'] == 'held')
                busy = daemon.call('POST', '/_local/unload', {})
                self.assertEqual((busy['status'], busy['body']), (409, {
                    'error': 'A request is running. Try again when it finishes.', 'retryable': True}))
                self.assertEqual(daemon.call('POST', '/_local/select', {'model': SECONDARY})['status'], 409)
                self.assertEqual(daemon.call('POST', '/_local/preload', {'model': SECONDARY})['status'], 409)
                second = pool.submit(daemon.call, 'POST', '/v1/chat/completions',
                                     chat(messages=[{'role': 'user', 'content': 'second'}]))
                wait_until(lambda: status(daemon)['queued_requests'] == 1)
                third = pool.submit(daemon.call, 'POST', '/v1/chat/completions',
                                    chat(messages=[{'role': 'user', 'content': 'third'}]))
                wait_until(lambda: status(daemon)['queued_requests'] == 2)
            finally:
                Path(daemon.env['FAKE_ENGINE_GATE']).touch()
            for future in (first, second, third):
                self.assertEqual(future.result(timeout=5)['status'], 200)
            requests = [row['body']['messages'][-1]['content'] for row in daemon.events()
                        if row['event'] == 'request' and row['method'] == 'POST']
            self.assertEqual(requests, ['[hold] first', 'second', 'third'])
            wait_until(lambda: status(daemon)['active_requests'] == 0)
            self.assertEqual(status(daemon)['queued_requests'], 0)
            self.assertEqual(len(launches(daemon)), 1)

    def test_queue_timeout_is_503_and_slot_recovers(self):
        with Daemon(configured=True, environ={'LOCAL_LLM_QUEUE_WAIT_SECONDS': '0.15'}) as daemon, \
                ThreadPoolExecutor(max_workers=1) as pool:
            first = pool.submit(daemon.call, 'POST', '/v1/chat/completions',
                                chat(messages=[{'role': 'user', 'content': '[hold]'}]))
            try:
                daemon.wait_event(lambda row: row['event'] == 'held')
                reply = daemon.call('POST', '/v1/chat/completions', chat())
                self.assertEqual(reply['status'], 503)
                self.assertEqual(reply['body']['error']['type'], 'local_backend_busy')
                self.assertEqual(reply['body']['error']['code'], 'service_unavailable')
                self.assertEqual(status(daemon)['queued_requests'], 0)
            finally:
                Path(daemon.env['FAKE_ENGINE_GATE']).touch()
            self.assertEqual(first.result(timeout=5)['status'], 200)
            self.assertEqual(daemon.call('POST', '/v1/chat/completions', chat())['status'], 200)

    def test_disconnect_cancels_engine_and_releases_slot(self):
        with Daemon(configured=True) as daemon:
            connection = http.client.HTTPConnection('127.0.0.1', daemon.port, timeout=3)
            try:
                connection.request('POST', '/v1/chat/completions',
                                   body=json.dumps(chat(messages=[{'role': 'user', 'content': '[hold]'}])),
                                   headers={'Content-Type': 'application/json'})
                daemon.wait_event(lambda row: row['event'] == 'held')
            finally:
                connection.close()
            daemon.wait_event(lambda row: row['event'] == 'cancelled')
            wait_until(lambda: status(daemon)['active_requests'] == 0)
            self.assertEqual(daemon.call('POST', '/v1/chat/completions', chat())['status'], 200)
            self.assertEqual(len(launches(daemon)), 1)

    def test_idle_keep_warm_unloads_and_next_request_reloads(self):
        with Daemon(configured=True, environ={'LOCAL_LLM_KEEP_WARM_IDLE_SECONDS': '0.15'}) as daemon:
            self.assertEqual(daemon.call('POST', '/v1/chat/completions', chat())['status'], 200)
            wait_until(lambda: status(daemon)['state'] == 'unloaded')
            self.assertEqual(len(launches(daemon)), 1)
            self.assertEqual(daemon.call('POST', '/v1/chat/completions', chat())['status'], 200)
            self.assertEqual(len(launches(daemon)), 2)

    def test_swap_limit_guard_refuses_launch(self):
        with Daemon(configured=True) as daemon:
            (daemon.home / 'host/cgroup/memory.swap.max').write_text('max\n')
            reply = daemon.call('POST', '/_local/load', {})
            self.assertEqual(reply['status'], 503)
            self.assertIn('MemorySwapMax=0', reply['body']['error'])
            self.assertEqual(launches(daemon), [])
            (daemon.home / 'host/cgroup/memory.swap.max').write_text('0\n')
            self.assertEqual(daemon.call('POST', '/_local/load', {})['status'], 200)

    def test_changed_engine_hash_is_refused(self):
        with Daemon(configured=True) as daemon:
            with (daemon.home / 'fake-engine').open('a') as stream:
                stream.write('\n# Synthetic identity change.\n')
            reply = daemon.call('POST', '/_local/load', {})
            self.assertEqual(reply['status'], 503)
            self.assertIn('oxe_engine_identity_changed', reply['body']['error'])
            self.assertEqual(launches(daemon), [])


if __name__ == '__main__':
    unittest.main()
