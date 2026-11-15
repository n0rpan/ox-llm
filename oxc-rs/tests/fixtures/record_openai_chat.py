"""Record synthetic Chat request kwargs and Python mock stream/meter outputs."""
import argparse
import copy
import json
import os
from pathlib import Path
import socket
import sys
import tempfile
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / 'oxc' / 'src'))


def record():
    from oxc.runtime import cloud, cloud_openai as source
    user = {'role': 'user', 'content': 'Synthetic λ'}
    tool = {'type': 'function', 'function': {'name': 'read', 'parameters': {'type': 'object'}}}
    cases = [
        ('reasoning', 'o3', {'messages': [user], 'tools': [tool], 'reasoning_effort': 'xhigh',
            'max_tokens': 100, 'temperature': .7, 'top_p': .9, 'n': 1, 'provider': {'zdr': False},
            'cache_prompt': True, 'n_predict': 5, 'store': True, 'stream': True,
            'stream_options': {'include_usage': False}, 'stop': 'END', 'seed': 3}),
        ('text_tools', 'gpt-4.1-mini', {'messages': [user], 'tools': [tool], 'tool_choice': 'required',
            'parallel_tool_calls': False, 'temperature': .2, 'max_completion_tokens': 9999,
            'response_format': {'type': 'json_object'}}),
        ('switch', 'o4-mini', {'messages': [user], 'chat_template_kwargs': {'enable_thinking': False}}),
        ('precedence', 'o3', {'messages': [user], 'max_tokens': 5, 'max_completion_tokens': 8}),
        ('null_limit', 'o3', {'messages': [user], 'max_completion_tokens': None, 'max_tokens': 5}),
        ('unknown', 'unreviewed-model', {'messages': [user]}),
    ]
    cases += [('effort_' + effort, 'o3', {'messages': [user], 'reasoning_effort': effort})
              for effort in ('none', 'minimal', 'low', 'medium', 'high')]
    cases += [('invalid_' + name, 'o3', payload) for name, payload in [
        ('n', {'n': True}), ('n_two', {'n': 2}), ('limit', {'max_tokens': 0}),
        ('effort', {'reasoning_effort': []}), ('messages', {'messages': [None]}),
        ('tool', {'tools': [{'type': 'function', 'function': {'name': 'read', 'strict': 'yes'}}]}),
        ('choice', {'tool_choice': 1}),
    ]]
    requests = []
    class MockUpstream:
        def create(self, **kwargs):
            return copy.deepcopy(kwargs)
    for name, model, payload in cases:
        row = {'name': name, 'model': model, 'input': payload}
        try:
            row['expected'] = MockUpstream().create(**source.request(payload, {'upstream_model': model, 'max_output': 1024}))
        except cloud.CloudError as error:
            row['error'] = {'status': error.status, 'code': error.code, 'param': error.param}
        requests.append(row)

    usage = {'prompt_tokens': 15, 'completion_tokens': 7, 'total_tokens': 22,
             'prompt_tokens_details': {'cached_tokens': 3, 'cache_write_tokens': 2},
             'completion_tokens_details': {'reasoning_tokens': 4}}
    call = {'index': 0, 'id': 'call_1', 'type': 'function', 'function': {'name': 'read', 'arguments': '{"x":1}'}}
    base = {'id': 'chatcmpl_synthetic', 'created': 1700000000, 'model': 'o3'}
    reply = {**base, 'object': 'chat.completion', 'choices': [{'index': 0, 'message': {
        'role': 'assistant', 'content': None, 'tool_calls': [call]}, 'finish_reason': 'tool_calls'}], 'usage': usage}
    def chunk(delta, finish=None):
        return {**base, 'object': 'chat.completion.chunk', 'choices': [{'index': 0, 'delta': delta, 'finish_reason': finish}]}
    events = [chunk({'role': 'assistant', 'content': ''}),
              chunk({'reasoning_content': 'Synthetic reasoning'}), chunk({'content': 'Synthetic λ'}),
              chunk({'tool_calls': [{**call, 'function': {'name': 'read', 'arguments': ''}}]}),
              chunk({'tool_calls': [{'index': 0, 'function': {'arguments': '{"x":'}}]}),
              chunk({'tool_calls': [{'index': 0, 'function': {'arguments': '1}'}}]}),
              chunk({}, 'tool_calls'), {**base, 'object': 'chat.completion.chunk', 'choices': [], 'usage': usage}]
    stream = cloud.CloudStream('synthetic-model')
    meter = cloud.Meter()
    expected = []
    for event in events:
        meter._object(event)
        raw = stream._event(None, json.dumps(event, ensure_ascii=False).encode())
        expected.append(json.loads(raw.removeprefix(b'data: ')))
    return requests, [{'name': 'tool_stream', 'response': reply,
        'expected_response': cloud.normalize_response(copy.deepcopy(reply), 'synthetic-model'),
        'events': events, 'expected': expected,
        'meter': {'prompt': meter.prompt, 'completion': meter.completion, 'cached': meter.cached,
                  'cache_written': meter.cache_written, 'finish': meter.finish}}]


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='ox-chat-fixtures-') as home:
        os.environ.clear()
        os.environ.update(HOME=home, XDG_CONFIG_HOME=home, XDG_DATA_HOME=home, XDG_CACHE_HOME=home)
        with patch.object(socket, 'socket', side_effect=AssertionError('network forbidden')):
            records = record()
    for name, rows in zip(('openai_chat_requests', 'openai_chat_streams'), records):
        data = ''.join(json.dumps(row, ensure_ascii=False, sort_keys=True, separators=(',', ':')) + '\n' for row in rows).encode()
        target = Path(__file__).with_name(name + '.jsonl')
        if args.check:
            assert target.read_bytes() == data, 'Python fixture drift: ' + name
        else:
            target.write_bytes(data)
