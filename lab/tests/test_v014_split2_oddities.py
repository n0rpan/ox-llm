"""SPLIT2-OLD-ODDITIES (v0.1.4): slot prefix store is llama-only, no dead F127 guard pick in
Flash admission, and the loading-PSI window follows each engine's own readiness wait.
Synthetic only: no engine, model, unit or real ~/.config/local-ai is touched."""
import inspect
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from snapdragon_ai_lab.runtime import controller, ctl_engines, ctl_lifecycle, memory_policy

CACHE = {'enabled': True, 'directory': '/synthetic/prefixes', 'max_bytes': 7}


class SlotPrefixStoreIsLlamaOnly(unittest.TestCase):
    def shaped(self, engine):
        manager = SimpleNamespace(prefix_store=None)
        with patch.object(controller, 'PrefixStore', side_effect=lambda directory, max_bytes: SimpleNamespace(
                root='/synthetic/prefixes/root')):
            args = engine.shape(manager, {}, ['/bin/server'], None, dict(CACHE))
        return manager, args

    def test_llama_gets_slots(self):
        manager, args = self.shaped(ctl_engines.LlamaLaunch())
        self.assertEqual(args, ['/bin/server', '--slots', '--slot-save-path', '/synthetic/prefixes/root'])
        self.assertIsNotNone(manager.prefix_store)

    def test_oxe_engines_never_get_slots(self):
        for engine in (ctl_engines.OxeMoeLaunch(), ctl_engines.OxeFlashLaunch()):
            with self.subTest(engine=type(engine).__name__):
                manager, args = self.shaped(engine)
                self.assertEqual(args, ['/bin/server'])
                self.assertIsNone(manager.prefix_store)


class FlashAdmissionHasNoDeadGuardPick(unittest.TestCase):
    def test_admit_does_not_compute_the_f127_guard(self):
        source = inspect.getsource(ctl_engines.OxeFlashLaunch.admit)
        self.assertNotIn('flash_swap_tolerant_guard', source)
        self.assertNotIn('flash_swap_guard', source)


class LoadingWindowFollowsReadiness(unittest.TestCase):
    def test_window_matches_each_engines_readiness_wait(self):
        source = inspect.getsource(ctl_lifecycle)
        self.assertIn('min(memory_policy.MAX_LOADING_SECONDS,', source)
        self.assertNotIn('min(300.0, ctl.START_SECONDS)', source)
        for start in (0.0, 10.0, 180.0, 300.0, 900.0):
            with self.subTest(start=start), patch.object(controller, 'START_SECONDS', start):
                flash = ctl_engines.OxeFlashLaunch().start_seconds()
                plain = ctl_engines.OxeMoeLaunch().start_seconds()
                self.assertEqual(min(memory_policy.MAX_LOADING_SECONDS, flash), 300.0)
                self.assertEqual(min(memory_policy.MAX_LOADING_SECONDS, plain), min(300.0, start))


if __name__ == '__main__':
    unittest.main()
