"""Interim Flash smoke + real-use session adapters: positive and negative controls, model-free
(a fake controller on loopback; synthetic session records). No model, no cargo, no oxa."""
import http.server
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest

LAB = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(LAB / 'tools/interim'), str(LAB / 'tools/tiny_real')]
import flash_smoke as fs  # noqa: E402
import real_use_session as ru  # noqa: E402

ALIAS = fs.ALIAS


class FakeController(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    cfg = {}
    state = {'loaded': False, 'prompt': 0}

    def log_message(self, *a):
        return

    def reply(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        st = self.state
        model = self.cfg.get('status_model', ALIAS)
        if self.cfg.get('status_fail'):
            return self.reply(500, {'error': 'x'})
        if self.cfg.get('status_partial'):
            return self.reply(200, {'model': model})          # no `state`: partial
        if self.cfg.get('status_partial_after_unload') and not st['loaded']:
            return self.reply(200, {})
        self.reply(200, {'state': 'ready' if st['loaded'] else 'idle', 'model': model})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get('Content-Length') or 0)) or b'{}')
        c = self.cfg
        if self.path == '/_local/load':
            if c.get('load_fail'):
                return self.reply(503, {'error': 'admission refused'})
            self.state['loaded'] = True
            return self.reply(200, {'state': 'ready'})
        if self.path == '/_local/unload':
            if c.get('unload_fail'):
                return self.reply(409, {'error': 'A request is running.'})
            self.state['loaded'] = False
            return self.reply(200, {'state': 'unloaded'})
        n = 4 if len(body['messages']) > 2 else 3
        text = c.get('answer', '{"ok": true, "n": %d}') % n if '%d' in c.get('answer', '%d') else c['answer']
        prompt = 40 + 20 * (len(body['messages']) > 2)
        cached = 0 if n == 3 else (c.get('cached', 40 + 12 - 1))
        chunks = [{'choices': [{'delta': {'reasoning_content': 'r'}}]},
                  {'choices': [{'delta': {'content': text}, 'finish_reason': 'stop'}]},
                  {'choices': [], 'usage': {'prompt_tokens': prompt, 'completion_tokens': 12,
                                            'prompt_tokens_details': {'cached_tokens': cached}}}]
        data = b''.join(b'data: ' + json.dumps(x).encode() + b'\n\n' for x in chunks) + b'data: [DONE]\n\n'
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)


class FlashSmokeTests(unittest.TestCase):
    def run_with(self, controller=True, **cfg):
        FakeController.cfg = cfg
        FakeController.state = {'loaded': False, 'prompt': 0}
        srv = http.server.ThreadingHTTPServer(('127.0.0.1', 0), FakeController)
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        try:
            with tempfile.TemporaryDirectory() as d:
                rc = fs.main(['--base-url', 'http://127.0.0.1:%d' % srv.server_address[1], '--out', d]
                             + ([] if controller else ['--no-controller']))
                rep = json.loads(Path(d, 'flash-smoke.json').read_text())
        finally:
            srv.shutdown()
        return rc, {s['step']: s['verdict'] for s in rep['steps']}

    def test_positive(self):
        rc, steps = self.run_with()
        self.assertEqual(rc, 0)
        self.assertEqual(steps, {'S1-load': 'PASS', 'S2-chat': 'PASS', 'S3-append': 'PASS', 'S4-unload': 'PASS'})

    def test_load_refused_fails_and_stops(self):
        rc, steps = self.run_with(load_fail=True)
        self.assertEqual((rc, steps), (1, {'S1-load': 'FAIL'}))

    def test_wrong_loaded_alias_fails(self):
        rc, steps = self.run_with(status_model='ornith-35b-a3b-local')
        self.assertEqual(rc, 1)
        self.assertEqual(steps['S1-load'], 'FAIL')

    def test_invalid_answer_fails(self):
        rc, steps = self.run_with(answer='not json')
        self.assertEqual(rc, 1)
        self.assertEqual(steps['S2-chat'], 'FAIL')

    def test_append_cache_miss_fails(self):
        rc, steps = self.run_with(cached=10)
        self.assertEqual(rc, 1)
        self.assertEqual(steps['S3-append'], 'FAIL')

    def test_missing_or_partial_status_fails_closed(self):
        for cfg, step in (({'status_fail': True}, 'S1-load'), ({'status_partial': True}, 'S1-load'),
                          ({'status_partial_after_unload': True}, 'S4-unload')):
            with self.subTest(cfg):
                rc, steps = self.run_with(**cfg)
                self.assertEqual(rc, 1)
                self.assertEqual(steps[step], 'FAIL')

    def test_codex_flash_controls(self):
        class Fake:
            def __init__(self, bad_status=False, bad_finish=False):
                self.bad_status, self.bad_finish, self.n = bad_status, bad_finish, 0

            def call(self, method, path, body=None):
                if path == '/_local/load':
                    return 200, {'state': 'ready'}
                if path == '/_local/unload':
                    return 200, {'state': 'unloaded'}
                if self.bad_status:
                    return 500, None
                return 200, {'state': 'ready' if self.n < 2 else 'idle', 'model': fs.ALIAS}

            def stream(self, body):
                self.n += 1
                return dict(status=200, done=True, finish='length' if self.bad_finish and self.n == 2 else 'stop',
                            content=json.dumps({'ok': True, 'n': 2 + self.n}), reasoning='',
                            usage={'prompt_tokens': 40 if self.n == 1 else 60, 'completion_tokens': 12,
                                   'prompt_tokens_details': {'cached_tokens': 51}}, timings={})
        steps = fs.run(Fake(bad_status=True), fs.ALIAS)
        self.assertFalse(len(steps) == 4 and all(x['verdict'] == 'PASS' for x in steps))
        steps = {x['step']: x['verdict'] for x in fs.run(Fake(bad_finish=True), fs.ALIAS)}
        self.assertEqual(steps['S3-append'], 'FAIL')

    def test_unload_refused_fails(self):
        rc, steps = self.run_with(unload_fail=True)
        self.assertEqual(rc, 1)
        self.assertEqual(steps['S4-unload'], 'FAIL')

    def test_no_controller_rehearsal_needs_both_chat_steps(self):
        rc, steps = self.run_with(controller=False)
        self.assertEqual((rc, steps), (0, {'S2-chat': 'PASS', 'S3-append': 'PASS'}))

    def test_non_loopback_refused(self):
        with self.assertRaises(SystemExit):
            fs.Client('http://192.0.2.1:8092')


class RealUseVerdictTests(unittest.TestCase):
    LINES = 300

    @staticmethod
    def st(i, tool, task, **args):
        return {'type': 'tool_execution_start', 'toolCallId': i, 'toolName': tool, 'args': args, '_task': task}

    @staticmethod
    def en(i, tool, task, err=False, text='', exit_code=None):
        details = {} if exit_code is None else {'exit': exit_code}
        return {'type': 'tool_execution_end', 'toolCallId': i, 'toolName': tool, 'isError': err, '_task': task,
                'result': {'content': [{'type': 'text', 'text': text}], 'details': details}}

    def good(self):
        T = ru.LARGE_READ
        tasks = [dict(task=t[0], error=None, compactions=0, wall_s=60.0) for t in ru.TASKS]
        tasks[0]['compactions'] = 1
        tasks[4]['aborted'] = True
        recs = [self.st('r1', 'read', 't1-large-read', path=T), self.en('r1', 'read', 't1-large-read', text='x\n\n[Showing lines 1-150 of 300. Use offset=151 to continue.]'),
                self.st('r2', 'read', 't1-large-read', path=T, offset=151), self.en('r2', 'read', 't1-large-read', text='y\n' * 150),
                self.st('b1', 'bash', 't2-bug', command='cd engine && cargo test -p gguf --offline'),
                self.en('b1', 'bash', 't2-bug', text='running 11 tests\ntest result: ok. 11 passed; 0 failed', exit_code=0),
                self.st('b2', 'bash', 't3-unittest', command='cd lab && ./tools/run_lab_tests.sh discover -s tests'),
                self.en('b2', 'bash', 't3-unittest', text='...\nRan 12 tests in 2.4s\n\nOK\n', exit_code=0),
                self.st('s1', 'subagent', 't4-subagent'), self.en('s1', 'subagent', 't4-subagent', text='list')]
        rows = [{'chat': True, 'status': 200, 'usage_seen': True, 'session_hdr': 'm', 't': 100.0 + 10 * i, 'dur_s': 9.0} for i in range(6)]
        second = {'statuses': [{'status': 200, 'done': True, 't0': 105.0, 't1': 108.0}, {'status': 503, 'done': False}]}
        chk = {'tests_unchanged': (True, ''), 'mutation_removed': (True, ''), 'diff_scoped': (True, ''), 'checker_tests': (True, '')}
        return tasks, recs, rows, second, chk

    def overall(self, args):
        v = ru.verdicts(*args, seal={'large_read_lines': self.LINES})
        return all(ok for ok, _ in v.values()), {k: ok for k, (ok, _) in v.items()}

    def test_positive(self):
        ok, v = self.overall(self.good())
        self.assertTrue(ok, v)

    def test_each_requirement_fails_closed(self):
        def drop(recs, ids):
            recs[:] = [r for r in recs if r.get('toolCallId') not in ids]
        cases = {
            'compaction': lambda t, r, w, s, c: t[0].update(compactions=0),
            'cancel': lambda t, r, w, s, c: t[4].update(aborted=False),
            'resume': lambda t, r, w, s, c: t[5].update(error='oxa exited'),
            'subagent': lambda t, r, w, s, c: drop(r, {'s1'}),
            'large_read': lambda t, r, w, s, c: drop(r, {'r2'}),                       # half the file only
            'cargo_via_oxa': lambda t, r, w, s, c: r.__setitem__(5, self.en('b1', 'bash', 't2-bug', err=True, exit_code=101)),
            'unittest_via_oxa': lambda t, r, w, s, c: drop(r, {'b2'}),
            'second_client': lambda t, r, w, s, c: s['statuses'].append({'status': 500}),
            'no_error_storm': lambda t, r, w, s, c: w.append({'chat': True, 'status': 502, 'session_hdr': 'm'}),
            'checker_checker_tests': lambda t, r, w, s, c: c.update(checker_tests=(False, 'exit 101')),
            'checker_tests_unchanged': lambda t, r, w, s, c: c.update(tests_unchanged=(False, 'changed')),
            'main_requests': lambda t, r, w, s, c: w.clear(),
            'budget': lambda t, r, w, s, c: [x.update(wall_s=100000) for x in t],
        }
        for name, mutate in cases.items():
            with self.subTest(name):
                args = self.good()
                mutate(*args)
                ok, v = self.overall(args)
                self.assertFalse(ok)
                self.assertFalse(v[name], v)

    # Codex 8f5b591 controls (S/v020-interim-8f5b591-review-20261113/controls.py), adapted to evidence
    def test_codex_row_flags_alone_never_pass(self):
        tasks = [{'task': 't1-large-read', 'tool_calls': ['read', 'read'], 'compactions': 1},
                 {'task': 't2-bug', 'tool_calls': ['bash'], 'ran_cargo_test': True},
                 {'task': 't3-unittest', 'tool_calls': ['bash'], 'ran_unittest': True},
                 {'task': 't4-subagent', 'tool_calls': ['subagent']}, {'task': 't5-cancel', 'aborted': True}, {'task': 't6-resume'}]
        recs = [{'type': 'tool_execution_end', 'toolName': 'read', 'isError': True, 'result': {'content': [{'text': 'file missing'}]}}] * 2
        recs += [{'type': 'tool_execution_end', 'toolName': 'bash', 'isError': True, 'result': {'content': [{'text': 'cargo test failed'}]}}]
        v = ru.verdicts(tasks, recs, [], {'statuses': [{'status': 200, 'done': False}]},
                        {k: (True, '') for k in ('tests_unchanged', 'mutation_removed', 'diff_scoped', 'checker_tests')},
                        seal={'large_read_lines': 100})
        for k in ('large_read', 'cargo_via_oxa', 'unittest_via_oxa', 'subagent', 'main_requests', 'second_client', 'no_error_storm'):
            self.assertFalse(v[k][0], k)

    def test_codex_4948435_controls(self):
        T = ru.LARGE_READ
        args = self.good()                                   # echo instead of the commands
        args[1][4]['args']['command'] = 'echo cargo test'
        args[1][6]['args']['command'] = 'echo run_lab_tests'
        ok, v = self.overall(args)
        self.assertFalse(v['cargo_via_oxa'])
        self.assertFalse(v['unittest_via_oxa'])
        args = self.good()                                   # two one-line pages without a footer
        args[1][0]['args']['limit'] = 1
        args[1][1]['result']['content'][0]['text'] = 'one line'
        args[1][2]['args'].update(offset=2, limit=1)
        args[1][3]['result']['content'][0]['text'] = 'second line'
        self.assertFalse(self.overall(args)[1]['large_read'])
        args = self.good()                                   # over budget
        for t in args[0]:
            t['wall_s'] = 100000
        self.assertFalse(self.overall(args)[1]['budget'])

    def test_failed_test_output_is_not_success(self):
        args = self.good()
        args[1][5]['result']['content'][0]['text'] = 'test result: FAILED. 1 passed; 1 failed'
        self.assertFalse(self.overall(args)[1]['cargo_via_oxa'])
        args = self.good()
        del args[1][5]['result']['details']['exit']          # exit unreported = unproven
        self.assertFalse(self.overall(args)[1]['cargo_via_oxa'])

    def test_second_client_needs_completed_overlap(self):
        args = self.good()
        args[3]['statuses'] = [{'status': 200, 'done': True, 't0': 1000.0, 't1': 1001.0}]   # after the session
        self.assertFalse(self.overall(args)[1]['second_client'])
        args = self.good()
        args[3]['unjoined'] = True
        self.assertFalse(self.overall(args)[1]['second_client'])

    def test_read_coverage_from_continuation_notes(self):
        T = ru.LARGE_READ
        recs = [self.st('a', 'read', 't1', path=T, limit=100), self.en('a', 'read', 't1', text='z\n\n[200 more lines in file. Use offset=101 to continue.]')]
        self.assertAlmostEqual(ru.read_coverage(recs, 300), 100 / 300)
        recs += [self.st('b', 'read', 't1', path=T, offset=101), self.en('b', 'read', 't1', text='line\n' * 200)]
        self.assertEqual(ru.read_coverage(recs, 300), 1.0)
        short = [self.st('c', 'read', 't1', path=T), self.en('c', 'read', 't1', text='only\ntwo')]   # no footer, 2 lines returned
        self.assertAlmostEqual(ru.read_coverage(short, 300), 2 / 300)
        self.assertEqual(ru.read_coverage([self.st('c', 'read', 't1', path='other.rs'), self.en('c', 'read', 't1')], 300), 0.0)

    def test_tool_success_needs_the_matching_end(self):
        def start(i, tool, cmd=None):
            return {'type': 'tool_execution_start', 'toolCallId': i, 'toolName': tool, 'args': {'command': cmd} if cmd else {'path': 'x'}}

        def end(i, tool, err, exit_code='absent'):
            details = {} if exit_code == 'absent' else {'exit': exit_code}
            return {'type': 'tool_execution_end', 'toolCallId': i, 'toolName': tool, 'isError': err, 'result': {'details': details}}
        ok = [start('a', 'read'), end('a', 'read', False), start('b', 'bash', 'cargo test -p gguf'), end('b', 'bash', False, 0),
              start('c', 'bash', './tools/run_lab_tests.sh discover'), end('c', 'bash', False, 0)]
        ok[3]['result']['content'] = [{'type': 'text', 'text': 'test result: ok. 3 passed'}]
        ok[5]['result']['content'] = [{'type': 'text', 'text': 'Ran 3 tests in 0.1s\n\nOK'}]
        self.assertEqual(ru.tool_outcomes(ok), dict(ok_reads=1, ran_cargo_test=True, ran_unittest=True))
        negatives = {
            'start only': [start('b', 'bash', 'cargo test')],
            'error end': [start('b', 'bash', 'cargo test'), end('b', 'bash', True, 101)],
            'missing isError': [start('b', 'bash', 'cargo test'), {'type': 'tool_execution_end', 'toolCallId': 'b', 'toolName': 'bash'}],
            'nonzero exit': [start('b', 'bash', 'cargo test'), end('b', 'bash', False, 1)],
            'unlinked end': [start('b', 'bash', 'cargo test'), end('z', 'bash', False, 0)],
            'other command': [start('b', 'bash', 'echo cargo'), end('b', 'bash', False, 0)],
        }
        for name, recs in negatives.items():
            with self.subTest(name):
                self.assertFalse(ru.tool_outcomes(recs)['ran_cargo_test'])
        self.assertEqual(ru.tool_outcomes([start('a', 'read'), end('a', 'read', True)])['ok_reads'], 0)

    def test_counters_null_without_client_outcomes(self):
        m = ru.metrics([], [], [{'chat': True, 'wire_calls': [['a', 'args_not_json']], 'text_fallback': False}])
        self.assertIsNone(m['counters']['repaired'])
        self.assertEqual(m['counters']['malformed'], 1)

    def test_task_seal_is_stable_and_covers_the_mutation(self):
        self.assertEqual(ru.task_seal(), ru.task_seal())
        self.assertNotEqual(ru.MUTATION['fixed'], ru.MUTATION['broken'])

    def test_prepare_refuses_when_the_mutation_does_not_apply(self):
        with tempfile.TemporaryDirectory() as d:
            repo = Path(d, 'repo')
            (repo / 'engine/crates/gguf/src').mkdir(parents=True)
            (repo / ru.MUTATION['path']).write_text('fn other() {}\n')
            g = ['git', '-c', 'user.name=t', '-c', 'user.email=t@t']
            subprocess.run(['git', 'init', '-q', str(repo)], check=True)
            subprocess.run(g + ['-C', str(repo), 'add', '.'], check=True)
            subprocess.run(g + ['-C', str(repo), 'commit', '-qm', 'x'], check=True)
            sha = subprocess.run(['git', '-C', str(repo), 'rev-parse', 'HEAD'], capture_output=True, text=True).stdout.strip()
            work = Path(d, 'work')
            work.mkdir()
            with self.assertRaises(SystemExit):
                ru.prepare(repo, sha, work)


class SessionCleanupTests(unittest.TestCase):
    """Codex cleanup_control: an exception while the second client is in flight must not leave it
    running after session() returns; a client that cannot be stopped is a recorded survivor."""
    def run_session(self, cooperative):
        from types import SimpleNamespace
        started = threading.Event()
        release = threading.Event()
        orig_run, orig_rpc, orig_bwrap, orig_reap = ru.SecondClient.run, ru.oxl.Rpc, ru.outer_bwrap, ru.SecondClient.reap

        def held_run(self):
            started.set()
            (self.stop_ev if cooperative else release).wait(5)

        class FakeRpc:
            def __init__(self, *a, **k):
                pass

            def close(self):
                pass

            def send(self, cmd):
                pass

            def wait_idle(self):
                return []

            def run_prompt(self, text, hook):
                if text == ru.TASKS[1][1]:
                    assert started.wait(2)
                    raise RuntimeError('synthetic task error while the second client is in flight')
                return [], {'success': True}
        ru.SecondClient.run = held_run
        ru.oxl.Rpc = FakeRpc
        ru.outer_bwrap = lambda *a, **k: []
        ru.SecondClient.reap = lambda self, timeout=1: orig_reap(self, timeout)
        try:
            with tempfile.TemporaryDirectory() as d:
                work = Path(d)
                for sub in ('mono', 'home'):
                    (work / sub).mkdir()
                a = SimpleNamespace(soft_tokens=32000, base_url='http://127.0.0.1:18791', proxy_port=0, oxa_bin='/usr/bin/true',
                                    model='synthetic', budget_min=45, task_cap_s=900)
                tasks, recs, rows, second = ru.session(a, work, work / 'mono', work / 'home')
                alive = [t for t in threading.enumerate() if t.name.startswith('Thread') and t.is_alive() and t is not threading.current_thread()]
                return tasks, second
        finally:
            release.set()
            ru.SecondClient.run, ru.oxl.Rpc, ru.outer_bwrap, ru.SecondClient.reap = orig_run, orig_rpc, orig_bwrap, orig_reap

    def test_exception_path_reaps_the_second_client(self):
        tasks, second = self.run_session(cooperative=True)
        self.assertIn('synthetic task error', tasks[1]['error'])
        self.assertEqual(second['survivors'], [])
        self.assertFalse(second['unjoined'])

    def test_unstoppable_client_is_a_recorded_survivor(self):
        tasks, second = self.run_session(cooperative=False)
        self.assertTrue(second['unjoined'])
        self.assertTrue(second['survivors'])


class ProbeExitTests(unittest.TestCase):
    def test_cargo_failure_is_not_swallowed(self):
        with tempfile.TemporaryDirectory() as d:
            d = Path(d)
            (d / 'bin').mkdir()
            fake = d / 'bin' / 'cargo'
            fake.write_text('#!/bin/sh\nif [ "$1" = "--version" ]; then echo cargo-fake; exit 0; fi\necho synthetic-compile-error >&2; exit 9\n')
            fake.chmod(0o755)
            repo = d / 'mono'
            subprocess.run(['git', 'init', '-q', str(repo)], check=True)
            subprocess.run(['git', '-c', 'user.name=t', '-c', 'user.email=t@t', '-C', str(repo), 'commit', '-q', '--allow-empty', '-m', 'x'], check=True)
            import os
            env = dict(os.environ, PATH='%s:/usr/bin:/bin' % (d / 'bin'))
            r = subprocess.run(['sh', '-c', ru.probe_script(repo, d)], env=env, capture_output=True, text=True)
            self.assertEqual(r.returncode, 9)
            self.assertIn('synthetic-compile-error', r.stdout)


class SelfContainedCloneTests(unittest.TestCase):
    """Codex shared_clone_control: git must work INSIDE the outer sandbox (no alternates)."""
    @unittest.skipUnless(__import__('shutil').which('bwrap'), 'bwrap not installed')
    def test_git_log_inside_the_sandbox(self):
        with tempfile.TemporaryDirectory() as d:
            base = Path(d)
            src, work = base / 'source', base / 'work'
            src.mkdir()
            work.mkdir()
            for n in (ru.MUTATION['path'],) + ru.PROTECTED:
                f = src / n
                f.parent.mkdir(parents=True, exist_ok=True)
                f.write_text(ru.MUTATION['fixed'] if n == ru.MUTATION['path'] else 'control\n')
            g = ['git', '-c', 'user.name=t', '-c', 'user.email=t@t', '-C', str(src)]
            for args in (['init', '-q'], ['add', '.'], ['commit', '-qm', 'control']):
                subprocess.run(g + args, check=True)
            rev = subprocess.check_output(g + ['rev-parse', 'HEAD'], text=True).strip()
            mono, seal = ru.prepare(src, rev, work)
            self.assertFalse((mono / '.git/objects/info/alternates').exists())
            r = subprocess.run(ru.outer_bwrap(work) + ['--unshare-net', 'git', '-C', str(mono), 'log', '-1', '--format=%H'],
                               capture_output=True, text=True, timeout=30)
            if r.returncode != 0 and 'Operation not permitted' in r.stderr:
                self.skipTest('user namespaces unavailable here')
            self.assertEqual((r.returncode, r.stdout.strip()), (0, rev), r.stderr)


class ArmDeadlineTests(unittest.TestCase):
    """Codex c5a95b6 deadline_controls: the arm wall includes the independent checker, and the
    second client waits for headers up to its ABSOLUTE deadline (a short poll is not a failure)."""
    def test_checker_time_counts_against_arm_budget(self):
        good = RealUseVerdictTests.good(RealUseVerdictTests())
        for task in good[0]:
            task['wall_s'] = 1000 / 6
        clock = [0.0]
        seen = {}

        def session(*a):
            clock[0] += 1000
            return good[0], good[1], good[2], good[3]

        def check(work, timeout_s=1800):
            seen['timeout'] = timeout_s
            clock[0] += 1790
            return good[4]
        names = ('prepare', 'private_home', 'session', 'check')
        orig = [getattr(ru, n) for n in names] + [ru.time.time]
        try:
            ru.prepare = lambda repo, cand, work: (work / 'mono', {'large_read_lines': 300})
            ru.private_home = lambda w: w / 'home'
            ru.session, ru.check, ru.time.time = session, check, lambda: clock[0]
            with tempfile.TemporaryDirectory() as d:
                out = Path(d) / 'out'
                rc = ru.main(['--out', str(out), '--budget-min', '45'])
                rep = json.loads((out / 'real-use.json').read_text())
        finally:
            for n, f in zip(names, orig):
                setattr(ru, n, f)
            ru.time.time = orig[-1]
        self.assertEqual((rc, rep['verdict'], rep['wall_s']), (1, 'FAIL', 2790.0))
        self.assertFalse(rep['verdicts']['budget']['ok'])
        self.assertAlmostEqual(seen['timeout'], 45 * 60 - 1000)

    def test_arm_wall_verdict(self):
        good = RealUseVerdictTests.good(RealUseVerdictTests())
        seal = {'large_read_lines': RealUseVerdictTests.LINES}
        self.assertTrue(ru.verdicts(*good, seal=seal, budget_min=45, arm_wall_s=2600)['budget'][0])
        self.assertFalse(ru.verdicts(*good, seal=seal, budget_min=45, arm_wall_s=2701)['budget'][0])

    def test_checker_timeout_fails_closed(self):
        orig = (ru.subprocess.run, ru.private_home, ru.outer_bwrap)

        def fake_run(cmd, *a, **k):
            if cmd[0] == 'git':
                return subprocess.CompletedProcess(cmd, 0, '', '')
            raise subprocess.TimeoutExpired(cmd, k.get('timeout'))
        with tempfile.TemporaryDirectory() as d:
            work = Path(d)
            f = work / 'mono' / ru.MUTATION['path']
            f.parent.mkdir(parents=True)
            f.write_text('fixed\n')
            (work / 'seal.json').write_text(json.dumps({'protected': {}}))
            try:
                ru.subprocess.run, ru.private_home, ru.outer_bwrap = fake_run, lambda w: w / 'home', lambda w: ['true']
                out = ru.check(work, 5)
            finally:
                ru.subprocess.run, ru.private_home, ru.outer_bwrap = orig
        self.assertFalse(out['checker_tests'][0])
        self.assertIn('remaining arm budget', out['checker_tests'][1])

    def test_second_client_headers_within_absolute_deadline(self):
        import time as _t

        class H(http.server.BaseHTTPRequestHandler):
            delay = 0

            def log_message(self, *a):
                return

            def do_POST(self):
                self.rfile.read(int(self.headers.get('Content-Length', '0')))
                _t.sleep(self.delay)
                data = b'data: [DONE]\n\n'
                try:
                    self.send_response(200)
                    self.send_header('Content-Length', str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                except OSError:
                    pass
        orig = ru.SecondClient.READ_TIMEOUT_S
        try:
            ru.SecondClient.READ_TIMEOUT_S = .05
            for delay in (0, .2):
                H.delay = delay
                srv = http.server.ThreadingHTTPServer(('127.0.0.1', 0), H)
                th = threading.Thread(target=srv.serve_forever, daemon=True)
                th.start()
                rec = {'model': 'synthetic', 'statuses': []}
                sc = ru.SecondClient('http://127.0.0.1:%d' % srv.server_address[1], rec, stop_after=1, deadline_s=2).start()
                sc.thread.join(3)
                reaped = sc.reap(1)
                srv.shutdown()
                srv.server_close()
                th.join(1)
                st = rec['statuses'][0]
                self.assertEqual((st.get('status'), st.get('done'), reaped), (200, True, True), (delay, st))
        finally:
            ru.SecondClient.READ_TIMEOUT_S = orig

    def run_body_server(self, body_delay, deadline_s=2, data=b'data: [DONE]\n\n'):
        """Codex 363d2dc body_deadline_control: headers at once, body after `body_delay`."""
        import time as _t

        class H(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def log_message(self, *a):
                return

            def do_POST(self):
                self.rfile.read(int(self.headers.get('Content-Length', '0')))
                self.send_response(200)
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.flush()
                _t.sleep(body_delay)
                try:
                    self.wfile.write(data)
                    self.wfile.flush()
                except OSError:
                    pass
        orig = ru.SecondClient.READ_TIMEOUT_S
        srv = http.server.ThreadingHTTPServer(('127.0.0.1', 0), H)
        srv.daemon_threads = True
        th = threading.Thread(target=srv.serve_forever, daemon=True)
        th.start()
        try:
            ru.SecondClient.READ_TIMEOUT_S = .05
            rec = {'model': 'synthetic', 'statuses': []}
            t0 = _t.time()
            sc = ru.SecondClient('http://127.0.0.1:%d' % srv.server_address[1], rec, stop_after=1, deadline_s=deadline_s).start()
            sc.thread.join(deadline_s + 3)
            return rec['statuses'][0], sc.reap(1), _t.time() - t0
        finally:
            ru.SecondClient.READ_TIMEOUT_S = orig
            srv.shutdown()
            srv.server_close()
            th.join(1)

    def test_second_client_body_within_absolute_deadline(self):
        for delay in (0, .2):
            st, reaped, _ = self.run_body_server(delay)
            self.assertEqual((st.get('status'), st.get('done'), reaped), (200, True, True), (delay, st))
            self.assertNotIn('error', st)

    def test_second_client_body_past_deadline_is_cut_at_deadline(self):
        st, reaped, _ = self.run_body_server(5, deadline_s=0.5)
        self.assertEqual((st.get('status'), st.get('done'), st.get('cut'), reaped), (200, False, 'deadline', True), st)
        self.assertLess(st['t1'] - st['t0'], 1.0)   # cut at the 0.5 s absolute deadline, not the 5 s body

    def test_second_client_stop_interrupts_header_wait(self):
        import time as _t
        gate = threading.Event()

        class H(http.server.BaseHTTPRequestHandler):
            def log_message(self, *a):
                return

            def do_POST(self):
                self.rfile.read(int(self.headers.get('Content-Length', '0')))
                gate.wait(10)
        srv = http.server.ThreadingHTTPServer(('127.0.0.1', 0), H)
        srv.daemon_threads = True
        th = threading.Thread(target=srv.serve_forever, daemon=True)
        th.start()
        try:
            rec = {'model': 'synthetic', 'statuses': []}
            sc = ru.SecondClient('http://127.0.0.1:%d' % srv.server_address[1], rec, stop_after=1, deadline_s=30).start()
            _t.sleep(.3)
            t0 = _t.time()
            self.assertTrue(sc.reap(5))
            self.assertLess(_t.time() - t0, 5)
        finally:
            gate.set()
            srv.shutdown()
            srv.server_close()
            th.join(1)


class R3FixtureTests(unittest.TestCase):
    """Codex 71abfd4 interim: (3) the independent checker must not use the host sccache (SUN_LEN
    overflow under a long scratch path); (4) a 0-compaction arm says whether the workload reached
    softTokens (fixture) or oxa skipped a due compaction (product finding)."""
    def test_checker_runs_without_sccache(self):
        seen = {}
        orig = (ru.subprocess.run, ru.private_home, ru.outer_bwrap, ru.os.environ.copy())

        def fake_run(cmd, *a, **k):
            if cmd[0] == 'git':
                return subprocess.CompletedProcess(cmd, 0, '', '')
            seen['env'] = dict(k['env'])
            return subprocess.CompletedProcess(cmd, 0, 'test result: ok', '')
        with tempfile.TemporaryDirectory() as d:
            work = Path(d)
            f = work / 'mono' / ru.MUTATION['path']
            f.parent.mkdir(parents=True)
            f.write_text('fixed\n')
            (work / 'seal.json').write_text(json.dumps({'protected': {}}))
            try:
                ru.os.environ.update(SCCACHE_DIR='/x', SCCACHE_SERVER_UDS='/x/s', RUSTC_WRAPPER='sccache')
                ru.subprocess.run, ru.private_home, ru.outer_bwrap = fake_run, lambda w: w / 'home', lambda w: ['true']
                out = ru.check(work, 5)
            finally:
                ru.subprocess.run, ru.private_home, ru.outer_bwrap = orig[:3]
                ru.os.environ.clear()
                ru.os.environ.update(orig[3])
        self.assertTrue(out['checker_tests'][0])
        self.assertEqual(seen['env']['RUSTC_WRAPPER'], '')
        self.assertFalse([k for k in seen['env'] if k.startswith('SCCACHE_')])

    @staticmethod
    def turns(*totals):
        return [{'type': 'message_end', 'message': {'role': 'assistant', 'usage': {'totalTokens': t}}} for t in totals]

    def compaction(self, recs, comps=0, soft=32000):
        good = RealUseVerdictTests.good(RealUseVerdictTests())
        tasks = [dict(t, compactions=0) for t in good[0]]
        tasks[0]['compactions'] = comps
        v = ru.verdicts(tasks, good[1] + recs, *good[2:], seal={'large_read_lines': RealUseVerdictTests.LINES},
                        soft_tokens=soft)
        return v['compaction']

    def test_compaction_not_reached_is_fixture(self):
        ok, why = self.compaction(self.turns(2620, 30667, 30914))   # the 71abfd4 arm's numbers
        self.assertFalse(ok)
        self.assertIn('never reached softTokens (peak context 30914 < 32000): fixture', why)

    def test_compaction_due_but_absent_is_oxa_finding(self):
        ok, why = self.compaction(self.turns(20000, 41000))
        self.assertFalse(ok)
        self.assertIn('oxa finding', why)

    def test_compaction_observed_passes(self):
        self.assertTrue(self.compaction(self.turns(41000), comps=1)[0])

    def test_no_usage_is_reported_not_guessed(self):
        ok, why = self.compaction([])
        self.assertFalse(ok)
        self.assertIn('peak context None', why)

    def test_task_v2_forbids_read_shortcuts(self):
        self.assertEqual(ru.TASK_VERSION, 2)
        self.assertIn('Use ONLY the read tool', dict(ru.TASKS)['t1-large-read'])


if __name__ == '__main__':
    unittest.main()
