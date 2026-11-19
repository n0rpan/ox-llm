"""Workflow steps install apt packages through tools/ci/apt_install.sh (retries, mirror fallback)."""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[3]
WORKFLOWS = ROOT / '.github' / 'workflows'
SCRIPT = ROOT / 'tools' / 'ci' / 'apt_install.sh'


class AptInstall(unittest.TestCase):
    def test_workflows_never_call_apt_get_directly(self):
        for path in sorted(WORKFLOWS.glob('*.yml')):
            self.assertNotRegex(path.read_text(), r'apt-get (update|install)',
                                f'{path.name}: use tools/ci/apt_install.sh, which retries mirror failures')

    def test_bwrap_steps_use_the_script_and_keep_the_hard_check(self):
        for name, count in (('checks.yml', 3), ('lane.yml', 1), ('build.yml', 1)):
            text = (WORKFLOWS / name).read_text()
            self.assertEqual(len(re.findall(r'tools/ci/apt_install\.sh bubblewrap', text)), count, name)
        for name in ('checks.yml', 'lane.yml', 'build.yml'):
            self.assertIn('bwrap --unshare-all --ro-bind / / true', (WORKFLOWS / name).read_text())

    def test_script_retries_with_backoff_and_falls_back_to_azure(self):
        text = SCRIPT.read_text()
        self.assertIn('Acquire::Retries=5', text)
        self.assertIn('for attempt in 1 2 3', text)
        self.assertIn('sleep "$delay"', text)
        self.assertIn('azure.archive.ubuntu.com', text)
        self.assertRegex(text, r'(?m)^exit 1$')
        self.assertTrue(SCRIPT.stat().st_mode & 0o111, 'apt_install.sh must be executable')


if __name__ == '__main__':
    unittest.main()
