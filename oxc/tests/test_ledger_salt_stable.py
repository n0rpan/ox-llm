"""The install salt keys session hints and state-store keys: stable across restarts, ledger on or off."""
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from oxc.runtime import request_ledger as rl

HEADERS = {'X-Session-Id': 'sess-1'}


def restart():
    rl._salt[0] = None
    rl._throwaway[0] = None


class LedgerSaltTests(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.dir = self.tmp / 'ledger'
        self.env = patch.dict(os.environ, {'LOCAL_AI_LEDGER_DIR': str(self.dir), 'LOCAL_AI_LEDGER': '1'})
        self.env.start()
        restart()

    def tearDown(self):
        restart()
        self.env.stop()

    def key(self, on):
        os.environ['LOCAL_AI_LEDGER'] = '1' if on else '0'
        return rl.session_key(HEADERS, {})

    def test_off_writes_nothing(self):
        self.key(False)
        self.assertFalse(self.dir.exists())

    def test_key_same_on_off_and_across_restart(self):
        on = self.key(True)
        self.assertTrue((self.dir / '.salt').exists())
        restart()
        self.assertEqual(self.key(False), on)
        restart()
        self.assertEqual(self.key(True), on)

    def test_clear_by_hint_matches_after_restart(self):
        hint = self.key(True)
        restart()
        self.assertEqual(self.key(False), hint)

    def test_enabling_later_uses_file_salt_not_throwaway(self):
        off = self.key(False)
        self.assertFalse(self.dir.exists())
        on = self.key(True)
        self.assertNotEqual(on, off)
        restart()
        self.assertEqual(self.key(False), on)


if __name__ == '__main__':
    unittest.main()
