"""Local mock cloud upstreams for Wave D tests. No network, no real credentials.

`MockOpenRouter` and `MockAnthropic` speak the provider wire formats on 127.0.0.1 so the
real controller routes can be exercised with an isolated HOME and a synthetic secrets file.
"""
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class _Base:
    def __init__(self):
        self.requests = []
        self.server = None
        self.thread = None

    def start(self):
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def log_message(self, *args):
                pass

            def do_GET(self):
                owner.handle(self, 'GET')

            def do_POST(self):
                owner.handle(self, 'POST')

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.server.daemon_threads = True
        self.port = self.server.server_port
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        return self

    @property
    def base(self):
        return 'http://127.0.0.1:%d' % self.port

    def close(self):
        if self.server is not None:
            self.server.shutdown()
            self.server.server_close()
        if self.thread is not None:
            self.thread.join(timeout=2)

    def _json(self, handler, status, value):
        data = json.dumps(value).encode()
        handler.send_response(status)
        handler.send_header('Content-Type', 'application/json')
        handler.send_header('Content-Length', str(len(data)))
        handler.end_headers()
        handler.wfile.write(data)

    def _sse(self, handler, events):
        handler.send_response(200)
        handler.send_header('Content-Type', 'text/event-stream')
        handler.send_header('Connection', 'close')
        handler.end_headers()
        for event in events:
            if isinstance(event, tuple):
                name, data = event
                block = ('event: %s\ndata: %s\n\n' % (name, json.dumps(data))).encode()
            else:
                block = ('data: %s\n\n' % (event if isinstance(event, str) else json.dumps(event))).encode()
            handler.wfile.write(block)
            handler.wfile.flush()
        handler.close_connection = True

    def _raw_sse(self, handler, chunks, delay=0.0):
        handler.send_response(200)
        handler.send_header('Content-Type', 'text/event-stream')
        handler.send_header('Connection', 'close')
        handler.end_headers()
        import time
        for chunk in chunks:
            handler.wfile.write(chunk if isinstance(chunk, bytes) else chunk.encode())
            handler.wfile.flush()
            if delay:
                time.sleep(delay)
        handler.close_connection = True


class MockOpenRouter(_Base):
    """An OpenAI-compatible upstream that records every request and its headers."""

    def __init__(self, status=200, error=None, reply='Cloud reply.', usage=None,
                 response=None, stream_frames=None, stream_chunks=None, stream_delay=0.0,
                 usage_missing=False, hook=None):
        super().__init__()
        self.status, self.error, self.reply = status, error, reply
        self.usage = usage or {'prompt_tokens': 11, 'completion_tokens': 4, 'total_tokens': 15}
        self.response = response
        self.stream_frames = stream_frames
        self.stream_chunks = stream_chunks
        self.stream_delay = stream_delay
        self.usage_missing = usage_missing
        self.hook = hook
        self.start()

    def handle(self, handler, method):
        length = int(handler.headers.get('Content-Length', '0'))
        raw = handler.rfile.read(length) if length else b''
        body = json.loads(raw) if raw else None
        record = {'method': method, 'path': handler.path, 'body': body,
                  'headers': {k.lower(): v for k, v in handler.headers.items()}}
        self.requests.append(record)
        if self.hook is not None:
            self.hook()
        if method == 'GET':
            return self._json(handler, 200, {'data': []})
        if self.status >= 400:
            error = self.error
            if isinstance(error, str):
                error = {'error': {'message': error, 'type': 'upstream_error', 'code': 'upstream_error'}}
            return self._json(handler, self.status, error)
        model = body.get('model') if isinstance(body, dict) else None
        if isinstance(body, dict) and body.get('stream') is True:
            if self.stream_chunks is not None:
                return self._raw_sse(handler, self.stream_chunks, self.stream_delay)
            if self.stream_frames is not None:
                frames = list(self.stream_frames)
                if (not self.usage_missing and isinstance(body.get('stream_options'), dict)
                        and body['stream_options'].get('include_usage')):
                    frames.append({'id': 'chatcmpl-mock', 'object': 'chat.completion.chunk', 'model': model,
                                   'choices': [], 'usage': self.usage})
                return self._sse(handler, frames + ['[DONE]'])
            frames = [
                {'id': 'chatcmpl-mock', 'object': 'chat.completion.chunk', 'model': model,
                 'choices': [{'index': 0, 'delta': {'role': 'assistant', 'content': ''}, 'finish_reason': None}]},
                {'id': 'chatcmpl-mock', 'object': 'chat.completion.chunk', 'model': model,
                 'choices': [{'index': 0, 'delta': {'content': self.reply}, 'finish_reason': None}]},
                {'id': 'chatcmpl-mock', 'object': 'chat.completion.chunk', 'model': model,
                 'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'stop'}]},
            ]
            if (not self.usage_missing and isinstance(body.get('stream_options'), dict)
                    and body['stream_options'].get('include_usage')):
                frames.append({'id': 'chatcmpl-mock', 'object': 'chat.completion.chunk', 'model': model,
                               'choices': [], 'usage': self.usage})
            return self._sse(handler, frames + ['[DONE]'])
        if self.response is not None:
            return self._json(handler, 200, {**self.response, 'model': model}
                              if 'model' not in self.response else self.response)
        reply = {
            'id': 'chatcmpl-mock', 'object': 'chat.completion', 'created': 1, 'model': model,
            'choices': [{'index': 0, 'message': {'role': 'assistant', 'content': self.reply}, 'finish_reason': 'stop'}]}
        if not self.usage_missing:
            reply['usage'] = self.usage
        self._json(handler, 200, reply)


class MockAnthropic(_Base):
    """A native Messages API mock: records the SDK request and answers in Anthropic's wire shape."""

    def __init__(self, status=200, error=None, message=None, stream_events=None, stream_failure=False):
        super().__init__()
        self.status, self.error = status, error
        self.message = message or {
            'id': 'msg_mock', 'type': 'message', 'role': 'assistant',
            'content': [{'type': 'text', 'text': 'Anthropic reply.'}],
            'model': 'claude-mock', 'stop_reason': 'end_turn', 'stop_sequence': None,
            'usage': {'input_tokens': 12, 'output_tokens': 5,
                      'cache_read_input_tokens': 3, 'cache_creation_input_tokens': 0}}
        self.stream_events = stream_events
        self.stream_failure = stream_failure
        self.start()

    def handle(self, handler, method):
        length = int(handler.headers.get('Content-Length', '0'))
        raw = handler.rfile.read(length) if length else b''
        body = json.loads(raw) if raw else None
        record = {'method': method, 'path': handler.path, 'body': body,
                  'headers': {k.lower(): v for k, v in handler.headers.items()}}
        self.requests.append(record)
        if self.status >= 400:
            error = self.error
            if isinstance(error, str):
                error = {'type': 'error', 'error': {'type': 'invalid_request_error', 'message': error}}
            return self._json(handler, self.status, error)
        if isinstance(body, dict) and body.get('stream') is True:
            return self._sse(handler, self.stream_events or self.default_stream())
        return self._json(handler, 200, self.message)

    def default_stream(self):
        usage_in = {'input_tokens': 12, 'output_tokens': 0,
                    'cache_read_input_tokens': 3, 'cache_creation_input_tokens': 0}
        return [
            ('message_start', {'type': 'message_start', 'message': {
                'id': 'msg_mock', 'type': 'message', 'role': 'assistant', 'content': [],
                'model': 'claude-mock', 'stop_reason': None, 'stop_sequence': None,
                'usage': usage_in}}),
            ('content_block_start', {'type': 'content_block_start', 'index': 0,
                                     'content_block': {'type': 'text', 'text': ''}}),
            ('content_block_delta', {'type': 'content_block_delta', 'index': 0,
                                     'delta': {'type': 'text_delta', 'text': 'Anthropic '}}),
            ('content_block_delta', {'type': 'content_block_delta', 'index': 0,
                                     'delta': {'type': 'text_delta', 'text': 'reply.'}}),
            ('content_block_stop', {'type': 'content_block_stop', 'index': 0}),
            ('message_delta', {'type': 'message_delta',
                               'delta': {'stop_reason': 'end_turn', 'stop_sequence': None},
                               'usage': {'output_tokens': 5}}),
            ('message_stop', {'type': 'message_stop'}),
        ]


class MockOpenAIResponses(_Base):
    """Native Responses wire mock; records API-key requests only on loopback."""

    def __init__(self, response=None, events=None):
        super().__init__()
        self.response = response or {
            'id': 'resp_mock', 'object': 'response', 'created_at': 1, 'status': 'completed',
            'output': [{'type': 'message', 'id': 'msg_mock', 'role': 'assistant',
                        'status': 'completed', 'content': [{'type': 'output_text', 'text': 'Done.'}]}],
            'usage': {'input_tokens': 11, 'output_tokens': 4,
                      'input_tokens_details': {'cached_tokens': 2},
                      'output_tokens_details': {'reasoning_tokens': 1}}}
        self.events = events
        self.start()

    def handle(self, handler, method):
        body = json.loads(handler.rfile.read(int(handler.headers.get('Content-Length', '0'))))
        self.requests.append({'method': method, 'path': handler.path, 'body': body,
                              'headers': {k.lower(): v for k, v in handler.headers.items()}})
        if handler.path != '/v1/responses':
            return self._json(handler, 404, {'error': {'message': 'wrong endpoint'}})
        if body.get('stream'):
            events = self.events
            if events is None:
                events = [{'type': 'response.created', 'response': {'id': 'resp_mock'}},
                          {'type': 'response.output_text.delta', 'output_index': 0, 'delta': 'Done.'},
                          {'type': 'response.completed', 'response': self.response}]
            return self._sse(handler, [(event['type'], event) for event in events])
        return self._json(handler, 200, self.response)
