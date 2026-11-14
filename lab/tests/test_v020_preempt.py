"""v0.2.0 EG1, model-free: `X-Oxe-Priority: background` pre-empts (ctl_queue, ctl_proxy_stream).

A running streaming background request that has sent no data byte is cut at the engine when an
interactive request arrives, re-queued at the front of the background class and re-issued with the
same bytes; its client sees only SSE comments, then the answer. After the first data byte it is
never cut. Driven over loopback through the real controller.Handler with a scripted fake engine
(the v0.1.3 proxy contract harness), plus unit tests of the scheduling and the commit/cancel race.
"""
import os as _os, tempfile as _tempfile
_os.environ.setdefault('LOCAL_AI_LEDGER_DIR', _tempfile.mkdtemp(prefix='local-ai-test-ledger-'))  # never the real request ledger
import json
import os
import re
import socket
import sys
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from snapdragon_ai_lab.runtime import ctl_queue, ctl_replay

sys.path.insert(0, str(__import__('pathlib').Path(__file__).resolve().parent))
import test_v013_proxy_contract as contract  # noqa: E402  (module import: its own class is not re-collected)

TIMEOUT = 10
SSE_HEAD = contract.head('200 OK', ('Content-Type', 'text/event-stream'), ('Connection', 'close'))


def wait_for(predicate, timeout=TIMEOUT):
    deadline = time.monotonic() + timeout
    while not predicate():
        if time.monotonic() > deadline:
            raise AssertionError('timed out')
        time.sleep(0.005)


class ScriptEngine:
    """A loopback engine: records every request (head, body) and answers it with
    `behave(index, body, conn)`; `cut[index]` is set when the controller closed the connection
    while the engine was still holding it (a cancel)."""

    def __init__(self, behave):
        self.behave = behave
        self.requests = []
        self.cut = {}
        self.lock = threading.Lock()
        self.sock = socket.socket()
        self.sock.bind(('127.0.0.1', 0))
        self.sock.listen(16)
        self.sock.settimeout(0.1)
        self.port = self.sock.getsockname()[1]
        self.stopped = threading.Event()
        threading.Thread(target=self._accept, daemon=True).start()

    def close(self):
        self.stopped.set()
        self.sock.close()

    def _accept(self):
        while not self.stopped.is_set():
            try:
                conn, _ = self.sock.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            threading.Thread(target=self._serve, args=(conn,), daemon=True).start()

    def _serve(self, conn):
        conn.settimeout(TIMEOUT)
        try:
            data = b''
            while b'\r\n\r\n' not in data:
                piece = conn.recv(65536)
                if not piece:
                    return
                data += piece
            raw_head, _, body = data.partition(b'\r\n\r\n')
            length = int(re.search(rb'(?im)^content-length:\s*(\d+)\s*$', raw_head).group(1))
            while len(body) < length:
                body += conn.recv(65536)
            with self.lock:
                index = len(self.requests)
                self.requests.append((raw_head.replace(str(self.port).encode(), b'ENGINE'), body))
            self.behave(index, body, conn)
        except OSError:
            pass
        finally:
            try:
                conn.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            conn.close()

    def hold_until_cut(self, index, conn):
        """Hold the request (prefill) until the controller closes the connection."""
        conn.settimeout(TIMEOUT)
        try:
            self.cut[index] = conn.recv(1) == b''
        except OSError:
            self.cut[index] = True


def answer(conn, *events):
    conn.sendall(SSE_HEAD + b''.join(b'data: %s\n\n' % e for e in events) + b'data: [DONE]\n\n')


class Client:
    """One raw streaming request on its own thread; `data` collects every reply byte."""

    def __init__(self, port, body, headers=(), leave_at=None):
        self.data = b''
        self.closed = False
        self.sock = socket.create_connection(('127.0.0.1', port), timeout=TIMEOUT)
        lines = ['POST %s HTTP/1.1' % contract.CHAT, 'Host: 127.0.0.1:%d' % port,
                 *('%s: %s' % pair for pair in headers), 'Content-Length: %d' % len(body)]
        self.sock.sendall(('\r\n'.join(lines) + '\r\n\r\n').encode() + body)
        self.leave_at = leave_at
        self.thread = threading.Thread(target=self._read, daemon=True)
        self.thread.start()

    def _read(self):
        try:
            while not self.data.endswith(b'\r\n0\r\n\r\n'):
                if self.leave_at is not None and self.leave_at in self.data:
                    self.sock.close()
                    self.closed = 'client'
                    return
                piece = self.sock.recv(65536)
                if not piece:
                    self.closed = True
                    return
                self.data += piece
        except OSError:
            self.closed = True

    def join(self):
        self.thread.join(TIMEOUT)
        self.sock.close()
        return self.data.decode('latin-1')


class PreemptOverHttp(unittest.TestCase):
    """The proxy contract harness (real Handler, recording fake Manager) with a real queue slot."""
    setUp_contract = contract.ProxyContract.setUp
    ev = contract.ProxyContract.ev

    def setUp(self):
        self.setUp_contract()
        os.environ[ctl_queue.ENV] = '1'
        os.environ.pop(ctl_replay.ENV, None)
        ctl_replay.STORE.entries.clear()
        self.addCleanup(ctl_replay.STORE.entries.clear)
        m = self.manager
        m.queue_cv, m.queue_waiters, m.queue_reserved, m.inferences = threading.Condition(), [], False, 0
        m.queue_background_run = None
        recorded_begin, recorded_end, case = m.begin_queued, m.end, self
        self.waiters = []

        def begin_queued(**kwargs):
            waiter = kwargs.pop('waiter', None)
            if waiter is not None:
                if waiter not in case.waiters:
                    case.waiters.append(waiter)
                ctl_queue.wait_turn(m, waiter)
            with m.queue_cv:
                m.queue_reserved = False
                m.inferences += 1
            return recorded_begin(**kwargs)

        def end(**kwargs):
            recorded_end(**kwargs)
            if kwargs.get('inference'):
                with m.queue_cv:
                    m.inferences -= 1
                    ctl_queue.released(m)
                    m.queue_cv.notify_all()
        m.begin_queued, m.end = begin_queued, end
        for item in (patch.object(ctl_queue, 'POLL_S', 0.02), patch.object(ctl_queue, 'KEEPALIVE_S', 0.1)):
            item.start()
            self.addCleanup(item.stop)

    def use_engine(self, behave):
        self.engine.close()
        self.engine = ScriptEngine(behave)
        self.addCleanup(self.engine.close)
        patcher = patch.object(contract.controller, 'BACKEND_PORT', self.engine.port)
        patcher.start()
        self.addCleanup(patcher.stop)

    def body(self, who, **extra):
        return json.dumps({'messages': [{'role': 'user', 'content': who}], 'stream': True, **extra}).encode()

    def background(self, leave_at=None, **extra):
        return Client(self.proxy.server_port, self.body('bg', **extra),
                      (('X-Oxe-Priority', 'background'), ('X-Session-Id', 'child.s1'), ('Idempotency-Key', 'k-1')),
                      leave_at=leave_at)

    def interactive(self):
        return Client(self.proxy.server_port, self.body('fg'), (('X-Session-Id', 'parent'),))

    def events_named(self, name):
        return [e for e in self.events if e[0] == name]

    def assert_one_answer(self, text, data):
        head, rest = text.split('\r\n\r\n', 1)
        self.assertEqual(text.count('HTTP/1.1 '), 1, text)
        self.assertTrue(head.startswith('HTTP/1.1 200'), text)
        self.assertIn('data: %s\n\n' % data, rest)
        self.assertEqual(rest.count('data: [DONE]'), 1, rest)
        self.assertTrue(rest.endswith('data: [DONE]\n\n\r\n0\r\n\r\n'), rest)
        return rest

    def requeued_waiting(self):
        with self.manager.queue_cv:
            return any(w.requeued for w in self.manager.queue_waiters)

    # ------------------------------------------------------------------ the cases
    def test_cut_before_first_data_byte_is_requeued_and_reissued_identically(self):
        engine_holder = {}

        def behave(index, body, conn):
            engine = engine_holder['e']
            if index == 0:  # background, in prefill: an engine keepalive comment, then held
                conn.sendall(SSE_HEAD + b': prefill 10%\n\n')
                engine.hold_until_cut(index, conn)
            elif b'"fg"' in body:
                wait_for(self.requeued_waiting)  # the background request waits again meanwhile
                answer(conn, b'"fg"')
            else:
                answer(conn, b'"bg-1"', b'"bg-2"')
        self.use_engine(behave)
        engine_holder['e'] = self.engine
        bg = self.background()
        wait_for(lambda: len(self.engine.requests) == 1)
        wait_for(lambda: b': prefill 10%' in bg.data)
        fg = self.interactive()
        fg_text, bg_text = fg.join(), bg.join()
        self.assert_one_answer(fg_text, '"fg"')
        rest = self.assert_one_answer(bg_text, '"bg-1"')
        self.assertIn('data: "bg-2"', rest)
        self.assertTrue(self.engine.cut.get(0), 'the background request was cancelled at the engine')
        self.assertEqual(len(self.engine.requests), 3)
        bodies = [b for _, b in self.engine.requests]
        self.assertIn(b'"fg"', bodies[1])
        self.assertEqual(self.engine.requests[0], self.engine.requests[2], 're-issued with the same bytes')
        self.assertIn(b'Idempotency-Key: k-1', self.engine.requests[2][0])
        # the client saw comments only before the answer: the engine's, then the queue's
        before = rest[:rest.index('data: ')]
        self.assertIn(': queued pos=2 wait_s=', before)  # behind the interactive request
        self.assertNotIn('data:', before)
        self.assertNotIn('error', bg_text)
        self.assertIn(['led.row', 'preempted', 1], self.events)
        # The controller sends the last byte first and then writes the ledger row and releases the slot (the
        # client never waits for bookkeeping), so a client can finish reading before both are recorded:
        # wait for them (bounded), then assert the exact counts. Under CPU load 12 of 30 runs read too early.
        wait_for(lambda: len(self.events_named('end')) >= 3 and self.manager.inferences == 0
                 and [e[1] for e in self.events_named('led.finish')].count(200) >= 2)
        self.assertEqual([e[1] for e in self.events_named('led.finish')].count(200), 2)
        self.assertEqual(len(self.events_named('end')), 3, 'one slot release per admission')
        with self.manager.queue_cv:
            self.assertEqual((self.manager.inferences, self.manager.queue_waiters), (0, []))
            self.assertIsNone(self.manager.queue_background_run)

    def test_cut_before_the_engine_head_gets_the_early_head(self):
        engine_holder = {}

        def behave(index, body, conn):
            if index == 0:
                engine_holder['e'].hold_until_cut(index, conn)  # not even a head yet
            elif b'"fg"' in body:
                wait_for(self.requeued_waiting)
                answer(conn, b'"fg"')
            else:
                answer(conn, b'"bg"')
        self.use_engine(behave)
        engine_holder['e'] = self.engine
        bg = self.background()
        wait_for(lambda: len(self.engine.requests) == 1)
        fg = self.interactive()
        fg.join()
        bg_text = bg.join()
        rest = self.assert_one_answer(bg_text, '"bg"')
        self.assertTrue(rest.startswith('19\r\n: queued pos=2 wait_s=0\n\n\r\n'), rest)
        self.assertNotIn('Connection: close', bg_text.split('\r\n\r\n', 1)[0])  # the early head, not the engine's
        self.assertEqual(self.engine.requests[0], self.engine.requests[2])

    def test_never_cut_after_the_first_data_byte(self):
        def behave(index, body, conn):
            if index == 0:
                conn.sendall(SSE_HEAD + b'data: "bg-1"\n\n')
                with self.manager.queue_cv:  # the interactive request is now waiting
                    pass
                wait_for(lambda: len(self.manager.queue_waiters) == 1)
                time.sleep(0.2)
                conn.sendall(b'data: "bg-2"\n\ndata: [DONE]\n\n')
            else:
                answer(conn, b'"fg"')
        self.use_engine(behave)
        bg = self.background()
        wait_for(lambda: b'bg-1' in bg.data)
        fg = self.interactive()
        bg_text, fg_text = bg.join(), fg.join()
        rest = self.assert_one_answer(bg_text, '"bg-2"')
        self.assertNotIn(': queued', rest)
        self.assertIn(': queued pos=1 wait_s=', self.assert_one_answer(fg_text, '"fg"'))
        self.assertEqual(len(self.engine.requests), 2)
        self.assertFalse(self.engine.cut)
        self.assertNotIn('preempted', [e[1] for e in self.events_named('led.row')])

    def test_interactive_never_preempts_interactive_and_non_stream_background_runs_on(self):
        def behave(index, body, conn):
            if index == 0:
                wait_for(lambda: len(self.manager.queue_waiters) == 1)
                time.sleep(0.1)
                conn.sendall(contract.head('200 OK', ('Content-Type', 'application/json'),
                                           ('Content-Length', 11)) + b'{"ok":true}')
            else:
                answer(conn, b'"fg"')
        self.use_engine(behave)
        port = self.proxy.server_port
        body = json.dumps({'messages': [{'role': 'user', 'content': 'bg'}]}).encode()  # not streaming
        bg = Client(port, body, (('X-Oxe-Priority', 'background'),))
        wait_for(lambda: len(self.engine.requests) == 1)
        fg = self.interactive()
        fg.join()
        bg.thread.join(2)
        bg.sock.close()
        self.assertIn('{"ok":true}', bg.data.decode())
        self.assertEqual(len(self.engine.requests), 2)
        self.assertFalse(self.engine.cut)

    def test_replay_on_preempted_request_records_once_and_leaves_no_entry(self):
        os.environ[ctl_replay.ENV] = '1'
        engine_holder = {}

        def behave(index, body, conn):
            if index == 0:
                conn.sendall(SSE_HEAD + b': prefill\n\n')
                engine_holder['e'].hold_until_cut(index, conn)
            elif b'"fg"' in body:
                wait_for(self.requeued_waiting)
                with ctl_replay.STORE.lock:
                    self.assertEqual(ctl_replay.STORE.entries, {}, 'the cut run left no replay entry')
                answer(conn, b'"fg"')
            else:
                answer(conn, b'"bg"')
        self.use_engine(behave)
        engine_holder['e'] = self.engine
        bg = self.background(temperature=0)
        wait_for(lambda: len(self.engine.requests) == 1)
        wait_for(lambda: b': prefill' in bg.data)
        fg = self.interactive()
        fg.join()
        self.assert_one_answer(bg.join(), '"bg"')
        self.assertEqual(len(self.engine.requests), 3)
        self.assertEqual(self.engine.requests[0], self.engine.requests[2])
        self.assertEqual(ctl_replay.STORE.entries, {}, 'a delivered answer is never kept for replay')

    def test_client_leaving_while_requeued_then_retry_runs_exactly_once(self):
        os.environ[ctl_replay.ENV] = '1'
        engine_holder = {}
        left = threading.Event()

        def behave(index, body, conn):
            if index == 0:
                conn.sendall(SSE_HEAD + b': prefill\n\n')
                engine_holder['e'].hold_until_cut(index, conn)
            elif b'"fg"' in body:
                self.assertTrue(left.wait(TIMEOUT))
                wait_for(lambda: not self.requeued_waiting())  # the waiter left the queue
                answer(conn, b'"fg"')
            else:
                answer(conn, b'"bg"')
        self.use_engine(behave)
        engine_holder['e'] = self.engine
        bg = self.background(leave_at=b': queued', temperature=0)
        wait_for(lambda: len(self.engine.requests) == 1)
        wait_for(lambda: b': prefill' in bg.data)
        fg = self.interactive()
        bg.thread.join(TIMEOUT)
        self.assertEqual(bg.closed, 'client')
        left.set()
        self.assert_one_answer(fg.join(), '"fg"')
        retry = self.background(temperature=0)
        self.assert_one_answer(retry.join(), '"bg"')
        bg_runs = [b for _, b in self.engine.requests if b'"bg"' in b]
        self.assertEqual(len(bg_runs), 2, 'the cut run and the retry; the cut one never answered')
        self.assertTrue(self.engine.cut.get(0))
        self.assertIn(['led.finish', 499, 'client_gone_queued'], self.events)


class Scheduling(unittest.TestCase):
    def waiter(self, session, background, stream=True):
        return ctl_queue.Waiter(SimpleNamespace(connection=None), stream=stream, session=session, background=background)

    def test_requeued_goes_first_in_the_background_class_only(self):
        a, b, c = self.waiter('A', True), self.waiter('B', True), self.waiter('C', False)
        a.requeued = True
        self.assertIs(ctl_queue._pick([b, a, c], {}), c, 'interactive still goes first')
        self.assertIs(ctl_queue._pick([b, a], {('local', 'B'): -5}), a)
        m = SimpleNamespace(queue_waiters=[b, a, c])
        self.assertEqual(ctl_queue.order(m), [c, a, b])

    def test_unserve_restores_the_lane_mark(self):
        m = SimpleNamespace(queue_cv=threading.Condition(), queue_waiters=[], queue_reserved=False, inferences=0,
                            queue_lane_served={('local', 'A'): 3}, queue_serve_count=7)
        w = self.waiter('A', True)
        ctl_queue.wait_turn(m, w)
        self.assertEqual((w.prev_served, m.queue_lane_served[('local', 'A')]), (3, 8))
        self.assertIsInstance(w.run, ctl_queue.BackgroundRun)
        self.assertIs(m.queue_background_run, w.run)
        ctl_queue.unserve(m, w)
        self.assertEqual(m.queue_lane_served[('local', 'A')], 3)

    def test_requeue_skips_the_cap(self):
        m = SimpleNamespace(queue_cv=threading.Condition(), queue_waiters=[self.waiter(str(i), False) for i in range(16)],
                            queue_reserved=False, inferences=0)
        w = self.waiter('A', True)
        with self.assertRaises(ctl_queue.QueueFull):
            ctl_queue.wait_turn(m, w)
        w.requeued = True
        m.queue_waiters.clear()
        ctl_queue.wait_turn(m, w)
        self.assertFalse(w.requeued)

    def test_only_streaming_background_is_preemptible(self):
        self.assertTrue(self.waiter('A', True).preemptible)
        self.assertFalse(self.waiter('A', True, stream=False).preemptible)
        self.assertFalse(self.waiter('A', False).preemptible)

    def test_interactive_arrival_cancels_background_but_not_background_arrival(self):
        m = SimpleNamespace(queue_cv=threading.Condition(), queue_waiters=[], queue_reserved=False, inferences=1)
        run = m.queue_background_run = ctl_queue.BackgroundRun()
        w = self.waiter('B', True, stream=False)
        w.deadline = None
        with patch.object(ctl_queue, 'POLL_S', 0.01), patch.object(ctl_queue.ctl, 'QUEUE_WAIT_SECONDS', 0.05, create=True):
            with self.assertRaises(ctl_queue.Unavailable):
                ctl_queue.wait_turn(m, w)  # a background arrival waits; nothing is cut
            self.assertFalse(run.cancelled)
            with self.assertRaises(ctl_queue.Unavailable):
                ctl_queue.wait_turn(m, self.waiter('C', False, stream=False))
        self.assertTrue(run.cancelled)


class CommitCancel(unittest.TestCase):
    def test_comments_and_blank_lines_are_not_data(self):
        run = ctl_queue.BackgroundRun()
        self.assertTrue(run.gate(b': prefill 10%\n\n'))
        self.assertTrue(run.gate(b': half a com'))
        self.assertTrue(run.in_comment)
        self.assertTrue(run.gate(b'ment\n\r\n'))
        self.assertFalse(run.committed)
        self.assertTrue(run.gate(b'da'))  # split data line: committed on its first byte
        self.assertTrue(run.committed)
        self.assertFalse(run.cancel(), 'never cut after the first data byte')

    def test_line_endings_lf_crlf_and_lone_cr(self):
        """SSE lines end at LF, CRLF or a lone CR; data after a comment ended by any of them commits,
        in one chunk or split across chunks, and then the request can no longer be cut."""
        for ending in (b'\n', b'\r\n', b'\r'):
            for pieces in ([b': prefill' + ending + b'data: {}' + ending + ending],
                           [b': prefill' + ending, b'data: {}' + ending + ending],
                           [b': pre', b'fill' + ending[:1], ending[1:] + b'd', b'ata: {}' + ending + ending]):
                run = ctl_queue.BackgroundRun()
                allowed_data = False
                for piece in pieces:
                    self.assertTrue(run.gate(piece), (ending, pieces))
                    allowed_data = allowed_data or run.committed
                self.assertTrue(allowed_data and run.committed, (ending, pieces))
                self.assertFalse(run.cancel(), 'no cut once a data byte went out: %r' % (pieces,))

    def test_comment_only_streams_stay_cuttable_with_every_ending(self):
        for ending in (b'\n', b'\r\n', b'\r'):
            run = ctl_queue.BackgroundRun()
            for piece in (b': prefill 10%' + ending, ending, b':' + ending, b': x' + ending[:1], ending[1:]):
                self.assertTrue(run.gate(piece))
            self.assertFalse(run.committed, ending)
            self.assertTrue(run.cancel(), ending)

    def test_crlf_split_does_not_open_a_data_line(self):
        run = ctl_queue.BackgroundRun()
        self.assertTrue(run.gate(b': a\r'))
        self.assertTrue(run.gate(b'\n: b\r'))  # the LF completes the CRLF; the next line is a comment
        self.assertFalse(run.committed)
        self.assertTrue(run.gate(b'\ndata: 1\r\n'))
        self.assertTrue(run.committed)

    def test_cut_wins_before_the_first_data_byte(self):
        run = ctl_queue.BackgroundRun()
        a, b = socket.socketpair()
        self.addCleanup(a.close)
        self.addCleanup(b.close)
        run.attach(a)
        self.assertTrue(run.gate(b': x\n\n'))
        self.assertTrue(run.cancel())
        self.assertEqual(b.recv(1), b'', 'the engine connection was shut down')
        self.assertFalse(run.gate(b': y\n\n'))
        self.assertFalse(run.gate(b'data: 1\n\n'))
        self.assertFalse(run.commit())

    def test_cancel_before_attach_applies_on_attach(self):
        run = ctl_queue.BackgroundRun()
        self.assertTrue(run.cancel())
        a, b = socket.socketpair()
        self.addCleanup(a.close)
        self.addCleanup(b.close)
        run.attach(a)
        self.assertEqual(b.recv(1), b'')

    def test_race_has_exactly_one_winner(self):
        for _ in range(300):
            run = ctl_queue.BackgroundRun()
            results = {}
            start = threading.Barrier(2)

            def commit():
                start.wait()
                results['commit'] = run.commit()

            def cancel():
                start.wait()
                results['cancel'] = run.cancel()
            threads = [threading.Thread(target=commit), threading.Thread(target=cancel)]
            for t in threads:
                t.start()
            for t in threads:
                t.join()
            self.assertNotEqual(results['commit'], results['cancel'], results)


if __name__ == '__main__':
    unittest.main()
