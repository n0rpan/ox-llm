import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('lane_select', Path(__file__).resolve().parents[1] / 'lane_select.py')
lane = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lane)


def package(name, workspace='oxa', dependencies=None, shared=None):
    return {'name': name, 'workspace': workspace,
            'manifest': {'dependencies': dependencies or {}}, 'shared': shared or {}}


class Selection(unittest.TestCase):
    def setUp(self):
        self.packages = {
            'oxa/leaf': package('leaf'),
            'oxa/app': package('app', dependencies={'leaf': {'path': '../leaf'}}),
            'oxa/unrelated': package('unrelated'),
            'oxc-rs': package('oxc', 'oxc-rs', {'app': {'path': '../oxa/app'}}),
        }

    def names(self, matrix):
        return {(entry['workspace'], entry['runner'], tuple(entry['packages'])) for entry in matrix['include']}

    def test_member_globs_do_not_admit_nested_fixture_manifests(self):
        self.assertTrue(lane.member_matches('crates/server', 'crates/*'))
        self.assertFalse(lane.member_matches('crates/server/tests/project', 'crates/*'))
        self.assertTrue(lane.member_matches('.', '.'))
        with self.assertRaises(ValueError):
            lane.member_matches('crates/server', '**')

    def test_leaf_change_covers_local_consumers_across_workspaces_only(self):
        matrix, deferred, _ = lane.select(self.packages, self.packages, ['oxa/leaf/src/lib.rs'])
        self.assertEqual(self.names(matrix), {
            ('oxa', 'ubuntu-24.04', ('app', 'leaf')),
            ('oxa', 'ubuntu-24.04-arm', ('app', 'leaf')),
            ('oxc-rs', 'ubuntu-24.04', ('oxc',)),
            ('oxc-rs', 'ubuntu-24.04-arm', ('oxc',)),
        })
        self.assertEqual(deferred, [])

    def test_docs_outside_packages_do_not_start_rust_tests(self):
        matrix, _, _ = lane.select(self.packages, self.packages, ['AGENTS.md', 'README.md'])
        self.assertEqual(matrix, {'include': []})

    def test_shared_oxa_behavior_inputs_cannot_skip_consumers(self):
        for path in ('oxa/examples/extensions/custom-tool/index.js',
                     'oxa/examples/skills/example.md', 'oxa/.config/nextest.toml'):
            with self.subTest(path=path):
                matrix, _, _ = lane.select(self.packages, self.packages, [path])
                self.assertEqual(len(matrix['include']), 4)
                for entry in matrix['include']:
                    expected = ['app', 'leaf', 'unrelated'] if entry['workspace'] == 'oxa' else ['oxc']
                    self.assertEqual(entry['packages'], expected)

    def test_shared_documentation_does_not_start_rust_tests(self):
        matrix, _, _ = lane.select(self.packages, self.packages,
                                  ['oxa/README.md', 'oxa/docs/pages/quickstart.md'])
        self.assertEqual(matrix, {'include': []})

    def test_workspace_lock_expands_that_workspace_and_real_consumers(self):
        matrix, _, _ = lane.select(self.packages, self.packages, ['oxa/Cargo.lock'])
        for entry in matrix['include']:
            expected = ['app', 'leaf', 'unrelated'] if entry['workspace'] == 'oxa' else ['oxc']
            self.assertEqual(entry['packages'], expected)

    def test_lane_execution_contract_is_global_but_other_workflows_are_not(self):
        matrix, _, reasons = lane.select(self.packages, self.packages, ['tools/ci/lane_run.py'])
        self.assertEqual(len(matrix['include']), 4)
        self.assertTrue(reasons)
        empty, _, _ = lane.select(self.packages, self.packages, ['.github/workflows/macos.yml'])
        self.assertEqual(empty, {'include': []})

    def test_rename_considers_old_and_new_package_without_testing_deleted_package(self):
        current = {'oxa/leaf': package('renamed'),
                   'oxa/app': package('app', dependencies={'renamed': {'path': '../leaf'}})}
        matrix, _, _ = lane.select(current, self.packages, ['oxa/leaf/Cargo.toml'])
        self.assertTrue(matrix['include'])
        for entry in matrix['include']:
            self.assertEqual(entry['packages'], ['app', 'renamed'])

    def test_workspace_aliases_and_dev_dependencies_propagate(self):
        packages = dict(self.packages)
        packages['oxa/app'] = package('app', dependencies={'alias': {'workspace': True}},
            shared={'alias': {'path': 'leaf', 'package': 'leaf'}})
        packages['oxa/unrelated']['manifest']['dev-dependencies'] = {'app': {'path': '../app'}}
        matrix, _, _ = lane.select(packages, packages, ['oxa/leaf/tests/contract.rs'])
        self.assertEqual(matrix['include'][0]['packages'], ['app', 'leaf', 'unrelated'])

    def test_deleted_dependency_still_checks_its_retained_consumers(self):
        current = {path: p for path, p in self.packages.items() if path != 'oxa/leaf'}
        matrix, _, _ = lane.select(current, self.packages, ['oxa/leaf/src/lib.rs'])
        self.assertEqual(matrix['include'][0]['packages'], ['app'])

    def test_binary_consumers_are_edges_even_without_rust_linkage(self):
        packages = {'oxa/oxa-cli': package('oxa-cli'), 'oxa/oxa-fake': package('oxa-fake'),
                    'oxa/oxa-parity': package('oxa-parity')}
        matrix, _, _ = lane.select(packages, packages, ['oxa/oxa-cli/src/args.rs'])
        self.assertEqual(matrix['include'][0]['packages'], ['oxa-cli', 'oxa-parity'])

    def test_native_engine_never_runs_on_free_arm_and_is_not_silently_certified(self):
        packages = {'engine/crates/server': package('server', 'engine'),
                    'engine/crates/model': package('model', 'engine'),
                    'engine/crates/oxe-cli': package('oxe-cli', 'engine')}
        matrix, deferred, _ = lane.select(packages, packages, ['engine/Cargo.lock'])
        self.assertEqual(matrix['include'], [{'workspace': 'engine', 'runner': 'ubuntu-24.04', 'packages': ['server']}])
        self.assertEqual(deferred, ['model', 'oxe-cli'])


if __name__ == '__main__':
    unittest.main()
