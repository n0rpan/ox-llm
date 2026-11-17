"""Synthetic comparison fixtures only; no installed lab state is read in CI."""
from datetime import date
import importlib.util
from pathlib import Path
import tempfile
import unittest
from zoneinfo import ZoneInfo

SPEC = importlib.util.spec_from_file_location(
    'compare_shadow', Path(__file__).resolve().parents[2] / 'work/compare_shadow.py')
compare = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(compare)


class ShadowComparisonTests(unittest.TestCase):
    def test_matches_and_differences_are_time_bounded(self):
        samples = [dict(schema_version=1, observed_at=1000, would_take=[
            dict(number=42, action='fix_required_ci'), dict(number=43, action='resolve_conflict')])]
        events = [(999, 43, 'resolve_conflict', 'night-watch.log'),
                  (1001, 42, 'fix_required_ci', 'night-watch.log'),
                  (1100, 44, 'ready', 'merge-flow.log'),
                  (1300, 43, 'resolve_conflict', 'night-watch.log')]
        rows = list(compare.differences(samples, events))
        self.assertEqual([(r['difference'], r['number']) for r in rows],
                         [('lab_only', 44), ('shadow_only', 43)])

    def test_lab_formats_and_midnight_without_retaining_private_text(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'merge-flow.log').write_text('23:59 enqueued #42 synthetic\n00:01 enqueued #43 synthetic\n')
            (root / 'auto-approve.log').write_text('00:02 approved #44 synthetic\n')
            (root / 'night-watch.log').write_text('00:03 worker idle -> PR #45 (Codex review findings to fix)\n')
            rows = compare.lab_events(root, date(2026, 1, 2), ZoneInfo('UTC'))
        self.assertEqual(rows[1][0] - rows[0][0], 120)
        self.assertEqual([r[2] for r in rows], ['ready'] * 3 + ['fix_findings'])
        self.assertNotIn('synthetic', str(rows))

    def test_unknown_action_is_reported_and_bad_schema_is_rejected(self):
        self.assertEqual(compare.action('unexpected operation'), 'unclassified')
        with self.assertRaises(ValueError):
            list(compare.differences([dict(schema_version=2)], []))
