"""doctor: additive coding-agent checks (bwrap, oxa --version, code endpoint) with injected PATH/home."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from oxc import doctor


def tool(folder, name, body):
    path = Path(folder) / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text('#!/bin/sh\n' + body + '\n')
    path.chmod(0o755)
    return path


class DoctorToolChecksTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.home = self.root / 'home'
        self.bin = self.root / 'bin'
        self.home.mkdir()
        self.bin.mkdir()

    def tearDown(self):
        self.tmp.cleanup()

    def test_nothing_present(self):
        value = doctor.tool_checks(path=str(self.bin), home=self.home)
        self.assertFalse(value['bwrap']['present'])
        self.assertIn('bubblewrap', value['bwrap']['note'])
        self.assertEqual(value['oxa'], {'present': False, 'path': None, 'version': None, 'error': None})
        self.assertEqual(value['code_endpoint'], 'http://127.0.0.1:8092')

    def test_bwrap_and_installed_oxa(self):
        tool(self.bin, 'bwrap', 'exit 0')
        oxa = tool(self.home / '.local/bin', 'oxa', 'echo "oxa 0.2.0"')
        value = doctor.tool_checks(path=str(self.bin), home=self.home)
        self.assertTrue(value['bwrap']['present'])
        self.assertIsNone(value['bwrap']['note'])
        self.assertEqual(value['oxa']['path'], str(oxa))
        self.assertEqual(value['oxa']['version'], 'oxa 0.2.0')

    def test_oxa_on_path(self):
        oxa = tool(self.bin, 'oxa', 'echo "oxa 9"')
        value = doctor.tool_checks(path=str(self.bin), home=self.home)
        self.assertEqual(value['oxa']['path'], str(oxa))
        self.assertEqual(value['oxa']['version'], 'oxa 9')

    def test_oxa_failure_does_not_raise(self):
        tool(self.bin, 'oxa', 'exit 7')
        value = doctor.tool_checks(path=str(self.bin), home=self.home)
        self.assertTrue(value['oxa']['present'])
        self.assertIsNone(value['oxa']['version'])
        self.assertIn('7', value['oxa']['error'])

    def test_oxa_timeout_is_bounded(self):
        tool(self.bin, 'oxa', 'exit 0')
        seen = {}

        def run(argv, **kwargs):
            seen.update(kwargs)
            raise subprocess.TimeoutExpired(argv, kwargs['timeout'])

        value = doctor.tool_checks(path=str(self.bin), home=self.home, run=run)
        self.assertEqual(seen['timeout'], 5)
        self.assertIn('timed out', value['oxa']['error'])

    def test_inspect_is_additive(self):
        value = doctor.inspect(path=str(self.bin), home=self.home)
        for key in ('schema_version', 'architecture', 'memory_bytes', 'power', 'platform_support',
                    'heavy_work_started'):
            self.assertIn(key, value)
        self.assertEqual(value['schema_version'], 1)
        self.assertIn('coding_agent', value)
        json.dumps(value)


if __name__ == '__main__':
    unittest.main()
