import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('paths', Path(__file__).resolve().parents[1] / 'paths.py')
paths = importlib.util.module_from_spec(spec)
spec.loader.exec_module(paths)


class Paths(unittest.TestCase):
    def test_component_source_and_test_changes(self):
        for component in ('engine', 'oxa', 'lab'):
            for suffix in ('src/code.rs', 'tests/test_unit.py', 'Cargo.lock'):
                with self.subTest(component=component, suffix=suffix):
                    got = paths.classify([component + '/' + suffix])
                    self.assertEqual({key for key in ('engine', 'oxa', 'lab') if got[key]}, {component})
                    self.assertFalse(got['full'])

    def test_docs_only_and_mixed_changes(self):
        got = paths.classify(['README.md', 'lab/docs/page.md', 'oxa/docs/page.rst'], True)
        self.assertTrue(got['docs'])
        self.assertFalse(got['full'])
        self.assertFalse(any(got[key] for key in ('engine', 'oxa', 'lab')))
        got = paths.classify(['lab/docs/page.md', 'oxa/src/code.rs'])
        self.assertTrue(got['oxa'])
        self.assertFalse(got['lab'])
        self.assertFalse(got['docs'])

    def test_unknown_workflow_and_tooling_changes_test_every_product(self):
        for path in ('.github/workflows/checks.yml', 'tools/ci/paths.py', 'new-product/code',
                     'engine-other/src/code.rs', 'lab-notes.txt'):
            got = paths.classify([path])
            self.assertTrue(all(got[key] for key in ('engine', 'oxa', 'lab')), path)

    def test_pr_main_and_scheduled_source_runs_are_full(self):
        self.assertTrue(paths.classify(['lab/src/code.py'], True)['full'])
        self.assertFalse(paths.classify([], True)['docs'])
        self.assertTrue(paths.classify([], True)['full'])


class GitPaths(unittest.TestCase):
    def test_pr_uses_merge_base_and_nul_paths(self):
        event = {'pull_request': {'base': {'sha': 'a' * 40}, 'head': {'sha': 'b' * 40}}}
        with patch.object(paths.subprocess, 'check_output', return_value=b'oxa/src/a.rs\0lab/docs/a.md\0') as run:
            self.assertEqual(paths.changed_paths(event, 'pull_request'), ['oxa/src/a.rs', 'lab/docs/a.md'])
        self.assertIn('a' * 40 + '...' + 'b' * 40, run.call_args.args[0])

    def test_new_branch_compares_to_main(self):
        event = {'before': '0' * 40, 'after': 'b' * 40}
        with patch.object(paths.subprocess, 'check_output', side_effect=['a' * 40 + '\n', b'engine/src/a.rs\0']):
            self.assertEqual(paths.changed_paths(event, 'push'), ['engine/src/a.rs'])

    def test_untrusted_commit_text_is_rejected_before_subprocess(self):
        with patch.object(paths.subprocess, 'check_output') as run:
            with self.assertRaises(ValueError):
                paths.changed_paths({'before': '--output=unsafe', 'after': 'b' * 40}, 'push')
            run.assert_not_called()
        self.assertEqual(paths.changed_paths({'deleted': True}, 'push'), [])
