"""Runtime composition of the extracted refusal stages; accounting is separate."""
import json
from pathlib import Path
import unittest

from cli_harness import invoke_case
import test_cloud_gateway as gateway
from environment import ROOT


class CloudRefusalContract(unittest.TestCase):
    def test_engine_free_runtime_refusal_replies(self):
        expected = json.loads(gateway.GOLDENS.read_text())
        cases = ('router-disabled', 'router-offline', 'router-no-key', 'router-key-mode',
                 'router-key-symlink', 'router-no-price', 'router-budget', 'router-no-budget')
        policy = json.loads((ROOT / 'oxc-gateway/conformance/policy.json').read_text())
        for name in cases:
            with self.subTest(case=name):
                observed = invoke_case(name, gateway.__file__, timeout=45)
                if name == 'router-no-budget':
                    self.assertEqual({key: observed['reply'][key] for key in ('status', 'body')}, policy['no-budget'])
                else:
                    self.assertEqual(observed['reply'], expected[name]['reply'])
                self.assertEqual(observed['upstream'], [])
                # Remote rows/reservations/settlement belong to the accounting
                # slice. This subset compares only the public refusal and zero
                # forwarding, retaining the harness's foreign-lease checks.


if __name__ == '__main__':
    unittest.main()
