"""The installed runtime CLI can report its version without a configured model."""
from pathlib import Path
import os
import subprocess
import sys
import tempfile
import unittest


class Version(unittest.TestCase):
    def test_version_with_empty_home(self):
        command = Path(__file__).resolve().parents[1] / 'src/snapdragon_ai_lab/runtime/local-ai'
        with tempfile.TemporaryDirectory() as home:
            result = subprocess.run([sys.executable, str(command), '--version'],
                                    env={'HOME': home, 'PATH': os.defpath},
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, 'local-ai 0.2.0\n')
            self.assertEqual(result.stderr, '')
            self.assertEqual(list(Path(home).iterdir()), [])
