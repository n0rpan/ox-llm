"""S5: behaviour that needs a real process, a real port or a real bind (no fakes of those).

- an engine child that crashes mid-stream: the client sees a truncated stream with no success
  terminator, the death is recorded, the slot is released and the next request starts afresh;
- a really occupied backend port goes through begin(): refused before any child is spawned;
- the controller's listener is bound to loopback only.
(The Studio listener control is in tests/test_studio_security.py.)
(The fourth S5 item, `oxc status` wording, is in tests/test_oxc_status_cli.py.)
"""
import http.client
import json
import os
import socket
import subprocess
import sys
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import patch

from oxc.runtime import controller

sys.path.insert(0, str(Path(__file__).resolve().parent))
from _stub_manager import StubManagerCase, free_port  # noqa: E402

SRC = Path(__file__).resolve().parents[1] / 'src'


def wait_until(predicate, seconds=5.0):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(.02)
    return predicate()


class ProxyCase(StubManagerCase):
    def setUp(self):
        super().setUp()
        self.server = controller.Server(('127.0.0.1', 0), controller.Handler)
        self.server.manager = self.manager
        # Track the per-request handler threads so tearDown can join them with a deadline.
        # A handler that has already sent its reply is still finishing its bookkeeping
        # (record_engine_death, the request_end diagnostic) into self.root/engine-stderr.log,
        # and must be done before StubManagerCase.tearDown removes the directory. The server's
        # own server_close() does not join them: Server.daemon_threads is True, so the handler
        # threads are daemon and ThreadingMixIn._threads skips daemon threads.
        self._handler_threads = set()
        self._handler_lock = threading.Lock()
        original = self.server.process_request_thread

        def tracked(request, client_address):
            thread = threading.current_thread()
            with self._handler_lock:
                self._handler_threads.add(thread)
            try:
                original(request, client_address)
            finally:
                with self._handler_lock:
                    self._handler_threads.discard(thread)
        self.server.process_request_thread = tracked
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def tearDown(self):
        # Stop the HTTP server and join every handler thread BEFORE the tmp dir is removed
        # (StubManagerCase.tearDown). The old server_close/shutdown addCleanup callbacks ran
        # LIFO *after* tearDown, i.e. after tmp.cleanup() had already removed the directory a
        # handler could still be writing into.
        self.server.shutdown()
        self.server.server_close()
        with self._handler_lock:
            threads = list(self._handler_threads)
        for thread in threads:
            thread.join(timeout=5)
        super().tearDown()

    def chat(self, stream=True):
        client = http.client.HTTPConnection('127.0.0.1', self.server.server_port, timeout=20)
        self.addCleanup(client.close)
        body = json.dumps({'model': 'stub-local', 'stream': stream,
                           'messages': [{'role': 'user', 'content': 'hi'}]}).encode()
        client.request('POST', '/v1/chat/completions', body=body,
                       headers={'Content-Type': 'application/json', 'Host': f'127.0.0.1:{self.server.server_port}'})
        response = client.getresponse()
        try:
            return response, response.read()
        except http.client.IncompleteRead as error:  # chunked body without its final chunk
            self.truncated = True
            return response, error.partial


class CrashMidStreamTests(ProxyCase):
    def test_engine_crash_mid_stream_truncates_records_and_recovers(self):
        self.use('--die-mid-stream')
        with self.run_stub():
            response, body = self.chat()
            self.assertEqual(response.status, 200)
            self.assertIn(b'partial', body)
            self.assertNotIn(b'[DONE]', body, 'a crashed engine must not look like a finished answer')
            self.assertTrue(getattr(self, 'truncated', False), 'the chunked body must end without its terminator')
            self.assertTrue(wait_until(lambda: self.manager.active == 0), 'slot not released after the crash')
            log = self.root / 'engine-stderr.log'
            self.assertTrue(wait_until(lambda: log.exists() and b'engine_died' in log.read_bytes()))
            died = [json.loads(line) for line in log.read_text().splitlines() if '"engine_died"' in line]
            self.assertEqual(died[0]['exit_code'], 9)
            # The next request starts a new child instead of talking to the dead one.
            self.use()
            response, body = self.chat()
        self.assertEqual(response.status, 200)
        self.assertIn(b'[DONE]', body)


class OccupiedPortTests(StubManagerCase):
    def test_real_occupied_port_is_refused_before_spawn(self):
        code = ('import socket,sys,time; s=socket.socket(); s.bind(("127.0.0.1",int(sys.argv[1]))); '
                's.listen(); print("up", flush=True); time.sleep(30)')
        holder = subprocess.Popen([sys.executable, '-c', code, str(self.port)], stdout=subprocess.PIPE,
                                  text=True, start_new_session=True)
        self.addCleanup(lambda: (holder.kill(), holder.wait(), holder.stdout.close()))
        self.assertEqual(holder.stdout.readline().strip(), 'up')
        with self.run_stub() as popen, self.assertRaisesRegex(controller.Unavailable, str(self.port)):
            self.manager.begin(allow_start=True, model_alias='stub-local')
        popen.assert_not_called()
        self.assertIsNone(self.manager.process)
        self.assertIn(str(self.port), self.manager.status()['last_error'])


class LoopbackBindTests(unittest.TestCase):
    def test_controller_listens_on_loopback_only(self):
        port = free_port()
        env = dict(os.environ, PYTHONPATH=str(SRC), LOCAL_LLM_PORT=str(port),
                   LOCAL_LLM_BACKEND_PORT=str(free_port()))
        child = subprocess.Popen([sys.executable, '-c', 'from oxc.runtime import controller; controller.main()'],
                                 env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.addCleanup(lambda: (child.terminate(), child.wait(10)))
        addresses = set()

        def listening():
            for name in ('/proc/net/tcp', '/proc/net/tcp6'):
                for line in Path(name).read_text().splitlines()[1:]:
                    fields = line.split()
                    local, state = fields[1], fields[3]
                    if state == '0A' and int(local.rsplit(':', 1)[1], 16) == port:
                        addresses.add(local.rsplit(':', 1)[0])
            return bool(addresses)
        self.assertTrue(wait_until(listening, 15), 'controller never listened')
        self.assertEqual(addresses, {'0100007F'}, 'controller must bind 127.0.0.1 only')


if __name__ == '__main__':
    unittest.main()
