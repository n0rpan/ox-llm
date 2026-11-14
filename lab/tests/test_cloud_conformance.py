"""Wave D D3: the OpenAI v1 conformance of the remote streaming proxy.

A mock OpenRouter upstream sends the provider-shaped frames; the controller must hand the
client the same OpenAI shape as a local reply: local-only request fields removed, reasoning
effort mapped, tool-call deltas kept (index, name, string arguments), reasoning exposed as
`reasoning_content`, and the final usage block completed.
"""
import json
import unittest

from mock_cloud import MockOpenRouter
from openai_api_stack import Stack  # noqa: F401  (keeps the test import path uniform)
from test_cloud_remote import CloudCase, REMOTE


class RequestMapping(CloudCase):
    def test_local_fields_are_removed_and_effort_is_forwarded(self):
        self.start()
        status, _ = self.call('POST', '/v1/chat/completions', {
            'model': REMOTE, 'messages': [{'role': 'user', 'content': 'hi'}],
            'reasoning_effort': 'high', 'chat_template_kwargs': {'enable_thinking': False},
            'cache_prompt': True, 'stream': True})
        self.assertEqual(status, 200)
        sent = self.upstream.requests[0]['body']
        self.assertEqual(sent['reasoning_effort'], 'high')
        self.assertNotIn('chat_template_kwargs', sent)
        self.assertNotIn('cache_prompt', sent)
        self.assertEqual(sent['stream_options']['include_usage'], True)

    def test_minimal_and_xhigh_use_the_documented_mapping(self):
        for requested, expected in (('none', 'none'), ('minimal', 'none'),
                                    ('low', 'low'), ('medium', 'medium'), ('high', 'high'),
                                    ('xhigh', 'high')):
            with self.subTest(requested=requested):
                self.start()
                status, _ = self.call('POST', '/v1/chat/completions', {
                    'model': REMOTE, 'messages': [{'role': 'user', 'content': 'hi'}],
                    'reasoning_effort': requested})
                self.assertEqual(status, 200)
                self.assertEqual(self.upstream.requests[-1]['body']['reasoning_effort'], expected)
                self.stack.close()
                self.stack = None

    def test_enable_thinking_maps_when_effort_is_absent(self):
        self.start()
        status, _ = self.call('POST', '/v1/chat/completions', {
            'model': REMOTE, 'messages': [{'role': 'user', 'content': 'hi'}],
            'chat_template_kwargs': {'enable_thinking': True}})
        self.assertEqual(status, 200)
        self.assertEqual(self.upstream.requests[0]['body']['reasoning_effort'], 'high')

    def test_a_non_string_effort_is_a_clear_400(self):
        self.start()
        status, body = self.call('POST', '/v1/chat/completions', {
            'model': REMOTE, 'messages': [{'role': 'user', 'content': 'hi'}],
            'reasoning_effort': 3})
        self.assertEqual(status, 400)
        self.assertEqual(json.loads(body)['error']['param'], 'reasoning_effort')
        self.assertEqual(self.upstream.requests, [])


class ResponseMapping(CloudCase):
    def test_non_streamed_reasoning_usage_and_model_alias(self):
        response = {'id': 'chatcmpl-mock', 'object': 'chat.completion', 'created': 1,
                    'model': 'ignored-upstream-model',
                    'choices': [{'index': 0, 'finish_reason': 'stop',
                                 'message': {'role': 'assistant', 'content': 'ok', 'reasoning': 'why'}}],
                    'usage': {'prompt_tokens': 10, 'completion_tokens': 5, 'total_tokens': 15,
                              'prompt_tokens_details': {'cached_tokens': 3}}}
        upstream = MockOpenRouter(response=response)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        status, body = self.chat()
        self.assertEqual(status, 200)
        value = json.loads(body)
        self.assertEqual(value['model'], REMOTE)
        message = value['choices'][0]['message']
        self.assertEqual(message['content'], 'ok')
        self.assertEqual(message['reasoning_content'], 'why')
        usage = value['usage']
        self.assertEqual(usage['prompt_tokens_details']['cached_tokens'], 3)
        self.assertIn('completion_tokens_details', usage)
        self.assertIn('reasoning_tokens', usage['completion_tokens_details'])

    def test_streamed_tool_calls_reasoning_and_usage_are_openai_shaped(self):
        frames = [
            {'id': 'chatcmpl-mock', 'object': 'chat.completion.chunk', 'model': 'upstream',
             'choices': [{'index': 0, 'delta': {'role': 'assistant', 'content': ''}, 'finish_reason': None}]},
            {'id': 'chatcmpl-mock', 'object': 'chat.completion.chunk', 'model': 'upstream',
             'choices': [{'index': 0, 'delta': {'reasoning': 'thinking'}, 'finish_reason': None}]},
            {'id': 'chatcmpl-mock', 'object': 'chat.completion.chunk', 'model': 'upstream',
             'choices': [{'index': 0, 'delta': {'tool_calls': [
                 {'id': 'call_1', 'type': 'function',
                  'function': {'name': 'read', 'arguments': {'path': 'a.txt'}}}]}, 'finish_reason': None}]},
            {'id': 'chatcmpl-mock', 'object': 'chat.completion.chunk', 'model': 'upstream',
             'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'tool_calls'}]},
        ]
        usage = {'prompt_tokens': 20, 'completion_tokens': 7, 'total_tokens': 27}
        upstream = MockOpenRouter(stream_frames=frames, usage=usage)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        status, body = self.chat(stream=True)
        self.assertEqual(status, 200)
        events = [json.loads(line[5:]) for line in body.decode().splitlines()
                  if line.startswith('data:') and line[5:].strip() not in ('', '[DONE]')]
        self.assertTrue(events)
        self.assertTrue(all(event['model'] == REMOTE for event in events))
        reasoning = [event['choices'][0]['delta']['reasoning_content'] for event in events
                     if event.get('choices') and event['choices'][0].get('delta', {}).get('reasoning_content')]
        self.assertEqual(reasoning, ['thinking'])
        calls = [call for event in events for choice in event.get('choices') or ()
                 for call in (choice.get('delta') or {}).get('tool_calls') or ()]
        self.assertEqual(calls[0]['index'], 0)
        self.assertEqual(calls[0]['function']['name'], 'read')
        self.assertEqual(calls[0]['function']['arguments'], '{"path": "a.txt"}')
        final = next(event for event in events if event.get('choices')
                     and event['choices'][0].get('finish_reason') == 'tool_calls')
        self.assertEqual(final['choices'][0]['finish_reason'], 'tool_calls')
        usage_event = next(event for event in events if event.get('usage'))
        self.assertEqual(usage_event['choices'], [])
        self.assertEqual(usage_event['usage']['prompt_tokens'], 20)
        self.assertIn('reasoning_tokens', usage_event['usage']['completion_tokens_details'])


if __name__ == '__main__':
    unittest.main()
