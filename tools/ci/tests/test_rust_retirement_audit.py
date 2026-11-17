import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import rust_retirement_audit as audit
sys.path.pop(0)


class RustRetirementAudit(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name) / 'checkout'
        self.destination = self.root.parent / 'report'
        self.source = self.root / 'oxa/src/lib.rs'
        self.source.parent.mkdir(parents=True)
        self.source.write_text('pub mod native;\npub fn active() {}\n')

    def run_audit(self, dependency_exit, dependency_output):
        def run(command, **kwargs):
            if command == ['cargo', 'machete']:
                return subprocess.CompletedProcess(command, dependency_exit, dependency_output)
            self.assertEqual(command, ['cargo', 'metadata', '--no-deps', '--locked', '--format-version', '1'])
            return subprocess.CompletedProcess(command, 0, '{}', '')

        with patch.object(audit, '__file__', str(self.root / 'tools/ci/rust_retirement_audit.py')), \
                patch.object(sys, 'argv', ['rust_retirement_audit.py', str(self.destination)]), \
                patch.object(audit.subprocess, 'check_output', side_effect=[b'oxa/src/lib.rs\0', 'a' * 40]), \
                patch.object(audit.subprocess, 'run', side_effect=run):
            audit.main()

    def test_public_modules_and_dependency_candidates_are_recorded(self):
        self.run_audit(1, 'candidate: example-crate\n')
        report = json.loads((self.destination / 'source-inventory.json').read_text())
        self.assertEqual([item['item'] for item in report['public_items']], ['native', 'active'])
        self.assertEqual((self.destination / 'oxa-machete-exit.txt').read_text(), '1\n')

    def test_manifest_errors_fail_even_with_success_or_candidate_exit(self):
        for code in (0, 1, 2):
            with self.subTest(code=code):
                message = 'error when handling ./example/Cargo.toml: synthetic parser error\n'
                with self.assertRaises(SystemExit):
                    self.run_audit(code, message)
                self.assertEqual((self.destination / 'oxa-machete.txt').read_text(), message)
