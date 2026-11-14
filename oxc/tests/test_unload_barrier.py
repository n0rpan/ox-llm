"""I1 (Codex 03:58, interim 71abfd4): after the terminal SSE the controller still finalizes the
request; an unload sent then gets 409 request-running. ScopedController must wait on the
controller's own status (bounded barrier), require an ACCEPTED unload, and a ladder must never
capture a rung after a refused unload. Model-free: the REAL controller Handler + Manager
(openai_api_stack.Stack, engine start replaced by a marker) in front of the stub FakeEngine."""
import os as _os, tempfile as _tempfile
_os.environ.setdefault('OXC_LEDGER_DIR', _tempfile.mkdtemp(prefix='oxc-test-ledger-'))  # never the real request ledger
import http.client
import json
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest

LAB = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(LAB / 'benchmarks/gatepack'), str(LAB / 'benchmarks/fixtures')]
from openai_api_stack import ORNITH, FakeEngine, Stack, StackManager  # noqa: E402
from scoped_controller import ScopedController, UnloadRefused  # noqa: E402
import fixture_lib as fx  # noqa: E402
import ladder  # noqa: E402

FINALIZE_DELAY_S = 1.0


class SlowFinalize(StackManager):
    """The real Manager's request end, delayed: the client has the terminal SSE ([DONE]) while the
    controller still counts the request as running (ledger/finalization)."""
    # Optional phase gates (set per test; None = the plain delayed finalize). `gate_begin` holds the request in
    # "still generating" (active, not yet finalizing); `began` fires once it is finalizing; `release_end` holds it
    # in "finalizing" until the test lets it finish.
    gate_begin = began = release_end = None

    def begin_finalizing(self):
        if self.gate_begin is not None:
            assert self.gate_begin.wait(10), 'gate_begin never released'
        super().begin_finalizing()
        if self.began is not None:
            self.began.set()

    def end(self, *, inference=False, touch=False, finalizing=False):
        if inference:
            if self.release_end is not None:
                assert self.release_end.wait(10), 'release_end never set'
            else:
                time.sleep(FINALIZE_DELAY_S)
        return super().end(inference=inference, touch=touch, finalizing=finalizing)


def scoped_on(port, out):
    sc = ScopedController.__new__(ScopedController)
    sc.port, sc.backend, sc.credential, sc.out, sc.log = port, None, 'unused', Path(out), []
    sc.native_fd = sc.native_pid = None
    return sc


class RealControllerUnloadBarrier(unittest.TestCase):
    def setUp(self):
        self.fake = FakeEngine()
        self.addCleanup(self.fake.close)
        self.tmp = Path(self.enterContext(tempfile.TemporaryDirectory()))

    def stack(self):
        import openai_api_stack
        orig = openai_api_stack.StackManager
        openai_api_stack.StackManager = SlowFinalize
        try:
            st = Stack(self.fake.port)
        finally:
            openai_api_stack.StackManager = orig
        self.enterContext(st)
        self.assertIsInstance(st.manager, SlowFinalize)
        return st

    def chat_until_done(self, port):
        c = http.client.HTTPConnection('127.0.0.1', port, timeout=10)
        c.request('POST', '/v1/chat/completions', json.dumps({
            'model': ORNITH, 'stream': True, 'max_tokens': 8,
            'messages': [{'role': 'user', 'content': 'hello'}]}).encode(),
            {'Content-Type': 'application/json', 'X-Local-AI-Origin': 'lab'})
        r = c.getresponse()
        self.assertEqual(r.status, 200)
        got = b''
        while b'[DONE]' not in got:
            chunk = r.read1(65536)
            if not chunk:
                break
            got += chunk
        self.assertIn(b'[DONE]', got)
        return c, r

    def test_unload_right_after_terminal_sse_waits_for_finalization(self):
        st = self.stack()
        sc = scoped_on(st.port, self.tmp)
        st.manager.gate_begin = threading.Event()
        c, r = self.chat_until_done(st.port)
        try:
            # the boundary I1 hit: the controller still runs the request after [DONE]. Held in "still
            # generating" (not yet finalizing) a raw unload is refused, deterministically.
            busy = sc.call('GET', '/_local/status')[1]
            self.assertEqual(busy['active_requests'], 1)
            self.assertEqual(st.manager.finalizing, 0)
            self.assertEqual(sc.call('POST', '/_local/unload', {})[0], 409)
            st.manager.gate_begin.set()
            t0 = time.monotonic()
            self.assertTrue(sc.unload(timeout=5, quiesce_s=10))
            waited = time.monotonic() - t0
        finally:
            r.close()
            c.close()
        self.assertGreater(waited, 0.2)
        events = [row['event'] for row in sc.log]
        self.assertEqual(events[-3:], ['quiesce', 'unload_response', 'unload'])
        resp = [row for row in sc.log if row['event'] == 'unload_response']
        self.assertEqual([(x['status'], x['ok']) for x in resp], [(200, True)])
        self.assertIsNone(st.manager.process)

    def test_raw_unload_phases_generating_409_then_finalizing_200(self):
        st = self.stack()
        sc = scoped_on(st.port, self.tmp)
        m = st.manager
        m.gate_begin, m.began, m.release_end = threading.Event(), threading.Event(), threading.Event()
        c, r = self.chat_until_done(st.port)
        try:
            self.assertEqual((m.active, m.finalizing), (1, 0))  # phase 1: still generating
            self.assertEqual(sc.call('POST', '/_local/unload', {})[0], 409)
            m.gate_begin.set()
            self.assertTrue(m.began.wait(10))
            self.assertEqual((m.active, m.finalizing), (1, 1))  # phase 2: only finalizing
            out = []
            t = threading.Thread(target=lambda: out.append(sc.call('POST', '/_local/unload', {})[0]))
            t.start()
            m.release_end.set()  # the POST either waits for this end or lands right after it: 200 both ways
            t.join(10)
            self.assertFalse(t.is_alive())
            self.assertEqual(out, [200])
        finally:
            m.gate_begin.set()
            m.release_end.set()
            r.close()
            c.close()
        self.assertIsNone(m.process)

    def test_barrier_deadline_refuses_and_leaves_model_loaded(self):
        st = self.stack()
        sc = scoped_on(st.port, self.tmp)
        c, r = self.chat_until_done(st.port)
        try:
            with self.assertRaises(UnloadRefused) as cm:
                sc.unload(timeout=5, quiesce_s=0.2)
        finally:
            r.close()
            c.close()
        self.assertEqual(str(cm.exception), 'quiesce_timeout')
        self.assertFalse([row for row in sc.log if row['event'] == 'unload_response'])  # no blind POST
        self.assertIsNotNone(st.manager.process)


class RejectedUnloadNeverYieldsARung(unittest.TestCase):
    """Negative: whatever the handle reports short of an accepted unload, build_replay captures no
    rung and the ladder is left 'failed' (gc removes it), never usable."""
    class Handle:
        alias, rehearsal, port, url = ORNITH, True, 1, 'http://127.0.0.1:1'

        def __init__(self, result):
            self.result, self.events = result, []

        def start(self, live):
            Path(live).mkdir(parents=True, exist_ok=True)
            self.events.append('start')

        def load(self):
            self.events.append('load')

        def unload(self):
            self.events.append('unload')
            if isinstance(self.result, Exception):
                raise self.result
            return self.result

        def stop(self):
            self.events.append('stop')

    class Session:
        def __init__(self, *a, **k):
            pass

        def to_json(self):
            return {'generator': 'test'}

        def grow(self, *a, **k):
            return {'measured': 1, 'last_body': {}}

    def test_refused_unload_fails_the_ladder(self):
        for result in (False, None, UnloadRefused('quiesce_timeout'), UnloadRefused('unload_rejected:409'),
                       RuntimeError('native_engine_needed_sigkill')):
            with self.subTest(result=result), tempfile.TemporaryDirectory() as d:
                d = Path(d)
                orig = ladder.replay_driver.Session
                ladder.replay_driver.Session = self.Session
                handle = self.Handle(result)
                try:
                    with self.assertRaises(fx.FixtureError) as cm:
                        ladder.build_replay(handle, d / 'root', 'm', 'tiled', 1, ['32K'], d / 'scratch', log=lambda *a: None)
                finally:
                    ladder.replay_driver.Session = orig
                self.assertTrue(str(cm.exception).startswith('unload_not_accepted:32K'), cm.exception)
                metas = list((d / 'root').rglob('ladder.json'))
                self.assertEqual(len(metas), 1)
                self.assertEqual(json.loads(metas[0].read_text())['state'], 'failed')
                self.assertEqual(list((d / 'root').rglob('rung.json')), [])
                self.assertFalse(list((d / 'root').rglob('rungs')))
                self.assertEqual(handle.events[-1], 'stop')


if __name__ == '__main__':
    unittest.main()
