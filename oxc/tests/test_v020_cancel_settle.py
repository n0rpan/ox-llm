"""v0.2.0 CANCEL-SETTLE (CI-TRUTH CT-15): a client that cancels a streaming request through the controller
(closes its connection mid-stream) must not make the NEXT request wait for the cancelled generation.

Before: a replay-eligible request (stream, temperature 0, X-Session-Id + Idempotency-Key: exactly what oxa
sends) that was cut kept the engine generating for ctl_replay.GRACE_S (15 s) or to its end, and a request of
another session (oxa's subagent/parent, any second client) waited for it. Now any request waiting for the slot
cancels unattached cut entries within ctl_queue.POLL_S. Driven over loopback through the real controller
Handler + queue with a scripted fake engine that would stream for 10 s unless cancelled."""
import os as _os, tempfile as _tempfile
_os.environ.setdefault('OXC_LEDGER_DIR', _tempfile.mkdtemp(prefix='oxc-test-ledger-'))
import socket
import sys
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(__import__('pathlib').Path(__file__).resolve().parent))
import test_v020_preempt as pre  # noqa: E402
from oxc.runtime import ctl_queue  # noqa: E402

SETTLE_LIMIT_S = 2.0


class CancelSettle(pre.PreemptOverHttp):
    def setUp(self):
        super().setUp()
        poll = patch.object(ctl_queue, 'POLL_S', 0.5)  # the production poll, not the harness' 0.02
        poll.start()
        self.addCleanup(poll.stop)

    def test_cut_stream_releases_the_slot_for_another_session_at_once(self):
        def behave(index, body, conn):
            if index == 0:  # a long generation: 10 s of tokens unless the controller cancels it
                conn.sendall(pre.SSE_HEAD)
                deadline = time.monotonic() + 10
                try:
                    while time.monotonic() < deadline:
                        conn.sendall(b'data: "tok"\n\n')
                        time.sleep(0.02)
                    self.engine.cut[index] = False
                except OSError:
                    self.engine.cut[index] = True
            else:
                pre.answer(conn, b'"second"')
        self.use_engine(behave)
        first = pre.Client(self.proxy.server_port, self.body('essay', temperature=0),
                           (('X-Session-Id', 'oxa.parent'), ('Idempotency-Key', 'k-essay')), leave_at=b'"tok"')
        pre.wait_for(lambda: first.closed == 'client')
        cut_at = time.monotonic()
        second = pre.Client(self.proxy.server_port, self.body('after'), (('X-Session-Id', 'oxa.child'),))
        pre.wait_for(lambda: b'"second"' in second.data, timeout=12)
        settle = time.monotonic() - cut_at
        second.join()
        first.join()
        self.assertLess(settle, SETTLE_LIMIT_S, 'the next request waited %.1f s for a cancelled generation' % settle)
        pre.wait_for(lambda: self.engine.cut.get(0) is not None, timeout=12)
        self.assertTrue(self.engine.cut[0], 'the cancelled generation was not cancelled at the engine')


if __name__ == '__main__':
    unittest.main()


class AttachRace(unittest.TestCase):
    """Codex L3 review de6dc53 (attach_race.py): a waiting request's yield must never abort a cut entry that an
    identical retry attached to after the entry was selected. Deterministic: the yield pauses right after
    selecting, the retry attaches, then the yield's cancel decision runs."""

    def setUp(self):
        from oxc.runtime import ctl_replay as r
        self.r = r
        self.store = r.Store()
        self.ident = r.Identity('key', 'body', 'lane')
        self.entry = r.Entry(self.ident)
        self.entry.state = 'cut'
        self.cancelled = []
        self.entry.upstream = type('Up', (), {'shutdown': lambda _s, how: self.cancelled.append(how)})()
        self.store.entries[self.entry.key] = self.entry

    def run_case(self, retry_first):
        import threading
        import types
        r = self.r
        selected, attached, served_done = threading.Event(), threading.Event(), threading.Event()
        decide = r._abort_if_unattached
        seen = {}

        def paused_decide(entry):
            selected.set()
            if retry_first:
                assert attached.wait(5), 'the retry never attached'
            seen['attached_at_decision'] = entry.attached
            return decide(entry)

        def serve(handler, st, entry):
            attached.set()
            assert served_done.wait(5)
        st = types.SimpleNamespace(robust=types.SimpleNamespace(replay_id=None))
        with patch.object(r, 'STORE', self.store), patch.object(r, 'identity', return_value=self.ident), \
                patch.object(r, '_abort_if_unattached', side_effect=paused_decide), patch.object(r, '_serve', side_effect=serve):
            y = threading.Thread(target=r.yield_cut_entries)
            y.start()
            assert selected.wait(5)
            def retry():
                try:
                    seen['retry'] = r.try_attach(types.SimpleNamespace(), st)
                except BaseException as exc:  # surfaced below: a failing thread must not pass silently
                    seen['retry_error'] = repr(exc)
            if retry_first:
                t = threading.Thread(target=retry)
                t.start()
                y.join(5)
                served_done.set()
                t.join(5)
            else:
                y.join(5)
                retry()
                served_done.set()
        self.assertNotIn('retry_error', seen)
        self.assertFalse(y.is_alive())
        return seen

    def test_retry_attached_after_selection_is_not_aborted(self):
        seen = self.run_case(retry_first=True)
        self.assertEqual(seen['attached_at_decision'], 1)
        self.assertIs(seen['retry'], self.r.ANSWERED)
        self.assertEqual(self.entry.served, 1, 'the replay was served to the retry')
        self.assertEqual(self.cancelled, [], 'the attached replay was cancelled at the engine')
        self.assertEqual(self.entry.state, 'cut')
        self.assertIn(self.entry.key, self.store.entries)

    def test_cancel_first_makes_the_retry_a_fresh_request(self):
        seen = self.run_case(retry_first=False)
        self.assertIsNone(seen['retry'], 'a retry after the cancel must not attach to the aborted entry')
        self.assertEqual(len(self.cancelled), 1)
        self.assertEqual(self.entry.state, 'aborted')
        self.assertNotIn(self.entry.key, self.store.entries)
