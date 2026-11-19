import importlib.util
import os
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('lane_run', Path(__file__).resolve().parents[1] / 'lane_run.py')
lane = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lane)


class LaneRuntime(unittest.TestCase):
    def test_empty_unit_inventory_is_rejected(self):
        empty = {'rust-suites': {'native': {'status': 'listed', 'testcases': {}}}}
        for workspace in ('oxa', 'engine'):
            with self.assertRaises(ValueError):
                lane.rust_test_count(empty, workspace)
        empty['rust-suites']['native']['status'] = 'skipped'
        with self.assertRaises(ValueError):
            lane.rust_test_count(empty, 'oxa')

    def test_only_selected_release_features_survive_the_tree(self):
        tree = 'oxa-cli v1 (/project/cli)|\noxa-engine v1 (/project/engine)|in-process-gateway\noxc-gateway v1 (/project/gateway)|catalog,ledger\nother v1|experimental\n'
        self.assertEqual(lane.parse_features(tree, ['oxc-gateway'], 'oxa-cli'),
                         ['oxc-gateway/catalog', 'oxc-gateway/ledger'])

    def test_tree_repeats_are_unified_without_duplicate_arguments(self):
        tree = 'root v1|\npkg v1|first\npkg v1|first,second\n'
        self.assertEqual(lane.parse_features(tree, ['pkg'], 'root'), ['pkg/first', 'pkg/second'])

    def test_disabled_production_defaults_stay_off_but_standalone_defaults_remain(self):
        tree = 'root v1|\npkg v1|live\n'
        self.assertEqual(lane.parse_features(tree, ['pkg', 'standalone'], 'root', {'pkg', 'standalone'}),
                         ['pkg/live', 'standalone/default'])

    def test_unknown_tree_shape_or_option_injection_is_a_failure(self):
        for tree in ('unexpected', 'pkg v1|--all-features', 'root v1|feature/other'):
            with self.assertRaises(ValueError):
                lane.parse_features(tree, ['pkg'], 'root')
        for packages in ([], ['--workspace'], ['name; echo unwanted']):
            with self.assertRaises(ValueError):
                lane.cargo_arguments(packages, [])

    def test_package_arguments_do_not_turn_into_workspace_tests(self):
        self.assertEqual(lane.cargo_arguments(['second', 'first', 'first'], ['first/live']),
                         ['--no-default-features', '-p', 'first', '-p', 'second', '--features', 'first/live'])

    def test_cache_and_job_credentials_do_not_reach_test_processes(self):
        inherited = {'PATH': '/usr/bin', 'CARGO_HOME': '/project/.lane/cargo',
                     'RUSTUP_HOME': '/project/.lane/rustup', 'RUSTUP_TOOLCHAIN': '1.99.1',
                     'ACTIONS_RUNTIME_TOKEN': 'x',
                     'GH_TOKEN': 'x', 'UNRELATED': 'excluded',
                     'RUSTC_WRAPPER': 'sccache'}
        with patch.dict(os.environ, inherited, clear=True):
            env = lane.runtime_environment(Path('/project'), Path('/fixture/home'), Path('/fixture/tmp'), 'oxa', True)
        for name in ('ACTIONS_RUNTIME_TOKEN', 'GH_TOKEN', 'UNRELATED', 'RUSTC_WRAPPER'):
            self.assertNotIn(name, env)
        self.assertEqual(env['HOME'], '/fixture/home')
        self.assertEqual(env['CARGO_TARGET_DIR'], '/project/oxa/target')
        self.assertEqual(env['OXA_REQUIRE_BWRAP'], '1')
        self.assertEqual(env['OXA_BIN'], '/project/oxa/target/ci/oxa')


if __name__ == '__main__':
    unittest.main()
