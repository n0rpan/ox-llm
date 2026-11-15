"""O-173 unload right after a reply: a request that already sent its terminal SSE and only finishes its bookkeeping
(ledger row, slot release) must not turn POST /_local/unload into a spurious 409 (interim leg I1: two 409s within
1 ms of the [DONE]). Real controller Handler + real Manager + the model-free llama stub on a real port; the
bookkeeping is made slow on purpose (ledger finish delayed) so the gap is deterministic, not timing luck."""
import http.client
import json
import sys
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import patch

from oxc.runtime import controller, ctl_routes, request_ledger

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_controller_behaviour import ProxyCase  # noqa: E402


class UnloadAfterReply(ProxyCase):
    def unload(self):
        client = http.client.HTTPConnection('127.0.0.1', self.server.server_port, timeout=20)
        try:
            client.request('POST', '/_local/unload', body=b'{}',
                           headers={'Host': '127.0.0.1:%d' % self.server.server_port, 'Content-Length': '2'})
            response = client.getresponse()
            return response.status, json.loads(response.read() or b'{}')
        finally:
            client.close()

    def slow_finish(self, seconds):
        original = request_ledger.Entry.finish

        def finish(entry, *args, **kwargs):
            time.sleep(seconds)  # the bookkeeping after the last byte, made slow
            return original(entry, *args, **kwargs)
        return patch.object(request_ledger.Entry, 'finish', finish)

    def test_unload_right_after_the_terminal_sse_waits_for_finalization_and_unloads(self):
        with self.run_stub(), self.slow_finish(0.5):
            response, body = self.chat()
            self.assertIn(b'[DONE]', body)
            self.assertEqual(self.manager.active, 1, 'the bookkeeping gap is open (the test premise)')
            t0 = time.monotonic()
            status, reply = self.unload()
        self.assertEqual((status, reply), (200, {'state': 'unloaded'}))
        self.assertLess(time.monotonic() - t0, ctl_routes.UNLOAD_FINALIZE_WAIT_S + 1)
        self.assertIsNone(self.manager.process)

    def test_unload_during_generation_is_still_refused_at_once(self):
        with self.manager.mu:
            self.manager.active += 1  # a request that has not sent its answer (no finalizing mark)
        try:
            t0 = time.monotonic()
            status, reply = self.unload()
            waited = time.monotonic() - t0
        finally:
            with self.manager.mu:
                self.manager.active -= 1
        self.assertEqual(status, 409)
        self.assertEqual(reply, {'error': 'A request is running. Try again when it finishes.', 'retryable': True})
        self.assertLess(waited, 1.0, 'a generating request is refused at once, not waited for')

    def test_finalization_that_outlasts_the_bounded_wait_gets_a_retryable_409(self):
        with self.run_stub(), self.slow_finish(2.0), patch.object(ctl_routes, 'UNLOAD_FINALIZE_WAIT_S', 0.3):
            response, body = self.chat()
            self.assertIn(b'[DONE]', body)
            t0 = time.monotonic()
            status, reply = self.unload()
            waited = time.monotonic() - t0
        self.assertEqual(status, 409)
        self.assertTrue(reply.get('retryable'))
        self.assertGreaterEqual(waited, 0.25)
        self.assertLess(waited, 1.5)


if __name__ == '__main__':
    unittest.main()
