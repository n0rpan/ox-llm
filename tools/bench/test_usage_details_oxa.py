"""Tests for usage_details_oxa. Run: python3 -I -m unittest discover -s tools/bench"""

import json
import os
import tempfile
import unittest

import usage_details_oxa as parser

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURE = os.path.join(HERE, "fixtures", "usage.jsonl")


def _record(request, prompt, cached, output, usage_reported=True):
    return {
        "request": request,
        "turn": request,
        "kind": "chat",
        "model": "example-model",
        "usage_reported": usage_reported,
        "prompt": prompt if usage_reported else None,
        "cached": cached if usage_reported else None,
        "output": output if usage_reported else None,
        "reasoning_tokens": None,
        "duration_ms": 1000,
    }


class UsageDetailsTests(unittest.TestCase):
    def _write(self, directory, text):
        path = os.path.join(directory, "usage.jsonl")
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text)
        return path

    def test_parser_reads_the_fixture_log(self):
        details = parser.parse_usage_log(FIXTURE)
        self.assertEqual(details["calls"], 3)
        self.assertEqual(details["unreported_calls"], 0)
        self.assertEqual(details["input"], 1000 + 1200 + 1500)
        self.assertEqual(details["cached"], 0 + 1000 + 1200)
        self.assertEqual(details["output"], 50 + 40 + 80)
        self.assertEqual(details["first_call_cached"], 0)
        self.assertEqual(details["duration_ms"], 1200 + 900 + 1500)
        self.assertAlmostEqual(details["cache_hit_rate_normalized"], 2200 / 3700)
        # 1500 fresh input at 3.00, 2200 cached at 0.30, 170 output at 15.00, per million.
        self.assertAlmostEqual(details["cost_first_cold_usd"], 0.00771)

    def test_first_call_cached_is_excluded_like_the_benchmark(self):
        records = [_record(1, 1000, 800, 50), _record(2, 1300, 1000, 40)]
        details = parser.summarize(records)
        self.assertEqual(details["first_call_cached"], 800)
        # The first call's cached tokens leave the rate: (1800 - 800) / 2300.
        self.assertAlmostEqual(details["cache_hit_rate_normalized"], 1000 / 2300)
        # The first call's cached tokens are priced as fresh: 1300 fresh, 1000 cached, 90 output.
        self.assertAlmostEqual(details["cost_first_cold_usd"], 0.00555)

    def test_parser_rejects_a_truncated_log(self):
        with open(FIXTURE, encoding="utf-8") as handle:
            lines = handle.read().splitlines(keepends=True)
        with tempfile.TemporaryDirectory() as directory:
            # The last line is cut in the middle of its JSON.
            cut = "".join(lines[:2]) + lines[2][: len(lines[2]) // 2]
            with self.assertRaises(parser.UsageLogError):
                parser.parse_usage_log(self._write(directory, cut))
            # Every line is whole but the final newline is missing.
            no_newline = "".join(lines)[:-1]
            with self.assertRaises(parser.UsageLogError):
                parser.parse_usage_log(self._write(directory, no_newline))
            # A missing request in the middle: request 3 without request 2.
            gap = lines[0] + lines[2]
            with self.assertRaises(parser.UsageLogError):
                parser.parse_usage_log(self._write(directory, gap))

    def test_empty_log_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(parser.UsageLogError):
                parser.parse_usage_log(self._write(directory, ""))

    def test_unreported_call_leaves_totals_and_cost_null(self):
        records = [_record(1, 1000, 0, 50), _record(2, None, None, None, usage_reported=False)]
        details = parser.summarize(records)
        self.assertEqual(details["unreported_calls"], 1)
        self.assertEqual(details["first_call_cached"], 0)
        self.assertIsNone(details["input"])
        self.assertIsNone(details["cached"])
        self.assertIsNone(details["cache_hit_rate_normalized"])
        self.assertIsNone(details["cost_first_cold_usd"])

    def test_omitted_cached_count_leaves_cache_and_cost_null(self):
        records = [_record(1, 1000, 800, 50), _record(2, 1000, None, 40)]
        details = parser.summarize(records)
        self.assertEqual(details["first_call_cached"], 800)
        self.assertIsNone(details["cached"])
        self.assertIsNone(details["cache_hit_rate_normalized"])
        self.assertIsNone(details["cost_first_cold_usd"])

    def test_cached_above_prompt_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            bad = json.dumps(_record(1, 100, 200, 5)) + "\n"
            with self.assertRaises(parser.UsageLogError):
                parser.parse_usage_log(self._write(directory, bad))

    def test_main_exits_2_on_a_truncated_log(self):
        with tempfile.TemporaryDirectory() as directory:
            path = self._write(directory, '{"request":1')
            self.assertEqual(parser.main([path]), 2)


if __name__ == "__main__":
    unittest.main()
