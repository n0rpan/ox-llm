#!/usr/bin/env python3
"""Fail-closed verdict of the oxa contract job (tools/oxa-contract/run.sh).

usage: evaluate.py --log TEST_LOG --rc EXIT_CODE --expected tests.txt [--xfail FILE]

PASS only when ALL hold:
- the test harness finished: exactly one `running N tests` line and one completion marker
  `test result: ok|FAILED. P passed; F failed; 0 ignored; ...` whose counts match the per-test lines;
  no `signal:` / abort line in the log;
- the exit code is 0, or 101 (cargo's code for failed tests) with a FAILED marker; any other code
  (a signal exit is 128+n) is FAIL whatever the log says;
- the executed tests (ok + FAILED lines) are exactly the pinned list (missing, extra, ignored = FAIL);
- the failed set equals the xfail list (empty without --xfail): an unexpected failure fails, and so
  does a listed test that passed (update the list).
Prints one line `OXA CONTRACT RESULT: PASS|FAIL ...`; exit 0 on PASS, 1 on FAIL.
"""
import argparse
import re
import sys


def names(path):
    if not path:
        return set()
    out = set()
    with open(path, encoding='utf-8') as f:
        for line in f:
            line = line.split('#', 1)[0].strip()
            if line:
                out.add(line)
    return out


def verdict(log, rc, expected, xfail):
    lines = log.splitlines()
    passed = {m.group(1) for l in lines if (m := re.fullmatch(r'test (\S+) \.\.\. ok', l))}
    failed = {m.group(1) for l in lines if (m := re.fullmatch(r'test (\S+) \.\.\. FAILED', l))}
    ignored = {m.group(1) for l in lines if (m := re.fullmatch(r'test (\S+) \.\.\. ignored.*', l))}
    running = [int(m.group(1)) for l in lines if (m := re.fullmatch(r'running (\d+) tests?', l))]
    markers = [m for l in lines if (m := re.fullmatch(
        r'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out;.*', l))]
    problems = []
    if any(re.search(r'signal: \d+|SIGABRT|SIGSEGV|SIGKILL|process didn\'t exit successfully', l) for l in lines):
        problems.append('the test process died (signal/abnormal exit in the log)')
    if len(running) != 1:
        problems.append('expected one "running N tests" line, saw %d' % len(running))
    if len(markers) != 1:
        problems.append('no single completion marker ("test result: ..."), saw %d' % len(markers))
    else:
        m = markers[0]
        status, p, f, i, _, filt = m.group(1), *map(int, m.groups()[1:])
        if (p, f) != (len(passed), len(failed)) or i or filt or (running and running[0] != p + f):
            problems.append('completion marker counts (%d passed, %d failed, %d ignored, %d filtered) do not match the '
                            'executed tests (%d ok, %d FAILED, running %s)' % (p, f, i, filt, len(passed), len(failed), running))
        if rc == 0 and status != 'ok':
            problems.append('exit 0 with a FAILED marker')
        if rc == 101 and status != 'FAILED':
            problems.append('exit 101 without a FAILED marker')
    if rc not in (0, 101):
        problems.append('exit code %d (signal or harness error)' % rc)
    executed = passed | failed
    if passed & failed:
        problems.append('tests reported both ok and FAILED: %s' % ','.join(sorted(passed & failed)))
    if ignored:
        problems.append('ignored: %s' % ','.join(sorted(ignored)))
    if executed - expected:
        problems.append('extra tests: %s' % ','.join(sorted(executed - expected)))
    if expected - executed:
        problems.append('missing tests: %s' % ','.join(sorted(expected - executed)))
    if xfail - expected:
        problems.append('xfail names not in the pinned list: %s' % ','.join(sorted(xfail - expected)))
    if failed - xfail:
        problems.append('unexpected failures: %s' % ','.join(sorted(failed - xfail)))
    if xfail & passed:
        problems.append('expected failures that now pass (update the xfail list): %s' % ','.join(sorted(xfail & passed)))
    if problems:
        return False, 'OXA CONTRACT RESULT: FAIL %d/%d passed; %s' % (len(passed), len(expected), '; '.join(problems))
    if failed:
        return True, 'OXA CONTRACT RESULT: PASS %d/%d passed, %d expected failures (oxa targets): %s' % (
            len(passed), len(expected), len(failed), ','.join(sorted(failed)))
    return True, 'OXA CONTRACT RESULT: PASS %d/%d' % (len(passed), len(expected))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--log', required=True)
    ap.add_argument('--rc', required=True, type=int)
    ap.add_argument('--expected', required=True)
    ap.add_argument('--xfail')
    a = ap.parse_args()
    with open(a.log, encoding='utf-8', errors='replace') as f:
        log = f.read()
    expected = names(a.expected)
    if not expected:
        print('OXA CONTRACT RESULT: FAIL the pinned test list is empty')
        return 1
    ok, line = verdict(log, a.rc, expected, names(a.xfail))
    print(line)
    return 0 if ok else 1


if __name__ == '__main__':
    sys.exit(main())
