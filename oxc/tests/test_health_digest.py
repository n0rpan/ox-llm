"""v0.1.4 daily health digest (OEM-DONE §2(d)): numbers only, organic rows only, alerts, soak.

Synthetic ledger files in a temp dir; never the real ~/.local/state/oxc (P56).
"""
import datetime as dt
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import tempfile
import time
import unittest
from contextlib import redirect_stdout
from unittest.mock import patch

from oxc.runtime import health_digest as hd

SCRIPT = Path(__file__).resolve().parents[1] / 'src/oxc/runtime/oxc'
SECRET = 'SECRET-PROMPT-TEXT'
DAY = dt.date(2026, 11, 10)


def row(**extra):
    base = {'id': 'abc', 't': time.mktime(DAY.timetuple()) + 3600, 'path': '/v1/chat/completions', 'origin': 'organic',
            'status': 200, 'error': None, 'completeness': 'complete', 'cache_n': 1000, 'prompt_n': 200,
            'prompt_ms': 1000.0, 'predicted_n': 100, 'predicted_ms': 5000.0, 'ttfb_ms': 900.0, 'wall_ms': 6000.0,
            'session': 'a' * 16, 'history_change': 'append', 'previous_reply': 'present', 'miss_class': 'append-ok'}
    base.update(extra)
    if 'ctx_tokens' not in extra and base.get('cache_n') is not None and base.get('prompt_n') is not None:
        base['cached_tokens'], base['ctx_tokens'] = base['cache_n'], base['cache_n'] + base['prompt_n']
    return base


def write_ledger(directory, day, rows):
    directory.mkdir(parents=True, exist_ok=True)
    with open(directory / f'requests-{day:%Y%m%d}.jsonl', 'w') as stream:
        for item in rows:
            stream.write(json.dumps(item) + '\n')


class Compute(unittest.TestCase):
    def test_lab_and_gate_rows_are_excluded_remote_kept(self):
        d = hd.compute(DAY, [row(), row(origin='lab'), row(origin='gate'), row(origin='remote'), row(origin=None)])
        self.assertEqual(d['requests'], 3)
        self.assertEqual(d['by_origin'], {'organic': 2, 'excluded': 2, 'remote': 1})

    def test_counts_cache_history_errors(self):
        rows = [row(), row(miss_class='retry'), row(history_change='same', previous_reply=None, miss_class=None),
                row(miss_class='unknown'), row(turn_kind='compaction', wall_ms=40000.0),
                row(history_change='truncated', previous_reply=None, miss_class='history-rewrite',
                    history_rewrite=True, fold_suspect=True, rewrite_class='ws-trailing'),
                row(history_change='edited', previous_reply=None, miss_class='compaction'),  # pre-v0.1.4 row
                row(queue_wait_ms=1500.0, queue_pos=1, session_switch=True, replay='cut-served'),
                row(status=502, error='backend_died', completeness='errored'),
                row(status=200, error=None, completeness='incomplete'),
                row(status=200, error='client_disconnected', completeness='incomplete'),
                row(resume_ms=1200.0, resume_src='disk')]
        d = hd.apply_alerts(hd.compute(DAY, rows))
        self.assertEqual(d['retries'], 1)
        self.assertEqual(d['duplicate_bodies'], 1)
        self.assertEqual(d['cache']['unknown_misses'], 1)
        self.assertEqual(d['history']['history_rewrites'], 2)
        self.assertEqual(d['history']['fold_suspects'], 1)
        self.assertEqual(d['history']['rewrite_classes'], {'ws-trailing': 1})
        self.assertEqual(d['history']['client_compactions'], 1)
        self.assertEqual(d['history']['compaction_wall_ms_median'], 40000.0)
        self.assertEqual(d['cache']['miss_classes'].get('history-rewrite'), 1)
        self.assertEqual(d['queue']['queued'], 1)
        self.assertEqual(d['queue']['session_switches'], 1)
        self.assertEqual(d['queue']['wait_ms']['p95'], 1500.0)
        self.assertEqual(d['queue']['replays'], {'cut-served': 1})
        self.assertEqual(d['errors']['http_5xx'], 1)
        self.assertEqual(d['errors']['stream_cut'], 1)  # a client cancel is not a cut stream
        self.assertEqual(d['errors']['by_label']['backend_died'], 1)
        self.assertEqual(d['latency']['restore_ms']['p95'], 1200.0)
        ids = {a['id'] for a in d['alerts']}
        self.assertTrue({'client_retries', 'duplicate_prefill', 'http_5xx', 'stream_cut', 'engine_death',
                         'unknown_cache_misses'} <= ids, ids)
        self.assertFalse(d['green'])

    def test_bands_and_rates(self):
        rows = [row(cache_n=40000, prompt_n=500, prompt_ms=2000.0, predicted_n=200, predicted_ms=10000.0)] * 6
        d = hd.compute(DAY, rows)
        self.assertEqual(d['series']['unknown']['decode_tok_s']['32-64k'], {'p50': 20.0, 'n': 6})
        self.assertEqual(d['series']['unknown']['append_prefill_tok_s']['32-64k'], {'p50': 250.0, 'n': 6})
        self.assertEqual(d['cache']['prefix_hit_rate'], 1.0)
        self.assertAlmostEqual(d['cache']['token_hit_rate'], 40000 / 40500, places=3)

    def test_privacy_no_text_ever_leaves(self):
        rows = [row(error=SECRET, miss_class=SECRET, rewrite_class=SECRET, replay=SECRET, turn_kind=SECRET, origin='organic',
                    prompt=SECRET, messages=[{'role': 'user', 'content': SECRET}], session=SECRET,
                    agent=SECRET, client=SECRET, model=SECRET + ' /x', mode=SECRET, path='/v1/chat/completions?' + SECRET)]
        events = [{'event': 'engine_died', 't': 1, 'guard_failure': SECRET + ' /home/x', 'model_alias': SECRET}]
        d = hd.apply_alerts(hd.compute(DAY, rows, events, store={'used_bytes': 1, 'budget_bytes': 2, 'path': SECRET}))
        blob = json.dumps(d) + json.dumps(hd.status_line(d)) + hd.text(d)
        self.assertNotIn(SECRET, blob)
        self.assertNotIn('/home', blob)
        self.assertEqual(d['errors']['by_label'], {'other': 1})
        self.assertEqual(d['guard']['by_group']['other'], 1)

    def test_guard_groups_from_engine_events(self):
        events = [{'event': 'engine_died', 't': 1, 'guard_failure': 'host_memory_pressure_sustained'},
                  {'event': 'engine_died', 't': 1, 'guard_failure': 'cgroup_oom'},
                  {'event': 'engine_died', 't': 1, 'guard_failure': 'own_swap_nonzero'},
                  {'event': 'engine_died', 't': 1}]
        d = hd.apply_alerts(hd.compute(DAY, [row()], events))
        self.assertEqual(d['guard']['trips'], 3)
        self.assertEqual(d['guard']['engine_deaths'], 4)
        self.assertEqual((d['guard']['by_group']['psi'], d['guard']['by_group']['oom'], d['guard']['by_group']['swap']),
                         (1, 1, 1))
        ids = {a['id'] for a in d['alerts']}
        self.assertTrue({'guard_trip', 'scope_oom', 'swap', 'engine_death'} <= ids, ids)

    def test_generic_labels_only_fold_suspect_is_counted_not_alerted(self):
        # O-133/O-135: no client swallow labels; a fold suspect is a known-issue count, not an alert.
        d = hd.apply_alerts(hd.compute(DAY, [row(), row(fold_suspect=True, history_change='initial',
                                                        previous_reply=None, miss_class=None),
                                             row(checkpoint_turn='would_restore')]))
        self.assertEqual(d['history']['fold_suspects'], 1)
        self.assertEqual(d['alerts'], [])
        self.assertNotIn('compaction', d)
        self.assertNotIn('would_restore', json.dumps(d))

    def test_thresholds(self):
        d = hd.apply_alerts(hd.compute(DAY, [row(resume_ms=6000.0, resume_src='disk')] +
                                       [row(cache_n=0, miss_class='history-edit')] * 3,
                                       store={'used_bytes': 95, 'budget_bytes': 100}))
        ids = {a['id'] for a in d['alerts']}
        self.assertTrue({'restore_slow', 'prefix_hits_low', 'store_full'} <= ids, ids)
        green = hd.apply_alerts(hd.compute(DAY, [row()] * 20, store={'used_bytes': 10, 'budget_bytes': 100}))
        self.assertEqual(green['alerts'], [])
        self.assertTrue(green['green'])

    def test_trend_alerts_need_history(self):
        slow = [row(predicted_ms=10000.0)] * 6   # 10 tok/s
        fast = hd.compute(DAY, [row()] * 6)       # 20 tok/s
        alone = hd.apply_alerts(hd.compute(DAY, slow))
        self.assertEqual(alone['alerts'], [])
        trend = hd.apply_alerts(hd.compute(DAY, slow), [fast, fast, fast])
        self.assertIn('decode_slower:unknown:0-8k', {a['id'] for a in trend['alerts']})
        self.assertEqual(trend['trend_basis_days'], 3)
        ttft = hd.apply_alerts(hd.compute(DAY, [row(ttfb_ms=5000.0)] * 6), [fast, fast, fast])
        self.assertIn('ttft_hit_slower:unknown', {a['id'] for a in ttft['alerts']})

    def test_two_models_never_mix_in_trends(self):
        # A day that switches from a fast model to a slow one is no regression of either.
        fast = [row(model='qwen3.6-35b-a3b-local', mode='fast')] * 6                       # 20 tok/s
        slow = [row(model='ornith-35b-a3b-local', mode='deep', predicted_ms=10000.0, ttfb_ms=5000.0)] * 6  # 10 tok/s
        past = [hd.compute(DAY, fast + slow)] * 3
        today = hd.apply_alerts(hd.compute(DAY, slow + [row(model='qwen3.6-35b-a3b-local', mode='fast')] * 6), past)
        self.assertEqual(today['alerts'], [])
        self.assertEqual(set(today['series']), {'qwen3.6-35b-a3b-local/fast', 'ornith-35b-a3b-local/deep'})
        self.assertEqual(today['series']['ornith-35b-a3b-local/deep']['decode_tok_s']['0-8k']['p50'], 10.0)
        self.assertEqual(today['series']['qwen3.6-35b-a3b-local/fast']['decode_tok_s']['0-8k']['p50'], 20.0)
        # Only-slow-model yesterdays vs a fast-model day: no alert either (no basis for that series).
        alone = hd.apply_alerts(hd.compute(DAY, slow), [hd.compute(DAY, fast)] * 3)
        self.assertEqual(alone['alerts'], [])
        # A real regression of one model is caught, named by its series.
        worse = [row(model='qwen3.6-35b-a3b-local', mode='fast', predicted_ms=10000.0)] * 6
        hit = hd.apply_alerts(hd.compute(DAY, worse + slow), past)
        self.assertEqual({a['id'] for a in hit['alerts']}, {'decode_slower:qwen3.6-35b-a3b-local/fast:0-8k'})
        # The same model in another mode is its own series.
        other_mode = hd.apply_alerts(hd.compute(DAY, [row(model='qwen3.6-35b-a3b-local', mode='deep',
                                                          predicted_ms=10000.0)] * 6), past)
        self.assertEqual(other_mode['alerts'], [])


class Files(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.ledger, self.out, self.log = root / 'ledger', root / 'digest', root / 'engine-stderr.log'
        self.env = patch.dict(os.environ, {'OXC_LEDGER_DIR': str(self.ledger), 'OXC_DIGEST_DIR': str(self.out),
                                           'OXC_ENGINE_LOG': str(self.log)})
        self.env.start()

    def tearDown(self):
        self.env.stop()
        self.tmp.cleanup()

    def days(self, last, n, rows):
        for back in range(n):
            write_ledger(self.ledger, last - dt.timedelta(days=back), rows)

    def test_engine_events_by_day_and_undated(self):
        start = time.mktime(DAY.timetuple())
        self.log.write_text('plain stderr line\n' + '\n'.join(json.dumps(e) for e in (
            {'event': 'engine_died', 't': start + 10}, {'event': 'engine_died', 't': start - 10},
            {'event': 'engine_died'}, {'event': 'request_end', 't': start + 5})) + '\n')
        events, undated = hd.engine_events(DAY)
        self.assertEqual((len(events), undated), (1, 1))

    def test_write_day_private_files_and_status_contract(self):
        write_ledger(self.ledger, DAY, [row()] * 5)
        digester = hd.Digester(today=DAY + dt.timedelta(days=1))
        value, status = hd.write_day(digester, DAY)
        self.assertEqual(stat.S_IMODE(self.out.stat().st_mode), 0o700)
        for name in (f'digest-{DAY:%Y%m%d}.json', 'status.json'):
            self.assertEqual(stat.S_IMODE((self.out / name).stat().st_mode), 0o600)
        stored = json.loads((self.out / 'status.json').read_text())
        self.assertEqual(set(stored), {'schema', 'day', 'state', 'line', 'alerts', 'alert_text', 'requests',
                                       'generated_at', 'soak'})
        self.assertEqual((stored['schema'], stored['state'], stored['line'], stored['requests']),
                         ('local-ai-digest-status/1', 'healthy', 'Healthy', 5))
        self.assertEqual(set(stored['soak']), {'green_days', 'ship_ready', 'pass', 'requests', 'history_rewrites'})
        self.assertEqual(hd.read_status(now=value['generated_at'] + 60)['state'], 'healthy')
        self.assertEqual(hd.read_status(now=value['generated_at'] + 40 * 3600)['state'], 'stale')
        self.assertEqual(hd.stored(DAY)['requests'], 5)

    def test_read_status_without_file(self):
        self.assertEqual(hd.read_status()['state'], 'no_data')

    def test_soak_pass_idle_skip_and_break(self):
        last = DAY
        good = [row()] * 45 + [row(history_change='truncated', previous_reply=None, miss_class='history-rewrite')] * 2
        self.days(last, 8, good)
        write_ledger(self.ledger, last - dt.timedelta(days=3), [])  # an idle day: neither counts nor breaks
        value = hd.soak(hd.Digester(today=last + dt.timedelta(days=1)), last)
        self.assertEqual(value['green_days'], 7)
        self.assertTrue(value['pass'], value)
        self.assertNotIn((last - dt.timedelta(days=3)).isoformat(), value['days'])
        write_ledger(self.ledger, last - dt.timedelta(days=1), good + [row(status=500, error='local_controller_error')])
        broken = hd.soak(hd.Digester(today=last + dt.timedelta(days=1)), last)
        self.assertEqual(broken['green_days'], 1)
        self.assertFalse(broken['pass'])
        self.assertFalse(broken['ship_ready'])

    def test_soak_needs_volume(self):
        self.days(DAY, 7, [row()] * 3)
        value = hd.soak(hd.Digester(today=DAY + dt.timedelta(days=1)), DAY)
        self.assertEqual(value['green_days'], 7)
        self.assertTrue(value['ship_ready'])
        self.assertFalse(value['pass'])  # 21 requests, 0 history rewrites

    def test_cli_digest_json_status_and_soak(self):
        loader = importlib.machinery.SourceFileLoader('digest_oxc', str(SCRIPT))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        cli = importlib.util.module_from_spec(spec)
        loader.exec_module(cli)
        write_ledger(self.ledger, DAY, [row()] * 4 + [row(origin='lab')])
        with patch.object(cli, 'cache_status', return_value={'cache_available': False}):
            out = io.StringIO()
            with redirect_stdout(out):
                self.assertEqual(cli.main(['digest', '--day', DAY.isoformat(), '--json']), 0)
            value = json.loads(out.getvalue())
            self.assertEqual((value['schema'], value['requests']), ('local-ai-digest/1', 4))
            out = io.StringIO()
            with redirect_stdout(out):
                self.assertEqual(cli.main(['digest', '--write', '--day', DAY.isoformat()]), 0)
            self.assertTrue((self.out / 'status.json').is_file())
            out = io.StringIO()
            with redirect_stdout(out):
                self.assertEqual(cli.main(['digest', '--status', '--json']), 0)
            self.assertEqual(json.loads(out.getvalue())['schema'], 'local-ai-digest-status/1')
            out = io.StringIO()
            with redirect_stdout(out):
                self.assertEqual(cli.main(['digest', '--soak', '--day', DAY.isoformat()]), 0)
            self.assertIn('green days', out.getvalue())
            with redirect_stdout(io.StringIO()), patch('sys.stderr', io.StringIO()):
                self.assertEqual(cli.main(['digest', '--day', 'not-a-day']), 2)


class InstalledVersions(unittest.TestCase):
    """v0.2.0: the digest reports the installed coding agent's version (temp HOME only, P56)."""

    def agent(self, home, script):
        path = Path(home) / '.local/bin/oxa'
        path.parent.mkdir(parents=True)
        path.write_text('#!/bin/sh\n' + script)
        path.chmod(0o700)

    def test_absent_unknown_and_version(self):
        with tempfile.TemporaryDirectory() as home:
            self.assertEqual(hd.installed_versions(home), {'oxa': 'absent'})
            self.agent(home, 'echo "oxa 0.2.0"\n')
            self.assertEqual(hd.installed_versions(home), {'oxa': '0.2.0'})
        with tempfile.TemporaryDirectory() as home:
            self.agent(home, 'echo "oxa /home/user/secret build"\nexit 0\n')
            self.assertEqual(hd.installed_versions(home), {'oxa': 'unknown'})
        with tempfile.TemporaryDirectory() as home:
            self.agent(home, 'echo 0.2.0; exit 3\n')
            self.assertEqual(hd.installed_versions(home), {'oxa': 'unknown'})

    def test_timeout_is_bounded_and_never_raises(self):
        seen = {}

        def run(args, **kwargs):
            seen.update(kwargs)
            raise __import__('subprocess').TimeoutExpired(args, kwargs['timeout'])
        with tempfile.TemporaryDirectory() as home:
            self.agent(home, 'sleep 60\n')
            self.assertEqual(hd.installed_versions(home, run=run), {'oxa': 'unknown'})
        self.assertEqual(seen['timeout'], 5)

    def test_digest_carries_only_version_tokens(self):
        d = hd.compute(DAY, [row()], versions={'oxa': '0.2.0'})
        self.assertEqual(d['versions'], {'oxa': '0.2.0'})
        self.assertIn('installed: oxa 0.2.0', hd.text(d))
        self.assertEqual(hd.compute(DAY, [row()], versions={'oxa': SECRET})['versions'], {'oxa': 'unknown'})
        self.assertIsNone(hd.compute(DAY, [row()])['versions'])
        digester = hd.Digester(ledger=Path('/nonexistent-ledger'), today=DAY, versions={'oxa': '0.2.0'})
        self.assertEqual(digester.day(DAY)['versions'], {'oxa': '0.2.0'})
        self.assertIsNone(digester.day(DAY - dt.timedelta(days=5))['versions'])


if __name__ == '__main__':
    unittest.main()
