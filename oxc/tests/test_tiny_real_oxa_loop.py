"""oxa_tiny_loop.py checks are fail-closed: synthetic oxa RPC records only (no oxa, no model)."""
import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location(
    'oxa_tiny_loop', Path(__file__).resolve().parents[1] / 'tools/tiny_real/oxa_tiny_loop.py')
loop = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(loop)

FIXED = loop.CALC.replace('return a - b', 'return a + b')


def call(cid, tool, args, text, is_error=False):
    """A tool_execution_start / _end pair as oxa's RPC mode writes them (pi's record shapes)."""
    start = {'type': 'tool_execution_start', 'toolCallId': cid, 'toolName': tool, 'args': args}
    end = {'type': 'tool_execution_end', 'toolCallId': cid, 'toolName': tool,
           'result': {'content': [{'type': 'text', 'text': text}], 'details': {}}, 'isError': is_error}
    return [start, end]


def good_reads():
    return call('r1', 'read', {'path': 'calc.py'}, loop.CALC) + call('r2', 'read', {'path': 'notes.txt'}, loop.NOTES)


class ReadTests(unittest.TestCase):
    def test_exact_reads_pass(self):
        self.assertTrue(loop.check_read(good_reads())[0])

    def test_failed_read_fails(self):
        recs = call('r1', 'read', {'path': 'calc.py'}, 'Could not read file: calc.py. ENOENT', True) + \
            call('r2', 'read', {'path': 'notes.txt'}, loop.NOTES)
        ok, why = loop.check_read(recs)
        self.assertFalse(ok)
        self.assertIn('calc.py', why)

    def test_error_flag_missing_is_not_success(self):
        recs = good_reads()
        del recs[1]['isError']
        self.assertFalse(loop.check_read(recs)[0])

    def test_wrong_content_fails(self):
        recs = call('r1', 'read', {'path': 'calc.py'}, loop.CALC.replace('a - b', 'a * b')) + \
            call('r2', 'read', {'path': 'notes.txt'}, loop.NOTES)
        self.assertFalse(loop.check_read(recs)[0])

    def test_partial_read_does_not_count(self):
        recs = call('r1', 'read', {'path': 'calc.py', 'offset': 3, 'limit': 2}, loop.CALC) + \
            call('r2', 'read', {'path': 'notes.txt'}, loop.NOTES)
        self.assertFalse(loop.check_read(recs)[0])

    def test_no_read_fails(self):
        self.assertFalse(loop.check_read([])[0])


class EditTests(unittest.TestCase):
    def edit(self, is_error=False):
        return call('e1', 'edit', {'path': 'calc.py', 'oldText': 'a - b', 'newText': 'a + b'}, 'Edited calc.py', is_error)

    def test_exact_fix_passes(self):
        self.assertTrue(loop.check_edit(self.edit(), FIXED)[0])

    def test_failed_edit_fails_even_if_file_is_right(self):
        self.assertFalse(loop.check_edit(self.edit(is_error=True), FIXED)[0])

    def test_extra_change_fails(self):
        self.assertFalse(loop.check_edit(self.edit(), FIXED + '# extra\n')[0])
        self.assertFalse(loop.check_edit(self.edit(), FIXED.replace('add(2, 3)', 'add(2, 4)'))[0])

    def test_unchanged_file_fails(self):
        self.assertFalse(loop.check_edit(self.edit(), loop.CALC)[0])


class BashTests(unittest.TestCase):
    SEED = '0123456789abcdef'

    def expect(self):
        return loop.bash_expect(self.SEED)

    def run_fixed(self):
        """Run the real fixed calc.py with the seed: the expected line is what it prints."""
        import subprocess, sys, tempfile
        with tempfile.TemporaryDirectory() as d:
            Path(d, 'calc.py').write_text(FIXED)
            Path(d, '.seed').write_text(self.SEED + '\n')
            return subprocess.run([sys.executable, '-I', 'calc.py'], cwd=d, capture_output=True, text=True).stdout

    def test_expected_line_is_what_fixed_calc_prints(self):
        self.assertEqual(self.run_fixed().strip(), self.expect())

    def test_expected_output_passes(self):
        out = self.run_fixed()
        for cmd in ('python3 calc.py', 'cd /tmp/x && python3 calc.py 2>&1', '/usr/bin/python3 ./calc.py'):
            with self.subTest(cmd=cmd):
                self.assertTrue(loop.check_bash(call('b1', 'bash', {'command': cmd}, out), self.expect(), out)[0])

    def test_failed_bash_fails(self):
        recs = call('b1', 'bash', {'command': 'python3 calc.py'}, self.expect() + '\nCommand exited with code 1', True)
        self.assertFalse(loop.check_bash(recs, self.expect())[0])

    def test_wrong_output_fails(self):
        recs = call('b1', 'bash', {'command': 'python3 calc.py'}, 'add(2, 3) = 5 check 000000000000\n')
        self.assertFalse(loop.check_bash(recs, self.expect())[0])

    def test_unfixed_code_output_fails(self):
        recs = call('b1', 'bash', {'command': 'python3 calc.py'}, 'add(2, 3) = -1 check abc\n')
        self.assertFalse(loop.check_bash(recs, self.expect())[0])

    def test_echo_only_forgery_fails(self):
        e = self.expect()
        for cmd in ('echo "{}"  # calc.py'.format(e), 'echo calc.py; echo "{}"'.format(e), 'printf "{}" calc.py'.format(e),
                    'python3 calc.py; echo "{}"'.format(e), 'cat out.txt calc.py', 'python3 calc.py > /dev/null; cat <<EOF\n{}\nEOF'.format(e)):
            with self.subTest(cmd=cmd):
                self.assertFalse(loop.check_bash(call('b1', 'bash', {'command': cmd}, e + '\n'), e)[0])

    def test_other_command_fails(self):
        e = self.expect()
        self.assertFalse(loop.check_bash(call('b1', 'bash', {'command': 'python3 -c "import calc"'}, e), e)[0])

    def test_independent_rerun_must_agree(self):
        e = self.expect()
        recs = call('b1', 'bash', {'command': 'python3 calc.py'}, e)
        self.assertFalse(loop.check_bash(recs, e, rerun='add(2, 3) = -1 check x')[0])
        self.assertTrue(loop.check_bash(recs, e, rerun=e + '\n')[0])


class RerunBoundaryTests(unittest.TestCase):
    """Codex P2 (a75e7e0): the driver must never execute model-controlled files on the host."""
    SEED = 'feedfacecafebeef'

    def setUp(self):
        import tempfile
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = Path(self.tmp.name, 'repo')
        self.repo.mkdir()
        (self.repo / '.seed').write_text(self.SEED + '\n')
        self.marker = Path(self.tmp.name, 'EXECUTED')

    def tearDown(self):
        self.tmp.cleanup()

    def good_recs(self):
        return call('b1', 'bash', {'command': 'python3 calc.py'}, loop.bash_expect(self.SEED) + '\n')

    def test_pinned_copy_passes(self):
        (self.repo / 'calc.py').write_text(loop.FIXED)
        self.assertTrue(loop.bash_verdict(self.good_recs(), self.repo, self.SEED)[0])

    def test_file_modified_after_edit_check_is_not_executed_and_fails(self):
        evil = loop.FIXED + 'open({!r}, "w").write("x")\n'.format(str(self.marker))
        (self.repo / 'calc.py').write_text(evil)
        ok, why = loop.bash_verdict(self.good_recs(), self.repo, self.SEED)
        self.assertFalse(ok)
        self.assertIn('not executed', why)
        self.assertFalse(self.marker.exists())
        out, note = loop.independent_rerun(self.repo, self.SEED)
        self.assertIsNone(out)
        self.assertFalse(self.marker.exists())

    def test_live_file_is_never_the_one_executed(self):
        # even when identical, the rerun runs the driver's copy in another directory
        (self.repo / 'calc.py').write_text(loop.FIXED)
        (self.repo / '.seed').write_text('different-seed\n')  # the live dir's seed would change the line
        out, _ = loop.independent_rerun(self.repo, self.SEED)
        self.assertIn(loop.bash_expect(self.SEED), out)

    def test_no_rerun_after_failed_or_invalid_bash(self):
        calls = []
        real = loop.independent_rerun
        loop.independent_rerun = lambda *a: calls.append(a) or ('', '')
        try:
            (self.repo / 'calc.py').write_text(loop.FIXED)
            e = loop.bash_expect(self.SEED)
            for recs in (call('b1', 'bash', {'command': 'python3 calc.py'}, e, True),
                         call('b1', 'bash', {'command': 'echo "{}"'.format(e)}, e),
                         call('b1', 'bash', {'command': 'python3 calc.py'}, 'wrong')):
                self.assertFalse(loop.bash_verdict(recs, self.repo, self.SEED)[0])
            self.assertEqual(calls, [])
        finally:
            loop.independent_rerun = real


class SubagentTests(unittest.TestCase):
    def test_success_passes(self):
        self.assertTrue(loop.check_subagent(call('s1', 'subagent', {'agent': 'helper'}, 'run calc.py, then tag'))[0])

    def test_error_or_empty_or_absent_fails(self):
        self.assertFalse(loop.check_subagent(call('s1', 'subagent', {}, 'unknown agent', True))[0])
        self.assertFalse(loop.check_subagent(call('s1', 'subagent', {'agent': 'helper'}, '  '))[0])
        self.assertFalse(loop.check_subagent([])[0])
        mixed = call('s1', 'subagent', {}, 'ok') + call('s2', 'subagent', {}, 'boom', True)
        self.assertFalse(loop.check_subagent(mixed)[0])


if __name__ == '__main__':
    unittest.main()
