"""Wave D D3: the OpenAI v1 conformance of the remote streaming proxy.

A mock OpenRouter upstream sends the provider-shaped frames; the controller must hand the
client the same OpenAI shape as a local reply: local-only request fields removed, reasoning
effort mapped, tool-call deltas kept (index, name, string arguments), reasoning exposed as
`reasoning_content`, and the final usage block completed.
"""
import json
import unittest

from snapdragon_ai_lab.runtime import cloud

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


class SSEParsing(CloudCase):
    """The SSE grammar, not just LF-LF: CRLF/CR, comments, split reads and multiline data."""

    def _event(self, payload, ending=b'\n'):
        return b'data: ' + json.dumps(payload).encode() + ending + ending

    def _chunk(self, delta, finish=None):
        return {'id': 'chatcmpl-mock', 'object': 'chat.completion.chunk', 'model': 'upstream',
                'choices': [{'index': 0, 'delta': delta, 'finish_reason': finish}]}

    def test_sse_decoder_accepts_every_line_ending_comment_and_split_boundary(self):
        for ending in (b'\n', b'\r\n', b'\r'):
            with self.subTest(ending=ending):
                raw = (b': keep-alive' + ending + b'data: first' + ending
                       + b'data: second' + ending + ending)
                parser = cloud.SSEDecoder()
                events = []
                for index in range(len(raw)):
                    events.extend(parser.feed(raw[index:index + 1]))
                events.extend(parser.finish())
                self.assertEqual(events, [(None, b'first\nsecond')])

    def test_sse_decoder_emits_the_first_event_before_the_next_is_complete(self):
        parser = cloud.SSEDecoder()
        self.assertEqual(parser.feed(b'data: one\n\ndata: two\n'), [(None, b'one')])
        self.assertEqual(parser.feed(b'\n'), [(None, b'two')])

    def test_each_line_ending_is_normalised_end_to_end(self):
        for ending in (b'\n', b'\r\n', b'\r'):
            with self.subTest(ending=ending):
                chunks = [
                    self._event(self._chunk({'content': 'ok'}), ending),
                    self._event(self._chunk({}, 'stop'), ending),
                    self._event({'id': 'chatcmpl-mock', 'object': 'chat.completion.chunk',
                                 'model': 'upstream', 'choices': [],
                                 'usage': {'prompt_tokens': 4, 'completion_tokens': 2}}, ending),
                    b'data: [DONE]' + ending + ending,
                ]
                upstream = MockOpenRouter(stream_chunks=chunks)
                self.addCleanup(upstream.close)
                self.start(mock=upstream)
                status, body = self.chat(stream=True)
                self.assertEqual(status, 200)
                self.assertNotIn(b'\r', body)
                events = [json.loads(line[5:]) for line in body.decode().splitlines()
                          if line.startswith('data:') and line[5:].strip() not in ('', '[DONE]')]
                self.assertTrue(events)
                self.assertTrue(all(event['model'] == REMOTE for event in events))
                self.assertEqual(events[0]['choices'][0]['delta']['content'], 'ok')
                self.assertEqual(events[1]['choices'][0]['finish_reason'], 'stop')
                self.assertIn('usage', events[2])
                self.stack.close()
                self.stack = None

    def test_comments_interleaved_with_data_do_not_bypass_normalisation(self):
        ending = b'\n'
        payload = self._chunk({'content': 'after comment'})
        raw = b': first keep-alive\n' + b'data: ' + json.dumps(payload).encode() + b'\n'
        chunks = [raw + b': interleaved\n' + ending,
                  b': another keep-alive\n',
                  self._event(self._chunk({}, 'stop'), ending),
                  b'data: [DONE]\n\n']
        upstream = MockOpenRouter(stream_chunks=chunks, stream_delay=0.01)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        status, body = self.chat(stream=True)
        self.assertEqual(status, 200)
        self.assertNotIn(b'keep-alive', body)  # comments are never forwarded as data events
        events = [json.loads(line[5:]) for line in body.decode().splitlines()
                  if line.startswith('data:') and line[5:].strip() not in ('', '[DONE]')]
        self.assertEqual(events[0]['model'], REMOTE)
        self.assertEqual(events[0]['choices'][0]['delta']['content'], 'after comment')
        self.assertEqual(events[1]['choices'][0]['finish_reason'], 'stop')

    def test_an_event_split_across_tcp_chunks_is_normalised_once(self):
        event = self._event(self._chunk({'reasoning': 'because', 'content': 'ok'}))
        cut = len(event) // 2
        upstream = MockOpenRouter(stream_chunks=[event[:cut], event[cut:],
                                                 self._event(self._chunk({}, 'stop')),
                                                 b'data: [DONE]\n\n'], stream_delay=0.02)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        status, body = self.chat(stream=True)
        self.assertEqual(status, 200)
        events = [json.loads(line[5:]) for line in body.decode().splitlines()
                  if line.startswith('data:') and line[5:].strip() not in ('', '[DONE]')]
        delta = events[0]['choices'][0]['delta']
        self.assertEqual(delta['content'], 'ok')
        self.assertEqual(delta['reasoning_content'], 'because')
        self.assertEqual(events[0]['model'], REMOTE)


if __name__ == '__main__':
    unittest.main()
