"""Cloud review regressions: paid controls and output admission use one boundary."""
import unittest
from unittest.mock import patch

from snapdragon_ai_lab.runtime import cloud, cloud_anthropic
from test_cloud_remote import profiles, REMOTE, CloudCase
from test_cloud_anthropic import anthropic_models, AnthropicBase
from mock_cloud import MockAnthropic


class CloudReviewLimits(unittest.TestCase):
    def test_only_supported_chat_fields_reach_compatible_upstreams(self):
        item = profiles()[REMOTE]
        payload = {'messages': [{'role': 'user', 'content': 'synthetic'}],
                   'tools': [{'type': 'function', 'function': {'name': 'read',
                              'parameters': {'type': 'object'}}}],
                   'tool_choice': 'auto', 'reasoning_effort': 'high', 'max_tokens': 10,
                   'plugins': [{'id': 'web'}], 'models': ['other/model'], 'route': 'fallback',
                   'transforms': ['middle-out'], 'service_tier': 'priority',
                   'provider': {'order': ['expensive'], 'max_price': {'prompt': 999}},
                   'stream_options': {'include_usage': False, 'unknown_paid_option': True}}
        body = cloud.upstream_payload(payload, item)
        for field in ('plugins', 'models', 'route', 'transforms', 'service_tier'):
            self.assertNotIn(field, body)
        self.assertEqual(body['provider'], {'zdr': True, 'data_collection': 'deny'})
        self.assertEqual(body['tools'], payload['tools'])
        self.assertEqual(body['tool_choice'], 'auto')
        self.assertEqual(body['reasoning_effort'], 'high')
        self.assertNotIn('stream_options', body)
        body = cloud.upstream_payload(dict(payload, stream=True), item)
        self.assertEqual(body['stream_options'], {'include_usage': True})

    def test_provider_hosted_tools_are_not_forwarded_as_client_functions(self):
        for tool in ({'type': 'web_search'}, {'type': 'function', 'function': {'name': 'read'},
                                              'provider_service': 'web'}):
            with self.subTest(tool=tool), self.assertRaises(cloud.CloudError):
                cloud.upstream_payload({'tools': [tool]}, profiles()[REMOTE])

    def test_anthropic_aliases_and_catalog_cap_agree_with_reservation(self):
        item = cloud.entry('anthropic/mock', next(iter(anthropic_models().values())))
        for limits, expected in (({'n_predict': 1}, 1), ({'max_tokens': 999999}, 8192),
                                 ({'max_completion_tokens': 3}, 3), ({}, 8192),
                                 ({'max_tokens': 4, 'max_completion_tokens': 7}, 4)):
            with self.subTest(limits=limits):
                payload = {'messages': [{'role': 'user', 'content': 'synthetic'}], **limits}
                params = cloud_anthropic.messages_request(payload, item)
                self.assertEqual(params['max_tokens'], expected)
                self.assertEqual(cloud._max_tokens(payload, item), expected)
                # The reservation is made on the translated request: its output part is the
                # shared catalog-bounded limit and it never exceeds the full-window bound.
                reserved = cloud.worst_case_cost(item, params)
                output = cloud._reservation_cost(item, 0, expected)
                full = cloud._reservation_cost(item, item['context_window'], expected)
                self.assertGreater(reserved, output)
                self.assertLessEqual(reserved, full)

    def test_invalid_anthropic_limit_fails_closed(self):
        item = cloud.entry('anthropic/mock', next(iter(anthropic_models().values())))
        for field in ('max_tokens', 'max_completion_tokens', 'n_predict'):
            for value in (True, 0, -1, '100', None):
                with self.subTest(field=field, value=value), self.assertRaises(cloud.CloudError):
                    cloud_anthropic.messages_request({field: value}, item)


class CompatibleForwarding(CloudCase):
    def test_paid_controls_are_removed_on_both_wire_modes(self):
        self.start()
        for stream in (False, True):
            with self.subTest(stream=stream):
                status, body = self.chat(stream=stream, tools=None,
                                         plugins=[{'id': 'web'}], models=['other/model'],
                                         service_tier='priority')
                self.assertEqual(status, 200, body)
                sent = self.upstream.requests[-1]['body']
                for field in ('plugins', 'models', 'service_tier', 'tools'):
                    self.assertNotIn(field, sent)
                self.assertEqual(sent['provider'], {'zdr': True, 'data_collection': 'deny'})


class AnthropicForwarding(AnthropicBase):
    def test_n_predict_and_catalog_cap_match_actual_admission(self):
        upstream = MockAnthropic()
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        for stream in (False, True):
            for limit, expected in ((1, 1), (999999, 8192)):
                with self.subTest(stream=stream, limit=limit):
                    with patch.object(cloud, 'reserve', wraps=cloud.reserve) as reserve:
                        status, body = self.chat(stream=stream, n_predict=limit)
                    self.assertEqual(status, 200, body)
                    sent = upstream.requests[-1]['body']
                    self.assertEqual(sent['max_tokens'], expected)
                    self.assertEqual(reserve.call_args.args[2]['max_tokens'], expected)
