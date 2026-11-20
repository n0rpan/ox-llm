import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "changelog.py"


class Changelog(unittest.TestCase):
    def run_renderer(self, files):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, text in files:
                (root / name).write_text(text, encoding="utf-8")
            return subprocess.run(
                [sys.executable, "-I", str(SCRIPT), directory],
                capture_output=True, text=True, encoding="utf-8",
            )

    def test_orders_fragments_without_dropping_entries(self):
        files = [
            ("z-last.md", "- Same text as another PR.\n- Last bullet.\n"),
            ("legacy-unreleased.md", "### Fixed\n\n- Earlier fix.\n"),
            ("README.md", "Instructions, not a release note.\n"),
            ("m-middle.md", "- Same text as another PR.\n"),
            ("a-first.md", "- Café `$(literal)`\n  with a continuation."),
        ]
        expected = (
            "## Changelog\n\n### Pull requests\n\n"
            "- Café `$(literal)`\n  with a continuation.\n\n"
            "- Same text as another PR.\n\n"
            "- Same text as another PR.\n- Last bullet.\n\n"
            "### Fixed\n\n- Earlier fix.\n"
        )
        for order in (files, list(reversed(files))):
            with self.subTest(first=order[0][0]):
                result = self.run_renderer(order)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, expected)

    def test_no_pending_fragments_produce_no_section(self):
        result = self.run_renderer([("README.md", "Instructions\n")])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "")

    def test_invalid_fragment_fails_without_partial_notes(self):
        for name, content in [("misspelled.txt", "- A fix.\n"), ("empty.md", "\n")]:
            with self.subTest(name=name):
                result = self.run_renderer([("good.md", "- A fix.\n"), (name, content)])
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(name, result.stderr)
                self.assertEqual(result.stdout, "")
