"""v0.1.4 (OEM-DONE R3): a memory-guard stop reads as a sentence in `local-ai status --json`.

The controller keeps the stable reason code (ledger, engine log, drills); only the CLI's
allowlisted last_failure shows the sentence. No controller, no network.
"""
import importlib.machinery
import importlib.util
from pathlib import Path
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'src/snapdragon_ai_lab/runtime/local-ai'
loader = importlib.machinery.SourceFileLoader('guard_text_local_ai', str(SCRIPT))
spec = importlib.util.spec_from_loader(loader.name, loader)
cli = importlib.util.module_from_spec(spec)
loader.exec_module(cli)


class GuardReasonText(unittest.TestCase):
    def test_every_guard_code_becomes_a_short_sentence(self):
        for code, text in cli.GUARD_REASON_TEXT.items():
            row = cli.failure_row({'reason': code, 'kind': 'guard', 't': 1_800_000_000}, {})
            self.assertEqual(row['reason'], text)
            self.assertNotIn('_', row['reason'].split('`')[0])
            self.assertLessEqual(len(row['reason']), 200)
            self.assertTrue(row['reason'].endswith('.'))

    def test_other_reasons_pass_unchanged(self):
        row = cli.failure_row({'reason': 'Ornith did not start: not enough free RAM', 't': 1}, {})
        self.assertEqual(row['reason'], 'Ornith did not start: not enough free RAM')


if __name__ == '__main__':
    unittest.main()
