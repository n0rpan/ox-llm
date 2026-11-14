"""V020-EXPLORE-1 finding 5 (observer fixes before sealed cancellation measurements), model-free:
(1) the recording proxy sees a client cancel on the client socket itself, not one heartbeat later;
(2) RPC JSONL evidence lines are never truncated; (3) the unload response is logged and checked."""
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import select
import socket
import sys
import tempfile
import threading
import time
import unittest

LAB = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(LAB / 'tools/tiny_real'), str(LAB / 'benchmarks/gatepack'), str(LAB / 'benchmarks/fixtures')]
import tiny_ab as ab  # noqa: E402
import oxa_tiny_loop as oxl  # noqa: E402
from scoped_controller import ScopedController  # noqa: E402


class ProxyCancelTests(unittest.TestCase):
    """Codex proxy_cancel_control: upstream EOF 0.00006 s direct vs 0.40 s through the proxy with
    0.2 s heartbeats. Through the proxy it must now be close to direct."""
    HEARTBEAT = 0.2

    def measure(self, via_proxy):
        closed = threading.Event()
        out = {}
        hb = self.HEARTBEAT

        class Upstream(BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def log_message(self, *a):
                return

            def do_POST(self):
                self.rfile.read(int(self.headers['Content-Length']))
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Connection', 'close')
                self.end_headers()
                self.wfile.write(b'data: {"choices":[{"delta":{"content":"x"}}]}\n\n')
                self.wfile.flush()
                start = time.monotonic()
                nxt = start + hb
                while time.monotonic() - start < 3:
                    try:
                        r, _, _ = select.select([self.connection], [], [], .005)
                        if r and self.connection.recv(1, socket.MSG_PEEK) == b'':
                            break
                        if time.monotonic() >= nxt:
                            self.wfile.write(b': heartbeat\n\n')
                            self.wfile.flush()
                            nxt += hb
                    except OSError:
                        break
                out['closed'] = time.monotonic()
                closed.set()

        up = ThreadingHTTPServer(('127.0.0.1', 0), Upstream)
        threading.Thread(target=up.serve_forever, daemon=True).start()
        servers = [up]
        port = up.server_address[1]
        rec = None
        with tempfile.TemporaryDirectory() as d:
            if via_proxy:
                rec = ab.Recorder(port, Path(d) / 'req.jsonl')
                px = ThreadingHTTPServer(('127.0.0.1', 0), ab.make_handler(rec))
                threading.Thread(target=px.serve_forever, daemon=True).start()
                servers.append(px)
                port = px.server_address[1]
            try:
                c = http.client.HTTPConnection('127.0.0.1', port, timeout=4)
                c.request('POST', '/v1/chat/completions',
                          json.dumps({'model': 'm', 'stream': True, 'messages': [{'role': 'user', 'content': 'u'}]}),
                          {'Content-Type': 'application/json'})
                sock = c.sock
                r = c.getresponse()
                r.read1(64)          # first chunk arrived: now cancel
                time.sleep(0.05)     # fall between heartbeats
                t_close = time.monotonic()
                sock.shutdown(socket.SHUT_RDWR)
                r.close()
                c.close()
                self.assertTrue(closed.wait(4))
                delay = out['closed'] - t_close
                if rec is not None:
                    for _ in range(100):
                        if rec.rows:
                            break
                        time.sleep(.02)
                return delay, (rec.rows if rec else None)
            finally:
                for s in servers:
                    s.shutdown()
                    s.server_close()

    def test_proxy_sees_cancel_without_heartbeat_delay(self):
        direct, _ = self.measure(False)
        proxied, rows = self.measure(True)
        self.assertLess(direct, 0.1)
        self.assertLess(proxied, 0.1, 'proxy delayed the cancel by %.3f s' % proxied)
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]['client_dropped'], 'client_eof')
        self.assertIsInstance(rows[0]['client_eof_s'], float)
        self.assertNotIn('proxy_error', rows[0])

    def test_cancel_while_waiting_for_upstream_headers(self):
        """Codex b97af0e preheader_cancel_control: the client disconnects after upstream received
        the request but BEFORE upstream sends headers (1 s header delay, e.g. a long prefill).
        Upstream must see EOF at once through the proxy, as it does direct."""
        ready, closed = threading.Event(), threading.Event()
        res = {}

        class Up(BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def log_message(self, *a):
                return

            def do_POST(self):
                self.rfile.read(int(self.headers['Content-Length']))
                ready.set()
                start, sent = time.monotonic(), False
                while time.monotonic() - start < 2:
                    try:
                        rd, _, _ = select.select([self.connection], [], [], .005)
                        if rd and self.connection.recv(1, socket.MSG_PEEK) == b'':
                            break
                        if not sent and time.monotonic() - start >= 1:
                            sent = True
                            data = b'data: [DONE]\n\n'
                            self.send_response(200)
                            self.send_header('Content-Length', str(len(data)))
                            self.end_headers()
                            self.wfile.write(data)
                            self.wfile.flush()
                    except OSError:
                        break
                res['closed'] = time.monotonic()
                closed.set()

        up = ThreadingHTTPServer(('127.0.0.1', 0), Up)
        threading.Thread(target=up.serve_forever, daemon=True).start()
        with tempfile.TemporaryDirectory() as d:
            rec = ab.Recorder(up.server_address[1], Path(d) / 'r.jsonl')
            px = ThreadingHTTPServer(('127.0.0.1', 0), ab.make_handler(rec))
            threading.Thread(target=px.serve_forever, daemon=True).start()
            delays = {}
            try:
                for name, port in (('direct', up.server_address[1]), ('proxy', px.server_address[1])):
                    ready.clear()
                    closed.clear()
                    c = socket.create_connection(('127.0.0.1', port), timeout=3)
                    body = json.dumps({'model': 'm', 'stream': True, 'messages': [{'role': 'user', 'content': 'u'}]}).encode()
                    c.sendall(('POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n'
                               'Content-Length: %d\r\n\r\n' % len(body)).encode() + body)
                    self.assertTrue(ready.wait(2))
                    t_close = time.monotonic()
                    c.shutdown(socket.SHUT_RDWR)
                    c.close()
                    self.assertTrue(closed.wait(3))
                    delays[name] = res['closed'] - t_close
                for _ in range(100):
                    if rec.rows:
                        break
                    time.sleep(.02)
            finally:
                for s in (px, up):
                    s.shutdown()
                    s.server_close()
                rec.f.close()
        self.assertLess(delays['direct'], 0.1)
        self.assertLess(delays['proxy'], 0.1, 'proxy waited %.3f s for upstream headers' % delays['proxy'])
        self.assertEqual(rec.rows[0]['client_dropped'], 'client_eof')
        self.assertNotIn('proxy_error', rec.rows[0])
        self.assertNotIn('status', rec.rows[0])

    def test_completed_stream_unchanged(self):
        class Up(BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def log_message(self, *a):
                return

            def do_POST(self):
                self.rfile.read(int(self.headers['Content-Length']))
                data = (b'data: {"choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}],'
                        b'"usage":{"prompt_tokens":3,"completion_tokens":1}}\n\ndata: [DONE]\n\n')
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                time.sleep(.1)
                self.wfile.write(data)
        up = ThreadingHTTPServer(('127.0.0.1', 0), Up)
        threading.Thread(target=up.serve_forever, daemon=True).start()
        with tempfile.TemporaryDirectory() as d:
            rec = ab.Recorder(up.server_address[1], Path(d) / 'r.jsonl')
            px = ThreadingHTTPServer(('127.0.0.1', 0), ab.make_handler(rec))
            threading.Thread(target=px.serve_forever, daemon=True).start()
            try:
                c = http.client.HTTPConnection('127.0.0.1', px.server_address[1], timeout=4)
                c.request('POST', '/v1/chat/completions',
                          json.dumps({'model': 'm', 'stream': True, 'messages': [{'role': 'user', 'content': 'u'}]}),
                          {'Content-Type': 'application/json'})
                body = c.getresponse().read()
                c.close()
                self.assertIn(b'[DONE]', body)
                for _ in range(100):
                    if rec.rows:
                        break
                    time.sleep(.02)
                self.assertEqual(rec.rows[0]['status'], 200)
                self.assertNotIn('client_dropped', rec.rows[0])
            finally:
                for s in (px, up):
                    s.shutdown()
                    s.server_close()


class RpcLogLineTests(unittest.TestCase):
    def test_long_record_stays_valid_complete_json(self):
        big = 'x' * 20000
        r = {'type': 'tool_execution_end', 'toolCallId': 't', 'result': {'content': [{'type': 'text', 'text': big}]},
             'messages': [{'role': 'user', 'content': big}]}
        line = oxl.log_line(r)
        back = json.loads(line)
        self.assertEqual(back['result']['content'][0]['text'], big)
        self.assertNotIn('messages', back)
        self.assertEqual(back['_omitted']['messages']['bytes'], len(json.dumps(r['messages'], sort_keys=True).encode()))
        self.assertEqual(len(back['_omitted']['messages']['sha256']), 64)
        self.assertNotIn('\n', line)

    def test_rpc_next_writes_full_lines(self):
        import io
        import queue
        rpc = oxl.Rpc.__new__(oxl.Rpc)
        rpc.q = queue.Queue()
        rpc.records = []
        rpc.log = io.StringIO()
        rpc.q.put({'type': 'message_end', 'text': 'y' * 9000})
        rpc.next(1)
        self.assertEqual(json.loads(rpc.log.getvalue().splitlines()[0])['text'], 'y' * 9000)


class UnloadResponseTests(unittest.TestCase):
    """Every unload response is logged and checked; since the I1 fix (Codex 03:58) a refused
    unload RAISES (no capture may follow) and a 409 goes back through the status barrier."""
    IDLE = {'active_requests': 0, 'queued_requests': 0, 'image_active': False, 'loaded_model': 'm',
            'unload': {'available': True}}

    def ctl(self, replies, statuses=None):
        sc = ScopedController.__new__(ScopedController)
        sc.log = []
        sc.native_fd = None
        sc.QUIESCE_POLL_S = 0.001
        replies = list(replies)
        statuses = list(statuses or [])
        calls = []

        def call(method, path, body=None, timeout=100, direct=False):
            if method == 'GET' and path == '/_local/status':
                return statuses.pop(0) if statuses else (200, dict(self.IDLE))
            calls.append(path)
            r = replies.pop(0)
            if isinstance(r, Exception):
                raise r
            return r
        sc.call = call
        sc.note = lambda **row: sc.log.append(row)
        sc._wait_native = lambda timeout: (sc.log.append({'event': 'native_waited'}), 'exited')[1]
        return sc, calls

    def test_accepted_first_response_is_logged(self):
        sc, calls = self.ctl([(200, {'state': 'unloaded'})])
        self.assertTrue(sc.unload())
        resp = [r for r in sc.log if r['event'] == 'unload_response']
        self.assertEqual([(r['attempt'], r['status'], r['ok']) for r in resp], [(1, 200, True)])
        self.assertIsInstance(resp[0]['wall_s'], float)
        self.assertEqual(len(calls), 1)
        self.assertIn({'event': 'native_waited'}, sc.log)

    def test_409_goes_back_through_the_barrier(self):
        sc, calls = self.ctl([(409, {'error': 'A request is running.'}), (200, {'state': 'unloaded'})])
        self.assertTrue(sc.unload(quiesce_s=5))
        self.assertEqual([r['ok'] for r in sc.log if r['event'] == 'unload_response'], [False, True])
        self.assertEqual(sum(1 for r in sc.log if r['event'] == 'quiesce'), 2)

    def test_failed_or_silent_or_wrong_state_response_raises(self):
        from scoped_controller import UnloadRefused
        for first in ((500, {'error': 'x'}), TimeoutError('timed out'), (200, {'state': 'loading'})):
            with self.subTest(first=first):
                sc, calls = self.ctl([first, (200, {'state': 'unloaded'})])
                with self.assertRaises(UnloadRefused):
                    sc.unload(quiesce_s=5)
                self.assertEqual(len(calls), 1)                       # no blind retry
                self.assertNotIn({'event': 'native_waited'}, sc.log)  # native left to stop()
                self.assertFalse(sc.last_unload)

    def test_barrier_timeout_raises_without_posting(self):
        from scoped_controller import UnloadRefused
        busy = dict(self.IDLE, active_requests=1, unload={'available': False})
        sc, calls = self.ctl([], statuses=[(200, busy)] * 100000)
        with self.assertRaises(UnloadRefused) as cm:
            sc.unload(quiesce_s=0.2)
        self.assertEqual(str(cm.exception), 'quiesce_timeout')
        self.assertEqual(calls, [])
        q = [r for r in sc.log if r['event'] == 'quiesce']
        self.assertEqual((q[-1]['ok'], q[-1]['status']['active_requests']), (False, 1))

    def test_forced_native_kill_after_accepted_unload_raises(self):
        from scoped_controller import UnloadRefused
        sc, _ = self.ctl([(200, {'state': 'unloaded'})])
        sc._wait_native = lambda timeout: 'forced'
        with self.assertRaises(UnloadRefused):
            sc.unload()


class DoneThenCloseTests(unittest.TestCase):
    """Codex 71abfd4 recorder-proof: oxa closes right after the semantic `data: [DONE]` frame,
    before the HTTP EOF. The row must keep its wire evidence; a partial frame or an early cancel
    stays incomplete. The upstream status is recorded as soon as headers arrive (never assumed)."""
    USAGE = (b'data: {"choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}]}\n\n'
             b'data: {"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":1,'
             b'"prompt_tokens_details":{"cached_tokens":5}}}\n\n')

    def run_case(self, payload, client_reads_until=None, header_delay=0.0, no_watch=False, hold=1.0):
        class Up(BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def log_message(self, *a):
                return

            def do_POST(self):
                self.rfile.read(int(self.headers['Content-Length']))
                time.sleep(header_delay)
                try:
                    self.send_response(200)
                    self.send_header('Content-Type', 'text/event-stream')
                    self.send_header('Connection', 'close')
                    self.end_headers()
                    self.wfile.write(payload)
                    self.wfile.flush()
                    end = time.monotonic() + hold        # no HTTP EOF yet: the stream stays open
                    while time.monotonic() < end:
                        rd, _, _ = select.select([self.connection], [], [], .01)
                        if rd and self.connection.recv(1, socket.MSG_PEEK) == b'':
                            return
                        if no_watch:
                            self.wfile.write(b': keepalive\n\n')
                            self.wfile.flush()
                except OSError:
                    return

        up = ThreadingHTTPServer(('127.0.0.1', 0), Up)
        up.daemon_threads = True
        threading.Thread(target=up.serve_forever, daemon=True).start()
        orig = ab.ClientEofWatch
        if no_watch:
            class Inert:
                def __init__(self, *a):
                    self.eof_t = None

                def stop(self):
                    pass
            ab.ClientEofWatch = Inert
        try:
            with tempfile.TemporaryDirectory() as d:
                rec = ab.Recorder(up.server_address[1], Path(d) / 'r.jsonl')
                px = ThreadingHTTPServer(('127.0.0.1', 0), ab.make_handler(rec))
                px.daemon_threads = True
                threading.Thread(target=px.serve_forever, daemon=True).start()
                try:
                    c = socket.create_connection(('127.0.0.1', px.server_address[1]), timeout=3)
                    body = json.dumps({'model': 'm', 'stream': True, 'messages': [{'role': 'user', 'content': 'u'}]}).encode()
                    c.sendall(('POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n'
                               'Content-Length: %d\r\n\r\n' % len(body)).encode() + body)
                    got = b''
                    deadline = time.monotonic() + 3
                    while client_reads_until is not None and client_reads_until not in got and time.monotonic() < deadline:
                        chunk = c.recv(65536)
                        if not chunk:
                            break
                        got += chunk
                    c.shutdown(socket.SHUT_RDWR)
                    c.close()
                    for _ in range(300):
                        if rec.rows:
                            break
                        time.sleep(.01)
                    rec.f.close()
                    return rec.rows[0] if rec.rows else None
                finally:
                    px.shutdown()
                    px.server_close()
        finally:
            ab.ClientEofWatch = orig
            up.shutdown()
            up.server_close()

    def test_normal_close_after_done_keeps_row(self):
        row = self.run_case(self.USAGE + b'data: [DONE]\n\n', client_reads_until=b'[DONE]')
        self.assertEqual(row['status'], 200)
        self.assertTrue(row['wire_complete'])
        self.assertEqual(row['metrics_source'], 'complete_upstream_sse')
        self.assertEqual((row['finish'], row['prompt'], row['completion'], row['cached']), ('stop', 7, 1, 5))
        self.assertTrue(row.get('usage_seen'))

    def test_partial_done_frame_stays_incomplete(self):
        row = self.run_case(self.USAGE + b'data: [DONE]', client_reads_until=b'[DONE]')
        self.assertEqual(row['status'], 200)
        self.assertFalse(row['wire_complete'])
        self.assertNotIn('prompt', row)
        self.assertNotIn('metrics_source', row)

    def test_early_cancel_stays_incomplete(self):
        row = self.run_case(b'data: {"choices":[{"delta":{"content":"x"}}]}\n\n', client_reads_until=b'"x"')
        self.assertEqual(row['client_dropped'], 'client_eof')
        self.assertFalse(row['wire_complete'])
        self.assertNotIn('prompt', row)

    def test_status_kept_when_only_a_broken_pipe_reveals_the_close(self):
        # the EOF watch is inert: the close is only seen as a failed downstream write (the race)
        row = self.run_case(b'data: {"choices":[{"delta":{"content":"x"}}]}\n\n', client_reads_until=b'"x"',
                            no_watch=True)
        self.assertIn(row.get('client_dropped'), ('BrokenPipeError', 'ConnectionResetError'))
        self.assertEqual(row['status'], 200)
        self.assertNotIn('proxy_error', row)

    def test_status_missing_stays_null(self):
        row = self.run_case(self.USAGE, client_reads_until=None, header_delay=0.5)
        self.assertEqual(row['client_dropped'], 'client_eof')
        self.assertIsNone(row.get('status'))

    def test_sse_done_complete(self):
        self.assertTrue(ab.sse_done_complete(b'data: x\n\ndata: [DONE]\n\n'))
        self.assertTrue(ab.sse_done_complete(b'data: x\r\n\r\ndata:[DONE]\r\n\r\n'))
        self.assertFalse(ab.sse_done_complete(b'data: x\n\ndata: [DONE]'))
        self.assertFalse(ab.sse_done_complete(b'data: x\n\ndata: [DON'))
        self.assertFalse(ab.sse_done_complete(b''))


if __name__ == '__main__':
    unittest.main()
