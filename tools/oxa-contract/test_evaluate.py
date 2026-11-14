"""Controls for evaluate.py (the oxa contract job's verdict). Run: python3 -I -m unittest discover -s tools/oxa-contract -p 'test_*.py'
Positive: a strict all-pass run, a run whose failures are exactly the xfail list. Negative: the two
false-greens Codex reproduced (a SIGABRT after one reported test; a missing expected test), plus an
unexpected failure, a fixed xfail, an extra test, a missing completion marker, and an odd exit code."""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import evaluate  # noqa: E402

ALL = {'a_one', 'b_two', 'c_three'}


def log(results, marker=True, tail=''):
    lines = ['running %d tests' % len(results)]
    lines += ['test %s ... %s' % (n, 'ok' if ok else 'FAILED') for n, ok in results]
    p = sum(1 for _, ok in results if ok)
    f = len(results) - p
    if marker:
        lines.append('')
        lines.append('test result: %s. %d passed; %d failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s'
                     % ('FAILED' if f else 'ok', p, f))
    return '\n'.join(lines) + '\n' + tail


class Controls(unittest.TestCase):
    def check(self, text, rc, xfail=(), want=True, needle=''):
        ok, line = evaluate.verdict(text, rc, ALL, set(xfail))
        self.assertEqual(ok, want, line)
        self.assertIn(needle, line)
        return line

    # positive controls
    def test_strict_all_pass(self):
        self.check(log([('a_one', True), ('b_two', True), ('c_three', True)]), 0, needle='PASS 3/3')

    def test_expected_failures_exactly(self):
        self.check(log([('a_one', True), ('b_two', False), ('c_three', False)]), 101, xfail={'b_two', 'c_three'},
                   needle='2 expected failures')

    # negative controls: Codex's two false-greens
    def test_sigabrt_after_one_reported_test(self):
        text = ('running 3 tests\ntest a_one ... ok\n'
                "error: test failed, to rerun pass `-p oxa-core --test zz_stub_contract`\n\nCaused by:\n"
                "  process didn't exit successfully: `zz_stub_contract` (signal: 6, SIGABRT: process abort signal)\n")
        self.check(text, 134, want=False, needle='FAIL')
        self.check(text, 101, want=False, needle='died')  # cargo's own exit code for the same death

    def test_missing_expected_test(self):
        self.check(log([('a_one', True), ('b_two', True)]), 0, want=False, needle='missing tests: c_three')

    # further negatives
    def test_unexpected_failure(self):
        self.check(log([('a_one', True), ('b_two', False), ('c_three', True)]), 101, want=False, needle='unexpected failures: b_two')

    def test_xfail_now_passing(self):
        self.check(log([('a_one', True), ('b_two', True), ('c_three', True)]), 0, xfail={'b_two'}, want=False,
                   needle='now pass')

    def test_extra_test(self):
        self.check(log([('a_one', True), ('b_two', True), ('c_three', True), ('d_four', True)]), 0, want=False,
                   needle='extra tests: d_four')

    def test_no_completion_marker(self):
        self.check(log([('a_one', True), ('b_two', True), ('c_three', True)], marker=False), 0, want=False,
                   needle='completion marker')

    def test_nonzero_exit_with_clean_log(self):
        self.check(log([('a_one', True), ('b_two', True), ('c_three', True)]), 1, want=False, needle='exit code 1')
        self.check(log([('a_one', True), ('b_two', True), ('c_three', True)]), 137, want=False, needle='exit code 137')

    def test_marker_counts_must_match(self):
        text = log([('a_one', True), ('b_two', True), ('c_three', True)]).replace('3 passed', '4 passed')
        self.check(text, 0, want=False, needle='do not match')


if __name__ == '__main__':
    unittest.main()
