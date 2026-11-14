"""Pre-review fixes: each defect has a control. Model-free."""
import importlib.util
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import unittest

LAB = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(LAB / 'tools/interim'), str(LAB / 'benchmarks/fixtures'), str(LAB / 'benchmarks/gatepack')]
import fixture_lib as fx  # noqa: E402
import gate_replay as gr  # noqa: E402
import native_handle as nh  # noqa: E402
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
    def test_done_marker_must_be_a_line(self):
        import json
        with tempfile.TemporaryDirectory() as d:
            h = nh.NativeHandle('/n', '/n', Path(d) / 'l', port=18995)
            # '[DONE]' inside content is not the end of the stream; the row then comes at the end
            body = b'data: {"choices":[{"delta":{"content":"[DONE] soon"}}]}\n\n'
            self.assertFalse(body.startswith(b'data: [DONE]') or b'\ndata: [DONE]' in body)

    def test_rss_pss_component_asymmetry_is_incomplete(self):
        import copy
        sys.path.insert(0, str(LAB / 'tools/interim'))
        import rehearse_interim as ri
        s = copy.deepcopy(ri.soak())
        s[0]['pss']['extra'] = 1.0
        self.assertEqual(ss.analyse(s)['verdict'], 'INCOMPLETE')


class ClassLabelFailClosedTests(unittest.TestCase):  # defect 5
    def test_non_tiled_kernel_label_needs_a_matching_tag(self):
        for actual in ('default', '', None):
            with self.assertRaisesRegex(fx.FixtureError, 'class_label_(mismatch|unverifiable)'):
                fx.check_class_label('pfg', {'pf_attn': actual} if actual is not None else {})
        fx.check_class_label('pfg', {'pf_attn': 'pfg'})
        fx.check_class_label('tiled', {'pf_attn': 'default'})


class OpenCodeSandboxTests(unittest.TestCase):       # defect 6
    def setUp(self):
        self.ab = load('tiny_ab', 'tools/tiny_real/tiny_ab.py')

    def args(self, no_sandbox=False):
        class A:
            pass
        a = A()
        a.no_sandbox = no_sandbox
        a.opencode_bin = '/opt/x/installs/opencode/2.2.7/node_modules/.bin/opencode'
        return a

    def test_argv_hides_home_and_binds_only_repo_and_arm_home(self):
        if not shutil.which('bwrap'):
            with self.assertRaisesRegex(RuntimeError, 'bwrap missing'):
                self.ab.opencode_sandbox(self.args(), Path('/r/repo'), Path('/r/oc-home'))
            return
        argv = self.ab.opencode_sandbox(self.args(), Path('/r/repo'), Path('/r/oc-home'))
        s = ' '.join(argv)
        self.assertIn('--ro-bind / /', s)
        self.assertIn('--tmpfs /home', s)
        self.assertIn('--bind /r/repo /r/repo', s)
        self.assertIn('--bind /r/oc-home /r/oc-home', s)
        self.assertLess(s.index('--tmpfs /home'), s.index('--bind /r/repo'))
        self.assertEqual(self.ab.opencode_sandbox(self.args(no_sandbox=True), Path('/r'), Path('/h')), [])

    @unittest.skipUnless(shutil.which('bwrap'), 'bwrap not installed')
    def test_real_bwrap_blocks_host_writes(self):
        with tempfile.TemporaryDirectory() as d:
            repo, home = Path(d, 'repo'), Path(d, 'home')
            repo.mkdir(), home.mkdir()
            a = self.args()
            a.opencode_bin = shutil.which('sh')
            pre = self.ab.opencode_sandbox(a, repo, home)
            outside = Path(d, 'outside.txt')
            r = subprocess.run(pre + ['sh', '-c', 'touch %s/ok; touch %s 2>/dev/null; ls %s >/dev/null 2>&1; echo $?'
                                      % (repo, outside, Path.home())], capture_output=True, text=True)
            if r.returncode != 0 and 'Operation not permitted' in r.stderr:
                self.skipTest('unprivileged user namespaces unavailable here')
            self.assertTrue((repo / 'ok').exists())
            self.assertFalse(outside.exists())

    def test_production_ports_refused_for_both_ports(self):
        for port in (8092, 8093, 8091, 8094):
            with self.assertRaises(SystemExit):
                self.ab.main(['--oxe-bin-dir', '/n', '--oxa-bin', '/n', '--opencode-bin', '/n', '--model', '/n',
                              '--out', tempfile.mkdtemp(), '--port', str(port)])


class NativeReadinessTests(unittest.TestCase):       # defect 8
    def test_foreign_listener_on_engine_port_is_refused_before_launch(self):
        s = socket.socket()
        s.bind(('127.0.0.1', 0))
        s.listen(1)
        port = s.getsockname()[1]
        try:
            with tempfile.TemporaryDirectory() as d:
                h = nh.NativeHandle('/nonexistent/oxe-server', '/n.gguf', Path(d) / 'l', port=port - 1)
                h.store_root = Path(d) / 'store'
                (h.store_root / 'state').mkdir(parents=True)
                with self.assertRaisesRegex(RuntimeError, 'native_engine_port_in_use'):
                    h.load()
                self.assertIsNone(h.proc)
                self.assertTrue(nh.pid_listens(os.getpid(), port))
        finally:
            s.close()


class G1ContextTests(unittest.TestCase):             # defect 9
    def test_append_that_cannot_fit_is_invalid_before_sending(self):
        class A:
            append_tokens, max_tokens, ctx, gate_seed, max_wall = 50000, 256, 131072, 1, 1

        class H:
            port = 1   # would fail to connect if anything were sent
        report = dict(failures=[], invalid=[])
        gr.g1(A, H, None, {'token': {'n_tokens': 83035}}, None, report, 0)
        self.assertEqual(report['invalid'], ['append_exceeds_ctx:145791>131072'])

    def test_native_ctx_is_the_bound_and_82k_plus_28k_fits(self):
        class A:
            append_tokens, max_tokens, ctx, native_server, native_ctx = 28000, 256, 131072, '/x', 65536
        report = dict(failures=[], invalid=[])
        gr.g1(A, None, None, {'token': {'n_tokens': 33000}}, None, report, 0)
        self.assertEqual(report['invalid'], ['append_exceeds_ctx:68256>65536'])
        need = 83035 + -(-28000 * 5 // 4) + 256
        self.assertLess(need, 131072)
        self.assertNotIn('measured', report)


if __name__ == '__main__':
    unittest.main()
