#!/usr/bin/env python3
"""Synthetic engine protocol peer, launched by oxc's real lifecycle.

No controller imports. Model contents are never interpreted. The gate file lets
conformance tests hold an inference deterministically and observe cancellation.
"""
import argparse
import json
import os
from pathlib import Path
import select
import socket
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REPLY = ['Synthetic', 'engine', 'reply.']
EVENT_LOCK = threading.Lock()


def event(kind, **fields):
    row = json.dumps({'event': kind, **fields}, separators=(',', ':')) + '\n'
    with EVENT_LOCK:
        with open(os.environ['FAKE_ENGINE_EVENTS'], 'a') as stream:
            stream.write(row)


class Handler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def json_reply(self, status, body):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def authorized(self):
        key = Path(os.environ['OXE_API_KEY_FILE']).read_text().strip()
        ok = self.headers.get('Authorization') == 'Bearer ' + key
        if not ok:
            self.json_reply(401, {'error': 'Engine authentication required.'})
        return ok

    def do_GET(self):
        if not self.authorized():
            return
        event('request', method='GET', path=self.path)
        if self.path == '/health':
            return self.json_reply(200, {'status': 'ok'})
        if self.path == '/props':
            return self.json_reply(200, {'sampling': {'presence_penalty': True,
                'frequency_penalty': True, 'logit_bias': True}})
        if self.path == '/v1/models':
            return self.json_reply(200, {'object': 'list', 'data': [
                {'id': self.server.alias, 'object': 'model', 'created': 0, 'owned_by': 'local'}]})
        return self.json_reply(404, {'error': {'message': 'Unknown synthetic route.',
                                             'type': 'not_found_error', 'code': 404}})

    def wait_gate(self, body):
        if '[hold]' not in json.dumps(body):
            return True
        event('held', path=self.path)
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if Path(os.environ['FAKE_ENGINE_GATE']).exists():
                return True
            ready, _, _ = select.select([self.connection], [], [], 0.02)
            if ready and not self.connection.recv(1, socket.MSG_PEEK):
                event('cancelled', path=self.path)
                return False
        event('gate-timeout', path=self.path)
        return False

    def do_POST(self):
        if not self.authorized():
            return
        body = json.loads(self.rfile.read(int(self.headers.get('Content-Length', '0'))))
        event('request', method='POST', path=self.path, body=body)
        if not self.wait_gate(body):
            self.close_connection = True
            return
        try:
            if '[upstream-error]' in json.dumps(body):
                self.json_reply(422, {'error': {'code': 422, 'message': 'Synthetic engine refusal.',
                                               'type': 'invalid_request_error'}})
            elif self.path in ('/v1/chat/completions', '/chat/completions'):
                self.chat(body)
            elif self.path == '/completion':
                self.completion(body)
            else:
                self.json_reply(404, {'error': {'message': 'Unknown synthetic route.',
                                               'type': 'not_found_error', 'code': 404}})
            event('finished', path=self.path)
        except (BrokenPipeError, ConnectionResetError):
            event('cancelled', path=self.path)

    def sse(self, frames, done=False):
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Connection', 'close')
        self.end_headers()
        for frame in frames:
            self.wfile.write(('data: ' + json.dumps(frame) + '\n\n').encode())
            self.wfile.flush()
        if done:
            self.wfile.write(b'data: [DONE]\n\n')
            self.wfile.flush()
        self.close_connection = True

    def chat(self, body):
        limit = body.get('max_tokens', body.get('max_completion_tokens', 99))
        words = REPLY[:limit]
        finish = 'length' if len(words) < len(REPLY) else 'stop'
        text = ' '.join(words)
        stops = body.get('stop', [])
        stops = [stops] if isinstance(stops, str) else stops
        for stop in stops:
            if stop and stop in text:
                text, finish = text.split(stop)[0], 'stop'
        calls = []
        for i, tool in enumerate(body.get('tools', [])[:1 if body.get('parallel_tool_calls') is False else 2]):
            if body.get('tool_choice') == 'none':
                break
            calls.append({'id': 'call_synthetic_' + str(i), 'type': 'function',
                          'function': {'name': tool['function']['name'], 'arguments': '{"value":1}'}})
        if calls:
            text, finish = '', 'tool_calls'
        thought = 'Synthetic reasoning.' if body.get('chat_template_kwargs', {}).get('enable_thinking') else None
        generated = len(words) if not calls else len(calls)
        usage = {'prompt_tokens': 8, 'completion_tokens': generated, 'total_tokens': 8 + generated,
                 'prompt_tokens_details': {'cached_tokens': 2}}
        timings = {'cache_n': 2, 'prompt_n': 6, 'predicted_n': generated}
        meta = {'id': 'chatcmpl-synthetic', 'created': 1, 'model': body.get('model'),
                'system_fingerprint': 'synthetic'}
        logprobs = {'content': [{'token': text, 'logprob': -0.5, 'bytes': list(text.encode()),
                    'top_logprobs': [{'token': text, 'logprob': -0.5, 'bytes': list(text.encode())}]}]}
        if body.get('stream') is not True:
            message = {'role': 'assistant', 'content': text}
            if calls:
                message['tool_calls'] = calls
            if thought:
                message['reasoning_content'] = thought
            return self.json_reply(200, {**meta, 'object': 'chat.completion', 'usage': usage,
                'timings': timings, 'choices': [{'index': 0, 'finish_reason': finish, 'message': message,
                    **({'logprobs': logprobs} if body.get('logprobs') is True else {})}]})
        def chunk(delta, reason=None):
            return {**meta, 'object': 'chat.completion.chunk', 'choices': [
                {'index': 0, 'finish_reason': reason, 'delta': delta}]}
        frames = [chunk({'role': 'assistant', 'content': None})]
        if thought:
            frames.append(chunk({'reasoning_content': thought}))
        if text:
            frames.append(chunk({'content': text}))
            if body.get('logprobs') is True:
                frames[-1]['choices'][0]['logprobs'] = logprobs
        for i, call in enumerate(calls):
            frames.append(chunk({'tool_calls': [{'index': i, 'id': call['id'], 'type': 'function',
                          'function': {'name': call['function']['name'], 'arguments': ''}}]}))
            for fragment in ('{"value":', '1}'):
                frames.append(chunk({'tool_calls': [{'index': i, 'function': {'arguments': fragment}}]}))
        frames.append(chunk({}, finish))
        if body.get('stream_options', {}).get('include_usage'):
            frames.append({**meta, 'object': 'chat.completion.chunk', 'choices': [], 'usage': usage})
        frames[-1]['timings'] = timings
        self.sse(frames, done=True)

    def completion(self, body):
        words = REPLY[:body.get('n_predict', 99)]
        pieces = [(' ' if i else '') + word for i, word in enumerate(words)]
        final = {'index': 0, 'content': '', 'tokens': [], 'id_slot': 0, 'stop': True,
                 'model': self.server.alias, 'tokens_predicted': len(words), 'tokens_evaluated': 8,
                 'stop_type': 'limit' if len(words) < len(REPLY) else 'eos',
                 'timings': {'cache_n': 2, 'prompt_n': 6, 'predicted_n': len(words)}}
        def probability(piece):
            return {'id': 1, 'token': piece, 'bytes': list(piece.encode()), 'logprob': -0.5,
                    'top_logprobs': [{'id': 1, 'token': piece, 'bytes': list(piece.encode()), 'logprob': -0.5}]}
        if body.get('stream') is True:
            frames = [{'index': 0, 'content': piece, 'tokens': [], 'stop': False, 'id_slot': 0,
                       **({'completion_probabilities': [probability(piece)]} if body.get('n_probs') else {})}
                      for piece in pieces]
            return self.sse(frames + [final])
        final['content'] = ''.join(pieces)
        if body.get('n_probs'):
            final['completion_probabilities'] = [probability(piece) for piece in pieces]
        self.json_reply(200, final)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--host', choices=['127.0.0.1'], required=True)
    parser.add_argument('--port', type=int, required=True)
    parser.add_argument('--alias', required=True)
    parser.add_argument('--model', required=True)
    args = parser.parse_args()
    if diagnostic := os.environ.get('FAKE_ENGINE_STDERR'):
        os.write(2, diagnostic.encode())
    # Keep this child's live /proc metadata visible through the host fixture.
    # Other host inputs (memory capacity, PSI and cgroup limits) are synthetic.
    proc_entry = Path(os.environ['FAKE_PROC_VIEW']) / str(os.getpid())
    if not proc_entry.exists():
        proc_entry.symlink_to('/run/oxc-conformance-proc/' + str(os.getpid()))
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    server.daemon_threads = True
    server.alias = args.alias
    event('started', pid=os.getpid(), alias=args.alias,
          affinity=sorted(os.sched_getaffinity(0)))
    server.serve_forever()


if __name__ == '__main__':
    main()
