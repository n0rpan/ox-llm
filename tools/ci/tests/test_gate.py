"""The `gate` job in checks.yml covers every job and every classifier output; caches save only on main.

Parsed line by line (no YAML library) so it runs with a bare `python3 -I`.
"""
import importlib.util
from pathlib import Path
import re
import unittest

WORKFLOWS = Path(__file__).resolve().parents[3] / '.github' / 'workflows'
SPEC = importlib.util.spec_from_file_location('paths', Path(__file__).resolve().parents[1] / 'paths.py')
PATHS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PATHS)

# Jobs outside the gate, with the reason.
UNGATED = {'gate': 'the gate itself', 'oxa-nightly-arm': 'nightly and manual opt-in only; never on PRs or the queue'}


def jobs(text):
    return re.findall(r'^  ([A-Za-z0-9_-]+):\s*$', text.split('\njobs:\n', 1)[1], re.M)


def job_block(text, name):
    body = text.split('\njobs:\n', 1)[1]
    return re.search(rf'^  {re.escape(name)}:\s*\n((?:(?:    .*|\s*)\n)*)', body + '\n', re.M).group(1)


class Gate(unittest.TestCase):
    def setUp(self):
        self.checks = (WORKFLOWS / 'checks.yml').read_text()

    def test_gate_needs_every_job(self):
        block = job_block(self.checks, 'gate')
        needs = set(re.search(r'^    needs: \[(.*)\]', block, re.M).group(1).replace(' ', '').split(','))
        self.assertIn('if: always()', block)
        self.assertEqual(needs, set(jobs(self.checks)) - set(UNGATED))

    def test_gate_checks_every_classifier_output(self):
        outputs = set(re.findall(r'^      ([a-z]+): \$\{\{ steps\.paths\.outputs\.\1 \}\}',
                                 job_block(self.checks, 'changes'), re.M))
        produced = set(PATHS.classify(['oxa/src/a.rs']))
        self.assertEqual(outputs, produced)
        checked = set(re.search(r"for k in \(([^)]*)\):\n\s*if flags", job_block(self.checks, 'gate'))
                      .group(1).replace("'", '').replace(' ', '').split(','))
        self.assertEqual(checked, produced)

    def test_scans_and_classifier_are_never_conditional(self):
        for name in ('changes', 'scans'):
            self.assertNotRegex(job_block(self.checks, name), r'(?m)^    if:', name)

    def test_gateway_executes_rust_tests_with_an_isolated_environment_and_network(self):
        block = job_block(self.checks, 'gateway')
        self.assertRegex(block, r'cargo test[^\n]*--all-features')
        self.assertIn('env -i ', block)
        self.assertIn('--unshare-net', block)

    def test_caches_restore_only_and_save_only_on_main(self):
        for path in sorted(WORKFLOWS.glob('*.y*ml')):
            text = path.read_text()
            self.assertNotRegex(text, r'actions/cache@', path.name)
            for match in re.finditer(r'actions/cache/save@.*\n(?:\s+#.*\n)?\s+if: (.*)', text):
                self.assertIn("github.ref == 'refs/heads/main'", match.group(1), path.name)
                self.assertIn("github.event_name != 'merge_group'", match.group(1), path.name)
            self.assertEqual(len(re.findall(r'actions/cache/save@', text)),
                             len(re.findall(r'actions/cache/restore@', text)), path.name)


if __name__ == '__main__':
    unittest.main()
