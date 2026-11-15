"""CI-TRUTH CT-7/CT-9: the Rust engine applies by capability (CPU features + RAM), not by MEMLAT; a memory
guard setup problem is named with its fix. Synthetic files only; never starts an engine."""
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from oxc import engine_report
from oxc.runtime import ctl_status, memory_policy, profile_policy
import test_oxe_production as oxe_t

FULL = 'processor\t: 0\nFeatures\t: fp asimd asimdhp asimddp i8mm bf16 sha3\n'


class Files:
    def __init__(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)

    def write(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        return str(path)


class HostCapability(unittest.TestCase):
    def setUp(self):
        self.f = Files()
        self.addCleanup(self.f.tmp.cleanup)
        check = patch.object(profile_policy, 'RUST_FIT_CHECK', True)
        check.start()
        self.addCleanup(check.stop)

    def test_full_aarch64_cpu_is_capable(self):
        self.assertIsNone(profile_policy.rust_host_reason(self.f.write('c', FULL), 'aarch64'))

    def test_other_arch_or_missing_features_are_named(self):
        self.assertIn('x86_64', profile_policy.rust_host_reason(self.f.write('c', FULL), 'x86_64'))
        reason = profile_policy.rust_host_reason(self.f.write('c', 'Features\t: fp asimd asimddp\n'), 'aarch64')
        self.assertIn('i8mm', reason)
        self.assertIn('bf16', reason)
        self.assertIn('unreadable', profile_policy.rust_host_reason('/nonexistent/cpuinfo', 'aarch64'))

    def test_memory_too_small_is_named(self):
        cpu = self.f.write('c', FULL)
        small = self.f.write('m', 'MemTotal:        8000000 kB\n')
        big = self.f.write('m2', 'MemTotal:       64000000 kB\n')
        p = {'expected_bytes': 17 * 1024**3}
        self.assertIn('needs about', profile_policy.rust_fit_reason(p, cpuinfo=cpu, meminfo=small, machine='aarch64'))
        self.assertIsNone(profile_policy.rust_fit_reason(p, cpuinfo=cpu, meminfo=big, machine='aarch64'))

    def test_check_can_be_switched_off(self):
        with patch.object(profile_policy, 'RUST_FIT_CHECK', False):
            self.assertIsNone(profile_policy.rust_fit_reason({}, cpuinfo='/nonexistent', machine='x86_64'))


class ResolveByCapability(unittest.TestCase):
    def test_rust_applies_on_a_stock_boot_when_capable(self):
        rust = profile_policy.resolve(oxe_t.profile(), available=False, capable=True, oxe_enabled=True)
        self.assertEqual(rust['backend_kind'], 'oxe')
        self.assertEqual(rust['execution_policy_mode'], 'stock')
        memlat = profile_policy.resolve(oxe_t.profile(), available=True, capable=True, oxe_enabled=True)
        self.assertEqual(memlat['execution_policy_mode'], 'memlat')

    def test_not_capable_keeps_classic_and_status_says_why(self):
        original = oxe_t.profile()
        self.assertEqual(profile_policy.resolve(original, available=False, capable=False, oxe_enabled=True), original)
        classic = profile_policy.resolve(original, available=True, capable=False, oxe_enabled=True)
        self.assertNotEqual(classic.get('backend_kind'), 'oxe')
        with patch.object(profile_policy, 'rust_fit_reason', return_value='the CPU lacks i8mm'):
            note = ctl_status.engine_summary(classic)['engine_note']
        self.assertIn('cannot run here: the CPU lacks i8mm', note)


class GuardSetup(unittest.TestCase):
    def setUp(self):
        self.f = Files()
        self.addCleanup(self.f.tmp.cleanup)

    def test_missing_swap_limit_gives_the_fix_not_free_memory(self):
        self.f.write('cg/memory.swap.max', 'max\n')
        cg = self.f.root / 'cg'
        req = memory_policy.guard_requirements(cg)
        self.assertFalse(req['ok'])
        message = memory_policy.start_refusal_message('own_swap_limit_not_zero', cg)
        self.assertIn('MemorySwapMax=0', message)
        self.assertNotIn('free memory', message)

    def test_correct_setup_and_real_shortage(self):
        self.f.write('cg/memory.swap.max', '0\n')
        cg = self.f.root / 'cg'
        self.assertTrue(memory_policy.guard_requirements(cg)['ok'])
        self.assertIn('free memory', memory_policy.start_refusal_message('host_available_below_floor', cg))

    def test_unreadable_cgroup_is_named(self):
        req = memory_policy.guard_requirements(self.f.root / 'missing')
        self.assertFalse(req['ok'])
        self.assertIn('systemd user service', req['fix'])


class DoctorReport(unittest.TestCase):
    def test_unit_memory_setup(self):
        f = Files()
        self.addCleanup(f.tmp.cleanup)
        unit = '.config/systemd/user/oxc-chat.service'
        f.write(unit, '[Service]\nMemoryMax=25G\nMemorySwapMax=0\n')
        self.assertTrue(engine_report.unit_memory_setup(f.root)['ok'])
        f.write(unit + '.d/x.conf', '[Service]\nMemorySwapMax=4G\n')
        row = engine_report.unit_memory_setup(f.root)
        self.assertFalse(row['ok'])
        self.assertIn('MemorySwapMax=0', row['fix'])

    def test_engine_profiles_name_why_not_rust(self):
        f = Files()
        self.addCleanup(f.tmp.cleanup)
        cfg = f.write('runtime.json', json.dumps({'profiles': {'m': oxe_t.profile('m')}}))
        with patch.object(profile_policy, 'rust_fit_reason', return_value='the CPU lacks bf16'):
            out = engine_report.engine_profiles(cfg)
        self.assertEqual(out['profiles']['m']['why_not_rust'], 'the CPU lacks bf16')


if __name__ == '__main__':
    unittest.main()
