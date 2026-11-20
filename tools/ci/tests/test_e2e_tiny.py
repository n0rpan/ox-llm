import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).parents[1] / "e2e_tiny.py"
SPEC = importlib.util.spec_from_file_location("e2e_tiny", SCRIPT)
e2e_tiny = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(e2e_tiny)


class E2eTinyUsageLogTests(unittest.TestCase):
    def test_each_invocation_reads_entire_replaced_usage_log(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            usage_log = out / "usage.jsonl"
            usage_log.write_text('{"request": 1, "output": 10}\n')

            def replace_log(*_args):
                usage_log.write_text('{"request": 1, "output": 20}\n')
                return 0, 12.0, {"type": "oxa_result"}

            with mock.patch.object(e2e_tiny, "run_oxa", side_effect=replace_log):
                _code, _wall, _result, invocation_records = e2e_tiny.run_oxa_with_usage(
                    "oxa", "prompt", out, {}, usage_log, 10, 5, 100, "turn2", out
                )

            self.assertEqual(invocation_records, [{"request": 1, "output": 20}])


if __name__ == "__main__":
    unittest.main()
