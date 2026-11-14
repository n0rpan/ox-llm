"""OEM release-gate drills, model-free CI versions (OEM-DONE.md R2-R5; tools/oem_drills.py ci).

kill -9, guard trip and swap x3 drive a real controller.Manager with the llama stub on a real
loopback port (tests/_stub_manager.py); the corrupt-block drill runs the controller side of the
disk state cache (state_cache.py, Manager.cache_usage/clear_state_cache) on a synthetic tree.
Check64 drop-and-recompute of a bad block is the engine's (engine repo, M2 CI): the
controller never reads block contents, so here it must only stay tolerant and never delete
another session's files. The hw mode's pure pieces (argument gate, receipt, the lab origin
header) run against a tiny fake HTTP server, never the real controller.
"""
import fcntl
import hashlib
import http.client
import importlib.util
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

from oxc.runtime import controller, state_cache as sc

sys.path.insert(0, str(Path(__file__).resolve().parent))
from _stub_manager import StubManagerCase  # noqa: E402

TOOL = Path(__file__).resolve().parents[1] / 'tools/oem_drills.py'
spec = importlib.util.spec_from_file_location('oem_drills', TOOL)
drills = importlib.util.module_from_spec(spec)
spec.loader.exec_module(drills)


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
    ok, why = drills.plain_message(text)
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
                 patch.dict(os.environ, {'OXC_LEDGER_DIR': str(side / 'ledger')})]
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
                       headers={'Content-Type': 'application/json', drills.ORIGIN_HEADER: drills.LAB_ORIGIN,
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
            self.assertTrue(drills.is_sentence(error['message']))
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
        self.assertTrue(drills.is_sentence(str(raised.exception)))
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


# ---------------------------------------------------------------- hw mode, pure pieces

class FakeController:
    """A loopback stand-in answering the routes the swap drill uses; records every header."""

    def __init__(self):
        self.requests, self.selected, self.served = [], 'ornith-35b-a3b-local', set()
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def reply(self, code, value):
                data = json.dumps(value).encode()
                self.send_response(code)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                outer.requests.append(('GET', self.path, dict(self.headers)))
                if self.path == '/_local/status':
                    return self.reply(200, {'state': 'ready', 'model': outer.selected, 'active_requests': 0,
                                            'stop_pending': None, 'last_error': None, 'last_failure': None})
                if self.path == '/v1/models':
                    return self.reply(200, {'object': 'list', 'data': [{'id': m, 'object': 'model'}
                                                                       for m in drills.MOE_MODELS]})
                self.reply(404, {'error': 'no'})

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers.get('Content-Length') or 0)) or b'{}')
                outer.requests.append(('POST', self.path, dict(self.headers)))
                if self.path == '/_local/select':
                    outer.selected = body['model']
                    return self.reply(200, {'selected_model': body['model'], 'state': 'unloaded'})
                if self.path == '/v1/chat/completions':
                    key = (body['model'], self.headers.get('X-Session-Id'))
                    cached = 120 if key in outer.served else 0
                    outer.served.add(key)
                    return self.reply(200, {'choices': [{'message': {'content': 'OK'}}],
                                            'timings': {'cache_n': cached}})
                self.reply(404, {'error': 'no'})

        self.server = HTTPServer(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f'http://127.0.0.1:{self.server.server_port}'

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class HwModePieces(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(dir='/tmp')
        self.addCleanup(tmp.cleanup)
        self.out = Path(tmp.name) / 'receipts'
        self.fake = FakeController()
        self.addCleanup(self.fake.close)

    def run_main(self, *argv):
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = drills.main(list(argv))
        return code, out.getvalue(), err.getvalue()

    def test_guard_and_all_refuse_without_the_memory_pressure_flag(self):
        for drill in ('guard', 'all'):
            with self.subTest(drill=drill):
                code, _, err = self.run_main('hw', drill, '--controller', self.fake.url, '--out', str(self.out))
                self.assertEqual(code, 2)
                self.assertIn('--i-understand-memory-pressure', err)
        self.assertEqual(self.fake.requests, [], 'a refused drill must not send anything')
        self.assertFalse(self.out.exists())

    def test_help_marks_guard_attended(self):
        with redirect_stdout(io.StringIO()) as out, self.assertRaises(SystemExit):
            drills.main(['hw', '--help'])
        self.assertIn('guard    expected 1-2 min, attended: recommended', out.getvalue())

    def test_non_loopback_controller_is_refused(self):
        code, _, err = self.run_main('hw', 'swap', '--controller', 'http://192.0.2.1:8092', '--out', str(self.out))
        self.assertEqual(code, 2)
        self.assertIn('loopback', err)

    def test_bad_arguments_are_rejected(self):
        for argv in (['hw', 'nope'], ['hw', 'swap', '--cap-seconds', '900'], ['hw', 'swap', '--backend-ports', 'x']):
            with self.subTest(argv=argv), redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                drills.main(argv)

    def test_swap_drill_on_a_fake_controller_writes_a_receipt_and_sends_lab_origin(self):
        code, out, _ = self.run_main('hw', 'swap', '--controller', self.fake.url, '--out', str(self.out),
                                     '--backend-ports', '1', '--cap-seconds', '60')
        self.assertEqual(code, 0, out)
        self.assertIn('PASS swap', out)
        self.assertTrue(self.fake.requests)
        for method, path, headers in self.fake.requests:
            self.assertEqual(headers.get(drills.ORIGIN_HEADER), 'lab', f'{method} {path}')
        receipt_path = self.out / 'swap.json'
        self.assertEqual(receipt_path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.out.stat().st_mode & 0o777, 0o700)
        receipt = json.loads(receipt_path.read_text())
        self.assertEqual(receipt['result'], 'PASS')
        self.assertEqual(receipt['attended'], 'no')
        self.assertEqual(receipt['origin'], 'lab')
        self.assertEqual(receipt['cap_seconds'], 60)
        for key in ('drill', 'controller', 'started_at', 'finished_at', 'wall_seconds', 'checks', 'observations', 'error'):
            self.assertIn(key, receipt)
        cycles = receipt['observations']['cycles']
        self.assertEqual([(r['cycle'], r['model']) for r in cycles],
                         [(c, m) for c in (1, 2, 3) for m in drills.MOE_MODELS])
        self.assertTrue(all(r['cache_n'] > 0 for r in cycles if r['cycle'] > 1))
        chats = [h.get('X-Session-Id') for m, p, h in self.fake.requests if p == '/v1/chat/completions']
        self.assertEqual(len(set(chats)), 2, 'one fixed session per model')

    def test_a_drill_that_finds_no_engine_fails_with_a_receipt(self):
        code, out, _ = self.run_main('hw', 'kill9', '--controller', self.fake.url, '--out', str(self.out),
                                     '--backend-ports', '1', '--cap-seconds', '30')
        self.assertEqual(code, 1)
        self.assertIn('FAIL kill9', out)
        receipt = json.loads((self.out / 'kill9.json').read_text())
        self.assertEqual(receipt['result'], 'FAIL')
        self.assertTrue(any(c['status'] == 'FAIL' and 'engine pid' in c['name'] for c in receipt['checks']))

    FAKE_AGENT = ('#!/usr/bin/env python3\nimport json, os, sys, urllib.request\n'
                  'if sys.argv[1:] == ["--version"]:\n    print("oxa @VERSION@"); sys.exit(0)\n'
                  'if sys.argv[1:] == ["--list-models"]:\n'
                  '    rows = json.load(urllib.request.urlopen(os.environ["OXA_ENGINE_URL"] + "/v1/models"))["data"]\n'
                  '    print("\\n".join(r["id"] for r in rows)); sys.exit(0)\n'
                  'sys.exit(2)\n')

    def agent(self, version='0.2.0'):
        path = self.out.parent / 'bin' / 'oxa'
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(self.FAKE_AGENT.replace('@VERSION@', version))
        path.chmod(0o700)
        return path

    def test_oxa_drill_checks_install_version_bwrap_and_the_controller_endpoint(self):
        agent = self.agent()
        with patch.object(drills.shutil, 'which', return_value='/usr/bin/bwrap'):
            code, out, _ = self.run_main('hw', 'oxa', '--controller', self.fake.url, '--out', str(self.out),
                                         '--agent', str(agent), '--cap-seconds', '30')
        self.assertEqual(code, 0, out)
        receipt = json.loads((self.out / 'oxa.json').read_text())
        self.assertEqual(receipt['observations']['version'], '0.2.0')
        self.assertTrue(any(c['name'] == 'oxa sees the selected model' and c['status'] == 'PASS'
                            for c in receipt['checks']))
        self.assertFalse([p for m, p, h in self.fake.requests if p == '/v1/chat/completions'], 'no prompt sent')

    def test_oxa_drill_fails_on_wrong_version_missing_bwrap_or_missing_agent(self):
        agent = self.agent('0.1.9')
        with patch.object(drills.shutil, 'which', return_value=None):
            code, out, _ = self.run_main('hw', 'oxa', '--controller', self.fake.url, '--out', str(self.out),
                                         '--agent', str(agent), '--cap-seconds', '30')
        self.assertEqual(code, 1)
        failed = {c['name'] for c in json.loads((self.out / 'oxa.json').read_text())['checks'] if c['status'] == 'FAIL'}
        self.assertEqual(failed, {'oxa is version 0.2.0', 'bwrap present (oxa bash tool sandbox)'})
        code, out, _ = self.run_main('hw', 'oxa', '--controller', self.fake.url, '--out', str(self.out),
                                     '--agent', str(agent.parent / 'missing'), '--cap-seconds', '30')
        self.assertEqual(code, 1)
        self.assertIn('oxa installed and executable', out)

    def test_receipt_result_rules(self):
        receipt = drills.Receipt('guard', 'http://127.0.0.1:1', 300)
        self.assertEqual(receipt.value['attended'], 'recommended')
        receipt.check('works', True)
        receipt.check('reads as a sentence', False, 'raw code', gap=True)
        self.assertEqual(receipt.finish()['result'], 'PASS')
        receipt.check('broken', False)
        self.assertEqual(receipt.finish()['result'], 'FAIL')
        empty = drills.Receipt('swap', 'x', 300)
        self.assertEqual(empty.finish()['result'], 'FAIL', 'a drill that checked nothing did not pass')

    def test_plain_message_and_sentence(self):
        for text in ('Local backend connection closed.', 'Local model stopped to preserve system memory.'):
            self.assertTrue(drills.plain_message(text)[0])
            self.assertTrue(drills.is_sentence(text))
        self.assertTrue(drills.plain_message('host_memory_pressure_sustained')[0])
        self.assertFalse(drills.is_sentence('host_memory_pressure_sustained'))
        for text in ('', None, 'Traceback (most recent call last): x', 'failed at /home/user/x',
                     'File "ctl.py", line 3, in x', 'a\nb', 'x' * 400):
            self.assertFalse(drills.plain_message(text)[0], repr(text))

    def test_cache_n_reads_timings_then_usage(self):
        self.assertEqual(drills.cache_n({'timings': {'cache_n': 7}}), 7)
        self.assertEqual(drills.cache_n({'usage': {'prompt_tokens_details': {'cached_tokens': 3}}}), 3)
        self.assertIsNone(drills.cache_n({'choices': []}))

    def test_default_out_is_under_xdg_state(self):
        with patch.dict(os.environ, {'XDG_STATE_HOME': str(self.out)}):
            self.assertTrue(str(drills.default_out()).startswith(str(self.out / 'oxc/drills/')))


if __name__ == '__main__':
    unittest.main()
