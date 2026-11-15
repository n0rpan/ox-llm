"""Issue #71: a client cancel mid-stream is ledgered 499/cancelled and settled on what streamed."""
import json
import socket
import time

from mock_cloud import MockAnthropic, MockOpenRouter
from test_cloud_anthropic import AnthropicBase, anthropic_models
from test_cloud_openai import models as openai_models
from test_cloud_remote import CloudCase, KEY, REMOTE


def _slow(self, handler, events):
    handler.send_response(200)
    handler.send_header('Content-Type', 'text/event-stream')
    handler.send_header('Connection', 'close')
    handler.end_headers()
    try:
        for event in events:
            if isinstance(event, tuple):
                block = ('event: %s\ndata: %s\n\n' % (event[0], json.dumps(event[1]))).encode()
            else:
                block = ('data: %s\n\n' % (event if isinstance(event, str) else json.dumps(event))).encode()
            handler.wfile.write(block)
            handler.wfile.flush()
            time.sleep(0.05)
    except OSError:
        pass
    handler.close_connection = True


class SlowOpenRouter(MockOpenRouter):
    _sse = _slow


class SlowAnthropic(MockAnthropic):
    _sse = _slow


def frames(count=60):
    head = {'id': 'c', 'object': 'chat.completion.chunk', 'model': 'm'}
    return [{**head, 'choices': [{'index': 0, 'delta': {'content': 'tok '}, 'finish_reason': None}]}
            for _ in range(count)]


def anthropic_events(count=60):
    start = ('message_start', {'type': 'message_start', 'message': {
        'id': 'm', 'type': 'message', 'role': 'assistant', 'content': [], 'model': 'claude-mock',
        'stop_reason': None, 'stop_sequence': None,
        'usage': {'input_tokens': 12, 'output_tokens': 0,
                  'cache_read_input_tokens': 0, 'cache_creation_input_tokens': 0}}})
    block = ('content_block_start', {'type': 'content_block_start', 'index': 0,
                                     'content_block': {'type': 'text', 'text': ''}})
    delta = ('content_block_delta', {'type': 'content_block_delta', 'index': 0,
                                     'delta': {'type': 'text_delta', 'text': 'tok '}})
    return [start, block] + [delta] * count


def cancel_after_first_chunk(case, model):
    body = json.dumps({'model': model, 'stream': True,
                       'messages': [{'role': 'user', 'content': 'hello'}]}).encode()
    sock = socket.create_connection(('127.0.0.1', case.stack.port), timeout=10)
    sock.sendall(b'POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:%d\r\nContent-Type: application/json\r\n'
                 b'Content-Length: %d\r\n\r\n' % (case.stack.port, len(body)) + body)
    seen = b''
    while b'tok' not in seen:
        chunk = sock.recv(4096)
        assert chunk, seen
        seen += chunk
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, b'\x01\x00\x00\x00\x00\x00\x00\x00')
    sock.close()


class Mixin:
    def check(self, model, reserved_floor):
        cancel_after_first_chunk(self, model)
        row = self.ledger_rows()[-1]
        self.assertEqual(row['status'], 499, row)
        self.assertEqual(row['error'], 'cancelled')
        self.assertEqual(row['settled'], 'estimated')
        self.assertLess(row['cost_usd'], reserved_floor / 10)
        self.assertGreater(row['cost_usd'], 0)
        self.assertTrue(row['remote'])


class OpenRouterCancel(Mixin, CloudCase):
    def test_cancel_is_499_and_estimated(self):
        self.upstream = SlowOpenRouter(stream_frames=frames())
        self.addCleanup(self.upstream.close)
        self.start(budget=5.0)
        self.check(REMOTE, 64000 * 1e-6)


class OpenAICancel(Mixin, CloudCase):
    def test_cancel_is_499_and_estimated(self):
        self.upstream = SlowOpenRouter(stream_frames=frames())
        self.addCleanup(self.upstream.close)
        self.secrets.write_text(json.dumps({'openai': {'api_key': KEY}}))
        self.start(budget=5.0, models=openai_models())
        self.check('cloud-openai', 200000 * 1e-6)


class AnthropicCancel(Mixin, AnthropicBase):
    def test_cancel_is_499_and_estimated(self):
        self.upstream = SlowAnthropic(stream_events=anthropic_events())
        self.addCleanup(self.upstream.close)
        self.start(budget=5.0)
        self.check('anthropic/claude-sonnet-5-5', 200000 * 3e-6)

    def test_later_request_still_sees_full_budget_fail_closed(self):
        self.upstream = SlowAnthropic(stream_events=anthropic_events())
        self.addCleanup(self.upstream.close)
        self.start(budget=5.0)
        cancel_after_first_chunk(self, 'anthropic/claude-sonnet-5-5')
        self.ledger_rows()
        from oxc.runtime import cloud
        self.assertEqual(cloud._reserved.get(cloud.spend_day(), 0), 0)
