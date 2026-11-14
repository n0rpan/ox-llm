"""v0.2.0 OpenAI v1 conformance on the controller side (M1, S1/S3/S4/S7, n, penalties, /v1/responses).

Model-free: pure-function tests plus the real controller Handler over the Python fake engine
(openai_api_stack). The SDK contract suite (tools/openai-contract) covers the same surface with
the real openai-python package against the real stub engine.
"""
import json
import unittest
import unittest.mock
from pathlib import Path
import importlib.util

from oxc.runtime import (ctl_models, ctl_replay, openai_api, openai_conformance, openai_responses,
                                       oxe_runtime, request_ledger)

from openai_api_stack import ORNITH, QWEN, FakeEngine, Stack, runtime_value

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('openai_api_check', ROOT / 'tools/openai_api_check.py')
check = importlib.util.module_from_spec(spec); spec.loader.exec_module(check)

USER = [{'role': 'user', 'content': 'hi'}]


def sse_frames(text):
    """[(event name or None, parsed data)] of an SSE body."""
    out = []
    for block in text.split('\n\n'):
        event, data = None, None
        for line in block.split('\n'):
            if line.startswith('event:'):
                event = line[6:].strip()
            elif line.startswith('data:'):
                data = line[5:].strip()
        if data is not None:
            out.append((event, data if data == '[DONE]' else json.loads(data)))
    return out


class DefaultOutputLimit(unittest.TestCase):
    """M1: on by default, configurable per alias, 0 = none, explicit values win."""

    def test_formula_and_keys(self):
        self.assertEqual(oxe_runtime.default_output_tokens({'context_size': 131072}), 16384)
        self.assertEqual(oxe_runtime.default_output_tokens({'context_size': 32768}), 8192)
        self.assertEqual(oxe_runtime.default_output_tokens({}), 16384)
        self.assertEqual(oxe_runtime.default_output_tokens({'context_size': 131072, 'max_output_tokens': 4096}), 4096)
        self.assertEqual(oxe_runtime.default_output_tokens({'context_size': 1000, 'max_output_tokens': 4096}), 1000)
        self.assertEqual(oxe_runtime.default_output_tokens({'context_size': 131072, 'default_max_tokens': 2048}), 2048)
        self.assertEqual(oxe_runtime.default_output_tokens({'context_size': 131072, 'max_output_tokens': 100,
                                                            'default_max_tokens': 2048}), 100)  # the v0.2.0 key first
        self.assertIsNone(oxe_runtime.default_output_tokens({'context_size': 131072, 'max_output_tokens': 0}))
        self.assertIsNone(oxe_runtime.default_output_tokens({'context_size': 131072, 'default_max_tokens': 0}))
        self.assertEqual(oxe_runtime.default_output_tokens({'context_size': 131072, 'max_output_tokens': '9'}), 16384)
        self.assertIsNone(oxe_runtime.default_output_tokens(None))

    def test_applied_only_without_a_length_field(self):
        config = {'context_size': 131072}
        body = oxe_runtime.request_defaults({'messages': USER}, '/v1/chat/completions', config)
        self.assertEqual(body['max_tokens'], 16384)
        for key, value in (('max_tokens', 99999), ('max_completion_tokens', 7), ('max_tokens', -1), ('n_predict', 3)):
            body = oxe_runtime.request_defaults({'messages': USER, key: value}, '/v1/chat/completions', config)
            self.assertEqual(body[key], value)  # never clamped
            self.assertTrue(key == 'max_tokens' or 'max_tokens' not in body)
        native = oxe_runtime.request_defaults({'prompt': 'x'}, '/completion', config)
        self.assertEqual(native['n_predict'], 16384)

    def test_models_rows_publish_the_limits(self):
        engine = FakeEngine()
        self.addCleanup(engine.close)
        profiles = runtime_value()['profiles']
        profiles[QWEN]['max_output_tokens'] = 4
        with Stack(engine.port, profiles=profiles) as stack:
            status, headers, value = check.call(stack.base, 'GET', '/v1/models')
            rows = {r['id']: r for r in value['data']}
            self.assertEqual((rows[ORNITH]['context_length'], rows[ORNITH]['max_output_tokens']), (32768, 8192))
            self.assertEqual(rows['local-default']['max_output_tokens'], 8192)  # an alias gets its target's
            self.assertEqual(rows[QWEN]['max_output_tokens'], 4)
            self.assertEqual(check.call(stack.base, 'GET', '/v1/models/' + QWEN)[2]['max_output_tokens'], 4)
            self.assertTrue(headers['x-request-id'].startswith('req_'))
            self.assertEqual(stack.manager.loads, 0)
            # the regression test for a client that sends no limit: the reply ends at the default
            status, _, reply = check.call(stack.base, 'POST', '/v1/chat/completions', {'model': QWEN, 'messages': USER})
            self.assertEqual((status, reply['choices'][0]['finish_reason'], reply['usage']['completion_tokens']), (200, 'length', 4))
            self.assertEqual(engine.requests[-1]['body']['max_tokens'], 4)


class ChatRequest(unittest.TestCase):
    def test_reasoning_effort_mapping(self):
        for effort, thinking in (('none', False), ('minimal', False), ('low', True), ('medium', True), ('high', True)):
            body, meta, notes = openai_conformance.chat_request({'messages': USER, 'reasoning_effort': effort})
            self.assertNotIn('reasoning_effort', body)
            self.assertEqual(body['chat_template_kwargs'], {'enable_thinking': thinking}, effort)
        body, _, _ = openai_conformance.chat_request({'messages': USER, 'reasoning_effort': 'none',
                                                      'chat_template_kwargs': {'enable_thinking': True}})
        self.assertEqual(body['chat_template_kwargs'], {'enable_thinking': True})  # explicit switch wins
        body, meta, notes = openai_conformance.chat_request({'messages': USER, 'reasoning_effort': 'galaxy'})
        self.assertNotIn('chat_template_kwargs', body)
        self.assertIn('reasoning_effort_unknown', notes)
        self.assertTrue(meta.warnings)
        body, meta, _ = openai_conformance.chat_request({'messages': USER, 'reasoning_effort': 'high'}, flash=True)
        self.assertNotIn('chat_template_kwargs', body)
        self.assertEqual(meta.ignored, ['reasoning_effort'])
        with self.assertRaises(openai_api.BadParam):
            openai_conformance.chat_request({'messages': USER, 'reasoning_effort': 3})
        # absent: the server default (the controller's enable_thinking false) is unchanged
        body, _, _ = openai_conformance.chat_request({'messages': USER})
        self.assertNotIn('chat_template_kwargs', body)

    def test_response_format(self):
        body, meta, _ = openai_conformance.chat_request({'messages': USER, 'response_format': {'type': 'json_object'}})
        self.assertNotIn('response_format', body)
        self.assertTrue(body['messages'][-1]['content'].startswith('hi\n\n'))
        self.assertIn('JSON object', body['messages'][-1]['content'])
        self.assertEqual(len(body['messages']), 1)  # the prefix (earlier messages) is unchanged
        schema = {'type': 'object', 'properties': {'a': {'type': 'integer'}}, 'required': ['a']}
        body, meta, _ = openai_conformance.chat_request(
            {'messages': [{'role': 'user', 'content': [{'type': 'text', 'text': 'q'}]}, {'role': 'tool', 'content': 'x', 'tool_call_id': 'c'}],
             'response_format': {'type': 'json_schema', 'json_schema': {'name': 'n', 'schema': schema}}})
        self.assertEqual(body['messages'][-1]['role'], 'user')  # not after a tool result: a new user message
        self.assertIn('"required":["a"]', body['messages'][-1]['content'])
        self.assertEqual(meta.json_mode, 'schema')
        with self.assertRaises(openai_api.BadParam) as caught:
            openai_conformance.chat_request({'messages': USER, 'response_format': {
                'type': 'json_schema', 'json_schema': {'name': 'n', 'schema': schema, 'strict': True}}})
        self.assertEqual(caught.exception.param, 'response_format.json_schema.strict')
        body, _, _ = openai_conformance.chat_request({'messages': USER, 'response_format': {'type': 'text'}})
        self.assertEqual(body['response_format'], {'type': 'text'})
        with self.assertRaises(openai_api.BadParam):
            openai_conformance.chat_request({'messages': USER, 'response_format': {'type': 'yaml'}})

    def test_json_check_of_a_reply(self):
        _, meta, _ = openai_conformance.chat_request({'messages': USER, 'response_format': {'type': 'json_object'}})
        message = {'content': 'Sure:\n```json\n{"a": 1}\n```'}
        openai_conformance.check_json(message, meta)
        self.assertEqual((message['content'], meta.warnings), ('{"a": 1}', []))
        message = {'content': 'no json here'}
        openai_conformance.check_json(message, meta)
        self.assertEqual(message['content'], 'no json here')
        self.assertTrue(meta.warnings)
        schema = {'type': 'object', 'required': ['a', 'b']}
        _, meta, _ = openai_conformance.chat_request({'messages': USER, 'response_format': {
            'type': 'json_schema', 'json_schema': {'name': 'n', 'schema': schema}}})
        openai_conformance.check_json({'content': '{"a": 1}'}, meta)
        self.assertIn('b', meta.warnings[0])

    def test_sampling_controls_are_forwarded_and_reported_only_when_unsupported(self):
        body = {'messages': USER, 'presence_penalty': 0.5, 'frequency_penalty': 0.25, 'logit_bias': {'0': 10}}
        for supported in (None, frozenset(oxe_runtime.SAMPLING_CONTROLS)):
            sent, meta, _ = openai_conformance.chat_request(body, sampling=supported)
            self.assertEqual({k: sent[k] for k in ('presence_penalty', 'frequency_penalty', 'logit_bias')},
                             {'presence_penalty': 0.5, 'frequency_penalty': 0.25, 'logit_bias': {'0': 10}})
            self.assertEqual(meta.headers(), [], supported)  # supported, or unknown (the engine decides)
        sent, meta, _ = openai_conformance.chat_request(body, sampling=frozenset())
        self.assertEqual(sent['logit_bias'], {'0': 10})  # still forwarded: the same body as on the engine port
        self.assertEqual(meta.headers(), [('X-Local-AI-Ignored', 'presence_penalty, frequency_penalty, logit_bias')])
        _, meta, _ = openai_conformance.chat_request({'messages': USER, 'logit_bias': {'5': 0}, 'presence_penalty': 0},
                                                     sampling=frozenset())
        self.assertEqual(meta.headers(), [])  # zero values are no-ops everywhere

    def test_engine_reports_its_sampling_controls(self):
        self.assertIsNone(oxe_runtime.sampling_support(None))
        self.assertEqual(oxe_runtime.sampling_support({'default_generation_settings': {'n_ctx': 5}}), frozenset())
        self.assertEqual(oxe_runtime.sampling_support({'default_generation_settings': {'max_output_tokens': 8}}),
                         frozenset(oxe_runtime.SAMPLING_CONTROLS))
        self.assertEqual(oxe_runtime.sampling_support({'oxe_hints': {'sampling': ['logit_bias', 'x']},
                                                       'default_generation_settings': {'max_output_tokens': 8}}),
                         frozenset({'logit_bias'}))

    def test_n(self):
        self.assertNotIn('n', openai_api.single_choice({'n': 1}, []))
        self.assertNotIn('n', openai_api.single_choice({'n': None}, []))
        for bad in (2, 0, 'x'):
            with self.assertRaises(openai_api.BadParam) as caught:
                openai_api.single_choice({'n': bad}, [])
            self.assertEqual(caught.exception.param, 'n')


class Usage(unittest.TestCase):
    def test_details_are_filled(self):
        usage = openai_conformance.complete_usage({'prompt_tokens': 5, 'completion_tokens': 10, 'total_tokens': 15}, 4,
                                                  {'cache_n': 3})
        self.assertEqual(usage['prompt_tokens_details'], {'cached_tokens': 3})
        self.assertEqual(usage['completion_tokens_details'], {'reasoning_tokens': 4})
        kept = openai_conformance.complete_usage({'completion_tokens': 10, 'completion_tokens_details': {'reasoning_tokens': 7},
                                                  'prompt_tokens_details': {'cached_tokens': 1}}, 4)
        self.assertEqual((kept['completion_tokens_details']['reasoning_tokens'], kept['prompt_tokens_details']['cached_tokens']), (7, 1))
        self.assertEqual(openai_conformance.reasoning_estimate({'reasoning_content': 'abcd', 'content': 'abcd'}, 10), 5)
        self.assertEqual(openai_conformance.reasoning_estimate({'content': 'abcd'}, 10), 0)

    def test_stream_counts_reasoning_and_passes_other_frames(self):
        meta = openai_conformance.ChatMeta()
        stream = openai_conformance.ChatStream(meta)
        frames = [b': queued pos=1 wait_s=0\n\n',
                  b'data: {"choices":[{"delta":{"reasoning_content":"a"}}]}\n\n',
                  b'data: {"choices":[{"delta":{"reasoning_content":"b"}}]}\n\n',
                  b'data: {"choices":[{"delta":{"content":"x"}}]}\n\n',
                  b'data: {"choices":[],"usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}}\n\n',
                  b'data: [DONE]\n\n']
        out = b''.join(stream.feed(f[:7]) + stream.feed(f[7:]) for f in frames) + stream.finish()
        self.assertTrue(out.startswith(frames[0] + frames[1] + frames[2] + frames[3]))
        usage = [v for _, v in sse_frames(out.decode()) if isinstance(v, dict) and 'usage' in v][0]['usage']
        self.assertEqual(usage['completion_tokens_details'], {'reasoning_tokens': 2})
        self.assertTrue(out.endswith(b'data: [DONE]\n\n'))


class Errors(unittest.TestCase):
    def test_string_codes_and_params(self):
        engine = openai_api.normalize_error(400, {'error': {'code': 400, 'message': 'the request exceeds the available context size',
                                                            'type': 'exceed_context_size_error', 'n_ctx': 5}})['error']
        self.assertEqual((engine['code'], engine['param'], engine['type'], engine['n_ctx']),
                         ('context_length_exceeded', 'messages', 'exceed_context_size_error', 5))
        self.assertEqual(openai_api.normalize_error(404, {'error': {'code': 404, 'message': 'File Not Found'}})['error']['code'], 'not_found')
        self.assertEqual(openai_api.normalize_error(500, {'error': 'boom'})['error']['code'], 'server_error')
        self.assertEqual(openai_api.normalize_error(404, {'error': 'Unknown local model.'})['error']['code'], 'model_not_found')
        kept = openai_api.normalize_error(409, {'error': {'message': 'm', 'code': 'model_busy'}})['error']
        self.assertEqual(kept['code'], 'model_busy')


class SessionHint(unittest.TestCase):
    def test_prompt_cache_key_is_the_session_header(self):
        with unittest.mock.patch.object(request_ledger, '_install_salt', return_value=b'k' * 32):
            by_header = request_ledger.session_key({'X-Session-Id': 'abc'}, {'messages': USER})
            by_body = request_ledger.session_key({}, {'prompt_cache_key': 'abc', 'messages': USER})
            self.assertEqual(by_header, by_body)
            self.assertNotEqual(by_body, request_ledger.session_key({}, {'messages': USER}))
            # the header wins when both are sent
            self.assertEqual(request_ledger.session_key({'X-Session-Id': 'h'}, {'prompt_cache_key': 'abc'}),
                             request_ledger.session_key({'X-Session-Id': 'h'}, {}))

    def test_replay_accepts_it_as_an_explicit_session(self):
        handler = unittest.mock.Mock(headers={})
        self.assertEqual(ctl_replay.explicit_session(handler, {'prompt_cache_key': 'abc'}), ('h:abc', None))
        self.assertEqual(ctl_replay.explicit_session(handler, {}), (None, None))


class ResponsesTranslation(unittest.TestCase):
    def test_request(self):
        chat, meta = openai_responses.chat_request({
            'model': 'local-default', 'instructions': 'Be brief.', 'max_output_tokens': 50, 'reasoning': {'effort': 'low'},
            'tools': [{'type': 'function', 'name': 'shell', 'parameters': {'type': 'object'}},
                      {'type': 'custom', 'name': 'apply_patch', 'description': 'patch'}],
            'input': [{'role': 'user', 'content': [{'type': 'input_text', 'text': 'go'}]},
                      {'type': 'reasoning', 'summary': []},
                      {'type': 'message', 'role': 'assistant', 'content': [{'type': 'output_text', 'text': 'ok'}]},
                      {'type': 'function_call', 'call_id': 'c1', 'name': 'shell', 'arguments': '{"cmd":"ls"}'},
                      {'type': 'function_call_output', 'call_id': 'c1', 'output': 'a.txt'}],
            'stream': True, 'store': True, 'prompt_cache_key': 's1', 'text': {'format': {'type': 'json_object'}}})
        self.assertEqual([m['role'] for m in chat['messages']], ['system', 'user', 'assistant', 'tool'])
        self.assertEqual(chat['messages'][2]['tool_calls'][0]['function'], {'name': 'shell', 'arguments': '{"cmd":"ls"}'})
        self.assertEqual(chat['messages'][3], {'role': 'tool', 'tool_call_id': 'c1', 'content': 'a.txt'})
        self.assertEqual(chat['max_completion_tokens'], 50)
        self.assertEqual(chat['reasoning_effort'], 'low')
        self.assertEqual(chat['stream_options'], {'include_usage': True})
        self.assertEqual(chat['tools'][1]['function']['parameters'], openai_responses.CUSTOM_PARAMETERS)
        self.assertEqual(chat['response_format'], {'type': 'json_object'})
        self.assertEqual(chat['prompt_cache_key'], 's1')
        self.assertEqual(meta['custom'], {'apply_patch'})
        self.assertEqual(openai_responses.chat_request({'input': 'hi'})[0]['messages'], USER)

    def test_refusals(self):
        for body, param in (({'input': 'x', 'previous_response_id': 'resp_1'}, 'previous_response_id'),
                            ({'input': 'x', 'conversation': 'c'}, 'conversation'),
                            ({'input': 'x', 'background': True}, 'background'),
                            ({'input': [{'type': 'item_reference', 'id': 'x'}]}, 'input[0]'),
                            ({'input': 'x', 'tools': [{'type': 'web_search'}]}, 'tools[0].type'),
                            ({'input': []}, 'input')):
            with self.assertRaises(openai_api.BadParam) as caught:
                openai_responses.chat_request(body)
            self.assertEqual(caught.exception.param, param, body)

    def test_body(self):
        _, meta = openai_responses.chat_request({'input': 'x', 'tools': [{'type': 'custom', 'name': 'apply_patch'},
                                                                         {'type': 'function', 'name': 'f'}]})
        chat = {'model': ORNITH, 'choices': [{'finish_reason': 'tool_calls', 'message': {
            'content': 'Doing it.', 'reasoning_content': 'plan',
            'tool_calls': [{'id': 'c1', 'function': {'name': 'f', 'arguments': '{"a":1}'}},
                           {'id': 'c2', 'function': {'name': 'apply_patch', 'arguments': '{"input":"*** Begin Patch"}'}}]}}],
            'usage': {'prompt_tokens': 10, 'completion_tokens': 8, 'total_tokens': 18, 'prompt_tokens_details': {'cached_tokens': 6}}}
        value = openai_responses.response_body(chat, meta)
        self.assertEqual([i['type'] for i in value['output']], ['reasoning', 'message', 'function_call', 'custom_tool_call'])
        self.assertEqual(value['output'][2]['call_id'], 'c1')
        self.assertEqual(value['output'][3]['input'], '*** Begin Patch')
        self.assertEqual((value['status'], value['object'], value['model']), ('completed', 'response', ORNITH))
        self.assertEqual(value['usage']['input_tokens_details'], {'cached_tokens': 6})
        self.assertGreater(value['usage']['output_tokens_details']['reasoning_tokens'], 0)
        chat['choices'][0]['finish_reason'] = 'length'
        self.assertEqual(openai_responses.response_body(chat, meta)['incomplete_details'], {'reason': 'max_output_tokens'})


class ResponsesStreamIdentity(unittest.TestCase):
    """Each output item keeps the one id and output_index it was announced with, whatever the
    interleaving of text, reasoning and tool calls (Codex HOLD on d69bc4f)."""

    def run_stream(self, deltas):
        _, meta = openai_responses.chat_request({'input': 'x', 'stream': True, 'tools': [
            {'type': 'function', 'name': 'f', 'parameters': {'type': 'object'}},
            {'type': 'function', 'name': 'g', 'parameters': {'type': 'object'}}]})
        stream = openai_responses.ResponseStream(meta)
        out = b''
        for delta in deltas:
            out += stream.feed(b'data: ' + json.dumps({'choices': [{'delta': delta}]}).encode() + b'\n\n')
        out += stream.feed(b'data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}\n\ndata: [DONE]\n\n')
        events = [v for _, v in sse_frames(out.decode())]
        added = {e['output_index']: e['item']['id'] for e in events if e['type'] == 'response.output_item.added'}
        done = {e['output_index']: e['item']['id'] for e in events if e['type'] == 'response.output_item.done'}
        final = [i['id'] for i in events[-1]['response']['output']]
        self.assertEqual(len(added), len([e for e in events if e['type'] == 'response.output_item.added']))  # unique indices
        self.assertEqual(added, done)
        self.assertEqual([added[i] for i in sorted(added)], final)
        for e in events:  # every delta names its item by the announced pair
            if 'item_id' in e:
                self.assertEqual(added[e['output_index']], e['item_id'], e['type'])
        return [i['type'] for i in events[-1]['response']['output']]

    def test_interleavings(self):
        call = lambda i, name, args: {'tool_calls': [{'index': i, 'id': 'c%d' % i, 'function': {'name': name, 'arguments': args}}]}
        more = lambda i, args: {'tool_calls': [{'index': i, 'function': {'arguments': args}}]}
        self.assertEqual(self.run_stream([{'content': 'a'}, call(0, 'f', '{}')]), ['message', 'function_call'])
        self.assertEqual(self.run_stream([call(0, 'f', '{}'), {'content': 'After.'}]), ['function_call', 'message'])
        self.assertEqual(self.run_stream([{'reasoning_content': 'r'}, call(0, 'f', '{'), {'content': 'x'}, more(0, '}'),
                                          call(1, 'g', '{}'), {'content': 'y'}]),
                         ['reasoning', 'function_call', 'message', 'function_call', 'message'])


class ResponsesStreamEnds(unittest.TestCase):
    """Pre-review follow-ups: a mid-stream error closes the open items; an empty reply has the
    non-streamed shape; a call is done as soon as other output follows it."""

    def events(self, frames, tools=False):
        body = {'input': 'x', 'stream': True}
        if tools:
            body['tools'] = [{'type': 'function', 'name': n, 'parameters': {'type': 'object'}} for n in ('f', 'g')]
        _, meta = openai_responses.chat_request(body)
        stream = openai_responses.ResponseStream(meta)
        return [v for _, v in sse_frames((b''.join(stream.feed(f) for f in frames) + stream.finish()).decode())]

    def test_error_mid_stream_closes_the_open_item(self):
        events = self.events([b'data: {"choices":[{"delta":{"content":"Part"}}]}\n\n',
                              b'event: error\ndata: {"error":{"code":"server_error","message":"engine gone"}}\n\n'])
        kinds = [e['type'] for e in events]
        self.assertLess(kinds.index('response.output_item.done'), kinds.index('error'))
        self.assertEqual(kinds[-1], 'response.failed')
        output = events[-1]['response']['output']
        self.assertEqual((output[0]['status'], output[0]['content'][0]['text']), ('incomplete', 'Part'))

    def test_empty_reply_matches_the_non_streamed_shape(self):
        events = self.events([b'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}\n\n', b'data: [DONE]\n\n'])
        streamed = events[-1]['response']['output']
        _, meta = openai_responses.chat_request({'input': 'x'})
        body = openai_responses.response_body({'choices': [{'finish_reason': 'stop', 'message': {'content': ''}}]}, meta)
        shape = lambda items: [(i['type'], [(c['type'], c['text']) for c in i['content']]) for i in items]
        self.assertEqual(shape(streamed), shape(body['output']))
        self.assertEqual(shape(streamed), [('message', [('output_text', '')])])

    def test_interleaved_call_fragments_stay_complete(self):
        """Codex HOLD on 9091966: another call or text starting does not complete an earlier call;
        every argument fragment is kept (by call index) and output_item.done comes at the end."""
        frag = lambda i, args, name=None, cid=None: ('data: ' + json.dumps({'choices': [{'delta': {'tool_calls': [
            {'index': i, **({'id': cid} if cid else {}),
             'function': {**({'name': name} if name else {}), 'arguments': args}}]}}]}) + '\n\n').encode()
        text = b'data: {"choices":[{"delta":{"content":"between"}}]}\n\n'
        end = b'data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}\n\ndata: [DONE]\n\n'
        for frames in ([frag(0, '{"x":', 'f', 'c0'), frag(1, '{"y":', 'g', 'c1'), text, frag(0, '1}'), frag(1, '2}'), end],
                       [frag(0, '{"x":', 'f', 'c0'), text, frag(0, '1}'), end]):
            events = self.events(frames, tools=True)
            done = [e['item'] for e in events if e['type'] == 'response.output_item.done' and e['item']['type'] == 'function_call']
            final = [i for i in events[-1]['response']['output'] if i['type'] == 'function_call']
            for item in done + final:
                self.assertIsInstance(json.loads(item['arguments']), dict)
            self.assertEqual([json.loads(i['arguments']) for i in final], [{'x': 1}, {'y': 2}][:len(final)])
            self.assertEqual([i['arguments'] for i in done], [i['arguments'] for i in final])
            args_done = {e['item_id']: e['arguments'] for e in events if e['type'] == 'response.function_call_arguments.done'}
            self.assertEqual(sorted(args_done.values()), sorted(i['arguments'] for i in final))
            deltas = {}
            for e in events:
                if e['type'] == 'response.function_call_arguments.delta':
                    deltas[e['item_id']] = deltas.get(e['item_id'], '') + e['delta']
            self.assertEqual(deltas, args_done)  # the deltas add up to the announced arguments


class ReplyHeaders(unittest.TestCase):
    def test_a_reissue_does_not_repeat_conformance_headers(self):
        from oxc.runtime import ctl_proxy_stream
        handler = unittest.mock.Mock(reply_extra_headers=[('x-request-id', 'req_1')])
        for value in ('w1', 'w2'):  # EG1: forward() runs again for the re-queued request
            ctl_proxy_stream.add_reply_headers(handler, [('X-Local-AI-Warning', value)])
        self.assertEqual(handler.reply_extra_headers, [('x-request-id', 'req_1'), ('X-Local-AI-Warning', 'w2')])


class StackConformance(unittest.TestCase):
    """The controller end to end over the fake engine (queue on, as by default)."""

    @classmethod
    def setUpClass(cls):
        cls.engine = FakeEngine(tool_reply=True, think_reply=True)
        cls.addClassCleanup(cls.engine.close)

    def stack(self, **config):
        stack = Stack(self.engine.port, **config)
        self.addCleanup(stack.close)
        return stack

    def test_unknown_v1_paths_never_load_a_model(self):
        stack = self.stack()
        for method, path in (('POST', '/v1/audio/speech'), ('POST', '/v1/images/generations'), ('GET', '/v1/files'),
                             ('POST', '/v1/batches'), ('POST', '/v1/embeddings')):
            status, headers, value = check.call(stack.base, method, path, {'model': 'local-default', 'input': 'x'} if method == 'POST' else None)
            self.assertEqual(status, 404, path)
            self.assertIn(value['error']['code'], ('unsupported_endpoint', 'unknown_url'), path)
            self.assertIn('x-request-id', headers)
        self.assertEqual(stack.manager.loads, 0)

    def test_n_and_strict_schema_are_clear_400s(self):
        stack = self.stack()
        status, _, value = check.call(stack.base, 'POST', '/v1/chat/completions', {'model': 'local-default', 'messages': USER, 'n': 2})
        self.assertEqual((status, value['error']['param'], value['error']['code']), (400, 'n', 'unsupported_value'))
        status, _, value = check.call(stack.base, 'POST', '/v1/chat/completions', {'model': 'local-default', 'messages': USER,
                                      'response_format': {'type': 'json_schema', 'json_schema': {'name': 'x', 'schema': {}, 'strict': True}}})
        self.assertEqual((status, value['error']['param']), (400, 'response_format.json_schema.strict'))

    def test_chat_usage_headers_and_reasoning(self):
        stack = self.stack()
        status, headers, value = check.call(stack.base, 'POST', '/v1/chat/completions', {
            'model': 'local-default', 'messages': USER, 'reasoning_effort': 'high', 'presence_penalty': 1})
        self.assertEqual(status, 200)
        self.assertEqual(self.engine.requests[-1]['body']['chat_template_kwargs']['enable_thinking'], True)
        self.assertNotIn('reasoning_effort', self.engine.requests[-1]['body'])
        self.assertGreater(value['usage']['completion_tokens_details']['reasoning_tokens'], 0)
        self.assertIn('cached_tokens', value['usage']['prompt_tokens_details'])
        self.assertNotIn('X-Local-AI-Ignored', headers)  # nothing loaded reported its controls: forwarded, not judged
        self.assertEqual(self.engine.requests[-1]['body']['presence_penalty'], 1)
        # the loaded engine reports no sampling controls (an older engine): named as ignored, still forwarded
        stack.manager.engine_sampling = frozenset()
        status, headers, _ = check.call(stack.base, 'POST', '/v1/chat/completions', {
            'model': 'local-default', 'messages': USER, 'presence_penalty': 1, 'logit_bias': {'7': 3}})
        self.assertEqual((status, headers.get('X-Local-AI-Ignored')), (200, 'presence_penalty, logit_bias'))
        self.assertEqual(self.engine.requests[-1]['body']['logit_bias'], {'7': 3})
        limits = stack.manager.model_limits()
        self.assertEqual(limits[ORNITH]['capabilities']['sampling'], [])
        stack.manager.engine_sampling = frozenset(oxe_runtime.SAMPLING_CONTROLS)
        self.assertEqual(stack.manager.model_limits()[ORNITH]['capabilities']['sampling'],
                         ['frequency_penalty', 'logit_bias', 'presence_penalty'])
        self.assertIsNone(stack.manager.model_limits()[QWEN]['capabilities']['sampling'])  # not the loaded one
        status, headers, _ = check.call(stack.base, 'POST', '/v1/chat/completions', {
            'model': 'local-default', 'messages': USER, 'presence_penalty': 1})
        self.assertNotIn('X-Local-AI-Ignored', headers)
        status, headers, text = check.call(stack.base, 'POST', '/v1/chat/completions', {
            'model': 'local-default', 'messages': USER, 'reasoning_effort': 'medium', 'stream': True,
            'stream_options': {'include_usage': True}})
        usage = [v for _, v in sse_frames(text) if isinstance(v, dict) and v.get('usage')][0]['usage']
        self.assertEqual(usage['completion_tokens_details']['reasoning_tokens'], 3)
        self.assertTrue(headers['x-request-id'].startswith('req_'))

    def test_json_object_best_effort(self):
        stack = self.stack()
        status, headers, value = check.call(stack.base, 'POST', '/v1/chat/completions', {
            'model': 'local-default', 'messages': USER, 'response_format': {'type': 'json_object'}})
        self.assertEqual(status, 200)  # the fake engine's prose is no JSON: returned as written, with a warning
        self.assertIn('not valid JSON', headers.get('X-Local-AI-Warning', ''))
        self.assertNotIn('response_format', self.engine.requests[-1]['body'])
        self.assertIn('JSON object', self.engine.requests[-1]['body']['messages'][-1]['content'])

    def test_responses_non_stream_and_stream(self):
        stack = self.stack()
        tools = [{'type': 'function', 'name': 'get_weather', 'parameters': {'type': 'object', 'properties': {'city': {'type': 'string'}}}}]
        status, headers, value = check.call(stack.base, 'POST', '/v1/responses', {'model': 'local-default', 'input': 'Weather?', 'tools': tools})
        self.assertEqual(status, 200, value)
        self.assertEqual(self.engine.requests[-1]['path'], '/v1/chat/completions')
        self.assertEqual([i['type'] for i in value['output']], ['function_call'])  # no text: no message item
        call = [i for i in value['output'] if i['type'] == 'function_call'][0]
        self.assertEqual((call['name'], json.loads(call['arguments'])), ('get_weather', {'city': 'Oslo'}))
        status, _, text = check.call(stack.base, 'POST', '/v1/responses', {
            'model': 'local-default', 'input': [{'role': 'user', 'content': 'hi'}], 'stream': True, 'reasoning': {'effort': 'high'}})
        events = sse_frames(text)
        kinds = [e for e, _ in events]
        self.assertEqual(kinds[:2], ['response.created', 'response.in_progress'])
        self.assertEqual(kinds[-1], 'response.completed')
        self.assertIn('response.reasoning_text.delta', kinds)
        self.assertIn('response.output_text.delta', kinds)
        final = events[-1][1]['response']
        self.assertEqual([i['type'] for i in final['output']], ['reasoning', 'message'])
        self.assertEqual(final['output'][1]['content'][0]['text'], 'Hello from the fake engine.')
        self.assertEqual(final['usage']['output_tokens_details']['reasoning_tokens'], 3)
        self.assertEqual([e[1]['sequence_number'] for e in events], list(range(len(events))))
        status, _, value = check.call(stack.base, 'POST', '/v1/responses', {'input': 'x', 'previous_response_id': 'resp_1'})
        self.assertEqual((status, value['error']['param']), (400, 'previous_response_id'))

    def test_engine_error_gets_a_string_code(self):
        stack = self.stack()
        status, _, value = check.call(stack.base, 'POST', '/tokenize', {'content': 'x'})  # the fake engine: 404, integer code
        self.assertEqual((status, value['error']['code'], value['error']['type']), (404, 'not_found', 'not_found_error'))

if __name__ == '__main__':
    unittest.main()
