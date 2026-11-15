"""D8: real gateway + loopback OpenAI mock; no SDK credentials or paid calls."""
import json
import os
from unittest.mock import patch

from snapdragon_ai_lab.runtime import cloud, cloud_openai, request_ledger
from mock_cloud import MockOpenRouter, MockOpenAIResponses
from test_cloud_remote import CloudCase, KEY

REMOTE = 'cloud-openai'


def models(upstream='o3'):
    return {REMOTE: {'kind': 'remote/openai', 'upstream_model': upstream,
                     'context_window': 200000, 'max_output': 4096,
                     'pricing': {'input_per_million': 1.0, 'output_per_million': 1.0}}}


class OpenAI(CloudCase):
    def setUp(self):
        super().setUp()
        self.secrets.write_text(json.dumps({'openai': {'api_key': KEY}}))

    def start(self, **kwargs):
        kwargs.setdefault('models', models())
        return super().start(**kwargs)

    def chat(self, **kwargs):
        kwargs.setdefault('model', REMOTE)
        return super().chat(**kwargs)

    def test_routing_limits_parameters_and_records_origin(self):
        self.start()
        status, body = self.chat(reasoning_effort='xhigh', max_tokens=100,
                                 temperature=0.7, top_p=0.9, n=1, provider={'zdr': False})
        self.assertEqual(status, 200, body)
        sent = self.upstream.requests[0]
        self.assertEqual(sent['path'], '/v1/chat/completions')
        self.assertEqual(sent['headers']['authorization'], 'Bearer ' + KEY)
        self.assertEqual(sent['body']['model'], 'o3')
        self.assertEqual(sent['body']['max_completion_tokens'], 100)
        self.assertEqual(sent['body']['reasoning_effort'], 'high')
        self.assertIs(sent['body']['store'], False)
        for name in ('max_tokens', 'temperature', 'top_p', 'provider'):
            self.assertNotIn(name, sent['body'])
        row = self.ledger_rows()[-1]
        self.assertTrue(row['remote'])
        self.assertEqual(row['remote_provider'], 'openai')
        self.assertIn('origin', row)
        self.assertEqual(row['settled'], 'usage')
        self.assertEqual(json.loads(body)['model'], REMOTE)

    def test_truncated_http_chunk_retains_reserved_charge(self):
        class Truncated(MockOpenRouter):
            def _sse(self, handler, events):
                handler.send_response(200)
                handler.send_header('Content-Type', 'text/event-stream')
                handler.send_header('Transfer-Encoding', 'chunked')
                handler.end_headers()
                handler.wfile.write(b'100\r\ndata: {"choices":[]}\n\n')
                handler.wfile.flush()
                handler.close_connection = True
        upstream = Truncated()
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        import http.client
        with self.assertRaises(http.client.IncompleteRead):
            self.chat(stream=True)
        rows = self.ledger_rows()
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]['settled'], 'reserved')
        self.assertGreater(rows[0]['cost_usd'], 0)
        self.assertEqual(cloud.spend_today(), rows[0]['cost_usd'])
        self.assertFalse(cloud._reserved)

    def test_unexpected_transport_error_is_settled_and_blocks_repeat_spend(self):
        class BrokenConnection:
            def request(self, *args, **kwargs):
                raise RuntimeError('synthetic transport failure')

            def close(self):
                pass
        self.start(budget=0.205)
        with patch.object(cloud, 'connection', return_value=(BrokenConnection(), '/mock', {})):
            status, body = self.chat()
            self.assertEqual(status, 503, body)
            row = self.ledger_rows()[0]
            self.assertEqual(row['settled'], 'reserved')
            self.assertGreater(row['cost_usd'], 0)
            self.assertEqual(self.chat()[0], 429)
        self.assertEqual(self.upstream.requests, [])

    def test_settlement_failure_blocks_future_spend_and_closes_transport(self):
        from snapdragon_ai_lab.runtime import ctl_cloud
        class BrokenConnection:
            closed = False

            def request(self, *args, **kwargs):
                raise RuntimeError('synthetic transport failure')

            def close(self):
                self.closed = True
        connection = BrokenConnection()
        self.start()
        import http.client
        with patch.object(cloud, 'connection', return_value=(connection, '/mock', {})), \
                patch.object(ctl_cloud, '_finish', side_effect=RuntimeError('synthetic settlement failure')):
            with self.assertRaises(http.client.RemoteDisconnected):
                self.chat()
        self.assertTrue(connection.closed)
        self.assertFalse(cloud._reserved)
        self.assertEqual(self.chat()[0], 503)
        self.assertEqual(self.upstream.requests, [])

    def test_truncated_nonstream_body_returns_failure_and_charges(self):
        class Truncated(MockOpenRouter):
            def _json(self, handler, status, value):
                handler.send_response(200)
                handler.send_header('Content-Length', '100')
                handler.end_headers()
                handler.wfile.write(b'{"choices":[')
                handler.wfile.flush()
                handler.close_connection = True
        upstream = Truncated()
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        status, _ = self.chat()
        self.assertEqual(status, 503)
        self.assertEqual(self.ledger_rows()[0]['settled'], 'reserved')

    def test_non_openai_reservation_ignores_responses_only_limit(self):
        item = cloud.entry('synthetic-router', {
            'kind': 'remote/openrouter', 'base_url': 'https://example.invalid/v1',
            'upstream_model': 'synthetic-model',
            'context_window': 100000, 'max_output': 32000,
            'pricing': {'input_per_million': 1, 'output_per_million': 10}})
        request = {'messages': [{'role': 'user', 'content': 'synthetic prompt'}]}
        self.assertEqual(cloud.worst_case_cost(item, request),
                         cloud.worst_case_cost(item, dict(request, max_output_tokens=1)))

    def test_malformed_credential_is_refused_without_disclosing_fragments(self):
        self.secrets.write_text(json.dumps({'openai': {'api_key': KEY + '\nsecond-line'}}))
        self.start()
        status, body = self.chat()
        self.assertEqual(status, 503)
        self.assertEqual(json.loads(body)['error']['code'], 'cloud_credentials_unavailable')
        for fragment in (KEY.encode(), b'second-line'):
            self.assertNotIn(fragment, body)
            self.assertNotIn(fragment.decode(), self.ledger_text())
        self.assertEqual(self.upstream.requests, [])

    def test_malformed_message_and_tool_shapes_are_typed_400(self):
        self.start()
        cases = [
            {'messages': [None]}, {'messages': ['invalid']},
            {'messages': [{'role': 'user', 'content': [None]}]},
            {'messages': [{'role': 'user', 'content': [{'type': 'image_url', 'image_url': None}]}]},
            {'messages': [{'role': 'assistant', 'content': None, 'tool_calls': [None]}]},
            {'messages': [{'role': 'tool', 'content': 'result'}]},
            {'messages': [{'role': 'user', 'content': 'hello'}], 'tools': [None]},
            {'messages': [{'role': 'user', 'content': 'hello'}],
             'tool_choice': {'type': 'function', 'function': None}},
        ]
        for payload in cases:
            with self.subTest(payload=payload):
                status, body = self.call('POST', '/v1/chat/completions', {'model': REMOTE, **payload})
                self.assertEqual(status, 400, body)
                self.assertEqual(json.loads(body)['error']['code'], 'invalid_value')
        self.assertEqual(self.upstream.requests, [])
        self.assertFalse(cloud._reserved)

    def test_nullable_tool_strict_preserves_supported_contract(self):
        mock = MockOpenAIResponses() if isinstance(self, Responses) else MockOpenRouter()
        self.addCleanup(mock.close)
        self.start(mock=mock, budget=10)
        for options in ({}, {'strict': False}, {'strict': True}, {'strict': None}):
            function = {'name': 'lookup', 'parameters': {'type': 'object', 'properties': {},
                        'additionalProperties': False, 'required': []}, **options}
            status, body = self.chat(tools=[{'type': 'function', 'function': function}])
            self.assertEqual(status, 200, body)
            if 'strict' in options:
                sent = mock.requests[-1]['body']['tools'][0]
                sent = sent.get('function', sent)
                self.assertIs(sent['strict'], options['strict'])
        for malformed in (3, 'false', []):
            status, _ = self.chat(tools=[{'type': 'function', 'function': {
                'name': 'lookup', 'strict': malformed}}])
            self.assertEqual(status, 400)
        self.assertEqual(len(mock.requests), 4)

    def test_supported_tool_text_arrays_and_nullable_limits(self):
        for model in ('gpt-4.1', 'gpt-6.1-sol'):
            item = cloud.entry(REMOTE, models(model)[REMOTE])
            assistant = {'role': 'assistant', 'content': None, 'tool_calls': [{
                'id': 'call_array', 'type': 'function',
                'function': {'name': 'lookup', 'arguments': '{}'}}]}
            history = [{'role': 'user', 'content': 'lookup'}]
            cloud_openai.remember(item, history, assistant, [{
                'type': 'function_call', 'call_id': 'call_array', 'name': 'lookup', 'arguments': '{}'}],
                'synthetic-array-scope')
            result = [{'type': 'text', 'text': 'synthetic '}, {'type': 'text', 'text': 'result'}]
            history.extend([assistant, {'role': 'tool', 'tool_call_id': 'call_array', 'content': result}])
            body = cloud_openai.request({'messages': history}, item, 'synthetic-array-scope')
            if cloud_openai.endpoint(item) == '/responses':
                self.assertEqual(body['input'][-1]['output'], 'synthetic result')
            else:
                self.assertEqual(body['messages'][-1]['content'], result)
            for tokens in ({'max_tokens': None}, {'max_completion_tokens': None},
                           {'max_completion_tokens': None, 'max_tokens': 12}):
                body = cloud_openai.request({'messages': [history[0]], 'n': None, **tokens}, item)
                field = 'max_output_tokens' if cloud_openai.endpoint(item) == '/responses' else 'max_completion_tokens'
                self.assertEqual(body[field], 12 if tokens.get('max_tokens') == 12 else 4096)

    def test_default_url_capabilities_and_no_local_features(self):
        item = cloud.entry(REMOTE, models()[REMOTE])
        self.assertEqual(item['base_url'], 'https://api.openai.com/v1')
        self.start()
        row = next(row for row in self.rows() if row['id'] == REMOTE)
        caps = row['capabilities']
        self.assertTrue(caps['remote'])
        self.assertEqual(caps['remote_provider'], 'openai')
        self.assertEqual(caps['context_window'], 200000)
        for name in ('echo', 'cache_hints', 'cancel', 'prefill'):
            self.assertFalse(caps[name], name)
        status, body = self.call('GET', '/_local/status')
        self.assertEqual(status, 200)
        self.assertTrue(json.loads(body)['cloud']['providers']['openai'])
        self.assertNotIn(KEY.encode(), body)
        self.assertNotIn(str(self.secrets).encode(), body)

    def test_cloud_off_and_offline_never_contact_upstream(self):
        for kwargs, expected in (({'enabled': False}, 404), ({'offline': True}, 403)):
            with self.subTest(kwargs=kwargs):
                self.start(**kwargs)
                self.assertNotIn(REMOTE, [row['id'] for row in self.rows()])
                self.assertEqual(self.chat()[0], expected)
                self.assertEqual(self.upstream.requests, [])
                self.stack.close()
                self.stack = None

    def test_private_credentials_read_again_each_request(self):
        self.start()
        self.assertEqual(self.chat()[0], 200)
        self.secrets.write_text(json.dumps({'openai': {'api_key': 'synthetic-rotated-mock-only'}}))
        self.assertEqual(self.chat()[0], 200)
        self.assertEqual(self.upstream.requests[-1]['headers']['authorization'],
                         'Bearer synthetic-rotated-mock-only')
        os.chmod(self.secrets, 0o644)
        self.assertEqual(self.chat()[0], 503)
        self.assertEqual(len(self.upstream.requests), 2)

    def test_unknown_model_and_multiple_choices_refused_before_spend(self):
        self.start(models=models('gpt-unknown'))
        self.assertEqual(self.chat()[0], 400)
        self.assertEqual(self.upstream.requests, [])
        self.stack.close()
        self.stack = None
        self.start()
        self.assertEqual(self.chat(n=2)[0], 400)
        self.assertEqual(self.upstream.requests, [])

    def test_reasoning_effort_validation_and_mapping(self):
        item = cloud.entry(REMOTE, models()[REMOTE])
        for effort, expected in (('none', 'low'), ('minimal', 'low'), ('low', 'low'),
                                 ('medium', 'medium'), ('high', 'high'), ('xhigh', 'high')):
            self.assertEqual(cloud_openai.chat_request({'reasoning_effort': effort}, item)
                             ['reasoning_effort'], expected)
        for effort in (3, {}, 'unknown'):
            with self.assertRaises(cloud.CloudError):
                cloud_openai.chat_request({'reasoning_effort': effort}, item)
        self.assertEqual(cloud_openai.chat_request(
            {'chat_template_kwargs': {'enable_thinking': True}}, item)['reasoning_effort'], 'high')
        item['upstream_model'] = 'gpt-4.1'
        body = cloud_openai.chat_request({'temperature': 0.2, 'reasoning_effort': 'high'}, item)
        self.assertEqual(body['temperature'], 0.2)
        self.assertNotIn('reasoning_effort', body)

    def test_output_cap_and_conservative_missing_usage(self):
        upstream = MockOpenRouter(usage_missing=True)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        status, _ = self.chat(max_tokens=100000)
        self.assertEqual(status, 200)
        self.assertEqual(upstream.requests[0]['body']['max_completion_tokens'], 4096)
        row = self.ledger_rows()[-1]
        self.assertEqual(row['settled'], 'reserved')
        self.assertGreaterEqual(row['cost_usd'], 0.004096)

    def test_cap_refuses_before_forward_and_failed_accounting_stays_closed(self):
        self.start(budget=0.000001)
        self.assertEqual(self.chat()[0], 429)
        self.assertEqual(self.upstream.requests, [])
        self.stack.close()
        self.stack = None
        self.start()
        with patch.object(request_ledger, 'write', return_value=False):
            self.assertEqual(self.chat()[0], 200)
        self.assertEqual(self.chat()[0], 503)
        self.assertEqual(len(self.upstream.requests), 1)

    def test_error_and_success_redaction(self):
        for status in (200, 500):
            upstream = MockOpenRouter(status=status, error=KEY, reply=KEY)
            self.addCleanup(upstream.close)
            self.start(mock=upstream)
            result, body = self.chat()
            self.assertEqual(result, status)
            self.assertNotIn(KEY.encode(), body)
            self.assertIn(b'[redacted]', body)
            self.assertNotIn(KEY, self.ledger_text())
            self.stack.close()
            self.stack = None

    def test_streamed_tool_loop_and_redaction_across_argument_fragments(self):
        call = {'id': 'call_mock', 'type': 'function',
                'function': {'name': 'lookup', 'arguments': ''}, 'index': 0}
        frames = [
            {'choices': [{'index': 0, 'delta': {'tool_calls': [call]}, 'finish_reason': None}]},
            {'choices': [{'index': 0, 'delta': {'tool_calls': [{'index': 0, 'function': {
                'arguments': '{"value":"' + KEY[:10]}}]}, 'finish_reason': None}]},
            {'choices': [{'index': 0, 'delta': {'tool_calls': [{'index': 0, 'function': {
                'arguments': KEY[10:] + '"}'}}]}, 'finish_reason': None}]},
            {'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'tool_calls'}]},
        ]
        upstream = MockOpenRouter(stream_frames=frames)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        status, body = self.chat(stream=True, tools=[{'type': 'function', 'function': {
            'name': 'lookup', 'parameters': {'type': 'object', 'properties': {}}}}])
        self.assertEqual(status, 200)
        events = [json.loads(line[5:]) for line in body.decode().splitlines()
                  if line.startswith('data:') and line[5:].strip() != '[DONE]']
        calls = [call for event in events for choice in event.get('choices', [])
                 for call in choice['delta'].get('tool_calls', [])]
        self.assertEqual(calls[0]['id'], 'call_mock')
        arguments = ''.join(call['function'].get('arguments', '') for call in calls)
        self.assertNotIn(KEY, arguments)
        self.assertEqual(json.loads(arguments), {'value': '[redacted]'})
        upstream.stream_frames = None
        status, _ = self.call('POST', '/v1/chat/completions', {'model': REMOTE, 'messages': [
            {'role': 'user', 'content': 'look up'},
            {'role': 'assistant', 'content': None, 'tool_calls': [{
                'id': 'call_mock', 'type': 'function', 'function': {
                    'name': 'lookup', 'arguments': '{"value":"example"}'}}]},
            {'role': 'tool', 'tool_call_id': 'call_mock', 'content': 'synthetic result'}]})
        self.assertEqual(status, 200)
        self.assertEqual(upstream.requests[-1]['body']['messages'][-1]['tool_call_id'], 'call_mock')
        self.assertNotIn(KEY, self.ledger_text(minimum=2))


class Responses(CloudCase):
    test_nullable_tool_strict_preserves_supported_contract = OpenAI.test_nullable_tool_strict_preserves_supported_contract
    test_malformed_message_and_tool_shapes_are_typed_400 = OpenAI.test_malformed_message_and_tool_shapes_are_typed_400

    def setUp(self):
        super().setUp()
        self.secrets.write_text(json.dumps({'openai': {'api_key': KEY}}))

    def chat(self, **kwargs):
        kwargs.setdefault('model', REMOTE)
        return super().chat(**kwargs)
    # The Chat Completions tests above remain separate; these exercise native wire events.
    def start(self, **kwargs):
        kwargs.setdefault('models', models('gpt-6.1-sol'))
        return CloudCase.start(self, **kwargs)

    def test_responses_translation_and_usage(self):
        upstream = MockOpenAIResponses()
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        status, body = self.chat(reasoning_effort='none', temperature=0.7, max_tokens=100)
        self.assertEqual(status, 200, body)
        value = json.loads(body)
        self.assertEqual(value['choices'][0]['message']['content'], 'Done.')
        self.assertEqual(value['usage']['completion_tokens_details']['reasoning_tokens'], 1)
        sent = upstream.requests[0]
        self.assertEqual(sent['path'], '/v1/responses')
        self.assertEqual(sent['headers']['authorization'], 'Bearer ' + KEY)
        self.assertEqual(sent['body']['reasoning'], {'effort': 'low'})
        self.assertEqual(sent['body']['max_output_tokens'], 100)
        self.assertFalse(sent['body']['store'])
        self.assertNotIn('temperature', sent['body'])
        self.assertEqual(sent['body']['input'], [{'role': 'user', 'content': 'hello'}])
        row = self.ledger_rows()[-1]
        self.assertEqual(row['remote_provider'], 'openai')
        self.assertEqual(row['cost_usd'], 0.000015)

    def test_native_streamed_tool_loop_preserves_encrypted_reasoning(self):
        native = {'id': 'resp_tool', 'status': 'completed', 'output': [
            {'type': 'reasoning', 'id': 'rs_mock', 'summary': [],
             'encrypted_content': 'synthetic-encrypted-reasoning'},
            {'type': 'function_call', 'id': 'fc_mock', 'call_id': 'call_mock',
             'name': 'lookup', 'arguments': '{"value":"example"}', 'status': 'completed'}],
            'usage': {'input_tokens': 12, 'output_tokens': 8}}
        events = [
            {'type': 'response.created', 'response': {'id': 'resp_tool'}},
            {'type': 'response.output_item.added', 'output_index': 1, 'item': {
                'type': 'function_call', 'call_id': 'call_mock', 'name': 'lookup', 'arguments': ''}},
            {'type': 'response.function_call_arguments.delta', 'output_index': 1, 'delta': '{"value":'},
            {'type': 'response.function_call_arguments.delta', 'output_index': 1, 'delta': '"example"}'},
            {'type': 'response.completed', 'response': native}]
        upstream = MockOpenAIResponses(events=events)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        history = [{'role': 'user', 'content': 'look up'}]
        status, body = self.call('POST', '/v1/chat/completions', {
            'model': REMOTE, 'messages': history, 'stream': True,
            'tools': [{'type': 'function', 'function': {'name': 'lookup', 'parameters': {
                'type': 'object', 'properties': {'value': {'type': 'string'}}}}}]})
        self.assertEqual(status, 200, body)
        self.assertNotIn(b'synthetic-encrypted-reasoning', body)
        chunks = [json.loads(line[5:]) for line in body.decode().splitlines()
                  if line.startswith('data:') and line[5:].strip() != '[DONE]']
        calls = [call for chunk in chunks for choice in chunk['choices']
                 for call in choice['delta'].get('tool_calls', [])]
        self.assertEqual(calls[0]['id'], 'call_mock')
        args = ''.join(call['function'].get('arguments', '') for call in calls)
        self.assertEqual(json.loads(args), {'value': 'example'})
        self.assertTrue(any(chunk['choices'] and chunk['choices'][0]['finish_reason'] == 'tool_calls'
                            for chunk in chunks))
        history.extend([{'role': 'assistant', 'content': None, 'tool_calls': [{
            'id': 'call_mock', 'type': 'function', 'function': {'name': 'lookup', 'arguments': args}}]},
            {'role': 'tool', 'tool_call_id': 'call_mock', 'content': 'synthetic result'}])
        status, body = self.call('POST', '/v1/chat/completions', {'model': REMOTE, 'messages': history})
        self.assertEqual(status, 200, body)
        sent = upstream.requests[-1]['body']
        self.assertEqual(sent['input'][1:3], native['output'])
        self.assertEqual(sent['input'][-1], {'type': 'function_call_output', 'call_id': 'call_mock',
                                            'output': 'synthetic result'})
        self.assertFalse(upstream.requests[0]['body']['tools'][0]['strict'])
        self.assertNotIn('synthetic-encrypted-reasoning', self.ledger_text(minimum=2))
        self.assertNotIn('synthetic result', self.ledger_text(minimum=2))

    def test_expired_closed_tool_history_recovers_on_a_new_turn_after_compaction(self):
        native = {'id': 'resp_directory', 'status': 'completed', 'output': [
            {'type': 'reasoning', 'id': 'rs_directory', 'summary': [],
             'encrypted_content': 'synthetic-private-reasoning'},
            {'type': 'function_call', 'call_id': 'call_directory', 'name': 'ls',
             'arguments': '{"path":"oxa"}'}],
            'usage': {'input_tokens': 12, 'output_tokens': 8}}
        upstream = MockOpenAIResponses(response=native)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        history = [{'role': 'user', 'content': 'inspect the synthetic directory'}]
        status, body = self.call('POST', '/v1/chat/completions', {'model': REMOTE, 'messages': history})
        self.assertEqual(status, 200)
        history.extend([json.loads(body)['choices'][0]['message'],
                        {'role': 'tool', 'tool_call_id': 'call_directory', 'content': 'synthetic listing'}])
        with cloud_openai._HISTORY_LOCK:
            cloud_openai._HISTORY.clear()
        # An active tool continuation still needs its opaque state: no unsafe paid retry.
        self.assertEqual(self.call('POST', '/v1/chat/completions', {
            'model': REMOTE, 'messages': history})[0], 409)
        self.assertEqual(len(upstream.requests), 1)
        history.insert(0, {'role': 'system', 'content': 'Synthetic compacted context.'})
        unresolved = history[:-1] + [{'role': 'user', 'content': 'A fresh turn cannot close a pending tool.'}]
        self.assertEqual(self.call('POST', '/v1/chat/completions', {
            'model': REMOTE, 'messages': unresolved})[0], 409)
        self.assertEqual(len(upstream.requests), 1)
        history.append({'role': 'user', 'content': 'Continue from the completed tool result.'})
        for _ in range(2):
            status, body = self.call('POST', '/v1/chat/completions', {'model': REMOTE, 'messages': history})
            self.assertEqual(status, 200, body)
        sent = upstream.requests[-1]['body']['input']
        call = next(part for part in sent if part.get('type') == 'function_call')
        self.assertEqual(call['name'], 'ls')
        self.assertEqual(call['call_id'], 'call_directory')
        self.assertTrue(any(part.get('type') == 'function_call_output' and
                            part['output'] == 'synthetic listing' for part in sent))
        self.assertNotIn('synthetic-private-reasoning', json.dumps(sent))
        self.assertNotIn('previous_response_id', upstream.requests[-1]['body'])

    def test_sol_and_luna_directory_name_fragments_continue_with_the_same_history(self):
        for model in ('gpt-6.1-sol', 'gpt-6-luna'):
            native = {'id': 'resp_directory', 'status': 'completed', 'output': [{
                'type': 'function_call', 'call_id': 'call_directory', 'name': 'ls',
                'arguments': '{"path":"oxa"}'}], 'usage': {'input_tokens': 12, 'output_tokens': 8}}
            events = [{'type': 'response.output_item.added', 'output_index': 0, 'item': {
                'type': 'function_call', 'call_id': 'call_directory', 'name': 'ls', 'arguments': ''}},
                {'type': 'response.function_call_arguments.delta', 'output_index': 0,
                 'delta': '{"path":"oxa"}'}, {'type': 'response.completed', 'response': native}]
            upstream = MockOpenAIResponses(events=events)
            self.addCleanup(upstream.close)
            self.start(mock=upstream, models=models(model))
            history = [{'role': 'user', 'content': 'list the synthetic directory'}]
            status, body = self.call('POST', '/v1/chat/completions', {
                'model': REMOTE, 'messages': history, 'stream': True})
            self.assertEqual(status, 200)
            chunks = [json.loads(line[5:]) for line in body.decode().splitlines()
                      if line.startswith('data:') and line[5:].strip() != '[DONE]']
            calls = [call for chunk in chunks for choice in chunk['choices']
                     for call in choice['delta'].get('tool_calls', [])]
            name = ''.join(call['function'].get('name', '') for call in calls)
            self.assertEqual(name, 'ls')
            args = ''.join(call['function'].get('arguments', '') for call in calls)
            history.extend([{'role': 'assistant', 'content': None, 'tool_calls': [{
                'id': 'call_directory', 'type': 'function', 'function': {'name': name, 'arguments': args}}]},
                {'role': 'tool', 'tool_call_id': 'call_directory', 'content': 'synthetic listing'}])
            self.assertEqual(self.call('POST', '/v1/chat/completions', {
                'model': REMOTE, 'messages': history})[0], 200)
            self.assertEqual(len(upstream.requests), 2)
            self.stack.close()
            self.stack = None

    def test_interrupted_stream_charges_reservation_and_ends_without_done(self):
        upstream = MockOpenAIResponses(events=[{'type': 'response.output_text.delta', 'delta': 'partial'}])
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        import http.client
        with self.assertRaises(http.client.IncompleteRead):
            self.chat(stream=True)
        row = self.ledger_rows()[-1]
        self.assertEqual(row['settled'], 'reserved')
        self.assertTrue(row['usage_missing'])
        self.assertGreater(row['cost_usd'], 0)

    def test_stale_tool_history_fails_before_paid_call(self):
        self.start(mock=self.upstream)
        status, body = self.call('POST', '/v1/chat/completions', {'model': REMOTE, 'messages': [
            {'role': 'assistant', 'content': None, 'tool_calls': [{
                'id': 'call_unknown', 'type': 'function', 'function': {'name': 'lookup', 'arguments': '{}'}}]},
            {'role': 'tool', 'tool_call_id': 'call_unknown', 'content': 'result'}]})
        self.assertEqual(status, 409, body)
        self.assertEqual(self.upstream.requests, [])

    def test_non_streamed_tool_loop_and_history_scope(self):
        native = {'id': 'resp_tool', 'status': 'completed', 'output': [
            {'type': 'reasoning', 'id': 'rs_mock', 'summary': [], 'encrypted_content': 'synthetic-only'},
            {'type': 'function_call', 'call_id': 'call_scope', 'name': 'lookup', 'arguments': '{}'}],
            'usage': {'input_tokens': 12, 'output_tokens': 8}}
        upstream = MockOpenAIResponses(response=native)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        history = [{'role': 'user', 'content': 'original synthetic question'}]
        status, body = self.call('POST', '/v1/chat/completions', {'model': REMOTE, 'messages': history})
        self.assertEqual(status, 200)
        assistant = json.loads(body)['choices'][0]['message']
        history.extend([assistant, {'role': 'tool', 'tool_call_id': 'call_scope', 'content': 'result'}])
        status, _ = self.call('POST', '/v1/chat/completions', {'model': REMOTE, 'messages': history})
        self.assertEqual(status, 200)
        self.assertEqual(upstream.requests[-1]['body']['input'][1:3], native['output'])
        history[0]['content'] = 'different synthetic question'
        self.assertEqual(self.call('POST', '/v1/chat/completions', {
            'model': REMOTE, 'messages': history})[0], 409)
        self.assertEqual(len(upstream.requests), 2)

    def test_native_history_scope_includes_image_content(self):
        item = cloud.entry(REMOTE, models('gpt-6.1-sol')[REMOTE])
        history = [{'role': 'user', 'content': [
            {'type': 'text', 'text': 'inspect this synthetic image'},
            {'type': 'image_url', 'image_url': {'url': 'https://example.invalid/first.png'}}]}]
        assistant = {'role': 'assistant', 'content': None, 'tool_calls': [{
            'id': 'call_image', 'type': 'function',
            'function': {'name': 'lookup', 'arguments': '{}'}}]}
        native = [{'type': 'function_call', 'call_id': 'call_image',
                   'name': 'lookup', 'arguments': '{}'}]
        cloud_openai.remember(item, history, assistant, native, 'synthetic-history-scope')
        continuation = history + [assistant, {'role': 'tool', 'tool_call_id': 'call_image',
                                              'content': 'synthetic result'}]
        self.assertEqual(cloud_openai.responses_request(
            {'messages': continuation}, item, 'synthetic-history-scope')['input'][1], native[0])
        image = history[0]['content'][1]['image_url']
        for changed in ({'url': 'https://example.invalid/second.png'},
                        {'url': 'https://example.invalid/first.png', 'detail': 'high'}):
            image.clear()
            image.update(changed)
            with self.assertRaises(cloud.CloudError) as caught:
                cloud_openai.responses_request({'messages': continuation}, item, 'synthetic-history-scope')
            self.assertEqual(caught.exception.code, 'cloud_continuation_unavailable')

    def test_long_tool_loop_survives_eviction_of_closed_earlier_steps(self):
        # #246: one user turn, many tool steps. Each step stores native output; the
        # bounds evict early steps, which are closed and need no opaque reasoning.
        item = cloud.entry(REMOTE, models('gpt-6.1-sol')[REMOTE])
        key = 'synthetic-long-loop'
        history = [{'role': 'user', 'content': 'a long synthetic task'}]
        steps = cloud_openai._HISTORY_ENTRIES + 20
        for step in range(steps):
            call = 'call_%d' % step
            native = [{'type': 'reasoning', 'id': 'rs_%d' % step, 'summary': [],
                       'encrypted_content': 'synthetic-reasoning-%d' % step},
                      {'type': 'function_call', 'call_id': call, 'name': 'bash',
                       'arguments': '{"n":%d}' % step}]
            body = cloud_openai.responses_request({'messages': history}, item, key)
            if step:
                self.assertIn(native_prev[0], body['input'])
            assistant = {'role': 'assistant', 'content': None, 'tool_calls': [{
                'id': call, 'type': 'function', 'function': {'name': 'bash', 'arguments': '{"n":%d}' % step}}]}
            cloud_openai.remember(item, history, assistant, native, key)
            history += [assistant, {'role': 'tool', 'tool_call_id': call, 'content': 'out %d' % step}]
            native_prev = native
        sent = cloud_openai.responses_request({'messages': history}, item, key)['input']
        self.assertIn({'type': 'function_call', 'call_id': 'call_0', 'name': 'bash',
                       'arguments': '{"n":0}', 'status': 'completed'}, sent)
        self.assertNotIn('synthetic-reasoning-0"', json.dumps(sent))

    def test_all_gpt6_models_bind_responses_and_reasoning_contract(self):
        for upstream, none_supported in cloud_openai.RESPONSES_MODELS.items():
            item = cloud.entry(REMOTE, models(upstream)[REMOTE])
            body = cloud_openai.responses_request({'reasoning_effort': 'none', 'temperature': 0.4}, item)
            self.assertEqual(cloud_openai.endpoint(item), '/responses')
            self.assertEqual(body['reasoning']['effort'], 'none' if none_supported else 'low')
            self.assertEqual('temperature' in body, none_supported)
            for effort in ('low', 'medium', 'high', 'xhigh'):
                body = cloud_openai.responses_request({'reasoning_effort': effort, 'top_p': 0.7}, item)
                self.assertEqual(body['reasoning']['effort'], effort)
                self.assertNotIn('top_p', body)

    def test_partial_usage_is_charged_conservatively(self):
        upstream = MockOpenAIResponses(response={'id': 'resp_mock', 'status': 'completed',
                                                 'output': [], 'usage': {'input_tokens': 11}})
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        self.assertEqual(self.chat()[0], 200)
        row = self.ledger_rows()[-1]
        self.assertEqual(row['settled'], 'reserved')
        self.assertGreaterEqual(row['cost_usd'], 0.204096)
