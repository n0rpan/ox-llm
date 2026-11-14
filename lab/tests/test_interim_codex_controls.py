"""Codex's independent controls on c247c3b (S/v020-interim-c247c3b-review-20261113/controls.py),
as regression tests: every missing / partial / invalid observation fails closed. Model-free.
The start-up controls of the lab-only A/B driver are tested with that driver."""
import copy
from pathlib import Path
import sys
import unittest

LAB = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(LAB / 'tools/interim')]
import rehearse_interim as ri  # noqa: E402
import soak_slope as ss  # noqa: E402
import thermal_aa as th  # noqa: E402
import tool_counters as tc  # noqa: E402


def verdict(s):
    return ss.analyse(s)['verdict']


class SoakControls(unittest.TestCase):
    def test_positive_complete(self):
        self.assertEqual(verdict(ri.soak()), 'PASS')

    def test_missing_all_controller_oxa_observations(self):
        s = copy.deepcopy(ri.soak())
        for r in s:
            for key in ('pids', 'rss', 'pss'):
                r[key] = {k: v for k, v in r[key].items() if k == 'engine'}
        self.assertEqual(verdict(s), 'INCOMPLETE')

    def test_one_missing_anchor_pss_sample(self):
        s = copy.deepcopy(ri.soak())
        s[0]['pss']['engine'] = None
        self.assertEqual(verdict(s), 'INCOMPLETE')

    def test_single_sample_anchor_no_20s_dwell(self):
        s = ri.soak()
        self.assertEqual(verdict([r for i, r in enumerate(s) if i % 5 in (0, 4)]), 'INCOMPLETE')

    def test_non_finite_and_non_monotonic(self):
        s = copy.deepcopy(ri.soak())
        s[3]['rss']['oxa'] = float('nan')
        self.assertEqual(verdict(s), 'INCOMPLETE')
        s = copy.deepcopy(ri.soak())
        s[2]['t'] = s[1]['t']
        self.assertEqual(verdict(s), 'INCOMPLETE')


class BoundaryControls(unittest.TestCase):
    """Codex 20:49 boundary controls (S/v020-interim-73e3a4b-review-20261113/boundary_controls.py)."""
    def test_15_second_anchor_dwell_is_incomplete(self):
        rows = []
        for n in range(5):
            for d in (0, 5, 10, 15):
                rows.append(dict(t=n * 600 + d, anchor='same', pids={'engine': 1, 'controller': 2, 'oxa': 3},
                                 rss={k: 100 for k in ('engine', 'controller', 'oxa')}, pss={k: 100 for k in ('engine', 'controller', 'oxa')}))
            rows.append(dict(rows[-1], t=n * 600 + 20, anchor=None))
        self.assertEqual(ss.analyse(rows)['verdict'], 'INCOMPLETE')

    def test_nan_bound_skin_is_incomplete(self):
        s = ri.series(0, [48.0] * 32)
        for x in s:
            x['skin'] = float('nan')
        self.assertEqual(th.start_ok(s, {'soc': 48.0, 'skin': 30.0}, True)[0], 'INCOMPLETE')
        for x in s:
            x['skin'] = 30.5
        self.assertEqual(th.start_ok(s, {'soc': 48.0, 'skin': 30.0}, True)[0], 'PASS')


class KeepWarmControls(unittest.TestCase):
    def test_no_reload_counter_and_two_samples(self):
        idle = [dict(t=t, pids={'engine': 4242}, loaded=True) for t in range(0, 615, 5)]
        self.assertEqual(ss.keepwarm_idle(idle)[0], 'INCOMPLETE')
        self.assertEqual(ss.keepwarm_idle([dict(idle[0], reloads=0), dict(idle[-1], reloads=0)])[0], 'INCOMPLETE')


class ThermalControls(unittest.TestCase):
    def test_positive_regular(self):
        self.assertEqual(th.start_ok(ri.series(0, [48.0] * 32), {'soc': 48.0})[0], 'PASS')

    def test_nan_values_and_reference(self):
        nan = ri.series(0, [float('nan')] * 32)
        self.assertEqual(th.start_ok(nan, {'soc': 48.0})[0], 'INCOMPLETE')
        self.assertEqual(th.start_ok(ri.series(0, [48.0] * 32), {'soc': float('nan')})[0], 'INCOMPLETE')

    def test_duplicate_and_gapped_timestamps(self):
        dup = [dict(t=0, soc=48.0)] + [dict(t=30, soc=48.0) for _ in range(30)]
        self.assertEqual(th.start_ok(dup, {'soc': 48.0})[0], 'INCOMPLETE')
        gap = [dict(t=0, soc=48.0)] + [dict(t=30 + i / 100, soc=48.0) for i in range(30)]
        self.assertEqual(th.start_ok(gap, {'soc': 48.0})[0], 'INCOMPLETE')

    def test_aa_rejects_non_finite(self):
        self.assertEqual(th.aa_check([(10.0, float('nan')), (10.0, 10.0), (10.0, 10.0)])[0], 'INCOMPLETE')


class CounterControls(unittest.TestCase):
    def test_missing_client_events_repaired_null(self):
        wire = ri.wire({'tool_calls': [ri.call('bad', 'read', '{"path": "x",}')]})
        self.assertIsNone(tc.count(wire, tc.oxa_outcomes([]))['repaired'])

    def test_missing_isError_tool_failure_null(self):
        wire = ri.wire({'tool_calls': [ri.call('valid', 'read', {'path': 'x'})]})
        self.assertIsNone(tc.count(wire, tc.oxa_outcomes([{'type': 'tool_execution_end', 'toolCallId': 'valid'}]))['legit_tool_failures'])

    def test_mixed_validity_has_tool_markup_in_text(self):
        self.assertFalse(tc.classify_response(ri.TOOLS, {'tool_calls': [ri.call('a', 'read', {'path': 'x'})],
                                                         'content': '<tool_call>bad</tool_call>'})[1])

    def test_successfully_repaired_call_does_not_create_error_turn(self):
        cnt = tc.count(ri.wire({'tool_calls': [ri.call('r', 'read', '{"path": "x",}')]}, {'content': 'done'}), {'r': True})
        self.assertEqual(cnt['repair_turns'], 0)

    def test_error_result_creates_a_repair_turn(self):
        cnt = tc.count(ri.wire({'tool_calls': [ri.call('r', 'read', '{"path": "x",}')]}, {'content': 'retry'}), {'r': False})
        self.assertEqual((cnt['repair_turns'], cnt['repaired']), (1, 0))


if __name__ == '__main__':
    unittest.main()
