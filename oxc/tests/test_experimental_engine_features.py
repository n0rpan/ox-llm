"""C4: default-off config reaches the existing native flags; metadata only, no engine/model."""
import copy
import unittest
from oxc.runtime import oxe_runtime, profile_policy
from test_oxe_runtime import config


def profile(experimental=None):
    value = config()
    block = {key: value[key] for key in ('server_binary', 'server_sha256', 'oxe_args', 'threads', 'context_size')}
    value['backend_kind'] = 'llama'
    value['rust_engine'] = {'oxe': block}
    if experimental is not None:
        block['experimental'] = experimental
    return value


class ExperimentalFeatures(unittest.TestCase):
    def resolve(self, value):
        return profile_policy.resolve(value, available=False, capable=True, oxe_enabled=True)

    def test_opt_in_settings_reach_the_native_argv(self):
        value = profile({'fair_quantum_ms': 25, 'mtp_guard': '0.6:256:512'})
        before = copy.deepcopy(value)
        resolved = self.resolve(value)
        argv = oxe_runtime.command(resolved, 8093)
        self.assertEqual(argv[-4:], ['--fair-quantum-ms', '25', '--mtp-guard', '0.6:256:512'])
        self.assertEqual(value, before)

    def test_absent_settings_preserve_the_existing_command(self):
        old = oxe_runtime.command(config(), 8093)
        new = oxe_runtime.command(self.resolve(profile()), 8093)
        self.assertEqual(old, new)
        self.assertNotIn('--fair-quantum-ms', new)
        self.assertNotIn('--mtp-guard', new)

    def test_explicit_off_is_forwarded_without_enabling_defaults(self):
        argv = oxe_runtime.command(self.resolve(profile({'fair_quantum_ms': 0, 'mtp_guard': 'off'})), 8093)
        self.assertEqual(argv[-4:], ['--fair-quantum-ms', '0', '--mtp-guard', 'off'])

    def test_bad_values_fail_before_launch(self):
        bad = [None, [], {'unknown': True}, {'fair_quantum_ms': True}, {'fair_quantum_ms': -1},
               {'fair_quantum_ms': 600001}, {'fair_quantum_ms': '25'}, {'mtp_guard': True},
               {'mtp_guard': 'nan'}, {'mtp_guard': '1.1'}, {'mtp_guard': '0.0'},
               {'mtp_guard': '0.6:0'}, {'mtp_guard': '0.6:2:-1'}, {'mtp_guard': '0.6:2:3:4'},
               {'mtp_guard': '0.6\n'}, {'mtp_guard': '0.6:18446744073709551616:1'},
               {'mtp_guard': '1e-50'}]
        for value in bad:
            with self.subTest(value=value):
                p = profile()
                p['rust_engine']['oxe']['experimental'] = value
                with self.assertRaises(ValueError):
                    self.resolve(p)

    def test_raw_flags_cannot_conflict_with_the_structured_setting(self):
        for flag, setting in (('--fair-quantum-ms', {'fair_quantum_ms': 25}),
                              ('--mtp-guard', {'mtp_guard': '0.6'})):
            for args in ([flag, 'off'], [flag + '=off']):
                p = profile(setting)
                p['rust_engine']['oxe']['oxe_args'].extend(args)
                with self.assertRaises(ValueError):
                    self.resolve(p)

    def test_the_historical_nested_location_uses_the_same_switches(self):
        p = profile({'fair_quantum_ms': 1})
        p['memlat_overrides'] = p.pop('rust_engine')
        resolved = self.resolve(p)
        self.assertEqual(oxe_runtime.command(resolved, 8093)[-2:], ['--fair-quantum-ms', '1'])

    def test_flash_uses_the_same_config_path(self):
        from test_flash_prefix_store import flash_profile
        p = flash_profile()
        p['memlat_overrides']['oxe_flash']['experimental'] = {'fair_quantum_ms': 2, 'mtp_guard': '0.5'}
        resolved = profile_policy.resolve(p, available=True, capable=True, oxe_flash_enabled=True)
        self.assertEqual(oxe_runtime.command(resolved, 8093)[-4:],
                         ['--fair-quantum-ms', '2', '--mtp-guard', '0.5'])

    def test_resolved_config_is_validated_again_before_command_construction(self):
        p = config()
        p['engine_features'] = {'mtp_guard': 'nan'}
        self.assertFalse(oxe_runtime.eligible(p))
        with self.assertRaises(ValueError):
            oxe_runtime.command(p, 8093)

    def test_native_boundary_and_ratio_only_syntax(self):
        args = profile_policy.experimental_engine_args(
            {'fair_quantum_ms': 600000, 'mtp_guard': '1:18446744073709551615:0'}, [])
        self.assertEqual(args[-1], '1:18446744073709551615:0')
        self.assertEqual(profile_policy.experimental_engine_args({'mtp_guard': '0.5'}, []),
                         ['--mtp-guard', '0.5'])
