"""Only PR events may cancel superseded runs (CONTRIBUTING.md).

A cancelled run is reported as a failure by the outside checker that dispatches the
lane workflow, so a workflow that cancels in-progress runs on a dispatch or a push
turns a green tree red. Every workflow must therefore either never cancel
(`false`) or cancel only on pull requests.
"""
from pathlib import Path
import re
import unittest

WORKFLOWS = Path(__file__).resolve().parents[3] / '.github' / 'workflows'
PR_ONLY = "${{ github.event_name == 'pull_request' }}"
ALLOWED = ('false', PR_ONLY)


class WorkflowConcurrency(unittest.TestCase):
    def test_only_pull_requests_cancel_superseded_runs(self):
        for path in sorted(WORKFLOWS.glob('*.y*ml')):
            text = path.read_text()
            for value in re.findall(r'^\s*cancel-in-progress:\s*(.+?)\s*$', text, re.M):
                self.assertIn(
                    re.sub(r'\s+', ' ', value).strip(),
                    ALLOWED,
                    f'{path.name} cancels superseded runs outside pull requests',
                )

    def test_the_lane_workflow_keeps_a_dispatched_run_alive(self):
        # The lane is dispatched by the outside checker; cancelling it reports a
        # green tree as red. It has no pull_request trigger, so it never cancels.
        text = (WORKFLOWS / 'lane.yml').read_text()
        self.assertIn('cancel-in-progress: false', text)
        self.assertNotRegex(text, r'cancel-in-progress:\s*true')


if __name__ == '__main__':
    unittest.main()
