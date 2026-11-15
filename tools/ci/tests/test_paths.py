import importlib.util
import json
import os
import tempfile
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('paths', Path(__file__).resolve().parents[1] / 'paths.py')
paths = importlib.util.module_from_spec(spec)
spec.loader.exec_module(paths)


class Paths(unittest.TestCase):
    def test_component_source_and_test_changes(self):
        for component, directory in (('engine', 'engine'), ('oxa', 'oxa'), ('lab', 'oxc')):
            for suffix in ('src/code.rs', 'tests/test_unit.py', 'Cargo.lock'):
                with self.subTest(component=component, suffix=suffix):
                    got = paths.classify([directory + '/' + suffix])
                    self.assertEqual({key for key in ('engine', 'oxa', 'lab') if got[key]}, {component})
                    self.assertFalse(got['full'])

    def test_docs_only_and_mixed_changes(self):
        got = paths.classify(['README.md', 'oxc/docs/page.md', 'oxa/docs/page.rst'], True)
        self.assertTrue(got['docs'])
        self.assertTrue(got['full'])
        self.assertFalse(any(got[key] for key in ('engine', 'oxa', 'lab')))
        got = paths.classify(['oxc/docs/page.md', 'oxa/src/code.rs'])
        self.assertTrue(got['oxa'])
        self.assertFalse(got['lab'])
        self.assertFalse(got['docs'])

    def test_runtime_markdown_is_source_for_branch_pr_and_main(self):
        for name in ('oxa/oxa-resources/builtin-skills/ox-llm/SKILL.md',
                     'oxa/oxa-resources/builtin-skills/oxa-extend/SKILL.md',
                     'oxc/src/prompts/system.md', 'engine/src/template.rst'):
            for full_event in (False, True):
                with self.subTest(name=name, full_event=full_event):
                    got = paths.classify([name], full_event)
                    self.assertFalse(got['docs'])
                    self.assertTrue(got[{'oxc': 'lab'}.get(name.split('/')[0], name.split('/')[0])])
                    self.assertEqual(got['full'], full_event)
        got = paths.classify(['README.md', 'oxa/oxa-resources/builtin-skills/ox-llm/SKILL.md'], True)
        self.assertFalse(got['docs'])
        self.assertTrue(got['full'])

    def test_unknown_workflow_and_tooling_changes_test_every_product(self):
        for path in ('.github/workflows/checks.yml', 'tools/ci/paths.py', 'new-product/code',
                     'engine-other/src/code.rs', 'lab-notes.txt'):
            got = paths.classify([path])
            self.assertTrue(all(got[key] for key in ('engine', 'oxa', 'lab')), path)

    def test_full_event_source_runs_and_empty_diffs_are_full(self):
        self.assertTrue(paths.classify(['oxc/src/code.py'], True)['full'])
        self.assertFalse(paths.classify([], True)['docs'])
        self.assertTrue(paths.classify([], True)['full'])

    def test_full_gate_is_independent_of_changed_paths(self):
        for event in ('merge_group', 'workflow_dispatch', 'schedule'):
            self.assertTrue(paths.full_gate(event, 'refs/heads/feature'))
        self.assertTrue(paths.full_gate('push', 'refs/heads/main'))
        self.assertFalse(paths.full_gate('push', 'refs/heads/feature'))
        self.assertFalse(paths.full_gate('pull_request', 'refs/pull/1/merge'))
        self.assertTrue(paths.classify(['README.md'], True)['full'])

    def test_event_entrypoint_preserves_full_gate_for_docs_and_merge_queue(self):
        cases = [('merge_group', 'refs/heads/queue', {}, True),
                 ('workflow_dispatch', 'refs/heads/feature', {}, True),
                 ('schedule', 'refs/heads/main', {}, True),
                 ('push', 'refs/heads/main', {'before': 'a' * 40, 'after': 'b' * 40}, True)]
        for draft in (False, True):
            cases.append(('pull_request', 'refs/pull/1/merge', {'pull_request': {
                'draft': draft, 'base': {'sha': 'a' * 40}, 'head': {'sha': 'b' * 40}}}, False))
        for event, ref, payload, full in cases:
            with self.subTest(event=event, ref=ref, draft=payload.get('pull_request', {}).get('draft')):
                with tempfile.TemporaryDirectory() as directory:
                    source, output = Path(directory) / 'event.json', Path(directory) / 'output'
                    source.write_text(json.dumps(payload))
                    with patch.dict(os.environ, {'GITHUB_EVENT_PATH': str(source),
                            'GITHUB_EVENT_NAME': event, 'GITHUB_REF': ref,
                            'GITHUB_OUTPUT': str(output)}, clear=True), \
                            patch.object(paths.subprocess, 'check_output', return_value=b'README.md\0'):
                        paths.main()
                    values = dict(line.split('=', 1) for line in output.read_text().splitlines())
                    self.assertEqual(values['full'], str(full).lower())
                    self.assertEqual(values['docs'], 'true')


class GitPaths(unittest.TestCase):
    def test_pr_uses_merge_base_and_nul_paths(self):
        event = {'pull_request': {'base': {'sha': 'a' * 40}, 'head': {'sha': 'b' * 40}}}
        with patch.object(paths.subprocess, 'check_output', return_value=b'oxa/src/a.rs\0oxc/docs/a.md\0') as run:
            self.assertEqual(paths.changed_paths(event, 'pull_request'), ['oxa/src/a.rs', 'oxc/docs/a.md'])
        self.assertIn('a' * 40 + '...' + 'b' * 40, run.call_args.args[0])

    def test_merge_queue_enumerates_sources_without_push_payload_fields(self):
        with patch.object(paths.subprocess, 'check_output', return_value=b'oxa/src/a.rs\0') as run:
            self.assertEqual(paths.changed_paths({'merge_group': {}}, 'merge_group'), ['oxa/src/a.rs'])
        self.assertEqual(run.call_args.args[0], ['git', 'ls-files', '-z'])

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
