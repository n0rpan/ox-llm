"""Pre-review fixes: each defect has a control. Model-free. The controls for defects 5, 6, 8 and 9
(lab-only A/B and fixture tooling) are tested with that tooling."""
import importlib.util
from pathlib import Path
import sys
import unittest

LAB = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(LAB / 'tools/interim')]
import soak_slope as ss  # noqa: E402


def load(name, rel):
    spec = importlib.util.spec_from_file_location(name, LAB / rel)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


class HopProbeEnvTests(unittest.TestCase):          # defect 1/2
    def test_tee_vars_survive_isolation_and_brave_does_not(self):
        loop = load('oxa_tiny_loop', 'tools/tiny_real/oxa_tiny_loop.py')
        base = {'OXA_REAL': '/x/oxa', 'HOP_RPC_OUT': '/x/rpc.jsonl', 'OXA_PROFILE': 'leak', 'BRAVE_SEARCH_API_KEY': 'placeholder',
                'PATH': '/usr/bin', 'XDG_CONFIG_HOME': '/real'}
        env = loop.client_env(base, Path('/h'), Path('/h/a'), Path('/h/s'), 'http://127.0.0.1:1')
        self.assertEqual((env['OXA_REAL'], env['HOP_RPC_OUT']), ('/x/oxa', '/x/rpc.jsonl'))
        self.assertNotIn('OXA_PROFILE', env)
        self.assertNotIn('BRAVE_SEARCH_API_KEY', env)
        self.assertEqual(env['XDG_CONFIG_HOME'], '/h/.config')

    def test_gate_script_unsets_brave_and_keeps_hop_probe(self):
        text = (LAB.parent / 'tools/gates/tiny_real.sh').read_text()
        self.assertIn('BRAVE_SEARCH_API_KEY', [w for line in text.splitlines() if line.startswith('unset ') for w in line.split()])
        self.assertIn('HOP_PROBE', text)
        self.assertIn('8093', text)  # production-port refusal covers the engine port too


class KeepWarmIdleTests(unittest.TestCase):          # defect 3
    def rows(self, step=5, **kw):
        return [dict(dict(t=float(t), pids=dict(engine=7), loaded=True, reloads=0), **kw) for t in range(0, 620, step)]

    def test_dense_complete_passes(self):
        self.assertEqual(ss.keepwarm_idle(self.rows())[0], 'PASS')

    def test_missing_reload_counter_is_incomplete(self):
        rows = [{'t': r['t'], 'pids': r['pids'], 'loaded': True} for r in self.rows()]
        self.assertEqual(ss.keepwarm_idle(rows)[0], 'INCOMPLETE')
        self.assertEqual(ss.keepwarm_idle([{'t': 0, 'pids': {'engine': 1}, 'loaded': True},
                                           {'t': 700, 'pids': {'engine': 1}, 'loaded': True}])[0], 'INCOMPLETE')

    def test_sparse_sampling_is_incomplete(self):
        self.assertEqual(ss.keepwarm_idle(self.rows(step=100))[0], 'INCOMPLETE')


class SoakFailClosedTests(unittest.TestCase):        # defect 4
    def samples(self, pss_hole=False, drop_pid=False):
        out = []
        for i in range(8):
            for j in range(5):
                t = i * 600 + j * 5
                pss = {'engine': None if pss_hole and (i * 4 + j) % 2 == 0 else 9e9, 'controller': 1e8, 'oxa': 1e8}
                pids = {'engine': 1, 'controller': 2, 'oxa': 3}
                if drop_pid and i == 5:
                    pids.pop('controller')
                out.append(dict(t=t, anchor='A', pids=pids, rss={'engine': 1e10, 'controller': 2e8, 'oxa': 1e8}, pss=pss))
            out.append(dict(t=i * 600 + 25, anchor=None, pids=out[-1]['pids'], rss=out[-1]['rss'], pss=out[-1]['pss']))
        return out

    def test_control_passes(self):
        self.assertEqual(ss.analyse(self.samples())['verdict'], 'PASS')

    def test_partial_pss_is_incomplete(self):
        r = ss.analyse(self.samples(pss_hole=True))
        self.assertEqual(r['verdict'], 'INCOMPLETE')
        self.assertIn('INCOMPLETE:missing PSS', r['reasons'])

    def test_vanished_pid_key_fails_same_pid(self):
        ok, why = ss.same_pid(self.samples(drop_pid=True))
        self.assertFalse(ok)
        self.assertEqual(ss.analyse(self.samples(drop_pid=True))['verdict'], 'INCOMPLETE')


class XreviewFollowupTests(unittest.TestCase):
    def test_rss_pss_component_asymmetry_is_incomplete(self):
        import copy
        sys.path.insert(0, str(LAB / 'tools/interim'))
        import rehearse_interim as ri
        s = copy.deepcopy(ri.soak())
        s[0]['pss']['extra'] = 1.0
        self.assertEqual(ss.analyse(s)['verdict'], 'INCOMPLETE')


if __name__ == '__main__':
    unittest.main()
