"""Controller recovery, memory-guard and corrupt-cache regressions using owned stub processes."""
import fcntl
import hashlib
import http.client
import re
import io
import json
import os
import signal
import sys
import tempfile
import threading
import time
import unittest
from contextlib import redirect_stderr, redirect_stdout
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from unittest.mock import patch

from snapdragon_ai_lab.runtime import controller, state_cache as sc

sys.path.insert(0, str(Path(__file__).resolve().parent))
from _stub_manager import StubManagerCase  # noqa: E402

ORIGIN_HEADER, LAB_ORIGIN = 'X-Local-AI-Origin', 'lab'

PATHLIKE = re.compile(r'(?:^|[\s(\'"=])(?:/home/|/tmp/|/usr/|/proc/|/run/|/var/|~/)|\.py\b|\.rs:\d')

def plain_message(text):
    """(ok, why): a short single-line text with no traceback and no file path."""
    if not isinstance(text, str) or not text.strip():
        return False, 'empty'
    if 'Traceback' in text or re.search(r'File "[^"]+", line \d+', text):
        return False, 'traceback'
    if PATHLIKE.search(text):
        return False, 'path'
    if '\n' in text.strip():
        return False, 'multi-line'
    if len(text) > 300:
        return False, 'too long'
    return True, ''

def is_sentence(text):
    """A plain message that reads as words, not a bare code like 'host_memory_pressure_sustained'."""
    ok, _ = plain_message(text)
    return ok and not re.fullmatch(r'[A-Za-z0-9_.:-]+', text.strip()) and len(text.split()) >= 3


def wait_until(predicate, seconds=5.0):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(.02)
    return predicate()


def assert_plain(case, text):
    """The user-facing message: non-empty, short, one line, no traceback, no path."""
    case.assertIsInstance(text, str)
    case.assertTrue(text.strip())
    case.assertNotIn('/home', text)
    case.assertNotIn('Traceback', text)
    ok, why = plain_message(text)
    case.assertTrue(ok, f'{text!r}: {why}')


class DrillCase(StubManagerCase):
    """StubManagerCase plus every path constant the drills reach patched into a temp dir."""

    def setUp(self):
        self.side = tempfile.TemporaryDirectory(dir='/tmp')
        self.addCleanup(self.side.cleanup)
        side = Path(self.side.name)
        (side / 'runtime.json').write_text('{}')
        extra = [patch.object(controller, 'ENGINE_LEDGER_LOG', side / 'engine-ledger.jsonl'),
                 patch.object(controller, 'CONFIG', side / 'runtime.json'),
                 patch.object(controller, 'HARNESS_STATE', ''),
                 patch.dict(os.environ, {'LOCAL_AI_LEDGER_DIR': str(side / 'ledger')})]
        for item in extra:
            item.start()
            self.addCleanup(item.stop)
        super().setUp()
        self.log = self.root / 'engine-stderr.log'

    def died_events(self):
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines() if '"engine_died"' in line]

    def lease_free(self):
        """True when nobody holds the (temp) inference lease."""
        with open(controller.LEASE, 'a') as probe:
            try:
                fcntl.flock(probe, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return False
            fcntl.flock(probe, fcntl.LOCK_UN)
            return True

    def start(self, alias='stub-local', origin=None):
        self.manager.begin(allow_start=True, model_alias=alias, origin=origin)
        self.manager.end(touch=True)
        return self.manager.process


class ProxyDrillCase(DrillCase):
    def setUp(self):
        super().setUp()
        self.server = controller.Server(('127.0.0.1', 0), controller.Handler)
        self.server.manager = self.manager
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)

    def chat(self):
        client = http.client.HTTPConnection('127.0.0.1', self.server.server_port, timeout=20)
        self.addCleanup(client.close)
        body = json.dumps({'model': 'stub-local', 'stream': True,
                           'messages': [{'role': 'user', 'content': 'hi'}]}).encode()
        client.request('POST', '/v1/chat/completions', body=body,
                       headers={'Content-Type': 'application/json', ORIGIN_HEADER: LAB_ORIGIN,
                                'Host': f'127.0.0.1:{self.server.server_port}'})
        response = client.getresponse()
        try:
            return response, response.read(), True
        except http.client.IncompleteRead as error:
            return response, error.partial, False


# ---------------------------------------------------------------- R2: kill -9

class Kill9Drill(ProxyDrillCase):
    def test_sigkill_is_reaped_logged_released_and_respawned(self):
        with self.run_stub():
            child = self.start()
            old_pid = child.pid
            os.killpg(old_pid, signal.SIGKILL)
            self.assertTrue(wait_until(lambda: child.poll() is not None))
            self.manager.reap_once()
            self.assertIsNone(self.manager.process)
            self.assertEqual(self.manager.state, 'unloaded')
            self.assertIsNone(self.manager.lease)
            self.assertTrue(self.lease_free(), 'lease leaked after the crash')
            died = self.died_events()
            self.assertEqual(len(died), 1)
            self.assertEqual(died[0]['pid'], old_pid)
            self.assertEqual(died[0]['exit_code'], -signal.SIGKILL)
            self.assertEqual(died[0]['model_alias'], 'stub-local')
            status = self.manager.status()
            self.assertIsNone(status['stop_pending'])
            self.assertIsNone(status['last_error'])  # a crash is not a start failure
            new = self.start()
        self.assertNotEqual(new.pid, old_pid)
        self.assertIsNone(new.poll())
        self.assertEqual(self.manager.status()['state'], 'ready')

    def test_crash_before_the_reply_is_a_plain_502_backend_died(self):
        self.use('--die-before-response')
        with self.run_stub():
            response, body, _ = self.chat()
            self.assertEqual(response.status, 502)
            error = json.loads(body)['error']
            self.assertEqual(error['type'], 'backend_died')
            assert_plain(self, error['message'])
            self.assertTrue(is_sentence(error['message']))
            self.assertTrue(wait_until(lambda: self.manager.active == 0), 'slot not released')
            self.assertTrue(wait_until(lambda: bool(self.died_events())))
            self.assertEqual(self.died_events()[0]['exit_code'], 9)
            self.use()
            response, body, complete = self.chat()  # the next request respawns
        self.assertEqual(response.status, 200)
        self.assertTrue(complete)
        self.assertIn(b'[DONE]', body)

    def test_crash_mid_stream_truncates_then_recovers(self):
        self.use('--die-mid-stream')
        with self.run_stub():
            response, body, complete = self.chat()
            self.assertEqual(response.status, 200)
            self.assertFalse(complete, 'a crashed stream must end without its terminator')
            self.assertNotIn(b'[DONE]', body)
            self.assertTrue(wait_until(lambda: bool(self.died_events())))
            self.assertTrue(wait_until(lambda: self.manager.active == 0))
            # Known limit (reported as GAP by the hw drill): no message reaches the user here.
            self.assertIsNone(self.manager.status()['last_error'])
            self.use()
            response, body, complete = self.chat()
        self.assertEqual(response.status, 200)
        self.assertIn(b'[DONE]', body)


# ---------------------------------------------------------------- R3: guard trip

class GuardDrill(DrillCase):
    REASON = 'host_memory_pressure_sustained'

    def guard(self):
        return controller.memory_policy.MemoryGuard.return_value

    def test_guard_trip_unloads_cleanly_reports_and_restarts(self):
        with self.run_stub():
            child = self.start()
            self.guard().failure.return_value = self.REASON
            self.manager.reap_once()
            self.assertIsNotNone(child.poll(), 'engine still running after the guard stop')
            self.assertIsNone(self.manager.process)
            self.assertEqual(self.manager.state, 'unloaded')
            self.assertTrue(self.lease_free())
            status = self.manager.status()
            failure = status['last_failure']
            self.assertEqual(failure['kind'], 'guard')
            self.assertEqual(failure['alias'], 'stub-local')
            self.assertEqual(failure['origin'], 'organic')
            # What the controller really shows: the guard's reason as recorded (today the raw
            # GUARD-2 code; a mapping to a sentence would also pass). Plain either way.
            assert_plain(self, failure['reason'])
            assert_plain(self, status['last_error'])
            self.assertEqual(status['last_error'], failure['reason'])
            died = self.died_events()
            self.assertEqual(died[-1]['pid'], child.pid)
            self.assertEqual(died[-1]['guard_failure'], self.REASON)
            # The guard clears: the next start runs a fresh engine.
            self.guard().failure.return_value = None
            new = self.start()
        self.assertNotEqual(new.pid, child.pid)
        status = self.manager.status()
        self.assertEqual(status['state'], 'ready')
        self.assertIsNone(status['last_failure'], 'a successful start supersedes the guard stop')

    def test_lab_origin_guard_trip_is_not_the_users_error(self):
        with self.run_stub():
            self.start(origin='lab')
            self.guard().failure.return_value = self.REASON
            self.manager.reap_once()
        status = self.manager.status()
        self.assertEqual(status['state'], 'unloaded')
        self.assertEqual(status['last_failure']['origin'], 'lab')
        self.assertIsNone(status['last_error'])
        assert_plain(self, status['last_failure']['reason'])

    def test_guard_during_loading_gives_a_plain_start_error(self):
        self.guard().failure.return_value = self.REASON
        with self.run_stub(), self.assertRaises(controller.Unavailable) as raised:
            self.manager.begin(allow_start=True, model_alias='stub-local')
        assert_plain(self, str(raised.exception))
        self.assertTrue(is_sentence(str(raised.exception)))
        self.assertIsNone(self.manager.process)
        self.assertTrue(self.lease_free())
        assert_plain(self, self.manager.status()['last_error'])


# ---------------------------------------------------------------- R4: corrupt block

M = 'c' * 16
NOW = 2_000_000_000.0


def h(name):
    return hashlib.sha256(name.encode()).hexdigest()


def put(path, data, mtime=NOW - 86400):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    os.utime(path, (mtime, mtime))
    return path


def block_rel(name):
    hh = h(name)
    return f'blocks/{hh[:2]}/{hh}.{"0" * 16}.kv'


def session(root, sid, blocks, atime=NOW - 7200, n_tokens=1024):
    d = root / M
    put(d / 'identity.json', b'{}')
    for name in blocks:
        put(d / block_rel(name), bytes(range(256)) * 16)
    hint = h('hint:' + sid)[:32]
    put(d / 'echo' / f'{hint}.jsonl', b'{}\n')
    manifest = {'version': 1, 'sid': sid, 'hint': hint, 'created': atime, 'atime': atime, 'n_tokens': n_tokens,
                'blocks': [[h(n), '0' * 16] for n in blocks], 'snaps': []}
    put(d / 'sessions' / f'{sid}.json', json.dumps(manifest).encode())
    return d


def tree(root):
    return {str(p.relative_to(root)): p.read_bytes() for p in root.rglob('*') if p.is_file()}


class CorruptDrill(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(dir='/tmp')
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name) / 'state'
        session(self.root, 'drill', ['shared', 'd1', 'd2'])
        session(self.root, 'other', ['shared', 'o1'])
        put(self.root / M / 'blocks' / 'ab' / 'x.kv.pending', b'half', mtime=NOW - 10)
        self.model = self.root / M

    def corrupt(self):
        flip = self.model / block_rel('d2')
        data = bytearray(flip.read_bytes())
        data[len(data) // 2] ^= 0xFF
        flip.write_bytes(bytes(data))
        cut = self.model / block_rel('d1')
        cut.write_bytes(cut.read_bytes()[:100])
        return flip, cut

    def tear(self, sid):
        path = self.model / 'sessions' / f'{sid}.json'
        path.write_bytes(path.read_bytes()[:40])  # a torn write: truncated JSON

    def other_files(self):
        return {rel: data for rel, data in tree(self.root).items()
                if 'other' in rel or block_rel('o1') in rel or block_rel('shared') in rel}

    def test_usage_survives_flip_truncate_and_a_torn_manifest(self):
        self.corrupt()
        self.tear('drill')
        value = sc.usage(self.root, 64 * 10**9, 604800, now=NOW)
        files = [p for p in self.root.rglob('*') if p.is_file() and not p.name.endswith('.pending')]
        self.assertEqual(value['used_bytes'], sum(p.stat().st_size for p in files))
        self.assertEqual(value['sessions']['count'], 1)  # the torn manifest is not counted
        self.assertEqual(value['models'], {M: value['used_bytes']})

    def test_torn_manifest_makes_the_model_opaque_and_sweeps_nothing(self):
        self.corrupt()
        self.tear('drill')
        before = tree(self.root)
        self.assertEqual(sc.gc(self.root, sc.MIN_BUDGET, 604800, now=NOW), 0)
        freed = sc.clear(self.root, 'session', M, 'other', now=NOW)
        after = tree(self.root)
        # only the named session's manifest goes; its blocks may be referenced by the torn one
        self.assertEqual(set(before) - set(after), {f'{M}/sessions/other.json'})
        self.assertEqual(freed, len(before[f'{M}/sessions/other.json']))
        self.assertTrue(all(after[rel] == before[rel] for rel in after))

    def test_clearing_the_corrupt_session_never_touches_another_sessions_files(self):
        flip, cut = self.corrupt()
        others = self.other_files()
        self.assertTrue(others)
        sc.clear(self.root, 'session', M, 'drill', now=NOW)
        self.assertFalse(flip.exists())
        self.assertFalse(cut.exists())
        after = tree(self.root)
        for rel, data in others.items():
            self.assertEqual(after.get(rel), data, rel)

    def test_budget_eviction_with_corrupt_blocks_keeps_valid_neighbours(self):
        self.corrupt()
        others = self.other_files()
        # evict one session: the older one goes first; make 'drill' the older
        manifest = json.loads((self.model / 'sessions/drill.json').read_text())
        manifest['atime'] = NOW - 5 * 86400
        (self.model / 'sessions/drill.json').write_text(json.dumps(manifest))
        used = sc.usage(self.root, 0, 604800, now=NOW)['used_bytes']
        freed = sc.gc(self.root, used - 1, 604800, now=NOW)
        self.assertGreater(freed, 0)
        after = tree(self.root)
        self.assertNotIn(f'{M}/sessions/drill.json', after)
        for rel, data in others.items():
            self.assertEqual(after.get(rel), data, rel)
        self.assertLessEqual(sc.usage(self.root, 0, 604800, now=NOW)['used_bytes'], used - freed)

    def test_controller_routes_stay_tolerant_on_a_corrupt_store(self):
        self.corrupt()
        self.tear('drill')
        with tempfile.TemporaryDirectory(dir='/tmp') as side:
            side = Path(side)
            (side / 'runtime.json').write_text('{}')
            with patch.object(controller, 'CONFIG', side / 'runtime.json'), \
                    patch.object(controller, 'LEASE', side / 'generation.lock'), \
                    patch.object(controller, 'HARNESS_STATE', ''):
                manager = controller.Manager()
                manager.stop_event.set()
                self.addCleanup(manager.thread.join, 2)
                with patch.object(type(manager), 'state_root', lambda self, runtime=None: self.root_for_test):
                    manager.root_for_test = self.root
                    usage = manager.cache_usage()
                    self.assertEqual(usage['sessions']['count'], 1)
                    self.assertGreater(usage['used_bytes'], 0)
                    before = tree(self.root)
                    with manager.mu:
                        result = manager.clear_state_cache('session', M, 'other')
                    self.assertEqual(result['applied_by'], 'controller')
                    after = tree(self.root)
                    self.assertEqual(set(before) - set(after), {f'{M}/sessions/other.json'})


# ---------------------------------------------------------------- R5: swap x3

class SwapDrill(DrillCase):
    ALIASES = ('stub-a-local', 'stub-b-local')

    def setUp(self):
        super().setUp()

        def config(alias=None):
            alias = alias or self.ALIASES[0]
            return {'model_alias': alias, 'ready': True, 'model_path': str(self.model),
                    'server_binary': sys.executable, 'extra_args': ['--alias', alias]}
        self.manager.config = config

    def test_swap_cycle_x3_stops_the_previous_engine_without_leaks(self):
        seen, previous = [], None
        with self.run_stub():
            for cycle in range(3):
                for alias in self.ALIASES:
                    child = self.start(alias)
                    with self.subTest(cycle=cycle, alias=alias):
                        self.assertEqual(self.manager.loaded_config['model_alias'], alias)
                        self.assertIsNone(child.poll())
                        if previous is not None:
                            self.assertIsNot(child, previous)
                            self.assertIsNotNone(previous.poll(), 'previous engine still running')
                        self.assertEqual(self.manager.active, 0)
                        self.assertEqual(self.manager.inferences, 0)
                        status = self.manager.status()
                        self.assertEqual(status['model'], alias)
                        self.assertEqual(status['state'], 'ready')
                        self.assertIsNone(status['last_failure'])
                        self.assertIsNone(status['last_error'])
                        self.assertIsNone(status['stop_pending'])
                        body = self.models_reply()
                        self.assertEqual(body['data'][0]['id'], alias, 'the started engine serves the wrong alias')
                    seen.append(child.pid)
                    previous = child
        self.assertEqual(len(set(seen)), 6)
        with self.manager.mu:
            self.manager.cleanup()
        self.assertTrue(self.lease_free())
        self.assertFalse(self.died_events(), 'a planned swap is not an engine death')

    def models_reply(self):
        client = http.client.HTTPConnection('127.0.0.1', self.port, timeout=5)
        try:
            client.request('GET', '/v1/models')
            return json.loads(client.getresponse().read())
        finally:
            client.close()

    def test_swap_while_a_request_runs_is_refused_plainly(self):
        with self.run_stub():
            self.start(self.ALIASES[0])
            self.manager.begin(allow_start=True, model_alias=self.ALIASES[0])  # a request in flight
            try:
                with self.assertRaises(controller.Unavailable) as raised:
                    self.manager.begin(allow_start=True, model_alias=self.ALIASES[1])
            finally:
                self.manager.end()
        assert_plain(self, str(raised.exception))
        self.assertEqual(self.manager.loaded_config['model_alias'], self.ALIASES[0])
        self.assertIsNone(self.manager.status()['last_error'])
