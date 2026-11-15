"""Review follow-up: refusal settlement, request-bounded reservation, ledger latch, price retry."""
import json
import unittest
from unittest.mock import patch

from mock_cloud import MockAnthropic, MockOpenRouter
from oxc.runtime import cloud, cloud_prices, request_ledger
from test_cloud_anthropic import AnthropicBase, anthropic_models
from test_cloud_remote import CloudCase, REMOTE, profiles


class CleanRefusalSettlesAtZero(CloudCase):
    def test_openai_compatible_refusals_cost_nothing(self):
        for status in (400, 401, 403, 404, 422, 429):
            with self.subTest(status=status):
                upstream = MockOpenRouter(status=status, error='refused')
                self.addCleanup(upstream.close)
                self.start(mock=upstream)
                self.assertEqual(self.chat()[0], status)
                row = self.ledger_rows()[-1]
                self.assertEqual(row['cost_usd'], 0.0)
                self.assertEqual(row['settled'], 'refused')
                self.assertEqual(cloud.spend_today(), 0.0)
                self.stack.close()
                self.stack = None
                for path in self.ledger.glob('requests-*.jsonl'):
                    path.unlink()

    def test_gateway_errors_may_have_generated_so_they_keep_the_worst_case(self):
        for status in (500, 502, 503, 529):
            with self.subTest(status=status):
                upstream = MockOpenRouter(status=status, error='boom')
                self.addCleanup(upstream.close)
                self.start(mock=upstream)
                self.assertEqual(self.chat()[0], status)
                row = self.ledger_rows()[-1]
                self.assertEqual(row['settled'], 'reserved')
                self.assertGreater(row['cost_usd'], 0)
                self.stack.close()
                self.stack = None
                for path in self.ledger.glob('requests-*.jsonl'):
                    path.unlink()

    def test_a_stream_that_dies_after_output_still_charges_the_worst_case(self):
        upstream = MockOpenRouter(usage_missing=True)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        self.assertEqual(self.chat(stream=True)[0], 200)
        row = self.ledger_rows()[-1]
        self.assertEqual(row['settled'], 'reserved')
        self.assertGreater(row['cost_usd'], 0)


class AnthropicRefusal(AnthropicBase):
    def test_refusals_cost_nothing(self):
        for status in (400, 401, 429):
            with self.subTest(status=status):
                upstream = MockAnthropic(status=status, error='refused')
                self.addCleanup(upstream.close)
                self.start(mock=upstream)
                for stream in (False, True):
                    self.assertEqual(self.chat(stream=stream)[0], status)
                    row = self.ledger_rows()[-1]
                    self.assertEqual(row['cost_usd'], 0.0, row)
                    self.assertEqual(row['settled'], 'refused')
                self.stack.close()
                self.stack = None
                for path in self.ledger.glob('requests-*.jsonl'):
                    path.unlink()


class AnthropicServerError(AnthropicBase):
    def test_server_errors_keep_the_worst_case(self):
        for status in (500, 529):
            with self.subTest(status=status):
                upstream = MockAnthropic(status=status, error='overloaded')
                self.addCleanup(upstream.close)
                self.start(mock=upstream)
                self.chat()
                row = self.ledger_rows()[-1]
                self.assertEqual(row['settled'], 'reserved', row)
                self.assertGreater(row['cost_usd'], 0)
                self.stack.close()
                self.stack = None
                for path in self.ledger.glob('requests-*.jsonl'):
                    path.unlink()


class RequestBoundedReservation(AnthropicBase):
    def test_responses_text_format_schema_is_reserved(self):
        small = {'input': 'hi', 'max_output_tokens': 10}
        schema = {'text': {'format': {'type': 'json_schema', 'schema': {'description': 'x' * 100000}}}}
        self.assertGreaterEqual(cloud.estimate_prompt_tokens({**small, **schema}),
                                cloud.estimate_prompt_tokens(small) + 100000)
        self.assertGreaterEqual(cloud.estimate_prompt_tokens({**small, 'instructions': 'y' * 5000}),
                                cloud.estimate_prompt_tokens(small) + 4900)

    def test_default_budget_admits_the_largest_window_for_a_small_request(self):
        upstream = MockAnthropic()
        self.addCleanup(upstream.close)
        self.start(mock=upstream, budget=1.0)   # opus: 200k window would reserve > 3 USD
        status, body = self.chat(model='anthropic/claude-opus-5-5')
        self.assertEqual(status, 200, body)

    def test_estimate_is_a_byte_upper_bound_capped_at_the_window(self):
        item = cloud.entry('anthropic/mock', anthropic_models()['anthropic/claude-opus-5-5'])
        text = 'é' * 1000 + '\U0001f600' * 100
        payload = {'messages': [{'role': 'user', 'content': text}], 'max_tokens': 10}
        self.assertGreaterEqual(cloud.estimate_prompt_tokens(payload), len(text.encode()))
        small = cloud.worst_case_cost(item, payload)
        full = cloud._reservation_cost(item, item['context_window'], 10)
        self.assertLess(small, full)
        huge = {'messages': [{'role': 'user', 'content': 'x' * 400000}], 'max_tokens': 10}
        self.assertEqual(cloud.worst_case_cost(item, huge), full)

    def test_non_text_content_reserves_the_full_window(self):
        item = cloud.entry('anthropic/mock', anthropic_models()['anthropic/claude-opus-5-5'])
        full = cloud._reservation_cost(item, item['context_window'], 10)
        for part in ({'type': 'image_url', 'image_url': {'url': 'data:image/png;base64,AA'}},
                     {'type': 'image', 'source': {'type': 'base64', 'data': 'AA'}},
                     {'type': 'file', 'file': {'file_id': 'f'}}):
            payload = {'messages': [{'role': 'user', 'content': [part]}], 'max_tokens': 10}
            with self.subTest(part=part['type']):
                self.assertIsNone(cloud.estimate_prompt_tokens(payload))
                self.assertEqual(cloud.worst_case_cost(item, payload), full)


class LedgerLatch(CloudCase):
    def test_latch_is_reported_and_clears_when_the_ledger_works_again(self):
        self.start()
        with patch.object(request_ledger, 'write', return_value=False):
            self.assertEqual(self.chat()[0], 200)
            status, body = self.call('GET', '/_local/status')
            cloud_status = json.loads(body)['cloud']
            self.assertFalse(cloud_status['ledger_ok'])
            self.assertTrue(cloud_status['ledger_latched'])
            status, body = self.chat()
            self.assertEqual(status, 503)
            message = json.loads(body)['error']['message']
            self.assertIn('Repair', message)
        before = cloud.spend_today()
        self.assertEqual(self.chat()[0], 200)
        # The spend whose row could not be written is restored when the latch clears.
        self.assertGreater(cloud.spend_today(), before + 0.0)
        cloud_status = json.loads(self.call('GET', '/_local/status')[1])['cloud']
        self.assertTrue(cloud_status['ledger_ok'])
        self.assertFalse(cloud_status['ledger_latched'])


class PriceRetry(unittest.TestCase):
    def test_failed_refresh_backs_off_then_goes_daily(self):
        refresher = cloud_prices.Refresher.__new__(cloud_prices.Refresher)
        refresher.allowed = lambda: True
        state = [0, 0]
        results = [False, False, False, False, False, True]
        due = []
        with patch.object(cloud_prices, 'refresh', side_effect=lambda: results.pop(0)):
            now = 0
            while results:
                refresher.step(now, state)
                due.append(state[0] - now)
                now = state[0]
        self.assertEqual(due, [60, 300, 1800, 3600, 3600, cloud_prices.DAY_SECONDS])
        self.assertEqual(state[1], 0)
        with patch.object(cloud_prices, 'refresh', side_effect=AssertionError):
            refresher.step(now - 10, state)   # not due yet


if __name__ == '__main__':
    unittest.main()
