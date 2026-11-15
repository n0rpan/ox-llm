"""Opt-in cloud disconnect supervision: real controller and owned loopback sockets."""
import json
import select
import socket
import threading
import time
from unittest.mock import patch

from mock_cloud import MockOpenAIResponses
from snapdragon_ai_lab.runtime import cloud, controller, ctl_cloud
from test_cloud_openai import models, REMOTE
from test_cloud_remote import CloudCase, KEY


class Quiet(MockOpenAIResponses):
    def __init__(self, headers, native=True):
        self.headers = headers
        self.native = native
        self.started = threading.Event()
        self.disconnected = threading.Event()
        self.release = threading.Event()
        super().__init__()

    def handle(self, handler, method):
        handler.rfile.read(int(handler.headers['Content-Length']))
        if self.headers:
            handler.send_response(200)
            handler.send_header('Content-Type', 'text/event-stream')
            handler.send_header('Connection', 'close')
            handler.end_headers()
            value = ({'type': 'response.output_text.delta', 'delta': 'working'} if self.native else
                     {'choices': [{'index': 0, 'delta': {'content': 'working'}, 'finish_reason': None}]})
            handler.wfile.write(('data: ' + json.dumps(value) + '\n\n').encode())
            handler.wfile.flush()
        self.started.set()
        try:
            while not self.release.wait(0.01):
                ready, _, _ = select.select([handler.connection], [], [], 0.01)
                if ready and not handler.connection.recv(1, socket.MSG_PEEK):
                    self.disconnected.set()
                    return
        finally:
            handler.close_connection = True


class CloudDisconnect(CloudCase):
    def setUp(self):
        super().setUp()
        self.secrets.write_text(json.dumps({p: {'api_key': KEY} for p in ('openai', 'openrouter')}))

    def start_watch(self, mock, enabled=True, router=False):
        table = models('gpt-6.1-sol')
        if router:
            table[REMOTE].update(kind='remote', provider='openrouter', upstream_model='synthetic/model')
        self.start(mock=mock, models=table)
        value = json.loads(self.stack.config_path.read_text())['cloud']
        value['orchestrator_disconnect_watch'] = enabled
        self.stack.set_config(cloud=value)

    def check_quiet(self, headers, router=False):
        upstream = Quiet(headers, native=not router)
        self.addCleanup(upstream.close)
        self.addCleanup(upstream.release.set)
        self.start_watch(upstream, router=router)
        request = json.dumps({'model': REMOTE, 'messages': [{'role': 'user', 'content': 'synthetic'}],
                              'stream': True}).encode()
        with socket.create_connection(('127.0.0.1', self.stack.port), timeout=2) as client:
            client.sendall(f'POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{self.stack.port}\r\nContent-Type: application/json\r\nContent-Length: '.encode()
                           + str(len(request)).encode() + b'\r\n\r\n' + request)
            self.assertTrue(upstream.started.wait(2))
            client.shutdown(socket.SHUT_RDWR)
        try:
            self.assertTrue(upstream.disconnected.wait(2), 'quiet upstream remained open')
            rows = self.ledger_rows()
            self.assertEqual(len(rows), 1)
            row = rows[0]
            self.assertEqual((row['status'], row['error']), (499, 'client_disconnected'))
            self.assertEqual(row['settled'], 'reserved')
            self.assertTrue(row['usage_missing'])
            self.assertGreater(row['cost_usd'], 0)
            self.assertNotIn(KEY, json.dumps(row))
            deadline = time.monotonic() + 1
            while time.monotonic() < deadline and (cloud._reserved or any(
                    t.name == 'local-ai-client-eof' and t.is_alive() for t in threading.enumerate())):
                time.sleep(0.01)
            self.assertFalse(cloud._reserved)
            self.assertFalse(any(t.name == 'local-ai-client-eof' and t.is_alive() for t in threading.enumerate()))
        finally:
            upstream.release.set()

    def test_quiet_stream_client_eof_closes_upstream_and_records_cancellation(self):
        self.check_quiet(headers=True)

    def test_openrouter_quiet_stream_also_closes_upstream(self):
        self.check_quiet(headers=True, router=True)

    def test_client_eof_before_upstream_headers_closes_upstream(self):
        self.check_quiet(headers=False)

    def test_complete_usage_survives_a_late_disconnect_observation(self):
        upstream = MockOpenAIResponses()
        self.addCleanup(upstream.close)
        self.start_watch(upstream)
        finish = ctl_cloud._finish
        observed = threading.Event()

        def late_disconnect(handler, state, item, meter, *args, **kwargs):
            self.assertIsNotNone(meter.finish)
            self.assertIsNotNone(state.watcher)
            state.watcher.disconnected.set()  # schedule the observed TLS completion race
            result = finish(handler, state, item, meter, *args, **kwargs)
            observed.set()
            return result

        with patch.object(ctl_cloud, '_finish', side_effect=late_disconnect):
            self.assertEqual(self.chat(model=REMOTE, stream=True)[0], 200)
            self.assertTrue(observed.wait(2), 'completion race was not exercised')
        row = self.ledger_rows()[0]
        self.assertEqual((row['status'], row['error']), (200, None))
        self.assertEqual(row['settled'], 'usage')
        self.assertEqual(row['cost_usd'], 0.000015)

    def test_late_disconnect_does_not_erase_an_explicit_failure(self):
        upstream = MockOpenAIResponses()
        self.addCleanup(upstream.close)
        self.start_watch(upstream)
        finish = ctl_cloud._finish
        for index, complete in enumerate((False, True), 1):
            with self.subTest(complete_usage=complete):
                observed = threading.Event()
                def explicit_failure(handler, state, item, meter, status, error=None, *args, **kwargs):
                    state.watcher.disconnected.set()
                    if not complete:
                        meter.finish = meter.prompt = meter.completion = None
                    kwargs['failed'] = True
                    result = finish(handler, state, item, meter, status, 'cloud_response_too_large', *args, **kwargs)
                    observed.set()
                    return result

                with patch.object(ctl_cloud, '_finish', side_effect=explicit_failure):
                    self.assertEqual(self.chat(model=REMOTE, stream=True)[0], 200)
                    self.assertTrue(observed.wait(2), 'explicit failure was not exercised')
                row = json.loads(self.ledger_text(minimum=index).splitlines()[-1])
                self.assertEqual(row['error'], 'cloud_response_too_large')
                self.assertEqual(row['settled'], 'reserved')
                self.assertGreater(row['cost_usd'], 0)

    def check_no_watcher(self, enabled, tls):
        upstream = MockOpenAIResponses()
        self.addCleanup(upstream.close)
        self.start_watch(upstream, enabled)
        with patch.object(controller.lan_share, 'is_tls', return_value=tls), \
                patch.object(controller, 'ClientDisconnect', side_effect=AssertionError('unexpected watcher')):
            self.assertEqual(self.chat(model=REMOTE, stream=True)[0], 200)
        self.assertEqual(self.ledger_rows()[0]['settled'], 'usage')

    def test_default_off_does_not_construct_a_watcher(self):
        self.check_no_watcher(False, False)

    def test_tls_downstream_does_not_use_the_plain_socket_peek_watcher(self):
        self.check_no_watcher(True, True)
