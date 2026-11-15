"""Wave D D6: the native Anthropic Messages upstream, against a local mock only."""
import http.client
import importlib.util
import json
import unittest

from snapdragon_ai_lab.runtime import cloud, cloud_anthropic

from mock_cloud import MockAnthropic
from test_cloud_remote import CloudCase, KEY


ANTHROPIC_SDK = importlib.util.find_spec("anthropic") is not None


def anthropic_models():
    def row(model, context=200000, output=8192, price=(1.0, 5.0)):
        return {'kind': 'remote/anthropic', 'provider': 'anthropic',
                'base_url': 'http://127.0.0.1:1', 'upstream_model': model,
                'context_window': context, 'max_output': output,
                # These fixed-price mocks do not charge a cache-write premium.
                'pricing': {'input_per_million': price[0], 'output_per_million': price[1],
                            'cache_write_per_million': price[0]}}
    return {'anthropic/claude-haiku-5-5': row('claude-haiku-5-5', price=(0.8, 4.0)),
            'anthropic/claude-sonnet-5-5': row('claude-sonnet-5-5', price=(3.0, 15.0)),
            'anthropic/claude-opus-5-5': row('claude-opus-5-5', price=(15.0, 75.0))}


@unittest.skipUnless(ANTHROPIC_SDK, "the official anthropic SDK is not installed")
class AnthropicBase(CloudCase):
    def start(self, **kwargs):
        kwargs.setdefault('models', anthropic_models())
        kwargs.setdefault('base_suffix', '')
        return super().start(**kwargs)

    def chat(self, *, stream=False, model='anthropic/claude-sonnet-5-5', text='hello', **extra):
        return self.call('POST', '/v1/chat/completions', {
            'model': model, 'messages': [{'role': 'user', 'content': text}],
            'stream': stream, **extra})


class Catalog(AnthropicBase):
    def test_the_three_anthropic_entries_are_listed_when_cloud_is_on(self):
        self.start()
        rows = {row['id']: row for row in self.rows()}
        for model in ('anthropic/claude-haiku-5-5', 'anthropic/claude-sonnet-5-5',
                      'anthropic/claude-opus-5-5'):
            self.assertIn(model, rows)
            self.assertTrue(rows[model]['remote'])
            self.assertEqual(rows[model]['provider'], 'anthropic')
        self.assertEqual(rows['anthropic/claude-sonnet-5-5']['context_length'], 200000)

    def test_offline_refuses_and_cloud_off_hides(self):
        self.start(offline=True)
        status, body = self.chat()
        self.assertEqual(status, 403)
        self.assertEqual(json.loads(body)['error']['code'], 'offline')
        self.stack.close()
        self.start(enabled=False)
        status, body = self.chat()
        self.assertEqual(status, 404)
        self.assertEqual(json.loads(body)['error']['code'], 'model_not_found')


class MessagesTranslation(AnthropicBase):
    def _tool_content(self, content):
        upstream = MockAnthropic()
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        body = {
            'model': 'anthropic/claude-sonnet-5-5', 'max_tokens': 50,
            'messages': [
                {'role': 'user', 'content': 'read x'},
                {'role': 'assistant', 'content': None,
                 'tool_calls': [{'id': 'call_1', 'type': 'function',
                                 'function': {'name': 'read', 'arguments': '{}'}}]},
                {'role': 'tool', 'tool_call_id': 'call_1', 'content': content}]}
        status, reply = self.call('POST', '/v1/chat/completions', body)
        return status, reply, upstream

    def test_array_tool_result_becomes_text_blocks(self):
        status, reply, upstream = self._tool_content(
            [{'type': 'text', 'text': 'one'}, {'type': 'text', 'text': 'two'}])
        self.assertEqual(status, 200, reply)
        blocks = upstream.requests[0]['body']['messages'][-1]['content'][0]['content']
        self.assertEqual([(b['type'], b['text']) for b in blocks],
                         [('text', 'one'), ('text', 'two')])

    def test_unsupported_tool_result_part_is_a_clear_error(self):
        status, reply, upstream = self._tool_content(
            [{'type': 'text', 'text': 'one'}, {'type': 'audio', 'data': 'x'}])
        self.assertEqual(status, 400, reply)
        self.assertEqual(upstream.requests, [])

    def test_openai_request_becomes_a_native_messages_request(self):
        upstream = MockAnthropic()
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        body = {
            'model': 'anthropic/claude-sonnet-5-5',
            'messages': [
                {'role': 'system', 'content': 'You are precise.'},
                {'role': 'user', 'content': 'read x'},
                {'role': 'assistant', 'content': None,
                 'tool_calls': [{'id': 'call_1', 'type': 'function',
                                 'function': {'name': 'read', 'arguments': '{"path":"x"}'}}]},
                {'role': 'tool', 'tool_call_id': 'call_1', 'content': 'X'},
            ],
            'tools': [{'type': 'function', 'function': {
                'name': 'read', 'description': 'read a file',
                'parameters': {'type': 'object', 'properties': {'path': {'type': 'string'}}}}}],
            'tool_choice': 'auto', 'max_tokens': 100, 'reasoning_effort': 'medium',
            'temperature': 0.2, 'top_p': 0.9, 'top_k': 40,
        }
        status, reply = self.call('POST', '/v1/chat/completions', body)
        self.assertEqual(status, 200, reply)
        sent = upstream.requests[0]
        self.assertEqual(sent['path'], '/v1/messages')
        self.assertEqual(sent['headers']['x-api-key'], KEY)
        self.assertEqual(sent['headers']['anthropic-version'], '2023-06-01')
        self.assertNotIn('authorization', sent['headers'])
        self.assertEqual(sent['body']['model'], 'claude-sonnet-5-5')
        self.assertEqual(sent['body']['max_tokens'], 100)
        for field in ('temperature', 'top_p', 'top_k'):
            self.assertNotIn(field, sent['body'])
        self.assertEqual(sent['body']['thinking'], {'type': 'adaptive'})
        self.assertEqual(sent['body']['output_config'], {'effort': 'medium'})
        self.assertEqual(sent['body']['system'][-1]['cache_control'], {'type': 'ephemeral'})
        self.assertEqual(sent['body']['tools'][-1]['cache_control'], {'type': 'ephemeral'})
        self.assertEqual(sent['body']['tool_choice'], {'type': 'auto'})
        self.assertEqual(sent['body']['messages'][-1]['content'][-1]['cache_control'],
                         {'type': 'ephemeral'})
        self.assertEqual(sent['body']['messages'][1]['content'][0]['type'], 'tool_use')
        tool_result = sent['body']['messages'][-1]['content'][0]
        self.assertEqual((tool_result['type'], tool_result['tool_use_id']), ('tool_result', 'call_1'))
        self.assertNotIn('provider', sent['body'])
        self.assertNotIn('cache_prompt', sent['body'])
        reply_value = json.loads(reply)
        self.assertEqual(reply_value['model'], 'anthropic/claude-sonnet-5-5')
        self.assertEqual(reply_value['usage']['prompt_tokens'], 15)
        self.assertEqual(reply_value['usage']['prompt_tokens_details']['cached_tokens'], 3)

    def test_effort_off_and_tool_choice_required(self):
        upstream = MockAnthropic()
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        status, _ = self.call('POST', '/v1/chat/completions', {
            'model': 'anthropic/claude-haiku-5-5',
            'messages': [{'role': 'user', 'content': 'x'}],
            'tools': [{'type': 'function', 'function': {'name': 'read', 'parameters': {'type': 'object'}}}],
            'tool_choice': 'required', 'reasoning_effort': 'none'})
        self.assertEqual(status, 200)
        sent = upstream.requests[0]['body']
        self.assertEqual(sent['thinking'], {'type': 'adaptive'})
        self.assertEqual(sent['output_config'], {'effort': 'low'})
        self.assertEqual(sent['tool_choice'], {'type': 'any'})

    def test_a_named_tool_choice_is_sent_as_tool(self):
        upstream = MockAnthropic()
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        status, _ = self.call('POST', '/v1/chat/completions', {
            'model': 'anthropic/claude-haiku-5-5',
            'messages': [{'role': 'user', 'content': 'x'}],
            'tools': [{'type': 'function', 'function': {'name': 'read', 'parameters': {'type': 'object'}}}],
            'tool_choice': {'type': 'function', 'function': {'name': 'read'}}})
        self.assertEqual(status, 200)
        self.assertEqual(upstream.requests[0]['body']['tool_choice'], {'type': 'tool', 'name': 'read'})


class Responses(AnthropicBase):
    def test_refusal_and_tool_use_become_openai_finish_reasons(self):
        message = {'id': 'msg_ref', 'type': 'message', 'role': 'assistant',
                   'content': [{'type': 'thinking', 'thinking': 'careful'},
                               {'type': 'text', 'text': 'I cannot.'},
                               {'type': 'tool_use', 'id': 'toolu_1', 'name': 'read',
                                'input': {'path': 'x'}}],
                   'model': 'claude-sonnet-5-5', 'stop_reason': 'refusal', 'stop_sequence': None,
                   'usage': {'input_tokens': 7, 'output_tokens': 2,
                             'cache_read_input_tokens': 1, 'cache_creation_input_tokens': 0}}
        upstream = MockAnthropic(message=message)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        status, body = self.chat()
        self.assertEqual(status, 200)
        value = json.loads(body)
        self.assertEqual(value['choices'][0]['finish_reason'], 'content_filter')
        assistant = value['choices'][0]['message']
        self.assertEqual(assistant['content'], 'I cannot.')
        self.assertEqual(assistant['reasoning_content'], 'careful')
        self.assertEqual(assistant['tool_calls'][0]['function']['name'], 'read')
        self.assertEqual(json.loads(assistant['tool_calls'][0]['function']['arguments']), {'path': 'x'})
        self.assertEqual(value['usage']['prompt_tokens'], 8)
        self.assertEqual(value['usage']['prompt_tokens_details']['cached_tokens'], 1)

    def test_streamed_text_thinking_and_tool_calls(self):
        events = [
            ('message_start', {'type': 'message_start', 'message': {
                'id': 'msg_stream', 'type': 'message', 'role': 'assistant', 'content': [],
                'model': 'claude-sonnet-5-5', 'stop_reason': None, 'stop_sequence': None,
                'usage': {'input_tokens': 10, 'output_tokens': 0,
                          'cache_read_input_tokens': 2, 'cache_creation_input_tokens': 0}}}),
            ('content_block_start', {'type': 'content_block_start', 'index': 0,
                                     'content_block': {'type': 'thinking', 'thinking': ''}}),
            ('content_block_delta', {'type': 'content_block_delta', 'index': 0,
                                     'delta': {'type': 'thinking_delta', 'thinking': 'plan'}}),
            ('content_block_stop', {'type': 'content_block_stop', 'index': 0}),
            ('content_block_start', {'type': 'content_block_start', 'index': 1,
                                     'content_block': {'type': 'text', 'text': ''}}),
            ('content_block_delta', {'type': 'content_block_delta', 'index': 1,
                                     'delta': {'type': 'text_delta', 'text': 'ok'}}),
            ('content_block_stop', {'type': 'content_block_stop', 'index': 1}),
            ('content_block_start', {'type': 'content_block_start', 'index': 2,
                                     'content_block': {'type': 'tool_use', 'id': 'toolu_2',
                                                       'name': 'read', 'input': {}}}),
            ('content_block_delta', {'type': 'content_block_delta', 'index': 2,
                                     'delta': {'type': 'input_json_delta', 'partial_json': '{"path"'}}),
            ('content_block_delta', {'type': 'content_block_delta', 'index': 2,
                                     'delta': {'type': 'input_json_delta', 'partial_json': ': "x"}'}}),
            ('content_block_stop', {'type': 'content_block_stop', 'index': 2}),
            ('message_delta', {'type': 'message_delta',
                               'delta': {'stop_reason': 'tool_use', 'stop_sequence': None},
                               'usage': {'output_tokens': 6}}),
            ('message_stop', {'type': 'message_stop'}),
        ]
        upstream = MockAnthropic(stream_events=events)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        status, body = self.chat(stream=True, stream_options={'include_usage': True})
        self.assertEqual(status, 200)
        text = body.decode()
        self.assertTrue(text.rstrip().endswith('data: [DONE]'))
        frames = [json.loads(line[5:]) for line in text.splitlines()
                  if line.startswith('data:') and line[5:].strip() not in ('', '[DONE]')]
        self.assertEqual(frames[0]['choices'][0]['delta']['role'], 'assistant')
        reasoning = [frame['choices'][0]['delta']['reasoning_content'] for frame in frames
                     if frame.get('choices') and frame['choices'][0]['delta'].get('reasoning_content')]
        self.assertEqual(reasoning, ['plan'])
        tool_starts = [call for frame in frames for choice in frame.get('choices') or ()
                       for call in (choice.get('delta') or {}).get('tool_calls') or () if 'id' in call]
        self.assertEqual(tool_starts[0]['index'], 0, "the tool index is not the content-block index")
        args = ''.join(call['function']['arguments'] for frame in frames for choice in frame.get('choices') or ()
                       for call in (choice.get('delta') or {}).get('tool_calls') or ()
                       if 'function' in call and 'arguments' in call['function'])
        self.assertEqual(args, '{"path": "x"}')
        final = next(frame for frame in frames if frame.get('choices')
                     and frame['choices'][0].get('finish_reason') == 'tool_calls')
        self.assertEqual(final['choices'][0]['finish_reason'], 'tool_calls')
        usage = next(frame for frame in frames if frame.get('usage'))
        self.assertEqual(usage['usage']['prompt_tokens'], 12)
        self.assertEqual(usage['usage']['prompt_tokens_details']['cached_tokens'], 2)
        self.assertEqual(usage['usage']['completion_tokens'], 6)


class KeyAndBudget(AnthropicBase):
    def test_key_never_appears_in_an_error_or_the_ledger(self):
        upstream = MockAnthropic(status=500, error='bad x-api-key ' + KEY)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        status, body = self.chat()
        self.assertEqual(status, 500)
        self.assertNotIn(KEY.encode(), body)
        ledger = self.ledger_text()
        self.assertNotIn(KEY, ledger)
        status, status_body = self.call('GET', '/_local/status')
        self.assertNotIn(KEY.encode(), status_body)

    def test_budget_cap_applies_to_anthropic_too(self):
        message = {'id': 'msg_cost', 'type': 'message', 'role': 'assistant',
                   'content': [{'type': 'text', 'text': 'ok'}], 'model': 'claude-haiku-5-5',
                   'stop_reason': 'end_turn', 'stop_sequence': None,
                   'usage': {'input_tokens': 1000, 'output_tokens': 1000,
                             'cache_read_input_tokens': 0, 'cache_creation_input_tokens': 0}}
        upstream = MockAnthropic(message=message)
        self.addCleanup(upstream.close)
        models = anthropic_models()
        models['anthropic/claude-haiku-5-5']['context_window'] = 1000
        self.start(mock=upstream, budget=0.005, models=models)
        self.assertEqual(self.chat(model='anthropic/claude-haiku-5-5', max_tokens=1000)[0], 200)
        rows = self.ledger_rows()
        self.assertEqual(rows[-1]['cost_usd'], 0.0048)
        status, body = self.chat(model='anthropic/claude-haiku-5-5', max_tokens=1000)
        self.assertEqual(status, 429)
        self.assertEqual(json.loads(body)['error']['code'], 'budget_exceeded')
        self.assertEqual(len(upstream.requests), 1)


class SuccessRedaction(AnthropicBase):
    def test_a_key_in_a_successful_non_streamed_reply_is_redacted(self):
        message = {'id': 'msg_key', 'type': 'message', 'role': 'assistant',
                   'content': [{'type': 'text', 'text': 'leak ' + KEY}],
                   'model': 'claude-sonnet-5-5', 'stop_reason': 'end_turn',
                   'stop_sequence': None,
                   'usage': {'input_tokens': 3, 'output_tokens': 2,
                             'cache_read_input_tokens': 0, 'cache_creation_input_tokens': 0}}
        upstream = MockAnthropic(message=message)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        status, body = self.chat()
        self.assertEqual(status, 200)
        self.assertNotIn(KEY.encode(), body)
        self.assertIn(b'[redacted]', body)
        self.assertNotIn(KEY, self.ledger_text())

    def test_a_key_split_across_successful_stream_deltas_is_redacted(self):
        events = [
            ('message_start', {'type': 'message_start', 'message': {
                'id': 'msg_key_stream', 'type': 'message', 'role': 'assistant', 'content': [],
                'model': 'claude-sonnet-5-5', 'stop_reason': None, 'stop_sequence': None,
                'usage': {'input_tokens': 3, 'output_tokens': 0,
                          'cache_read_input_tokens': 0, 'cache_creation_input_tokens': 0}}}),
            ('content_block_start', {'type': 'content_block_start', 'index': 0,
                                     'content_block': {'type': 'text', 'text': ''}}),
            ('content_block_delta', {'type': 'content_block_delta', 'index': 0,
                                     'delta': {'type': 'text_delta', 'text': 'leak ' + KEY[:6]}}),
            ('content_block_delta', {'type': 'content_block_delta', 'index': 0,
                                     'delta': {'type': 'text_delta', 'text': KEY[6:] + ' end'}}),
            ('content_block_stop', {'type': 'content_block_stop', 'index': 0}),
            ('message_delta', {'type': 'message_delta',
                               'delta': {'stop_reason': 'end_turn', 'stop_sequence': None},
                               'usage': {'output_tokens': 2}}),
            ('message_stop', {'type': 'message_stop'}),
        ]
        upstream = MockAnthropic(stream_events=events)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        status, body = self.chat(stream=True)
        self.assertEqual(status, 200)
        self.assertNotIn(KEY.encode(), body)
        self.assertIn(b'[redacted]', body)
        self.assertNotIn(KEY, self.ledger_text())


class ThinkingRoundTrip(AnthropicBase):
    def _tools(self):
        return [{'type': 'function', 'function': {
            'name': 'read', 'parameters': {'type': 'object', 'properties': {'path': {'type': 'string'}}}}}]

    def _tool_result(self):
        return {'role': 'tool', 'tool_call_id': 'toolu_1', 'content': 'X'}

    def test_native_thinking_blocks_round_trip_unchanged_across_a_tool_turn(self):
        native = [
            {'type': 'thinking', 'thinking': 'plan', 'signature': 'sig-abc-123'},
            {'type': 'redacted_thinking', 'data': 'opaque-encrypted-blob'},
            {'type': 'text', 'text': 'I will read.'},
            {'type': 'tool_use', 'id': 'toolu_1', 'name': 'read', 'input': {'path': 'x'}},
        ]
        message = {'id': 'msg_tool', 'type': 'message', 'role': 'assistant',
                   'content': native, 'model': 'claude-sonnet-5-5',
                   'stop_reason': 'tool_use', 'stop_sequence': None,
                   'usage': {'input_tokens': 10, 'output_tokens': 4,
                             'cache_read_input_tokens': 0, 'cache_creation_input_tokens': 0}}
        upstream = MockAnthropic(message=message)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        body = {'model': 'anthropic/claude-sonnet-5-5',
                'messages': [{'role': 'user', 'content': 'read x'}],
                'tools': self._tools(), 'max_tokens': 100}
        status, reply = self.call('POST', '/v1/chat/completions', body)
        self.assertEqual(status, 200, reply)
        assistant = json.loads(reply)['choices'][0]['message']
        followup = {'model': 'anthropic/claude-sonnet-5-5',
                    'messages': [{'role': 'user', 'content': 'read x'}, assistant, self._tool_result()],
                    'tools': self._tools(), 'max_tokens': 100}
        status, _ = self.call('POST', '/v1/chat/completions', followup)
        self.assertEqual(status, 200)
        sent = upstream.requests[1]['body']['messages']
        self.assertEqual(sent[1]['content'], native)
        self.assertEqual(sent[1]['content'][0]['signature'], 'sig-abc-123')
        self.assertEqual(sent[1]['content'][1]['data'], 'opaque-encrypted-blob')

    def test_response_then_messages_request_round_trips_without_gateway_history(self):
        item = cloud.entry('anthropic/claude-sonnet-5-5',
                           anthropic_models()['anthropic/claude-sonnet-5-5'])
        message = {'id': 'msg_direct', 'type': 'message', 'role': 'assistant',
                   'content': [{'type': 'thinking', 'thinking': 'plan', 'signature': 'sig-direct'},
                               {'type': 'text', 'text': 'ok'}],
                   'model': 'claude-sonnet-5-5', 'stop_reason': 'end_turn',
                   'stop_sequence': None,
                   'usage': {'input_tokens': 1, 'output_tokens': 1,
                             'cache_read_input_tokens': 0, 'cache_creation_input_tokens': 0}}
        reply = cloud_anthropic.response(message, item)
        assistant = reply['choices'][0]['message']
        params = cloud_anthropic.messages_request({'messages': [assistant]}, item)
        self.assertEqual(params['messages'][0]['content'][0],
                         {'type': 'thinking', 'thinking': 'plan', 'signature': 'sig-direct'})
        self.assertEqual(params['messages'][0]['content'][1]['type'], 'text')
        self.assertEqual(params['messages'][0]['content'][1]['text'], 'ok')

    def test_explicit_thinking_blocks_keep_the_rest_of_the_assistant_turn(self):
        upstream = MockAnthropic()
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        assistant = {'role': 'assistant', 'content': 'I will read.',
                     'thinking_blocks': [{'type': 'thinking', 'thinking': 'plan',
                                          'signature': 'sig-explicit'}],
                     'tool_calls': [{'id': 'toolu_1', 'type': 'function',
                                     'function': {'name': 'read', 'arguments': '{"path":"x"}'}}]}
        status, _ = self.call('POST', '/v1/chat/completions', {
            'model': 'anthropic/claude-sonnet-5-5',
            'messages': [{'role': 'user', 'content': 'read x'}, assistant],
            'tools': self._tools(), 'max_tokens': 100})
        self.assertEqual(status, 200)
        blocks = upstream.requests[0]['body']['messages'][1]['content']
        self.assertEqual(blocks[0], {'type': 'thinking', 'thinking': 'plan',
                                     'signature': 'sig-explicit'})
        self.assertEqual(blocks[1], {'type': 'text', 'text': 'I will read.'})
        self.assertEqual(blocks[2]['type'], 'tool_use')
        self.assertEqual(blocks[2]['input'], {'path': 'x'})

    def test_streamed_thinking_and_tool_use_round_trip_byte_for_byte(self):
        events = [
            ('message_start', {'type': 'message_start', 'message': {
                'id': 'msg_stream_tool', 'type': 'message', 'role': 'assistant', 'content': [],
                'model': 'claude-sonnet-5-5', 'stop_reason': None, 'stop_sequence': None,
                'usage': {'input_tokens': 10, 'output_tokens': 0,
                          'cache_read_input_tokens': 0, 'cache_creation_input_tokens': 0}}}),
            ('content_block_start', {'type': 'content_block_start', 'index': 0,
                                     'content_block': {'type': 'thinking', 'thinking': ''}}),
            ('content_block_delta', {'type': 'content_block_delta', 'index': 0,
                                     'delta': {'type': 'thinking_delta', 'thinking': 'plan'}}),
            ('content_block_delta', {'type': 'content_block_delta', 'index': 0,
                                     'delta': {'type': 'signature_delta', 'signature': 'sig-stream'}}),
            ('content_block_stop', {'type': 'content_block_stop', 'index': 0}),
            ('content_block_start', {'type': 'content_block_start', 'index': 1,
                                     'content_block': {'type': 'tool_use', 'id': 'toolu_1',
                                                       'name': 'read', 'input': {}}}),
            ('content_block_delta', {'type': 'content_block_delta', 'index': 1,
                                     'delta': {'type': 'input_json_delta', 'partial_json': '{"path"'}}),
            ('content_block_delta', {'type': 'content_block_delta', 'index': 1,
                                     'delta': {'type': 'input_json_delta', 'partial_json': ': "x"}'}}),
            ('content_block_stop', {'type': 'content_block_stop', 'index': 1}),
            ('message_delta', {'type': 'message_delta',
                               'delta': {'stop_reason': 'tool_use', 'stop_sequence': None},
                               'usage': {'output_tokens': 4}}),
            ('message_stop', {'type': 'message_stop'}),
        ]
        upstream = MockAnthropic(stream_events=events)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        status, body = self.call('POST', '/v1/chat/completions', {
            'model': 'anthropic/claude-sonnet-5-5',
            'messages': [{'role': 'user', 'content': 'read x'}],
            'tools': self._tools(), 'max_tokens': 100, 'stream': True})
        self.assertEqual(status, 200)
        frames = [json.loads(line[5:]) for line in body.decode().splitlines()
                  if line.startswith('data:') and line[5:].strip() not in ('', '[DONE]')]
        final = next(frame for frame in frames if frame.get('choices')
                     and frame['choices'][0].get('finish_reason') == 'tool_calls')
        self.assertEqual(final['choices'][0]['finish_reason'], 'tool_calls')
        assistant = {'role': 'assistant', 'content': None,
                     'tool_calls': [{'id': 'toolu_1', 'type': 'function',
                                     'function': {'name': 'read', 'arguments': '{"path": "x"}'}}]}
        followup = {'model': 'anthropic/claude-sonnet-5-5',
                    'messages': [{'role': 'user', 'content': 'read x'}, assistant, self._tool_result()],
                    'tools': self._tools(), 'max_tokens': 100}
        status, _ = self.call('POST', '/v1/chat/completions', followup)
        self.assertEqual(status, 200)
        sent = upstream.requests[1]['body']['messages']
        self.assertEqual(sent[1]['content'][0],
                         {'type': 'thinking', 'thinking': 'plan', 'signature': 'sig-stream'})
        self.assertEqual(sent[1]['content'][1],
                         {'type': 'tool_use', 'id': 'toolu_1', 'name': 'read', 'input': {'path': 'x'}})


class FailedStreams(AnthropicBase):
    def test_a_failed_stream_after_input_usage_writes_conservative_spend(self):
        events = [
            ('message_start', {'type': 'message_start', 'message': {
                'id': 'msg_fail', 'type': 'message', 'role': 'assistant', 'content': [],
                'model': 'claude-sonnet-5-5', 'stop_reason': None, 'stop_sequence': None,
                'usage': {'input_tokens': 100, 'output_tokens': 0,
                          'cache_read_input_tokens': 0, 'cache_creation_input_tokens': 0}}}),
            ('content_block_start', {'type': 'content_block_start', 'index': 0,
                                     'content_block': {'type': 'text', 'text': ''}}),
            ('content_block_delta', {'type': 'content_block_delta', 'index': 0,
                                     'delta': {'type': 'text_delta', 'text': 'partial'}}),
            ('error', {'type': 'error',
                       'error': {'type': 'overloaded_error', 'message': 'upstream overloaded'}}),
        ]
        upstream = MockAnthropic(stream_events=events)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        connection = http.client.HTTPConnection('127.0.0.1', self.stack.port, timeout=15)
        try:
            data = json.dumps({'model': 'anthropic/claude-sonnet-5-5',
                               'messages': [{'role': 'user', 'content': 'hello'}],
                               'stream': True, 'max_tokens': 1000}).encode()
            connection.request('POST', '/v1/chat/completions', body=data,
                               headers={'Content-Type': 'application/json'})
            response = connection.getresponse()
            try:
                response.read()
            except http.client.IncompleteRead:
                pass  # a broken stream closes without the chunked terminator
        finally:
            connection.close()
        row = self.ledger_rows()[-1]
        payload = {'max_tokens': 1000, 'messages': [{'role': 'user', 'content': 'hello'}]}
        entry = cloud.entry('anthropic/claude-sonnet-5-5', anthropic_models()['anthropic/claude-sonnet-5-5'])
        expected = cloud.worst_case_cost(entry, cloud_anthropic.messages_request(payload, entry))
        self.assertEqual(row['prompt_tokens'], 100)
        self.assertEqual(row['cost_usd'], expected)
        self.assertTrue(row['usage_missing'])
        self.assertEqual(row['settled'], 'reserved')
        self.assertEqual(row['error'], 'cloud_stream_error')


class TypedErrors(AnthropicBase):
    def test_a_non_string_effort_is_a_clear_400(self):
        self.start()
        status, body = self.call('POST', '/v1/chat/completions', {
            'model': 'anthropic/claude-sonnet-5-5',
            'messages': [{'role': 'user', 'content': 'x'}], 'reasoning_effort': 3})
        self.assertEqual(status, 400)
        self.assertEqual(json.loads(body)['error']['code'], 'invalid_value')
        self.assertEqual(self.upstream.requests, [])

    def test_forced_tool_choice_is_rejected_for_a_model_that_refuses_it(self):
        self.start()
        status, body = self.call('POST', '/v1/chat/completions', {
            'model': 'anthropic/claude-sonnet-5-5',
            'messages': [{'role': 'user', 'content': 'x'}],
            'tools': [{'type': 'function', 'function': {
                'name': 'read', 'parameters': {'type': 'object'}}}],
            'tool_choice': 'required'})
        self.assertEqual(status, 400)
        self.assertEqual(json.loads(body)['error']['code'], 'invalid_value')
        self.assertEqual(self.upstream.requests, [])


if __name__ == '__main__':
    unittest.main()
