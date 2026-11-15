"""Cancel accounting bounds hidden reasoning tokens by elapsed time; secrets-file repo detection."""
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from oxc.runtime import cloud

ITEM = {'id': 'm', 'provider': 'openai', 'context_window': 1000, 'max_output': 4096,
        'pricing': {'input_per_million': 1.0, 'output_per_million': 10.0}}


def settle(row, elapsed, streamed=10, output=4096, hold_usd=1.0):
    hold = cloud.BudgetHold('d', hold_usd, output, started=0.0)
    return cloud.cancelled_cost(row, hold, ITEM, 100, streamed, elapsed)


class CancelCost(unittest.TestCase):
    def test_quick_cancel_charges_elapsed_bound(self):
        row = settle({}, 2.0)
        self.assertGreaterEqual(row['cost_usd'], cloud.cost(ITEM, 100, 600))

    def test_long_cancel_is_capped_by_reservation_output(self):
        row = settle({}, 3600.0)
        self.assertAlmostEqual(row['cost_usd'], cloud.cost(ITEM, 100, 4096))

    def test_reported_usage_wins(self):
        row = settle({'prompt_tokens': 5, 'completion_tokens': 7, 'cost_usd': 0.0001}, 3600.0)
        self.assertEqual(row['cost_usd'], 0.0001)
        self.assertEqual(row['settled'], 'usage')

    def test_elapsed_uses_injectable_clock(self):
        hold = cloud.BudgetHold('d', 1.0, 10, started=5.0)
        with patch.object(cloud, 'monotonic', return_value=9.0):
            self.assertEqual(cloud.elapsed_since(hold), 4.0)


class RepoDetection(unittest.TestCase):
    def check(self, with_lab):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve() / 'tree'
            (root / 'engine').mkdir(parents=True)
            (root / 'AGENTS.md').write_text('x')
            if with_lab:
                (root / 'oxc').mkdir()
            with self.assertRaises(cloud.CloudError):
                cloud.secrets_file({'cloud': {'secrets_file': str(root / 'k.json')}})

    def test_refused_with_lab(self):
        self.check(True)

    def test_refused_without_lab(self):
        self.check(False)

    def test_outside_tree_accepted(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp).resolve() / 'k.json'
            self.assertEqual(cloud.secrets_file({'cloud': {'secrets_file': str(p)}}), p)


if __name__ == '__main__':
    unittest.main()
