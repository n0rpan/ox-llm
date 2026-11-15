"""Native SDK cancellation under the default-off orchestrator opt-in."""
import json
import asyncio
import os
import select
import socket
import ssl
import subprocess
import threading
import time
import types
import unittest
from unittest.mock import patch

from snapdragon_ai_lab.runtime import cloud_anthropic, controller
from snapdragon_ai_lab.runtime.orchestrator_anthropic import Client, Canceled

from mock_cloud import MockAnthropic
import mock_cloud
from openai_api_stack import FakeEngine
from test_cloud_anthropic import AnthropicBase
from test_cloud_remote import KEY


class CancellationCleanup(unittest.TestCase):
    def test_repeated_shutdown_does_not_interrupt_owned_resource_cleanup(self):
        started, closing, release, closed = (threading.Event() for _ in range(4))

        class OwnedSDK:
            def __init__(self, **kwargs):
                self.messages = self

            async def __aenter__(self):
                return self

            async def create(self, **params):
                started.set()
                await asyncio.Future()

            async def __aexit__(self, *args):
                closing.set()
                while not release.is_set():
                    await asyncio.sleep(0.01)
                closed.set()

        errors = []
        owner = Client({'base_url': 'http://127.0.0.1:1'}, KEY, 2)

        def consume():
            try:
                owner.create(stream=True)
            except Exception as error:
                errors.append(error)

        with patch.dict('sys.modules', {'anthropic': types.SimpleNamespace(AsyncAnthropic=OwnedSDK)}):
            consumer = threading.Thread(target=consume)
            consumer.start()
            try:
                self.assertTrue(started.wait(2))
                owner.shutdown()  # downstream EOF observer
                self.assertTrue(closing.wait(2))
                owner.shutdown()  # request owner also observes the interruption
            finally:
                release.set()
                owner.close()
                consumer.join(timeout=2)
        self.assertFalse(consumer.is_alive())
        self.assertTrue(closed.is_set(), 'repeated cancellation interrupted resource cleanup')
        self.assertEqual(len(errors), 1)
        self.assertIsInstance(errors[0], Canceled)


class QuietAnthropic(MockAnthropic):
    def __init__(self, headers):
        self.headers = headers
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
            event = {'type': 'message_start', 'message': {
                'id': 'msg_synthetic', 'type': 'message', 'role': 'assistant',
                'model': 'claude-sonnet-5-5', 'content': [], 'stop_reason': None,
                'usage': {'input_tokens': 10, 'output_tokens': 0}}}
            handler.wfile.write(('event: message_start\ndata: ' + json.dumps(event) + '\n\n').encode())
            handler.wfile.flush()
        self.started.set()
        try:
            while not self.release.wait(0.01):
                ready, _, _ = select.select([handler.connection], [], [], 0.01)
                if ready:
                    flags = 0 if isinstance(handler.connection, ssl.SSLSocket) else socket.MSG_PEEK
                    try:
                        data = handler.connection.recv(1, flags)
                    except (BrokenPipeError, ConnectionResetError, ssl.SSLEOFError, ssl.SSLZeroReturnError):
                        # A canceled TLS peer may reset rather than send close_notify.
                        self.disconnected.set()
                        return
                    if not data:
                        self.disconnected.set()
                        return
        finally:
            handler.close_connection = True


class AnthropicDisconnect(AnthropicBase):
    def tls_upstream(self, headers, trusted=True):
        certificate, key = self.tmp / 'mock-cert.pem', self.tmp / 'mock-key.pem'
        subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes',
                        '-keyout', str(key), '-out', str(certificate), '-days', '1',
                        '-subj', '/CN=127.0.0.1', '-addext', 'subjectAltName=IP:127.0.0.1'],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        key.chmod(0o600)
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(certificate, key)
        server_type = mock_cloud.ThreadingHTTPServer

        def tls_server(*args, **kwargs):
            server = server_type(*args, **kwargs)
            server.socket = context.wrap_socket(server.socket, server_side=True)
            return server

        with patch.object(mock_cloud, 'ThreadingHTTPServer', tls_server):
            upstream = QuietAnthropic(headers)
        # Local metadata routes remain plain HTTP and separate from the provider.
        backend = FakeEngine()
        self.addCleanup(backend.close)
        if trusted:
            trust = patch.dict(os.environ, {'SSL_CERT_FILE': str(certificate)})
            trust.start()
            self.addCleanup(trust.stop)
        endpoint = types.SimpleNamespace(base=upstream.base.replace('http://', 'https://', 1),
                                         port=backend.port)
        return upstream, endpoint

    def enable_watch(self):
        value = json.loads(self.stack.config_path.read_text())['cloud']
        value['orchestrator_disconnect_watch'] = True
        self.stack.set_config(cloud=value)

    def check_disconnect(self, headers, enabled=True, stream=True, tls=False):
        if tls:
            upstream, endpoint = self.tls_upstream(headers)
        else:
            upstream = endpoint = QuietAnthropic(headers)
        self.addCleanup(upstream.close)
        self.addCleanup(upstream.release.set)
        self.start(mock=endpoint)
        if enabled:
            self.enable_watch()
        body = json.dumps({'model': 'anthropic/claude-sonnet-5-5',
                           'messages': [{'role': 'user', 'content': 'synthetic'}],
                           'stream': stream}).encode()
        with socket.create_connection(('127.0.0.1', self.stack.port), timeout=2) as client:
            client.sendall(f'POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{self.stack.port}\r\nContent-Type: application/json\r\nContent-Length: '.encode()
                           + str(len(body)).encode() + b'\r\n\r\n' + body)
            self.assertTrue(upstream.started.wait(2))
            client.shutdown(socket.SHUT_RDWR)
        if not enabled:
            self.assertFalse(upstream.disconnected.wait(0.2))
            upstream.release.set()
            self.assertNotEqual(self.ledger_rows()[0]['status'], 499)
            return
        self.assertTrue(upstream.disconnected.wait(2), 'native SDK retained canceled upstream')
        deadline = time.monotonic() + 2
        rows = []
        while time.monotonic() < deadline:
            rows = self.ledger_rows()
            if rows:
                break
            time.sleep(0.01)
        self.assertEqual(len(rows), 1)
        row = rows[0]
        self.assertEqual((row['status'], row['error']), (499, 'client_disconnected'))
        self.assertEqual(row['settled'], 'reserved')
        self.assertTrue(row['usage_missing'])
        self.assertGreater(row['cost_usd'], 0)
        self.assertNotIn(KEY, json.dumps(rows))
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline and any(t.name in (
                'local-ai-client-eof', 'local-ai-anthropic-request') for t in threading.enumerate()):
            time.sleep(0.01)
        self.assertFalse(any(t.name in ('local-ai-client-eof', 'local-ai-anthropic-request')
                             for t in threading.enumerate()))

    def test_native_quiet_stream_cancels(self):
        self.check_disconnect(True)

    def test_native_pre_header_request_cancels(self):
        self.check_disconnect(False)

    def test_native_watcher_remains_off_by_default(self):
        self.check_disconnect(False, enabled=False)

    def test_native_nonstream_pre_header_request_cancels(self):
        self.check_disconnect(False, stream=False)

    def test_native_tls_quiet_stream_cancels(self):
        self.check_disconnect(True, tls=True)

    def test_native_tls_pre_header_request_cancels(self):
        self.check_disconnect(False, tls=True)

    def test_native_tls_rejects_an_untrusted_certificate(self):
        upstream, endpoint = self.tls_upstream(False, trusted=False)
        self.addCleanup(upstream.close)
        self.addCleanup(upstream.release.set)
        self.start(mock=endpoint)
        self.enable_watch()
        status, body = self.chat(stream=True)
        self.assertEqual(status, 503, body)
        self.assertFalse(upstream.started.is_set(), 'unverified TLS sent a provider request')
        row = self.ledger_rows()[0]
        self.assertNotEqual(row.get('error'), 'client_disconnected')
        self.assertNotIn(KEY, json.dumps(row))

    def test_tls_downstream_does_not_use_plain_socket_watcher(self):
        upstream = MockAnthropic()
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        self.enable_watch()
        with patch.object(controller.lan_share, 'is_tls', return_value=True), \
                patch.object(controller, 'ClientDisconnect', side_effect=AssertionError('unexpected watcher')):
            self.assertEqual(self.chat()[0], 200)

    def test_watched_complete_stream_preserves_usage(self):
        upstream = MockAnthropic()
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        self.enable_watch()
        status, body = self.chat(stream=True, stream_options={'include_usage': True})
        self.assertEqual(status, 200, body)
        self.assertIn(b'[DONE]', body)
        row = self.ledger_rows()[0]
        self.assertEqual(row['status'], 200)
        self.assertEqual(row['settled'], 'usage')
        self.assertFalse(row.get('usage_missing', False))

    def test_watched_nonstream_response_preserves_usage(self):
        upstream = MockAnthropic()
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        self.enable_watch()
        status, body = self.chat()
        self.assertEqual(status, 200, body)
        self.assertEqual(self.ledger_rows()[0]['settled'], 'usage')

    def test_watched_explicit_provider_failure_is_preserved_and_redacted(self):
        upstream = MockAnthropic(status=429, error='synthetic refusal ' + KEY)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        self.enable_watch()
        status, body = self.chat()
        self.assertEqual(status, 429, body)
        self.assertNotIn(KEY.encode(), body)
        row = self.ledger_rows()[0]
        self.assertEqual((row['status'], row['error']), (429, 'cloud_upstream_error'))
        self.assertNotIn(KEY, json.dumps(row))

    def test_complete_usage_survives_downstream_failure_after_message_stop(self):
        upstream = MockAnthropic()
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        self.enable_watch()
        write = cloud_anthropic._write
        observed = threading.Event()

        def late_disconnect(handler, state, data):
            if b'[DONE]' in data:
                state.watcher.cancel()
                observed.set()
                raise BrokenPipeError('synthetic downstream failure')
            return write(handler, state, data)

        with patch.object(cloud_anthropic, '_write', side_effect=late_disconnect):
            # Read until EOF directly: the intentionally interrupted chunked
            # downstream has no terminating chunk and must not be normalized.
            body = json.dumps({'model': 'anthropic/claude-sonnet-5-5',
                               'messages': [{'role': 'user', 'content': 'synthetic'}],
                               'stream': True}).encode()
            with socket.create_connection(('127.0.0.1', self.stack.port), timeout=2) as client:
                client.sendall(f'POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{self.stack.port}\r\nContent-Type: application/json\r\nContent-Length: '.encode()
                               + str(len(body)).encode() + b'\r\n\r\n' + body)
                while client.recv(65536):
                    pass
            self.assertTrue(observed.wait(2))
        row = self.ledger_rows()[0]
        self.assertEqual((row['status'], row['error']), (200, None))
        self.assertEqual(row['settled'], 'usage')
        self.assertEqual(row['completion_tokens'], 5)
