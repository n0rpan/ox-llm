"""I0-R2 (Codex 04:32): helper 4848595 never bound the oxa profile, so oxa ran pi (939 get_state
samples) while the arm claimed local (softTokens is a local-profile setting). The arm now binds the
intended profile (OXA_PROFILE + oxa.profile), asserts it with RPC get_state before the first model
request and records it. Model-free: a fake `oxa --mode rpc` that resolves the profile the way
oxa-config does (OXA_PROFILE > settings oxa.profile > default pi)."""
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from types import SimpleNamespace

LAB = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(LAB / 'tools/interim'), str(LAB / 'tools/tiny_real')]
import real_use_session as ru  # noqa: E402
import oxa_tiny_loop as oxl  # noqa: E402

FAKE_OXA = r'''
import json, os, sys
stubborn = '--always-pi' in sys.argv
def profile():
    if stubborn:
        return 'pi'
    env = os.environ.get('OXA_PROFILE')
    if env in ('pi', 'local'):
        return env
    try:
        s = json.load(open(os.path.join(os.environ['OXA_AGENT_DIR'], 'settings.json')))
        v = (s.get('oxa') or {}).get('profile')
        if v in ('pi', 'local'):
            return v
    except (OSError, KeyError, ValueError):
        pass
    return 'pi'
for line in sys.stdin:
    cmd = json.loads(line)
    if cmd.get('type') == 'get_state':
        print(json.dumps({'type': 'response', 'id': cmd['id'], 'command': 'get_state', 'success': True,
                          'data': {'profile': profile(), 'isStreaming': False, 'isCompacting': False}}), flush=True)
'''


class ProfileBindTests(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.fake = self.tmp / 'fake_oxa.py'
        self.fake.write_text(FAKE_OXA)
        self.home = self.tmp / 'home'
        self.agent = self.home / '.oxa' / 'agent'
        self.agent.mkdir(parents=True)

    SETTINGS = {'compaction': {'enabled': True}, 'oxa': {'compaction': {'softTokens': 32000}, 'subagent': {'enabled': True}}}

    def start(self, parent_env, settings, bind=None, stubborn=False):
        env = oxl.client_env(parent_env, self.home, self.agent, self.tmp / 'sessions', 'http://127.0.0.1:1')
        if bind is not None:
            env, settings = ru.bind_profile(bind, env, settings)
        (self.agent / 'settings.json').write_text(json.dumps(settings))
        argv = [sys.executable, '-I', str(self.fake)] + (['--always-pi'] if stubborn else [])
        rpc = oxl.Rpc(argv, env, str(self.tmp), str(self.tmp / ('rpc-%d.jsonl' % len(list(self.tmp.glob('rpc-*.jsonl'))))))
        self.addCleanup(rpc.close)
        return rpc

    def cleared(self, **extra):
        return dict({'PATH': os.environ.get('PATH', '/usr/bin')}, **extra)

    def test_local_intent(self):
        # old helper (no binding, cleared env): oxa runs the default pi -> the assertion fails
        obs = []
        with self.assertRaises(ru.ProfileMismatch):
            ru.check_profile(self.start(self.cleared(), self.SETTINGS), 'local', 'main', obs)
        self.assertEqual(obs[-1]['actual'], 'pi')
        # new helper: bound -> local
        obs = []
        self.assertEqual(ru.check_profile(self.start(self.cleared(), self.SETTINGS, bind='local'), 'local', 'main', obs), 'local')
        self.assertEqual(obs, [dict(obs[0], process='main', intended='local', actual='local')])

    def test_pi_intent(self):
        # a stray local setting (and a parent OXA_PROFILE=local, stripped by client_env) must not leak in
        hostile = {**self.SETTINGS, 'oxa': {**self.SETTINGS['oxa'], 'profile': 'local'}}
        obs = []
        with self.assertRaises(ru.ProfileMismatch):
            ru.check_profile(self.start(self.cleared(OXA_PROFILE='local'), hostile), 'pi', 'main', obs)
        self.assertEqual(obs[-1]['actual'], 'local')
        obs = []
        self.assertEqual(ru.check_profile(self.start(self.cleared(OXA_PROFILE='local'), hostile, bind='pi'), 'pi', 'main', obs), 'pi')

    def test_oxa_ignoring_the_binding_aborts(self):
        obs = []
        with self.assertRaises(ru.ProfileMismatch):
            ru.check_profile(self.start(self.cleared(), self.SETTINGS, bind='local', stubborn=True), 'local', 'main', obs)
        self.assertEqual((obs[-1]['intended'], obs[-1]['actual']), ('local', 'pi'))

    def test_bind_profile_sets_both_layers_and_rejects_unknown(self):
        env, settings = ru.bind_profile('local', {'A': '1'}, self.SETTINGS)
        self.assertEqual((env['OXA_PROFILE'], settings['oxa']['profile']), ('local', 'local'))
        self.assertEqual(settings['oxa']['compaction']['softTokens'], 32000)
        self.assertNotIn('profile', self.SETTINGS['oxa'])           # the input is not mutated
        with self.assertRaises(ValueError):
            ru.bind_profile('pi-local', {}, self.SETTINGS)


class MainProfileTests(unittest.TestCase):
    def run_main(self, session, extra):
        names = ('prepare', 'private_home', 'session')
        orig = [getattr(ru, n) for n in names]
        try:
            ru.prepare = lambda repo, cand, work: (work / 'mono', {'large_read_lines': 300})
            ru.private_home = lambda w: w / 'home'
            ru.session = session
            with tempfile.TemporaryDirectory() as d:
                out = Path(d) / 'out'
                rc = ru.main(['--out', str(out)] + extra)
                rep = out / 'real-use.json'
                return rc, (json.loads(rep.read_text()) if rep.exists() else None)
        finally:
            for n, f in zip(names, orig):
                setattr(ru, n, f)

    def test_missing_profile_refuses_to_run(self):
        called = []
        rc, rep = self.run_main(lambda *a: called.append(1), [])
        self.assertEqual((rc, rep, called), (2, None, []))

    def test_mismatch_aborts_the_arm_and_records_it(self):
        def session(a, *rest):
            a.profile_observed.append(dict(process='main', intended=a.profile, actual='pi'))
            raise ru.ProfileMismatch("oxa main process runs profile 'pi', the arm intends 'local'")
        rc, rep = self.run_main(session, ['--profile', 'local'])
        self.assertEqual((rc, rep['verdict']), (2, 'ABORTED'))
        self.assertIn('profile_mismatch', rep['reason'])
        self.assertEqual(rep['oxa_profile'], {'intended': 'local', 'observed': [
            {'process': 'main', 'intended': 'local', 'actual': 'pi'}]})


if __name__ == '__main__':
    unittest.main()
