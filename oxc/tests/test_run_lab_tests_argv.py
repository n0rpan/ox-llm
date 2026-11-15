"""run_lab_tests.sh forwards unittest options unchanged; only bare module operands are rewritten."""
import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

LAB = Path(__file__).resolve().parents[1]
SCRIPT = LAB / 'tools/run_lab_tests.sh'


def forwarded(*args):
    """The argv the script hands to python3 (a stub python3 on PATH prints it, one per line)."""
    with tempfile.TemporaryDirectory() as tmp:
        stub = Path(tmp) / 'python3'
        stub.write_text('#!/bin/sh\nfor a in "$@"; do printf "%s\\n" "$a"; done\n')
        stub.chmod(stub.stat().st_mode | stat.S_IXUSR)
        env = dict(os.environ, PATH=f'{tmp}:{os.environ["PATH"]}')
        env.pop('LAB_TEST_SHARD', None)
        out = subprocess.run(['sh', str(SCRIPT), *args], capture_output=True, text=True, env=env,
                             check=True, timeout=30).stdout
    return out.splitlines()


class RunLabTestsArgv(unittest.TestCase):
    def test_bare_module_is_rewritten(self):
        self.assertEqual(forwarded('test_run_lab_tests_argv'), ['-m', 'unittest', 'tests.test_run_lab_tests_argv'])

    def test_repeated_k_values_pass_through_unchanged(self):
        argv = forwarded('discover', '-s', 'tests', '-k', 'test_run_lab_tests_argv', '-k', 'second_filter')
        self.assertEqual(argv, ['-m', 'unittest', 'discover', '-s', 'tests', '-k',
                                'test_run_lab_tests_argv', '-k', 'second_filter'])

    def test_k_value_is_not_rewritten_without_discover_either(self):
        argv = forwarded('-k', 'test_run_lab_tests_argv', 'test_run_lab_tests_argv')
        self.assertEqual(argv, ['-m', 'unittest', '-k', 'test_run_lab_tests_argv', 'tests.test_run_lab_tests_argv'])

    def test_quoted_arguments_survive(self):
        argv = forwarded('discover', '-p', 'test_x*.py', '-k', 'a b')
        self.assertEqual(argv[-2:], ['-k', 'a b'])


if __name__ == '__main__':
    unittest.main()
