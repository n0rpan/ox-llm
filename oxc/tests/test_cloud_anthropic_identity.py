"""Opaque native thinking state belongs to one endpoint, model, credential and exact history."""
import json
import unittest
from unittest.mock import patch

from oxc.runtime import cloud, cloud_anthropic as native
from test_cloud_anthropic import AnthropicBase, anthropic_models
from mock_cloud import MockAnthropic


class ThinkingIdentity(unittest.TestCase):
    def setUp(self):
        self.item = cloud.entry('anthropic/mock', next(iter(anthropic_models().values())))
        self.blocks = [{'type': 'thinking', 'thinking': 'synthetic plan', 'signature': 'signature-a'},
                       {'type': 'text', 'text': 'next'}]
        self.assistant = {'role': 'assistant', 'content': 'next'}
        self.history = [{'role': 'user', 'content': 'synthetic question'}]
        self.cache = patch.object(native, '_THINKING', native.OrderedDict())
        self.cache.start()
        self.addCleanup(self.cache.stop)
        size = patch.object(native, '_THINKING_SIZE', [0])
        size.start()
        self.addCleanup(size.stop)

    def test_same_alias_changed_upstream_or_endpoint_cannot_restore_old_blocks(self):
        native._remember_blocks(self.item, self.history, self.assistant, self.blocks)
        messages = self.history + [self.assistant]
        self.assertEqual(native._restore_blocks(self.item, messages, 1), self.blocks)
        for field, value in (('upstream_model', 'different-upstream'),
                             ('base_url', 'http://127.0.0.1:2/v1'), ('id', 'different-alias')):
            with self.subTest(field=field):
                changed = dict(self.item, **{field: value})
                self.assertIsNone(native._restore_blocks(changed, messages, 1))
                params = native.messages_request({'messages': messages}, changed)
                self.assertFalse(any(block['type'] in native.THINKING_TYPES
                                     for turn in params['messages'] for block in turn['content']))

    def test_streamed_remembrance_uses_the_same_hashed_credential_scope(self):
        key = 'synthetic-identity-credential'
        stream = native._Stream(self.item, False, self.history, key)
        for index, block in enumerate(self.blocks):
            field = 'thinking' if block['type'] == 'thinking' else 'text'
            start = {'type': block['type'], field: ''}
            stream.event({'type': 'content_block_start', 'index': index, 'content_block': start})
            stream.event({'type': 'content_block_delta', 'index': index,
                          'delta': {'type': field + '_delta', field: block[field]}})
            if 'signature' in block:
                stream.event({'type': 'content_block_delta', 'index': index,
                              'delta': {'type': 'signature_delta', 'signature': block['signature']}})
            stream.event({'type': 'content_block_stop', 'index': index})
        stream.event({'type': 'message_stop'})
        messages = self.history + [self.assistant]
        self.assertEqual(native._restore_blocks(self.item, messages, 1, key), self.blocks)
        self.assertIsNone(native._restore_blocks(self.item, messages, 1, 'rotated-synthetic-credential'))
        self.assertNotIn(key, repr(list(native._THINKING)))
        self.assertTrue(all(len(cache_key[3]) == 64 for cache_key in native._THINKING))

    def test_swapped_valid_tool_result_ids_do_not_share_a_history_key(self):
        calls = [{'id': name, 'type': 'function', 'function': {'name': 'read', 'arguments': '{}'}}
                 for name in ('call_a', 'call_b')]
        history = [*self.history, {'role': 'assistant', 'content': None, 'tool_calls': calls},
                   {'role': 'tool', 'tool_call_id': 'call_a', 'content': 'first result'},
                   {'role': 'tool', 'tool_call_id': 'call_b', 'content': 'second result'}]
        swapped = [*history[:2], dict(history[2], tool_call_id='call_b'),
                   dict(history[3], tool_call_id='call_a')]
        native._remember_blocks(self.item, history, self.assistant, self.blocks)
        self.assertEqual(native._restore_blocks(self.item, history + [self.assistant], 4), self.blocks)
        self.assertNotEqual(native._assistant_key(history[2]), native._assistant_key(swapped[2]))
        self.assertIsNone(native._restore_blocks(self.item, swapped + [self.assistant], 4))


class GatewayCredentialIdentity(AnthropicBase):
    def test_rotating_credentials_for_the_same_alias_does_not_restore_native_state(self):
        upstream = MockAnthropic()
        self.addCleanup(upstream.close)
        upstream.message['content'] = [
            {'type': 'thinking', 'thinking': 'synthetic plan', 'signature': 'old-credential-signature'},
            {'type': 'text', 'text': 'answer'}]
        self.start(mock=upstream)
        status, body = self.chat()
        self.assertEqual(status, 200, body)
        assistant = json.loads(body)['choices'][0]['message']
        messages = [{'role': 'user', 'content': 'hello'}, assistant,
                    {'role': 'user', 'content': 'continue'}]
        self.assertEqual(self.chat(messages=messages)[0], 200)
        self.assertTrue(any(b['type'] == 'thinking'
                            for m in upstream.requests[-1]['body']['messages'] for b in m['content']))
        rotated = 'another-synthetic-credential'
        self.secrets.write_text(json.dumps({'anthropic': {'api_key': rotated}}))
        status, body = self.chat(messages=messages)
        self.assertEqual(status, 200, body)
        self.assertFalse(any(b['type'] == 'thinking'
                             for m in upstream.requests[-1]['body']['messages'] for b in m['content']))
        self.assertNotIn(rotated, repr(list(native._THINKING)))
