"""Synthetic pricing contracts; no provider API or real credentials."""
import unittest

from oxc.runtime import cloud
from test_cloud_remote import profiles, REMOTE
from test_cloud_anthropic import AnthropicBase, anthropic_models
from mock_cloud import MockAnthropic


def tiered():
    raw = profiles()[REMOTE]
    raw['pricing'] = {'tiers': [
        {'max_prompt_tokens': 100, 'input_per_million': 1, 'output_per_million': 2,
         'cache_read_per_million': 0.1, 'cache_write_per_million': 1.25},
        {'input_per_million': 5, 'output_per_million': 10,
         'cache_read_per_million': 0.5, 'cache_write_per_million': 6.25}]}
    return cloud.entry(REMOTE, raw)


class Pricing(unittest.TestCase):
    def test_actual_prompt_selects_the_inclusive_tier(self):
        self.assertEqual(cloud.cost(tiered(), 100, 10), 0.00012)
        self.assertEqual(cloud.cost(tiered(), 101, 10), 0.000605)

    def test_cache_reads_and_writes_have_distinct_rates(self):
        self.assertEqual(cloud.cost(tiered(), 100, 10, cache_read=60, cache_write=20), 0.000071)
        self.assertEqual(cloud.cost(tiered(), 101, 10, cache_read=60, cache_write=20), 0.00036)

    def test_admission_keeps_the_highest_known_window_and_write_bound(self):
        item = tiered()
        item['context_window'] = 100
        self.assertGreaterEqual(cloud.worst_case_cost(item, {'max_tokens': 10}),
                                (100 * 6.25 + 10 * 10) / 1e6)

    def test_invalid_tiers_are_unknown_and_never_free(self):
        for tiers in ([], [{'max_prompt_tokens': 10, 'input_per_million': 1, 'output_per_million': 2}],
                      [{'input_per_million': -1, 'output_per_million': 2}],
                      [{'max_prompt_tokens': True, 'input_per_million': 1, 'output_per_million': 2},
                       {'input_per_million': 3, 'output_per_million': 4}]):
            with self.subTest(tiers=tiers):
                raw = profiles()[REMOTE]
                raw['pricing'] = {'tiers': tiers}
                item = cloud.entry(REMOTE, raw)
                self.assertIsNone(cloud.pricing(item))
                with self.assertRaises(cloud.CloudError):
                    cloud.validate_remote_request({}, item)

    def test_invalid_cache_counts_never_create_a_discount(self):
        for read, write in ((-1, 0), (True, 0), (80, 80), (None, 0)):
            with self.subTest(read=read, write=write):
                self.assertGreaterEqual(cloud.cost(tiered(), 100, 10, cache_read=read, cache_write=write),
                                        cloud.cost(tiered(), 100, 10))

    def test_unknown_anthropic_cache_write_rate_uses_the_documented_long_ttl_bound(self):
        item = profiles()[REMOTE]
        item['provider'] = 'anthropic'
        self.assertEqual(cloud.cost(item, 100, 10, cache_write=100), 0.00021)

    def test_usage_meter_applies_cache_rates_and_rejects_inconsistent_counts(self):
        item = tiered()
        meter = cloud.Meter()
        meter._object({'usage': {'prompt_tokens': 100, 'completion_tokens': 10,
                                 'prompt_tokens_details': {'cached_tokens': 60, 'cache_write_tokens': 20}}})
        self.assertEqual(meter.row(item)['cost_usd'], 0.000071)
        self.assertEqual(meter.row(item)['cache_write_tokens'], 20)
        meter._object({'usage': {'cost': 0.5, 'prompt_tokens_details': {'cached_tokens': 999}}})
        row = cloud.conservative_cost(meter.row(item), cloud.BudgetHold('unused', 1))
        self.assertEqual(row['cost_usd'], 1)
        self.assertTrue(row['usage_missing'])

    def test_anthropic_preserves_write_counts_and_rejects_incomplete_usage(self):
        from oxc.runtime import cloud_anthropic
        usage = {'input_tokens': 20, 'output_tokens': 10,
                 'cache_read_input_tokens': 60, 'cache_creation_input_tokens': 20}
        normalized = cloud_anthropic._usage(usage)
        self.assertEqual(normalized['prompt_tokens'], 100)
        self.assertEqual(normalized['prompt_tokens_details']['cache_write_tokens'], 20)
        row = cloud_anthropic._usage_row({}, normalized, tiered())
        self.assertEqual(row['cost_usd'], 0.000071)
        self.assertFalse(cloud_anthropic._has_usage({'usage': {}}))
        self.assertFalse(cloud_anthropic._has_usage({'usage': dict(usage, cache_creation_input_tokens=-1)}))

    def test_small_nonzero_catalog_cost_is_not_rounded_to_free(self):
        item = profiles()[REMOTE]
        item['pricing'] = {'input_per_million': 0.001, 'output_per_million': 0.001}
        self.assertEqual(cloud.cost(item, 1, 0), 0.000000001)


class CacheLedger(AnthropicBase):
    def test_native_cache_write_and_read_counts_reach_the_ledger_in_both_modes(self):
        upstream = MockAnthropic()
        self.addCleanup(upstream.close)
        usage = {'input_tokens': 20, 'output_tokens': 10,
                 'cache_read_input_tokens': 60, 'cache_creation_input_tokens': 20}
        upstream.message['usage'] = usage
        events = upstream.default_stream()
        events[0][1]['message']['usage'] = dict(usage, output_tokens=0)
        events[-2][1]['usage'] = {'output_tokens': 10}
        upstream.stream_events = events
        models = anthropic_models()
        models['anthropic/claude-sonnet-5-5']['pricing'] = {
            'input_per_million': 1, 'output_per_million': 2,
            'cache_read_per_million': 0.1, 'cache_write_per_million': 1.25}
        self.start(mock=upstream, models=models)
        for index, stream in enumerate((False, True), 1):
            with self.subTest(stream=stream):
                status, body = self.chat(stream=stream)
                self.assertEqual(status, 200, body)
                self.ledger_text(minimum=index)
                row = self.ledger_rows()[-1]
                self.assertEqual(row['prompt_tokens'], 100)
                self.assertEqual(row['cached_tokens'], 60)
                self.assertEqual(row['cache_write_tokens'], 20)
                self.assertEqual(row['cost_usd'], 0.000071)
