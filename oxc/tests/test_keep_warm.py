"""O-160 smart keep-warm and POST /_local/preload: synthetic only (a fake memory source, a fake
engine process); no /proc read decides anything here and no engine starts."""
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from types import SimpleNamespace

from oxc.runtime import controller, ctl_proxy_ledger, keep_warm, request_ledger

GIB = 1024 ** 3
BASE = {'idle_unload_seconds': 7200.0, 'psi_some_avg10': 10.0, 'min_available_mib': 1024,
        'pressure_grace_seconds': 60.0}


class FakeMemory:
    def __init__(self, psi=0.0, available=16 * GIB):
        self.psi, self.available = psi, available
        self.reads = 0

    def psi_some_avg10(self):
        self.reads += 1
        return self.psi

    def available_bytes(self):
        return self.available


def decide_n(policy, n, config=None, idle_for=600.0, base=BASE):
    return [policy.decide(config or {}, idle_for, base) for _ in range(n)]


class PolicyTests(unittest.TestCase):
    def test_plenty_of_ram_and_a_short_idle_keeps_the_model(self):
        policy = keep_warm.Policy(FakeMemory())
        self.assertEqual(decide_n(policy, 10, idle_for=1800.0), [None] * 10)

    def test_long_idle_unloads(self):
        policy = keep_warm.Policy(FakeMemory())
        self.assertEqual(policy.decide({}, 7200.0, BASE), 'idle')
        self.assertEqual(policy.last_unload['reason'], 'idle')

    def test_psi_pressure_unloads_after_confirmation(self):
        policy = keep_warm.Policy(FakeMemory(psi=25.0))
        got = decide_n(policy, keep_warm.PRESSURE_CONFIRM_PASSES)
        self.assertEqual(got[:-1], [None] * (keep_warm.PRESSURE_CONFIRM_PASSES - 1))
        self.assertEqual(got[-1], 'pressure_psi')
        self.assertEqual(policy.last_unload['psi_some_avg10'], 25.0)

    def test_a_single_spike_does_not_unload(self):
        memory = FakeMemory(psi=25.0)
        policy = keep_warm.Policy(memory)
        self.assertIsNone(policy.decide({}, 600.0, BASE))
        memory.psi = 0.0
        self.assertEqual(decide_n(policy, 5), [None] * 5)

    def test_low_memavailable_unloads(self):
        policy = keep_warm.Policy(FakeMemory(available=512 * 1024 * 1024))
        self.assertEqual(decide_n(policy, keep_warm.PRESSURE_CONFIRM_PASSES)[-1], 'pressure_available')

    def test_no_pressure_check_within_the_grace_after_a_request(self):
        memory = FakeMemory(psi=90.0, available=0)
        policy = keep_warm.Policy(memory)
        self.assertEqual(decide_n(policy, 5, idle_for=10.0), [None] * 5)
        self.assertEqual(memory.reads, 0)

    def test_psi_absent_falls_back_to_memavailable(self):
        policy = keep_warm.Policy(FakeMemory(psi=None, available=16 * GIB))
        self.assertEqual(decide_n(policy, 5), [None] * 5)
        policy = keep_warm.Policy(FakeMemory(psi=None, available=100 * 1024 * 1024))
        self.assertEqual(decide_n(policy, keep_warm.PRESSURE_CONFIRM_PASSES)[-1], 'pressure_available')

    def test_idle_unload_off_still_unloads_under_pressure(self):
        base = {**BASE, 'idle_unload_seconds': None}
        policy = keep_warm.Policy(FakeMemory())
        self.assertIsNone(policy.decide({}, 10 ** 7, base))
        policy = keep_warm.Policy(FakeMemory(psi=50.0))
        self.assertEqual(decide_n(policy, keep_warm.PRESSURE_CONFIRM_PASSES, idle_for=10 ** 7, base=base)[-1],
                         'pressure_psi')

    def test_old_600_second_behaviour_is_selectable(self):
        policy = keep_warm.Policy(FakeMemory())
        base = {**BASE, 'idle_unload_seconds': 600.0}
        self.assertIsNone(policy.decide({}, 599.0, base))
        self.assertEqual(policy.decide({}, 600.0, base), 'idle')


class ProcSourceTests(unittest.TestCase):
    def test_missing_pressure_file_is_none_and_meminfo_is_parsed(self):
        with tempfile.TemporaryDirectory() as root:
            meminfo = Path(root) / 'meminfo'
            meminfo.write_text('MemTotal:       32000000 kB\nMemAvailable:    2048 kB\n')
            source = keep_warm.ProcMemorySource(Path(root) / 'no-pressure', meminfo)
            self.assertIsNone(source.psi_some_avg10())
            self.assertEqual(source.available_bytes(), 2048 * 1024)
            source = keep_warm.ProcMemorySource(Path(root) / 'none', Path(root) / 'none')
            self.assertIsNone(source.available_bytes())

    def test_psi_parse(self):
        text = ('some avg10=12.50 avg60=3.00 avg300=1.00 total=12345\n'
                'full avg10=99.00 avg60=0.00 avg300=0.00 total=1\n')
        self.assertEqual(keep_warm.parse_psi_some_avg10(text), 12.5)
        for bad in ('', 'full avg10=1.0', 'some avg10=x', 'some avg10=nan', 'some avg60=1.0'):
            self.assertIsNone(keep_warm.parse_psi_some_avg10(bad))


class SettingsTests(unittest.TestCase):
    def test_env_defaults_and_the_legacy_variable_is_ignored(self):
        self.assertEqual(keep_warm.env_settings({}), BASE)
        self.assertEqual(keep_warm.env_settings({'LOCAL_LLM_IDLE_SECONDS': '600'})['idle_unload_seconds'], 7200.0)
        got = keep_warm.env_settings({'LOCAL_LLM_KEEP_WARM_IDLE_SECONDS': '600',
                                      'LOCAL_LLM_PRESSURE_PSI_SOME_AVG10': 'off',
                                      'LOCAL_LLM_PRESSURE_MIN_AVAILABLE_MIB': '2048',
                                      'LOCAL_LLM_PRESSURE_GRACE_SECONDS': '0'})
        self.assertEqual(got, {'idle_unload_seconds': 600.0, 'psi_some_avg10': None,
                               'min_available_mib': 2048.0, 'pressure_grace_seconds': 0.0})
        self.assertIsNone(keep_warm.env_settings({'LOCAL_LLM_KEEP_WARM_IDLE_SECONDS': 'off'})['idle_unload_seconds'])
        for bad in ('0', '-5', 'nan', 'inf', 'soon'):  # an invalid value never disables (audit N8)
            self.assertEqual(keep_warm.env_settings({'LOCAL_LLM_KEEP_WARM_IDLE_SECONDS': bad})['idle_unload_seconds'],
                             7200.0)

    def test_runtime_json_block_and_flash_profile(self):
        config = {'keep_warm': {'idle_unload_seconds': None, 'psi_some_avg10': 5, 'min_available_mib': 'bad'}}
        got = keep_warm.settings(config, BASE)
        self.assertEqual((got['idle_unload_seconds'], got['psi_some_avg10'], got['min_available_mib']),
                         (None, 5.0, 1024))
        flash = {'backend_kind': 'oxe', 'oxe_flash': {'idle_unload_seconds': 900}}
        self.assertEqual(keep_warm.settings(flash, BASE)['idle_unload_seconds'], 900.0)
        self.assertEqual(keep_warm.settings({'backend_kind': 'oxe', 'oxe_flash': {}}, BASE)['idle_unload_seconds'],
                         7200.0)

    def test_idle_seconds_follows_the_controller_value(self):
        with patch.object(controller, 'IDLE_SECONDS', 600.0):
            self.assertEqual(controller.idle_seconds(None), 600.0)
        with patch.object(controller, 'IDLE_SECONDS', None):
            self.assertIsNone(controller.idle_seconds({}))


class ManagerCase(unittest.TestCase):
    def setUp(self):
        self.manager = controller.Manager()
        self.manager.stop_event.set()
        self.manager.thread.join(2)
        self.memory = FakeMemory()
        self.manager.keep_warm = keep_warm.Policy(self.memory)
        self.manager.cleanup = Mock(side_effect=self.unload)
        self.process = Mock(pid=4242)
        self.process.poll.return_value = None

    def unload(self):
        self.manager.process = None
        self.manager.loaded_config = None
        self.manager.state = 'unloaded'

    def load(self, alias='stub-local', idle_for=600.0):
        m = self.manager
        m.process, m.state = self.process, 'ready'
        m.loaded_config = {'model_alias': alias}
        m.last_used = time.monotonic() - idle_for

    def reap(self, n=keep_warm.PRESSURE_CONFIRM_PASSES):
        for _ in range(n):
            self.manager.reap_once()


class ReaperTests(ManagerCase):
    def test_pressure_unloads_an_idle_model(self):
        self.load()
        self.memory.psi = 40.0
        self.reap()
        self.manager.cleanup.assert_called_once()
        self.assertEqual(self.manager.status()['keep_warm']['last_unload']['reason'], 'pressure_psi')

    def test_plenty_of_ram_keeps_it(self):
        self.load(idle_for=3600.0)
        self.reap(10)
        self.manager.cleanup.assert_not_called()

    def test_long_idle_unloads(self):
        self.load(idle_for=controller.IDLE_SECONDS + 1)
        self.reap(1)
        self.manager.cleanup.assert_called_once()

    def test_active_or_queued_request_blocks_every_unload(self):
        self.load(idle_for=10 ** 6)
        self.memory.psi = 99.0
        self.manager.active = 1
        self.reap(10)
        self.manager.active = 0
        self.manager.queue_waiters.append(object())
        self.reap(10)
        self.manager.queue_waiters.clear()
        self.manager.queue_reserved = True
        self.reap(10)
        self.manager.cleanup.assert_not_called()
        self.manager.queue_reserved = False
        self.reap(1)
        self.manager.cleanup.assert_called_once()


class PreloadTests(ManagerCase):
    def setUp(self):
        super().setUp()
        self.manager.config = Mock(side_effect=lambda alias=None: {'model_alias': alias or 'stub-local'})
        self.release = threading.Event()
        self.begins = []

        def begin(**kwargs):
            self.begins.append(kwargs)
            self.release.wait(5)
            self.load(kwargs.get('model_alias') or 'stub-local', idle_for=0)
            self.manager.launches += 1
            self.manager.active += 1
            return self.process
        self.manager.begin = Mock(side_effect=begin)
        self.manager.preload_state = Mock(return_value=True)

    def test_preload_starts_once_and_is_idempotent(self):
        m = self.manager
        self.assertEqual(m.preload_model(), (202, {'state': 'loading', 'started': True, 'model': 'stub-local'}))
        self.assertEqual(m.preload_model()[1]['started'], False)
        self.assertEqual(m.preload_model()[0], 202)
        self.release.set()
        m.model_preload_thread.join(5)
        self.assertEqual(len(self.begins), 1)
        self.assertTrue(self.begins[0]['allow_start'] and self.begins[0]['explicit_load'])
        self.assertEqual(m.status()['model_preload']['state'], 'ready')
        self.assertEqual(m.active, 0)  # the preload's own admission ended
        m.preload_state.assert_called_once_with('stub-local')
        self.assertEqual(m.preload_model(), (200, {'state': 'ready', 'started': False, 'model': 'stub-local'}))
        self.assertEqual(len(self.begins), 1)

    def test_preload_never_switches_models(self):
        self.load('other-local')
        code, reply = self.manager.preload_model('stub-local')
        self.assertEqual((code, reply['state']), (409, 'other_model_loaded'))
        self.assertEqual(self.manager.preload_model()[0], 200)  # no model named: what is loaded is fine
        self.manager.begin.assert_not_called()

    def test_preload_failure_is_reported(self):
        self.manager.begin = Mock(side_effect=controller.Unavailable('Model setup is still running.'))
        self.assertEqual(self.manager.preload_model()[0], 202)
        self.manager.model_preload_thread.join(5)
        info = self.manager.status()['model_preload']
        self.assertEqual((info['state'], info['error']), ('failed', 'Model setup is still running.'))
        self.assertEqual(self.manager.active, 0)

    def test_bad_body(self):
        self.assertEqual(self.manager.preload_model(7)[0], 400)



class PureDecide(unittest.TestCase):
    """keep_warm.decide: the pure function the reaper uses and fixtures import directly."""
    BASE = {'idle_unload_seconds': 7200.0, 'psi_some_avg10': 10.0, 'min_available_mib': 1024,
            'pressure_grace_seconds': 60.0}
    GIB = 1024 ** 3

    def run_passes(self, n, **kw):
        streak, out = 0, None
        for _ in range(n):
            out = keep_warm.decide(1000.0, 0.0, kw.get('mem', 8 * self.GIB), kw.get('psi', 0.0), None,
                                   streak=streak, base=self.BASE)
            streak = out.streak
        return out

    def test_no_pressure_keeps(self):
        self.assertEqual(self.run_passes(5), keep_warm.Decision('keep', None, 0))

    def test_psi_pressure_unloads_after_three_passes(self):
        self.assertEqual(self.run_passes(2, psi=25.0).action, 'keep')
        self.assertEqual(self.run_passes(3, psi=25.0), keep_warm.Decision('unload', 'pressure_psi', 0))

    def test_low_memory_unloads_and_unknown_psi_falls_back(self):
        out = self.run_passes(3, psi=None, mem=512 * 1024 * 1024)
        self.assertEqual(out, keep_warm.Decision('unload', 'pressure_available', 0))

    def test_long_idle_unloads(self):
        out = keep_warm.decide(7300.0, 0.0, 8 * self.GIB, 0.0, None, base=self.BASE)
        self.assertEqual(out, keep_warm.Decision('unload', 'idle', 0))
        off = dict(self.BASE, idle_unload_seconds=None)
        self.assertEqual(keep_warm.decide(10 ** 6, 0.0, 8 * self.GIB, 0.0, None, base=off).action, 'keep')

    def test_preload_just_used_keeps_even_under_pressure(self):
        # a preload sets last_use = now: inside the grace window pressure never unloads it
        out = keep_warm.decide(500.0, 500.0, 100 * 1024 * 1024, 90.0, None, streak=5, base=self.BASE)
        self.assertEqual(out, keep_warm.Decision('keep', None, 0))


class PreloadNoSwitchRace(unittest.TestCase):
    """Codex L3 review b59812f (preload_race.py): an ordinary request loads B after preload('A') was
    scheduled but before its worker is admitted; the worker must leave B loaded (busy), never clean it up."""

    def test_preload_worker_never_unloads_a_model_loaded_meanwhile(self):
        m = controller.Manager()
        m.stop_event.set()
        m.thread.join(2)
        process = Mock(pid=4242)
        process.poll.return_value = None
        m.config = Mock(side_effect=lambda alias=None: {'model_alias': alias or 'A', 'backend_kind': 'synthetic-no-start'})
        cleaned = []

        def cleanup():
            cleaned.append((m.loaded_config or {}).get('model_alias'))
            m.process, m.loaded_config, m.state = None, None, 'unloaded'
        m.cleanup = Mock(side_effect=cleanup)

        class Deferred:
            def __init__(self, target, args, **kwargs):
                self.target, self.args = target, args

            def start(self):
                pass
        with patch('threading.Thread', Deferred):
            code, reply = m.preload_model('A')
        self.assertEqual((code, reply['started']), (202, True))
        m.process, m.loaded_config, m.state, m.active = process, {'model_alias': 'B', 'backend_kind': 'synthetic-no-start'}, 'ready', 0
        m.model_preload_thread.target(*m.model_preload_thread.args)
        self.assertEqual(cleaned, [])
        self.assertEqual(m.loaded_config['model_alias'], 'B')
        self.assertEqual(m.model_preload['state'], 'busy')
        self.assertEqual(m.active, 0)


class RequestPathTimings(unittest.TestCase):
    """v0.2.0 #8: engine request-path timings pass through to the ledger; controller prep time."""

    def test_engine_fields_pass_through(self):
        tail = (b'data: {"timings":{"prompt_n":5,"render_ms":1.5,"tokenize_ms":2,"restore_ms":3.25,'
                b'"ckpt_ms":4,"queue_ms":0.5,"bogus_ms":9}}\n\n')
        got = request_ledger.parse_tail(tail)
        self.assertEqual({k: got[k] for k in ('render_ms', 'tokenize_ms', 'restore_ms', 'ckpt_ms', 'queue_ms')},
                         {'render_ms': 1.5, 'tokenize_ms': 2, 'restore_ms': 3.25, 'ckpt_ms': 4, 'queue_ms': 0.5})
        self.assertNotIn('bogus_ms', got)

    def test_backend_prep_sums_over_readmission(self):
        st = SimpleNamespace(led=SimpleNamespace(row={}))
        ctl_proxy_ledger.note_backend_prep(st, 0.002)
        ctl_proxy_ledger.note_backend_prep(st, 0.003)
        self.assertAlmostEqual(st.led.row['ctl_backend_ms'], 5.0)
        ctl_proxy_ledger.note_backend_prep(SimpleNamespace(led=None), 1.0)  # no ledger: no error


if __name__ == '__main__':
    unittest.main()
