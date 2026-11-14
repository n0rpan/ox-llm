"""Wave D D6: the native Anthropic Messages upstream, against a local mock only."""
import importlib.util
import json
import unittest

from mock_cloud import MockAnthropic
from test_cloud_remote import CloudCase, KEY


ANTHROPIC_SDK = importlib.util.find_spec("anthropic") is not None


def anthropic_models():
    def row(model, context=200000, output=8192, price=(1.0, 5.0)):
        return {'kind': 'remote/anthropic', 'provider': 'anthropic',
                'base_url': 'http://127.0.0.1:1', 'upstream_model': model,
                'context_window': context, 'max_output': output,
                'pricing': {'input_per_million': price[0], 'output_per_million': price[1]}}
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
            'temperature': 0.2,
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
        self.assertEqual(sent['body']['temperature'], 0.2)
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
        self.assertEqual(sent['thinking'], {'type': 'disabled'})
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
        self.start(mock=upstream, budget=0.001)
        self.assertEqual(self.chat(model='anthropic/claude-haiku-5-5')[0], 200)
        rows = self.ledger_rows()
        self.assertEqual(rows[-1]['cost_usd'], 0.0048)
        status, body = self.chat(model='anthropic/claude-haiku-5-5')
        self.assertEqual(status, 429)
        self.assertEqual(json.loads(body)['error']['code'], 'budget_exceeded')
        self.assertEqual(len(upstream.requests), 1)


if __name__ == '__main__':
    unittest.main()
