"""Record synthetic Python Responses requests and stream events; sockets forbidden."""
import argparse
import json
import os
from pathlib import Path
import socket
import sys
import tempfile
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / 'oxc' / 'src'))
CREATED = 1700000000


def record():
    from oxc.runtime import cloud_openai as source
    item = {'id': 'synthetic-model', 'upstream_model': 'gpt-6-sol', 'max_output': 1024,
            'base_url': 'https://example.invalid/v1'}
    user = {'role': 'user', 'content': 'Synthetic λ'}
    call = {'id': 'call_1', 'type': 'function', 'function': {'name': 'read', 'arguments': '{"x":1}'}}
    assistant = {'role': 'assistant', 'content': None, 'tool_calls': [call]}
    tool_result = {'role': 'tool', 'tool_call_id': 'call_1', 'content': [{'type': 'text', 'text': 'Synthetic result'}]}
    native = [{'type': 'reasoning', 'encrypted_content': 'synthetic-opaque-state', 'summary': []},
              {'type': 'function_call', 'call_id': 'call_1', 'name': 'read', 'arguments': '{"x":1}', 'status': 'completed'}]
    terminal = {'id': 'response_synthetic', 'created_at': CREATED, 'status': 'completed', 'output': native,
                'usage': {'input_tokens': 10, 'output_tokens': 7,
                          'input_tokens_details': {'cached_tokens': 3}, 'output_tokens_details': {'reasoning_tokens': 4}}}
    cases = [
        ('basic', {'messages': [user]}, None),
        ('tools_format', {'messages': [{'role': 'system', 'content': 'Synthetic system'}, user],
            'tools': [{'type': 'function', 'function': {'name': 'read', 'parameters': {'type': 'object'}}}],
            'tool_choice': {'type': 'function', 'function': {'name': 'read'}}, 'parallel_tool_calls': False,
            'reasoning_effort': 'xhigh', 'max_completion_tokens': 2000, 'stream': True,
            'response_format': {'type': 'json_schema', 'json_schema': {'name': 'synthetic', 'schema': {'type': 'object'}}}}, None),
        ('images_none', {'messages': [{'role': 'user', 'content': [{'type': 'text', 'text': 'Synthetic'},
            {'type': 'image_url', 'image_url': {'url': 'https://example.invalid/image.png', 'detail': 'low'}}]}],
            'reasoning_effort': 'none', 'temperature': .2, 'top_p': .8, 'top_logprobs': 1,
            'response_format': {'type': 'json_object'}}, None),
        ('switch_minimal', {'messages': [user], 'chat_template_kwargs': {'enable_thinking': False}}, None),
        ('closed_replay', {'messages': [user, assistant, tool_result, user]}, None),
        ('open_missing', {'messages': [user, assistant, tool_result]}, None),
        ('restored', {'messages': [user, assistant, tool_result]}, terminal),
    ]
    cases += [('invalid_' + name, payload, None) for name, payload in [
        ('role', {'messages': [{'role': 'other', 'content': 'x'}]}),
        ('messages', {'messages': {}}), ('null_user', {'messages': [{'role': 'user', 'content': None}]}),
        ('part', {'messages': [{'role': 'user', 'content': [1]}]}),
        ('image', {'messages': [{'role': 'user', 'content': [{'type': 'image_url', 'image_url': {'url': ''}}]}]}),
        ('call', {'messages': [{'role': 'assistant', 'tool_calls': [{'type': 'function'}]}]}),
        ('tool', {'tools': [{'type': 'function', 'function': {'name': 'read', 'parameters': []}}]}),
        ('strict', {'tools': [{'type': 'function', 'function': {'name': 'read', 'strict': 'yes'}}]}),
        ('choice', {'tool_choice': 'other'}), ('n', {'n': True}),
        ('limit', {'max_completion_tokens': 0}), ('effort', {'reasoning_effort': 'other'}),
    ]]
    requests = []
    for name, payload, remembered in cases:
        source._HISTORY.clear()
        row = {'name': name, 'item': item, 'input': payload}
        if remembered:
            source.response(remembered, item, [user], None)
            row['remember'] = {'history': [user], 'assistant': assistant, 'output': native}
        try:
            # Mock upstream capture: exactly the kwargs the HTTP layer would send.
            row['expected'] = dict(source.request(payload, item))
        except source.cloud.CloudError as error:
            row['error'] = {'status': error.status, 'code': error.code, 'param': error.param}
        requests.append(row)
    streams = []
    events = [{'type': 'response.created'},
        {'type': 'response.reasoning_summary_text.delta', 'delta': 'Synthetic reasoning'},
        {'type': 'response.output_text.delta', 'delta': 'Synthetic λ'},
        {'type': 'response.refusal.delta', 'delta': 'Synthetic refusal'},
        {'type': 'response.output_item.added', 'output_index': 2, 'item': native[1]},
        {'type': 'response.function_call_arguments.delta', 'output_index': 2, 'delta': '{"x":'},
        {'type': 'response.function_call_arguments.delta', 'output_index': 2, 'delta': '1}'}]
    for status, reason in [('completed', None), ('incomplete', 'max_output_tokens'), ('incomplete', 'content_filter')]:
        value = {**terminal, 'status': status}
        if reason:
            value['incomplete_details'] = {'reason': reason}
        incoming = events + [{'type': 'response.' + status, 'response': value}]
        stream = source.ResponsesStream(item, [user], None)
        chunks = []
        with patch.object(source.time, 'time', return_value=CREATED):
            for event in incoming:
                wire = stream._translate(event)
                for frame in wire.split(b'\n\n'):
                    if frame and frame != b'data: [DONE]':
                        chunks.append(json.loads(frame.removeprefix(b'data: ')))
            reply = source.response(value, item, [user], None)
        streams.append({'name': status + '_' + str(reason), 'events': incoming,
                        'expected': chunks, 'terminal': value, 'reply': reply})
    return requests, streams


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='ox-responses-fixtures-') as home:
        os.environ.clear()
        os.environ.update(HOME=home, XDG_CONFIG_HOME=home, XDG_DATA_HOME=home, XDG_CACHE_HOME=home)
        with patch.object(socket, 'socket', side_effect=AssertionError('network forbidden')):
            records = record()
    for name, rows in zip(('openai_responses_requests', 'openai_responses_streams'), records):
        data = ''.join(json.dumps(row, ensure_ascii=False, sort_keys=True, separators=(',', ':')) + '\n' for row in rows).encode()
        target = Path(__file__).with_name(name + '.jsonl')
        if args.check:
            assert target.read_bytes() == data, 'Python fixture drift: ' + name
        else:
            target.write_bytes(data)
