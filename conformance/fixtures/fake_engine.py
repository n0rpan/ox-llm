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
import stat
import sys
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
        if self.path == '/state-cache' and self.server.state_root:
            return self.state_reply({'budget_bytes': self.server.state_budget, 'used_bytes': 1234,
                                     'preload': self.server.preload_status})
        return self.json_reply(404, {'error': {'message': 'Unknown synthetic route.',
                                             'type': 'not_found_error', 'code': 404}})

    def state_reply(self, body, status=200):
        # Independently scripted protocol receipts. The mock never trims the
        # seeded KV files, so tests detect any controller mutation while owned.
        fault = Path(os.environ['FAKE_ENGINE_EVENTS']).with_name('engine-cache-fault')
        mode = fault.read_text().strip() if fault.exists() else ''
        if mode == 'error':
            return self.json_reply(503, {'error': 'Synthetic cache refusal.'})
        if mode == 'malformed':
            return self.json_reply(200, ['synthetic non-object'])
        if mode == 'oversized':
            return self.json_reply(200, {'synthetic': 'x' * (1024**2 + 1)})
        return self.json_reply(status, body)

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
        if os.environ.get('FAKE_ENGINE_HEADERS') == '1':
            # Only bounded synthetic hint headers; never authentication material.
            names = ('X-Oxe-Session', 'X-Oxe-Turn-Kind', 'X-Oxe-Snapshot', 'X-Oxe-Probe')
            event('hints', values={name: self.headers.get(name) for name in names})
        if self.server.state_root and self.path == '/state-cache/budget':
            self.server.state_budget = body['bytes']
            return self.state_reply({'used_bytes': 777, 'freed_bytes': 222})
        if self.server.state_root and self.path == '/state-cache/clear':
            return self.state_reply({'freed_bytes': 333, 'sessions': 2})
        if self.server.state_root and self.path == '/state-cache/preload':
            self.server.preload_status = {'status': 'done', 'pos': 256, 'ms': 12.345}
            return self.state_reply({'status': 'started'}, status=202)
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
        self.ledger()
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
        self.ledger()
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

    def ledger(self):
        if self.server.ledger is None:
            return
        row = {'n_prompt': 8, 'predicted_n': 3, 'prompt_ms': 12.5,
               'cache_n': 2, 'predicted_ms': 1.25, 'prefix_reason': 'hit',
               'session_reason': 'empty-index', 'cache_src': 'preload',
               'resume_src': 'disk', 'preload_src': 'disk', 'stop': 'eos',
               'prompt': 'SYNTHETIC-PRIVATE-LEDGER-TEXT',
               'credential': Path(os.environ['OXE_API_KEY_FILE']).read_text().strip(),
               'unknown': {'text': 'SYNTHETIC-PRIVATE-LEDGER-TEXT'}}
        mode = os.environ.get('FAKE_ENGINE_LEDGER_CASE', 'enabled')
        lines = [json.dumps(row).encode()]
        if mode == 'invalid':
            lines = [b'not json', b'[]', b'{}',
                     b'{"n_prompt":true,"predicted_n":3,"prompt_ms":1}',
                     b'{"n_prompt":8,"predicted_n":3,"prompt_ms":Infinity}',
                     json.dumps({**row, 'cache_n': False, 'prefix_reason': 'PRIVATE',
                                 'session_reason': ['hit'], 'predicted_ms': float('nan'),
                                 'park_ms': float('inf'), 'resume_ms': '2'}).encode()]
        elif mode == 'chunked':
            lines = [b'x' * 9000, *lines]
        raw = b'\n'.join(lines) + (b'' if mode == 'tail' else b'\n')
        with EVENT_LOCK:
            if mode == 'chunked':
                for offset in range(0, len(raw), 31):
                    self.server.ledger.write(raw[offset:offset + 31])
            else:
                self.server.ledger.write(raw)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--host', choices=['127.0.0.1'], required=True)
    parser.add_argument('--port', type=int, required=True)
    parser.add_argument('--alias', required=True)
    parser.add_argument('--model', required=True)
    parser.add_argument('--ctx-size', type=int)
    parser.add_argument('--state-cache')
    parser.add_argument('--state-budget-bytes', type=int)
    parser.add_argument('--state-ttl-secs', type=int)
    parser.add_argument('--prefix-cache')
    parser.add_argument('--session-cache')
    parser.add_argument('--prefix-cache-max-bytes', type=int)
    parser.add_argument('--prefix-cache-min-tokens', type=int)
    parser.add_argument('--prefix-cache-ttl-h', type=float)
    parser.add_argument('--ledger')
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
    server.state_root, server.state_budget = args.state_cache, args.state_budget_bytes
    server.preload_status = {'status': 'idle'}
    server.ledger = open(args.ledger, 'ab', buffering=0) if args.ledger else None
    ledger = None
    if os.environ.get('FAKE_ENGINE_LEDGER_CASE'):
        ledger = {'argument': bool(args.ledger),
                  'pipe': stat.S_ISFIFO(os.fstat(server.ledger.fileno()).st_mode) if server.ledger else False,
                  'stdin_eof': os.read(0, 1) == b''}
        # A separate stream must never be mistaken for the numeric ledger pipe.
        print('{"n_prompt":999,"predicted_n":999,"prompt_ms":999}', flush=True)
    state_args = None
    if args.state_cache:
        directory = Path(args.state_cache)
        before = directory.exists()
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        state_args = {'root': args.state_cache, 'budget': args.state_budget_bytes,
                      'ttl': args.state_ttl_secs, 'existed': before}
    legacy = None
    if os.environ.get('FAKE_ENGINE_LEGACY_STORES') == '1':
        legacy = {'argv': sys.argv[sys.argv.index('--alias') + 2:], 'directories': {}}
        for name, path in (('prefix', args.prefix_cache), ('sessions', args.session_cache)):
            if path:
                directory = Path(path)
                legacy['directories'][name] = {'path': path, 'exists': directory.exists(),
                                               'mode': directory.stat().st_mode & 0o777 if directory.exists() else None}
    event('started', pid=os.getpid(), alias=args.alias,
          affinity=sorted(os.sched_getaffinity(0)),
          **({'context_size': args.ctx_size} if args.ctx_size is not None else {}),
          **({'state_cache': state_args} if os.environ.get('FAKE_ENGINE_STATE_CACHE') == '1' else {}),
          **({'legacy': legacy} if legacy is not None else {}))
    if ledger is not None:
        event('ledger-started', **ledger)
    server.serve_forever()


if __name__ == '__main__':
    main()
