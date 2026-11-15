"""WD-17 (v0.1.4): the panel polls `share status` every 5 s while the Storage tab (where the
sharing row lives) is open, honouring the poll switch and pause, besides the open/tab/action
refreshes. Static read of the repo Panel.qml; no Quickshell, no installed plugin."""
from pathlib import Path
import re
import unittest

PANEL = Path(__file__).resolve().parents[1] / 'src/oxc/omarchy_plugin/Panel.qml'


class SharePoll(unittest.TestCase):
    def test_share_timer_runs_only_while_storage_is_open(self):
        text = PANEL.read_text()
        match = re.search(r'Timer \{\s*id: shareTimer(.*?)\n    \}', text, re.S)
        self.assertIsNotNone(match, 'no shareTimer')
        body = match.group(1)
        self.assertIn('interval: 5000', body)
        self.assertIn('repeat: true', body)
        self.assertIn('onTriggered: root.refreshShare()', body)
        running = re.search(r'running:(.*?)\n\s*repeat', body, re.S).group(1)
        for term in ('root.opened', 'root.view === "storage"', 'root.setting("pollEnabled", true)',
                     '!root.pollingPaused'):
            self.assertIn(term, running)

    def test_open_tab_and_action_refreshes_stay(self):
        text = PANEL.read_text()
        self.assertEqual(text.count('root.refreshShare()'), 4)  # open, Storage tab, after an action, timer


if __name__ == '__main__':
    unittest.main()
