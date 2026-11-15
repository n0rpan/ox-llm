"""v0.1.4 CORE-ROBUST, model-free: the queue (ctl_queue), replay (ctl_replay) and turn diagnostics
(ctl_diag). V014-REPLAN sections 2-3; each feature behind its own flag, default off.

- Queue: round-robin across session keys and FIFO inside one, background yields, disconnect
  dequeues, the cap (503 + Retry-After: 30, never 429), no stream timeout, early SSE headers and
  `: queued pos=<n> wait_s=<s>` comments, an error after early headers is one SSE error event without [DONE].
- Free slot byte-identical: every v0.1.3 proxy contract golden is replayed with the flags on.
- Replay: hit, miss, expiry, cross-session refusal, a delivered answer is never replayed.
- Diagnostics: rewrite_class, history_rewrite, fold_suspect (labels only).
"""
import os as _os, tempfile as _tempfile
_os.environ.setdefault('OXC_LEDGER_DIR', _tempfile.mkdtemp(prefix='oxc-test-ledger-'))  # never the real request ledger
import hashlib
import io
import json
import os
import socket
import sys
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from oxc.runtime import controller, ctl_diag, ctl_proxy_stream, ctl_queue, ctl_replay, request_ledger
from oxc.runtime.ctl_proxy_prep import ANSWERED

sys.path.insert(0, str(__import__('pathlib').Path(__file__).resolve().parent))
import test_v013_proxy_contract as contract  # noqa: E402  (module import: its own class is not re-collected)

FLAGS = (ctl_queue.ENV, ctl_replay.ENV, ctl_diag.ENV)


class FakeHandler:
    """The Handler surface the robustness modules use; the wire bytes land in wfile."""

    def __init__(self, connection=None, headers=None, command='POST'):
        self.connection = connection
        self.headers = headers if headers is not None else {}
        self.command = command
        self.wfile = io.BytesIO()
        self.close_connection = False
        self.manager = SimpleNamespace(launches=3)

    def send_response(self, code):
        self.wfile.write(b'HTTP/1.1 %d\r\n' % code)

    def send_header(self, name, value):
        self.wfile.write(('%s: %s\r\n' % (name, value)).encode())

    def end_headers(self):
        self.wfile.write(b'\r\n')

    def cors_reply_headers(self):
        return []

    def out(self):
        return self.wfile.getvalue()


def fake_manager():
    """A Manager whose begin() only does the bookkeeping the real one does on success."""
    m = controller.Manager()
    m.stop_event.set()

    def begin(*, inference=False, allow_start=False, model_alias=None, origin=None, explicit_load=False):
        with m.mu:
            if inference and m.inferences:
                raise controller.Unavailable('A language-model request is already running.')
            m.active += 1
            if inference:
                m.inferences += 1
    m.begin = begin
    return m


def wait_for(predicate, timeout=5.0):
    deadline = time.monotonic() + timeout
    while not predicate():
        if time.monotonic() > deadline:
            raise AssertionError('timed out')
        time.sleep(0.005)


class DefaultOnTests(unittest.TestCase):
    """v0.2.0 A5: queue and replay are on by default; the documented off-switch is =0 (off/false/no)."""

    def test_default_on_and_off_switch(self):
        for module in (ctl_queue, ctl_replay):
            with patch.dict(os.environ):
                os.environ.pop(module.ENV, None)
                self.assertTrue(module.enabled(), module.ENV)
                for on in ('1', 'on', 'yes', 'true', ''):
                    os.environ[module.ENV] = on
                    self.assertTrue(module.enabled(), (module.ENV, on))
                for off in ('0', 'off', 'OFF', 'false', 'no', ' 0 '):
                    os.environ[module.ENV] = off
                    self.assertFalse(module.enabled(), (module.ENV, off))


class QueueTests(unittest.TestCase):
    def setUp(self):
        for item in (patch.object(ctl_queue, 'POLL_S', 0.02), patch.dict(os.environ, {ctl_queue.ENV: '1'})):
            item.start()
            self.addCleanup(item.stop)

    def run_waiters(self, m, specs):
        """Hold the slot, queue `specs` [(name, session, background)] strictly in order, release
        it, and return the admission order."""
        m.begin_queued(inference=True)  # the running request
        order, threads, waiters = [], [], {}
        for name, session, background in specs:
            waiter = waiters[name] = ctl_queue.Waiter(FakeHandler(), stream=False, session=session,
                                                     background=background)

            def run(name=name, waiter=waiter):
                m.begin_queued(inference=True, waiter=waiter)
                order.append(name)
                time.sleep(0.01)
                m.end(inference=True)
            thread = threading.Thread(target=run, daemon=True)
            thread.start()
            threads.append(thread)
            wait_for(lambda: m.queue_length() == len(threads))
        m.end(inference=True)
        for thread in threads:
            thread.join(10)
        self.assertEqual((m.inferences, m.queue_length()), (0, 0))
        return order, waiters

    def test_round_robin_across_sessions_fifo_inside_one(self):
        order, waiters = self.run_waiters(fake_manager(), [('A1', 'A', False), ('A2', 'A', False), ('A3', 'A', False),
                                                           ('B1', 'B', False), ('B2', 'B', False)])
        self.assertEqual(order, ['A1', 'B1', 'A2', 'B2', 'A3'])
        self.assertTrue(waiters['B1'].session_switch)
        self.assertEqual(waiters['A1'].position, 1)
        self.assertEqual(waiters['B1'].position, 2)  # the round-robin position, not the arrival index

    def test_one_session_is_plain_fifo(self):
        order, _ = self.run_waiters(fake_manager(), [(str(i), 'S', False) for i in range(4)])
        self.assertEqual(order, ['0', '1', '2', '3'])

    def test_background_yields_to_interactive(self):
        order, _ = self.run_waiters(fake_manager(), [('bg', 'A', True), ('i1', 'B', False), ('i2', 'C', False)])
        self.assertEqual(order, ['i1', 'i2', 'bg'])

    def test_free_slot_sends_nothing_and_does_not_wait(self):
        m = fake_manager()
        handler = FakeHandler()
        waiter = ctl_queue.Waiter(handler, stream=True, session='A')
        m.begin_queued(inference=True, waiter=waiter)
        self.assertEqual(handler.out(), b'')
        self.assertFalse(waiter.waited or waiter.early)
        m.end(inference=True)

    def test_busy_stream_gets_early_headers_and_queued_comments(self):
        m = fake_manager()
        m.begin_queued(inference=True)
        a, b = socket.socketpair()
        self.addCleanup(a.close)
        self.addCleanup(b.close)
        handler = FakeHandler(connection=a)
        waiter = ctl_queue.Waiter(handler, stream=True, session='A')
        with patch.object(ctl_queue, 'KEEPALIVE_S', 0.1), patch.object(controller, 'QUEUE_WAIT_SECONDS', 0.05):
            thread = threading.Thread(target=m.begin_queued, kwargs={'inference': True, 'waiter': waiter}, daemon=True)
            thread.start()
            time.sleep(0.45)  # far beyond the non-stream bound: a connected stream waiter never times out
            self.assertTrue(thread.is_alive())
            m.end(inference=True)
            thread.join(5)
        out = handler.out()
        self.assertTrue(out.startswith(b'HTTP/1.1 200\r\nContent-Type: text/event-stream\r\n'
                                       b'Cache-Control: no-store\r\nTransfer-Encoding: chunked\r\n\r\n'), out)
        comment = b': queued pos=1 wait_s=0\n\n'
        self.assertGreaterEqual(out.count(b'%x\r\n' % len(comment) + comment + b'\r\n'), 3)
        self.assertNotIn(b'data:', out)
        self.assertTrue(waiter.early and waiter.active())
        m.end(inference=True)

    def test_queued_comment_has_the_shared_format_and_counts_seconds_waited(self):
        """F10: `: queued pos=<n> wait_s=<s>`, the engine's queued line without pos; wait_s counts
        whole seconds (rounded down) from the first wait, pos is the current place (1 = next)."""
        self.assertEqual(ctl_queue.queued_comment(3, 61.9), b': queued pos=3 wait_s=61\n\n')
        self.assertEqual(ctl_queue.queued_comment(1, -0.2), b': queued pos=1 wait_s=0\n\n')
        a, b = socket.socketpair()
        self.addCleanup(a.close)
        self.addCleanup(b.close)
        handler = FakeHandler(connection=a)
        waiter = ctl_queue.Waiter(handler, stream=True, session='A')
        clock = [100.0]
        with patch.object(ctl_queue.time, 'monotonic', lambda: clock[0]), patch.object(ctl_queue, 'KEEPALIVE_S', 15.0):
            self.assertTrue(waiter.waiting(2))
            clock[0] = 116.5
            self.assertTrue(waiter.waiting(1))
        out = handler.out()
        self.assertIn(b': queued pos=2 wait_s=0\n\n', out)
        self.assertIn(b': queued pos=1 wait_s=16\n\n', out)
        self.assertEqual(waiter.position, 2, 'the ledger keeps the first position')
        for line in out.split(b'\n'):
            if line.startswith(b': queued'):
                fields = dict(f.split(b'=', 1) for f in line.split()[2:])
                self.assertEqual(sorted(fields), [b'pos', b'wait_s'], line)

    def test_non_stream_wait_is_bounded_as_before(self):
        m = fake_manager()
        m.begin_queued(inference=True)
        waiter = ctl_queue.Waiter(FakeHandler(), stream=False, session='A')
        with patch.object(controller, 'QUEUE_WAIT_SECONDS', 0.1):
            with self.assertRaisesRegex(controller.Unavailable, 'exceeded'):
                m.begin_queued(inference=True, waiter=waiter)
        self.assertEqual(m.queue_length(), 0)

    def test_disconnect_dequeues(self):
        m = fake_manager()
        m.begin_queued(inference=True)
        a, b = socket.socketpair()
        self.addCleanup(a.close)
        waiter = ctl_queue.Waiter(FakeHandler(connection=a), stream=True, session='A')
        raised = []

        def run():
            try:
                m.begin_queued(inference=True, waiter=waiter)
            except ctl_queue.ClientGone as error:
                raised.append(error)
        thread = threading.Thread(target=run, daemon=True)
        thread.start()
        wait_for(lambda: m.queue_length() == 1)
        b.close()
        thread.join(5)
        self.assertEqual(len(raised), 1)
        self.assertEqual(m.queue_length(), 0)
        self.assertEqual(m.inferences, 1)  # the running request is untouched

    def test_cap_per_principal_and_total(self):
        m = fake_manager()
        m.begin_queued(inference=True)
        m.queue_waiters.extend(ctl_queue.Waiter(FakeHandler(), stream=True, session='x%d' % i)
                               for i in range(ctl_queue.PER_PRINCIPAL))
        with self.assertRaises(ctl_queue.QueueFull):
            m.begin_queued(inference=True, waiter=ctl_queue.Waiter(FakeHandler(), stream=True, session='new'))
        m.queue_waiters.clear()
        m.queue_waiters.extend(ctl_queue.Waiter(FakeHandler(), stream=True, session='y%d' % i, principal='other')
                               for i in range(ctl_queue.TOTAL))
        with self.assertRaises(ctl_queue.QueueFull):
            m.begin_queued(inference=True, waiter=ctl_queue.Waiter(FakeHandler(), stream=True, session='new'))
        self.assertEqual(len(m.queue_waiters), ctl_queue.TOTAL)  # nothing was added
        handler = FakeHandler()
        ctl_queue.reply_full(handler)
        out = handler.out()
        self.assertTrue(out.startswith(b'HTTP/1.1 503\r\n'), out)
        self.assertIn(b'Retry-After: 30\r\n', out)
        self.assertNotIn(b'429', out)
        body = json.loads(out.split(b'\r\n\r\n', 1)[1])
        self.assertEqual(body['error']['code'], 'queue_full')

    def test_error_after_early_headers_is_one_sse_error_event_without_done(self):
        handler = FakeHandler()
        waiter = ctl_queue.Waiter(handler, stream=True, session='A')
        waiter.early = True
        ctl_queue.fail(handler, waiter, 503, json.dumps({'error': {'message': 'gone', 'type': 'local_backend_error'}}).encode())
        ctl_queue.fail(handler, waiter, 502, b'{}')  # a second failure sends nothing
        out = handler.out()
        size, rest = out.split(b'\r\n', 1)
        event = rest[:int(size, 16)]
        self.assertTrue(event.startswith(b'event: error\ndata: '), event)
        self.assertEqual(json.loads(event.split(b'data: ', 1)[1])['error']['code'], 'service_unavailable')  # v0.2.0: a string code
        self.assertTrue(out.endswith(b'\r\n0\r\n\r\n'))
        self.assertEqual(out.count(b'event: error'), 1)
        self.assertNotIn(b'[DONE]', out)
        self.assertTrue(handler.close_connection)

    def test_off_means_no_waiter(self):
        st = SimpleNamespace(remote=False, inference=True, payload={'stream': True}, led=None)
        handler = FakeHandler()
        with patch.dict(os.environ, {ctl_queue.ENV: '0'}):
            self.assertIsNone(ctl_queue.waiter_for(handler, st))
        self.assertIsNotNone(ctl_queue.waiter_for(handler, st))
        st.remote = True  # LAN share keeps v0.1.3's rule
        self.assertIsNone(ctl_queue.waiter_for(handler, st))

    def test_priority_header_marks_background(self):
        st = SimpleNamespace(remote=False, inference=True, payload={'stream': True}, led=SimpleNamespace(session='s'))
        waiter = ctl_queue.waiter_for(FakeHandler(headers={'X-Oxe-Priority': 'Background'}), st)
        self.assertTrue(waiter.background)
        self.assertEqual(waiter.lane, ('local', 's'))


# ---------------------------------------------------------------------- replay
def replay_request(body=None, session='ses-1', handler_headers=None, **payload_extra):
    payload = {'stream': True, 'temperature': 0, 'messages': [{'role': 'user', 'content': 'hi'}], **payload_extra}
    raw = body if body is not None else json.dumps(payload).encode()
    headers = {'X-Session-Id': session} if session else {}  # replay needs an explicit session
    headers.update(handler_headers or {})
    st = SimpleNamespace(inference=True, remote=False, path='/v1/chat/completions', raw_body=raw,
                         payload=json.loads(raw), target={'model_alias': 'm'}, robust=ctl_queue.Extras(),
                         led=SimpleNamespace(session=session, row={'mode': 'fast'}, observe=lambda chunk: None,
                                             finish=lambda status, error=None: None),
                         client_write_failed=False, gone=False)
    st.client_gone = lambda: st.gone or st.client_write_failed
    return FakeHandler(headers=headers), st


class FakeUpstream:
    def __init__(self):
        self.shut = threading.Event()

    def shutdown(self, how):
        self.shut.set()


def write_client(handler, st, data):
    if st.gone:
        st.client_write_failed = True
        raise BrokenPipeError()
    handler.wfile.write(data)


class ReplayTests(unittest.TestCase):
    def setUp(self):
        env = patch.dict(os.environ, {ctl_replay.ENV: '1'})
        env.start()
        self.addCleanup(env.stop)
        ctl_replay.STORE.entries.clear()
        self.addCleanup(ctl_replay.STORE.entries.clear)

    def producer(self, **kwargs):
        """An eligible request that reached the engine; returns (handler, st, recorder, upstream)."""
        handler, st = replay_request(**kwargs)
        self.assertIsNone(ctl_replay.try_attach(handler, st))  # nothing to attach: it runs
        upstream = FakeUpstream()
        guard = ctl_replay.start(st, upstream)
        self.assertIsInstance(guard, ctl_replay.Guard)
        recorder = ctl_replay.recorder(st)
        recorder.head(200, [('Content-Type', 'text/event-stream')])
        return handler, st, recorder, upstream, guard

    def attach(self, **kwargs):
        handler, st = replay_request(**kwargs)
        result = []
        thread = threading.Thread(target=lambda: result.append(ctl_replay.try_attach(handler, st)), daemon=True)
        thread.start()
        return handler, st, thread, result

    def test_cut_then_identical_retry_attaches_from_byte_zero_with_live_tail(self):
        handler, st, recorder, upstream, guard = self.producer()
        recorder.relay(handler, st, b'data: 1\n\n', write_client)
        st.gone = True
        guard.shutdown(socket.SHUT_RDWR)  # the watcher saw EOF: the replay arms instead of cancelling
        self.assertFalse(upstream.shut.is_set())
        recorder.relay(handler, st, b'data: 2\n\n', write_client)  # the engine keeps running into RAM
        retry, _, thread, result = self.attach()
        time.sleep(0.1)
        recorder.relay(handler, st, b'data: [DONE]\n\n', write_client)
        self.assertTrue(recorder.done(st))  # the gone producer writes no final chunk
        thread.join(5)
        self.assertEqual(result, [ANSWERED])
        out = retry.out()
        self.assertTrue(out.startswith(b'HTTP/1.1 200\r\nContent-Type: text/event-stream\r\n'), out)
        body = out.split(b'\r\n\r\n', 1)[1]
        self.assertEqual(body, b''.join(b'%x\r\n%s\r\n' % (len(c), c) for c in
                                        (b'data: 1\n\n', b'data: 2\n\n', b'data: [DONE]\n\n')) + b'0\r\n\r\n')
        self.assertEqual(handler.out(), b'%x\r\ndata: 1\n\n\r\n' % len(b'data: 1\n\n'))
        recorder.release(st)
        self.assertEqual(st.led.row.get('replay'), 'cut-served')

    def test_different_body_misses(self):
        handler, st, recorder, upstream, guard = self.producer()
        st.gone = True
        guard.shutdown(socket.SHUT_RDWR)
        other, other_st = replay_request(messages=[{'role': 'user', 'content': 'other'}])
        self.assertIsNone(ctl_replay.try_attach(other, other_st))
        self.assertEqual(other.out(), b'')
        self.assertTrue(upstream.shut.is_set())  # the same session moved on: the cut entry is cancelled

    def test_other_session_is_refused_and_does_not_cancel(self):
        handler, st, recorder, upstream, guard = self.producer()
        st.gone = True
        guard.shutdown(socket.SHUT_RDWR)
        other, other_st = replay_request(session='ses-2')
        self.assertIsNone(ctl_replay.try_attach(other, other_st))
        self.assertEqual(other.out(), b'')
        self.assertFalse(upstream.shut.is_set())
        self.assertNotEqual(ctl_replay.identity(*replay_request(session='ses-2')).key,
                            ctl_replay.identity(*replay_request()).key)

    def test_ineligible_request_of_the_same_session_cancels_the_cut_entry(self):
        handler, st, recorder, upstream, guard = self.producer()
        st.gone = True
        guard.shutdown(socket.SHUT_RDWR)
        other, other_st = replay_request(temperature=0.7)
        self.assertIsNone(ctl_replay.try_attach(other, other_st))
        self.assertTrue(upstream.shut.is_set())
        self.assertEqual(ctl_replay.STORE.entries, {})

    def cut_producer(self):
        handler, st, recorder, upstream, guard = self.producer()
        recorder.relay(handler, st, b'data: 1\n\n', write_client)
        st.gone = True
        guard.shutdown(socket.SHUT_RDWR)
        return recorder, upstream

    def assert_regraced(self, recorder, upstream):
        entry = recorder.entry
        self.assertEqual((entry.state, entry.attached), ('cut', 0))
        self.assertIsNotNone(entry.expires)  # bounded again: a new grace after the last reader left
        self.assertTrue(entry.timer.is_alive())
        self.assertTrue(upstream.shut.wait(2))  # GRACE_S (patched) later the engine is cancelled
        self.assertEqual(ctl_replay.STORE.entries, {})

    def test_retry_that_leaves_before_its_head_rearms_the_grace(self):
        # Codex review R1: the retry cancelled the cut timer, then lost its own client
        with patch.object(ctl_replay, 'GRACE_S', 0.2):
            recorder, upstream = self.cut_producer()
            retry, retry_st = replay_request()

            def broken(_status):
                raise BrokenPipeError('retry gone')
            retry.send_response = broken
            self.assertIs(ctl_replay.try_attach(retry, retry_st), ANSWERED)
            self.assertFalse(upstream.shut.is_set())
            self.assert_regraced(recorder, upstream)

    def test_retry_that_leaves_mid_body_rearms_the_grace(self):
        class Broken(io.BytesIO):
            def write(self, data):
                raise BrokenPipeError('retry gone')

        with patch.object(ctl_replay, 'GRACE_S', 0.2):
            recorder, upstream = self.cut_producer()
            retry, retry_st = replay_request()
            end = retry.end_headers

            def end_then_break():
                end()
                retry.wfile = Broken()
            retry.end_headers = end_then_break
            self.assertIs(ctl_replay.try_attach(retry, retry_st), ANSWERED)
            self.assert_regraced(recorder, upstream)

    def test_first_user_fallback_is_never_a_replay_session(self):
        # Codex review R3: the REAL ledger fallback gives two new sessions the same key
        a, sa = replay_request(session=None)
        b, sb = replay_request(session=None)
        sa.led.session = request_ledger.session_key(a.headers, sa.payload)
        sb.led.session = request_ledger.session_key(b.headers, sb.payload)
        self.assertEqual(sa.led.session, sb.led.session)
        self.assertIsNone(ctl_replay.identity(a, sa))
        self.assertIsNone(ctl_replay.identity(b, sb))
        # an explicit session header or an Idempotency-Key is the evidence
        self.assertIsNotNone(ctl_replay.identity(*replay_request(session='s-explicit')))
        self.assertIsNotNone(ctl_replay.identity(*replay_request(session=None, handler_headers={'Idempotency-Key': 'k'})))
        self.assertNotEqual(ctl_replay.identity(*replay_request(session=None, handler_headers={'Idempotency-Key': 'k1'})).lane,
                            ctl_replay.identity(*replay_request(session=None, handler_headers={'Idempotency-Key': 'k2'})).lane)

    def run_forward_with_prefill_cut(self, chunks):
        """Codex review R2, through the real forward(): the watcher sees the client leave before
        the engine answers, and the original client's head cannot be sent."""
        handler, st = replay_request()
        self.assertIsNone(ctl_replay.try_attach(handler, st))
        upstream = FakeUpstream()
        pending = list(chunks)

        class Response:
            status = 200

            def getheader(self, name, default=''):
                return 'text/event-stream' if name.lower() == 'content-type' else default

            def getheaders(self):
                return [('Content-Type', 'text/event-stream')]

            def read1(self, _size):
                return pending.pop(0) if pending else b''

        class Backend:
            sock = upstream

            def request(self, *_args, **_kwargs):
                pass

            def getresponse(self):
                return Response()

        class Watcher:
            def __init__(self, _client, watched):
                self.watched = watched

            def start(self):
                st.gone = True
                self.watched.shutdown(socket.SHUT_RDWR)
                return self

            def stop(self):
                pass

        def broken(_status):
            raise BrokenPipeError('client gone')
        handler.path = st.path
        handler.send_response = broken
        st.preempt = None
        st.preempted = lambda: False
        st.connection = Backend()
        st.backend_path = st.path
        st.backend_headers = {}
        st.body = st.raw_body
        st.harness_key = st.runtime = st.completion_meta = st.request_process = st.request_error = None
        st.started_response = False
        st.watcher = None
        st.backend_status = st.content_type = None
        with patch.object(controller, 'ClientDisconnect', Watcher):
            ctl_proxy_stream.forward(handler, st)
        return handler, st, upstream

    def test_prefill_cut_through_forward_keeps_the_generation(self):
        chunks = [b'data: 1\n\n', b'data: [DONE]\n\n']
        handler, st, upstream = self.run_forward_with_prefill_cut(chunks)
        recorder = ctl_replay.recorder(st)
        self.assertEqual(recorder.entry.status, 200)
        self.assertEqual(recorder.entry.chunks, chunks)
        recorder.release(st)  # proxy_request's finally
        self.assertEqual(recorder.entry.state, 'done')
        self.assertFalse(upstream.shut.is_set())
        retry, retry_st = replay_request()
        self.assertIs(ctl_replay.try_attach(retry, retry_st), ANSWERED)
        body = retry.out().split(b'\r\n\r\n', 1)[1]
        self.assertEqual(body, b''.join(b'%x\r\n%s\r\n' % (len(c), c) for c in chunks) + b'0\r\n\r\n')

    def test_prefill_cut_with_replay_off_cancels_as_before(self):
        with patch.dict(os.environ, {ctl_replay.ENV: '0'}):
            handler, st, upstream = self.run_forward_with_prefill_cut([b'data: 1\n\n'])
        self.assertIsNone(ctl_replay.recorder(st))
        self.assertTrue(upstream.shut.is_set())  # the watcher cancelled the engine directly
        self.assertTrue(handler.close_connection)

    def test_grace_expiry_cancels_and_misses(self):
        with patch.object(ctl_replay, 'GRACE_S', 0.1):
            handler, st, recorder, upstream, guard = self.producer()
            st.gone = True
            guard.shutdown(socket.SHUT_RDWR)
            self.assertTrue(upstream.shut.wait(2))  # no attach within the grace: cancelled as v0.1.3
        retry, retry_st = replay_request()
        self.assertIsNone(ctl_replay.try_attach(retry, retry_st))
        self.assertEqual(ctl_replay.STORE.entries, {})

    def test_delivered_answer_is_never_replayed(self):
        handler, st, recorder, upstream, guard = self.producer()
        recorder.relay(handler, st, b'data: 1\n\n', write_client)
        self.assertFalse(recorder.done(st))
        recorder.release(st)
        retry, retry_st = replay_request()
        self.assertIsNone(ctl_replay.try_attach(retry, retry_st))
        self.assertEqual(ctl_replay.STORE.entries, {})

    def test_ineligible_requests_have_no_identity(self):
        self.assertIsNone(ctl_replay.identity(*replay_request(temperature=0.7)))
        self.assertIsNone(ctl_replay.identity(*replay_request(stream=False)))
        self.assertIsNone(ctl_replay.identity(*replay_request(session=None)))
        handler, st = replay_request()
        st.remote = True
        self.assertIsNone(ctl_replay.identity(handler, st))
        with patch.dict(os.environ, {ctl_replay.ENV: '0'}):
            self.assertIsNone(ctl_replay.identity(*replay_request()))

    def test_idempotency_key_still_needs_the_same_body(self):
        a = ctl_replay.identity(*replay_request(handler_headers={'Idempotency-Key': 'k1'}))
        b = ctl_replay.identity(*replay_request(handler_headers={'Idempotency-Key': 'k1'},
                                                messages=[{'role': 'user', 'content': 'other'}]))
        self.assertEqual(a.key, b.key)
        self.assertNotEqual(a.body_hash, b.body_hash)  # compared on attach: never replayed
        handler, st, recorder, upstream, guard = self.producer(handler_headers={'Idempotency-Key': 'k1'})
        st.gone = True
        guard.shutdown(socket.SHUT_RDWR)
        other, other_st = replay_request(handler_headers={'Idempotency-Key': 'k1'},
                                         messages=[{'role': 'user', 'content': 'other'}])
        self.assertIsNone(ctl_replay.try_attach(other, other_st))
        self.assertEqual(other.out(), b'')

    def test_oversize_disarms(self):
        with patch.object(ctl_replay, 'MAX_BYTES', 8):
            handler, st, recorder, upstream, guard = self.producer()
            recorder.relay(handler, st, b'data: 123456789\n\n', write_client)
            self.assertFalse(recorder.armed)
            self.assertEqual(ctl_replay.STORE.entries, {})
            st.gone = True
            with self.assertRaises(ConnectionAbortedError):  # not armed: a vanished client cancels
                recorder.relay(handler, st, b'x', write_client)


# ---------------------------------------------------------------------- diagnostics
def sse(content, finish='stop', calls=None):
    delta = {'content': content}
    if calls:
        delta['tool_calls'] = calls
    return (b'data: ' + json.dumps({'choices': [{'delta': delta}]}).encode() + b'\n\n'
            + b'data: ' + json.dumps({'choices': [{'delta': {}, 'finish_reason': finish}]}).encode() + b'\n\n'
            + b'data: [DONE]\n\n')


def diag_turn(messages, change, reply, session='s1'):
    raw = json.dumps({'messages': messages}).encode()
    row = {'history_change': change}
    st = SimpleNamespace(inference=True, remote=False, raw_body=raw, target={'model_alias': 'm'},
                         led=SimpleNamespace(session=session, row=row), robust=ctl_queue.Extras(),
                         turn={'turn_kind': None, 'tools_hash': None, 'fold_suspect': None}, backend_status=200)
    ctl_diag.start(FakeHandler(), st)
    if reply is not None:
        ctl_diag.feed(st, reply)
    ctl_diag.finish(st)
    return st


class DiagTests(unittest.TestCase):
    T = 'Here is the full answer to your question, with enough text to count.'

    def setUp(self):
        env = patch.dict(os.environ, {ctl_diag.ENV: '1'})
        env.start()
        self.addCleanup(env.stop)
        ctl_diag._sessions.clear()
        ctl_diag._recent.clear()

    def test_fold_suspect_on_a_folded_turn_start(self):
        diag_turn([{'role': 'user', 'content': 'q'}], 'initial', sse(self.T))
        st = diag_turn([{'role': 'user', 'content': 'Summary so far: ' + self.T + ' Continue.'}], 'edited', None)
        self.assertIs(st.turn['fold_suspect'], True)
        self.assertEqual(st.led.row['fold_basis'], 'session')
        self.assertIs(st.led.row['history_rewrite'], True)

    def test_append_is_not_a_fold(self):
        diag_turn([{'role': 'user', 'content': 'q'}], 'initial', sse(self.T))
        st = diag_turn([{'role': 'user', 'content': 'q'}, {'role': 'assistant', 'content': self.T},
                        {'role': 'user', 'content': 'quote: ' + self.T}], 'append', None)
        self.assertIsNone(st.turn['fold_suspect'])
        self.assertIs(st.led.row['history_rewrite'], False)
        self.assertEqual(st.led.row['rewrite_class'], 'identical')

    def test_tool_call_reply_never_arms_fold(self):
        call = [{'index': 0, 'id': 'c1', 'function': {'name': 'f', 'arguments': '{"a":1}'}}]
        diag_turn([{'role': 'user', 'content': 'q'}], 'initial', sse(self.T, 'tool_calls', call))
        st = diag_turn([{'role': 'user', 'content': self.T}], 'edited', None)
        self.assertIsNone(st.turn['fold_suspect'])

    def test_recent_basis_for_a_new_session(self):
        diag_turn([{'role': 'user', 'content': 'q'}], 'initial', sse(self.T), session='s1')
        st = diag_turn([{'role': 'user', 'content': 'ctx ' + self.T}], 'initial', None, session='s2')
        self.assertIs(st.turn['fold_suspect'], True)
        self.assertEqual(st.led.row['fold_basis'], 'recent')

    def test_rewrite_classes(self):
        def cls(sent, calls=None, reasoning=None, reply_calls=None, reply_reasoning=None):
            reply = ctl_diag.forms(self.T, reply_calls or [], reply_reasoning)
            message = {'role': 'assistant', 'content': sent}
            if calls is not None:
                message['tool_calls'] = calls
            if reasoning is not None:
                message['reasoning_content'] = reasoning
            return ctl_diag.classify(reply, message)
        call = lambda cid, args: [{'id': cid, 'function': {'name': 'f', 'arguments': args}}]
        self.assertEqual(cls(self.T), 'identical')
        self.assertEqual(cls(self.T + '  \n'), 'ws-trailing')
        self.assertEqual(cls(self.T.replace(' ', '  ')), 'ws-inner')
        self.assertEqual(cls(self.T, call('c1', '{"b": 2, "a": 1}'), reply_calls=[('c1', 'f', '{"a":1,"b":2}')]),
                         'args-reserialised')
        self.assertEqual(cls(self.T, call('c9', '{"a":1}'), reply_calls=[('c1', 'f', '{"a":1}')]), 'id-changed')
        self.assertEqual(cls(self.T, reply_reasoning='thinking'), 'reasoning-dropped')
        self.assertEqual(cls('something else entirely'), 'other')
        self.assertEqual(set(ctl_diag.REWRITE_CLASSES), {'identical', 'ws-trailing', 'ws-inner', 'args-reserialised',
                                                         'reasoning-dropped', 'id-changed', 'other'})

    def test_contains_rolling_hash(self):
        record = ctl_diag.fold_record(self.T)
        self.assertTrue(ctl_diag.contains(('x' * 1000 + self.T + 'y').encode(), record))
        self.assertFalse(ctl_diag.contains(('x' * 1000 + self.T[:-1]).encode(), record))
        self.assertIsNone(ctl_diag.fold_record('short'))
        self.assertEqual(record[2], hashlib.sha256(self.T.encode()).hexdigest())

    def test_off_writes_nothing(self):
        with patch.dict(os.environ, {ctl_diag.ENV: '0'}):
            diag_turn([{'role': 'user', 'content': 'q'}], 'initial', sse(self.T))
            st = diag_turn([{'role': 'user', 'content': self.T}], 'edited', None)
        self.assertEqual(st.led.row, {'history_change': 'edited'})
        self.assertEqual(ctl_diag._sessions, {})


# ---------------------------------------------------------------------- the proxy, flags on
class FlagsOnProxyContract(contract.ProxyContract):
    """Every v0.1.3 proxy contract golden, replayed with OXC_QUEUE, OXC_REPLAY and
    OXC_TURN_DIAG on: with a free slot the client bytes, the engine bytes and every recorded
    call are unchanged. Plus the busy-slot path over HTTP (early headers, then the engine body;
    an admission error after early headers)."""

    def setUp(self):
        super().setUp()  # its os.environ patch restores these flags afterwards
        for name in FLAGS:
            os.environ[name] = '1'
        ctl_diag._sessions.clear()
        ctl_diag._recent.clear()
        ctl_replay.STORE.entries.clear()
        self.addCleanup(ctl_replay.STORE.entries.clear)
        self.slot = SimpleNamespace(queue_cv=threading.Condition(), queue_waiters=[], queue_reserved=False,
                                    inferences=0)
        self.waiters = []
        recorded = self.manager.begin_queued
        case = self

        def begin_queued(**kwargs):
            waiter = kwargs.pop('waiter', None)  # the golden records v0.1.3's arguments
            if waiter is not None:
                case.waiters.append(waiter)
                ctl_queue.wait_turn(case.slot, waiter)
                with case.slot.queue_cv:
                    case.slot.queue_reserved = False
            return recorded(**kwargs)
        self.manager.begin_queued = begin_queued
        for item in (patch.object(ctl_queue, 'POLL_S', 0.02), patch.object(ctl_queue, 'KEEPALIVE_S', 0.1)):
            item.start()
            self.addCleanup(item.stop)

    def check(self, transcript, name=None):
        super().check(transcript, name)
        self.assertFalse(any(w.waited or w.early for w in self.waiters))

    def test_client_disconnect_mid_stream(self):
        os.environ.pop(ctl_replay.ENV)  # replay changes what a cut does (by design; ReplayTests)
        super().test_client_disconnect_mid_stream()

    def busy_for(self, seconds):
        self.slot.inferences = 1

        def free():
            with self.slot.queue_cv:
                self.slot.inferences = 0
                self.slot.queue_cv.notify_all()
        timer = threading.Timer(seconds, free)
        timer.start()
        self.addCleanup(timer.cancel)

    def test_busy_slot_stream_gets_early_headers_then_the_engine_body(self):
        self.busy_for(0.35)
        self.sse_engine(b'data: {"n":1}\n\n', b'data: [DONE]\n\n')
        client, closed = self.exchange('POST', contract.CHAT, self.chat(stream=True))
        head, body = client.split('\r\n\r\n', 1)
        self.assertTrue(head.startswith('HTTP/1.1 200 *\r\n'), client)
        self.assertIn('Content-Type: text/event-stream', head)
        self.assertNotIn('Connection: close', head)  # the engine's own headers are not sent again
        self.assertTrue(body.startswith('19\r\n: queued pos=1 wait_s=0\n\n\r\n'), body)
        self.assertGreaterEqual(body.count(': queued pos=1 wait_s='), 2)
        self.assertIn('data: {"n":1}\n\n', body)
        self.assertTrue(body.endswith('data: [DONE]\n\n\r\n0\r\n\r\n'), body)
        self.assertTrue(self.waiters and self.waiters[0].early)

    def test_error_after_early_headers_is_an_sse_error_event(self):
        self.busy_for(0.25)
        self.manager.begin_error = controller.Unavailable('synthetic load failure')
        client, closed = self.exchange('POST', contract.CHAT, self.chat(stream=True))
        head, body = client.split('\r\n\r\n', 1)
        self.assertTrue(head.startswith('HTTP/1.1 200 *\r\n'), client)
        self.assertIn(': queued pos=1 wait_s=', body)
        self.assertEqual(body.count('event: error\ndata: '), 1)
        self.assertIn('synthetic load failure', body)
        self.assertNotIn('[DONE]', body)
        self.assertTrue(body.endswith('\r\n0\r\n\r\n'), body)
        self.assertTrue(closed)
        self.assertEqual(self.engine.requests, [])


if __name__ == '__main__':
    unittest.main()
