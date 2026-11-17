"""Qualify the existing offscreen widget suite; optional skips are never evidence.

Run only in hosted CI with Qt installed and an isolated HOME/XDG environment. The
preview uses synthetic fixtures and stand-in Process/Quickshell types, not the
installed widget or controller. No legacy controller modules are imported.
"""
import json
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[2]
THP_TEST = ('test_panel_v013_states.PanelV013States.'
            'test_v_cache_thp_shared_setting_default_toggle_and_failure')


def test_ids(suite):
    for test in suite:
        if isinstance(test, unittest.TestSuite):
            yield from test_ids(test)
        else:
            yield test.id()


def qualify(suite, stream):
    inventory = list(test_ids(suite))
    if THP_TEST not in inventory or len(inventory) != len(set(inventory)):
        print('Widget qualification requires a unique inventory including ' + THP_TEST,
              file=stream)
        return False
    result = unittest.TextTestRunner(stream=stream, verbosity=2).run(suite)
    passed = (result.wasSuccessful() and result.testsRun == len(inventory)
              and not result.skipped and not result.expectedFailures)
    print('PANEL-QUALIFICATION ' + json.dumps({
        'qualified': passed, 'expected': len(inventory), 'run': result.testsRun,
        'skipped': len(result.skipped), 'expected_failures': len(result.expectedFailures),
        'failures': len(result.failures), 'errors': len(result.errors),
        'required_test': THP_TEST,
    }, sort_keys=True), file=stream)
    return passed


def main():
    suite = unittest.TestLoader().discover(
        str(ROOT / 'oxc/tests'), pattern='test_panel_v013_states.py')
    return 0 if qualify(suite, sys.stderr) else 1


if __name__ == '__main__':
    sys.exit(main())
