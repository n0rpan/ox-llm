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


class MockOpenRouter(_Base):
    """An OpenAI-compatible upstream that records every request and its headers."""

    def __init__(self, status=200, error=None, reply='Cloud reply.', usage=None,
                 response=None, stream_frames=None):
        super().__init__()
        self.status, self.error, self.reply = status, error, reply
        self.usage = usage or {'prompt_tokens': 11, 'completion_tokens': 4, 'total_tokens': 15}
        self.response = response
        self.stream_frames = stream_frames
        self.start()

    def handle(self, handler, method):
        length = int(handler.headers.get('Content-Length', '0'))
        raw = handler.rfile.read(length) if length else b''
        body = json.loads(raw) if raw else None
        record = {'method': method, 'path': handler.path, 'body': body,
                  'headers': {k.lower(): v for k, v in handler.headers.items()}}
        self.requests.append(record)
        if method == 'GET':
            return self._json(handler, 200, {'data': []})
        if self.status >= 400:
            error = self.error
            if isinstance(error, str):
                error = {'error': {'message': error, 'type': 'upstream_error', 'code': 'upstream_error'}}
            return self._json(handler, self.status, error)
        model = body.get('model') if isinstance(body, dict) else None
        if isinstance(body, dict) and body.get('stream') is True:
            if self.stream_frames is not None:
                frames = list(self.stream_frames)
                if isinstance(body.get('stream_options'), dict) and body['stream_options'].get('include_usage'):
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
            if isinstance(body.get('stream_options'), dict) and body['stream_options'].get('include_usage'):
                frames.append({'id': 'chatcmpl-mock', 'object': 'chat.completion.chunk', 'model': model,
                               'choices': [], 'usage': self.usage})
            return self._sse(handler, frames + ['[DONE]'])
        if self.response is not None:
            return self._json(handler, 200, {**self.response, 'model': model} if 'model' not in self.response else self.response)
        self._json(handler, 200, {
            'id': 'chatcmpl-mock', 'object': 'chat.completion', 'created': 1, 'model': model,
            'choices': [{'index': 0, 'message': {'role': 'assistant', 'content': self.reply}, 'finish_reason': 'stop'}],
            'usage': self.usage})
