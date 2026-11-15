"""Record synthetic Python translator contracts into a mock Messages upstream.

Run with python3 -I; --check verifies the committed JSONL instead of writing it.
Object keys are sorted for byte comparison; arrays and all string bytes retain order.
Empty/array tool-result corrections required by #334 are tested separately in Rust.
"""
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
    from oxc.runtime import cloud_anthropic as source

    class MockMessages:
        def create(self, **parameters):
            self.body = copy.deepcopy(parameters)

    user = {'role': 'user', 'content': 'synthetic question λ'}
    tool = {'type': 'function', 'function': {'name': 'read', 'description': 'Read synthetic text',
            'parameters': {'type': 'object', 'properties': {'path': {'type': 'string'}}}}}
    call = {'id': 'call_1', 'type': 'function',
            'function': {'name': 'read', 'arguments': '{"path":"sample"}'}}
    cases = [
        ('text', {'messages': [user]}),
        ('system_tools', {'messages': [
            {'role': 'system', 'content': 'Synthetic system'},
            {'role': 'developer', 'content': [{'type': 'text', 'text': 'Synthetic policy'}]}, user,
            {'role': 'assistant', 'content': None, 'tool_calls': [call]},
            {'role': 'tool', 'tool_call_id': 'call_1', 'content': 'Synthetic result'}],
            'tools': [tool, {'function': {'name': 'other'}}], 'tool_choice': 'auto',
            'reasoning_effort': 'medium', 'max_tokens': 100, 'stream': True,
            'temperature': .2, 'top_p': .9, 'top_k': 40, 'cache_prompt': True}),
        ('merge_roles', {'messages': [user, user, {'role': 'assistant', 'content': 'A'},
                                    {'role': 'assistant', 'content': 'B'}]}),
        ('empty_history', {'messages': []}),
        ('image_omitted', {'messages': [{'role': 'user', 'content': [
            {'type': 'text', 'text': 'Synthetic'}, {'type': 'image_url',
             'image_url': {'url': 'https://example.invalid/synthetic.png'}}]}]}),
        ('invalid_arguments', {'messages': [user, {'role': 'assistant', 'tool_calls': [
            {'function': {'arguments': 'broken'}}, {'function': {'arguments': '[1,2]'}},
            {'function': {'arguments': None}}, {'function': {'arguments': '{"x":"λ"}'}}]}]}),
        ('explicit_thinking', {'messages': [user, {'role': 'assistant', 'content': 'A',
            'thinking_blocks': [{'type': 'thinking', 'thinking': 'Synthetic thought',
                                 'signature': 'synthetic-signature'}]}]}),
        ('native_thinking_last', {'messages': [user, {'role': 'assistant', 'content': [
            {'type': 'redacted_thinking', 'data': 'synthetic-opaque-state'}]}]}),
        ('limits_stop', {'messages': [user], 'max_tokens': 99999, 'max_completion_tokens': 1,
                        'stop': ['END', 3, 'STOP'] + [str(n) for n in range(20)]}),
        ('thinking_switch', {'messages': [user], 'chat_template_kwargs': {'enable_thinking': False},
                             'stop': 'END', 'max_completion_tokens': 81}),
        ('required', {'messages': [user], 'tools': [tool], 'tool_choice': 'required'}),
        ('named', {'messages': [user], 'tools': [tool], 'tool_choice': {
            'type': 'function', 'function': {'name': 'read'}}, 'reasoning_effort': 'xhigh'}),
        ('none', {'messages': [user], 'tools': [tool], 'tool_choice': 'none'}),
        ('unknown_effort', {'messages': [user], 'reasoning_effort': 'unrecognized'}),
    ]
    rows = []
    for name, payload in cases:
        item = {'upstream_model': 'claude-haiku-5-5', 'max_output': 1024, 'id': 'synthetic-model'}
        mock = MockMessages()
        mock.create(**source.messages_request(copy.deepcopy(payload), item))
        rows.append({'name': name, 'input': payload, 'item': item, 'expected': mock.body})
    for field, value in [('max_tokens', 0), ('max_completion_tokens', True), ('n_predict', -1),
                         ('reasoning_effort', 1)]:
        payload = {'messages': [user], field: value}
        try:
            source.messages_request(payload, item)
            raise AssertionError('expected source rejection')
        except source.cloud.CloudError as error:
            rows.append({'name': 'invalid_' + field, 'input': payload, 'item': item,
                         'error_param': error.param})
    payload = {'messages': [user], 'tools': [tool], 'tool_choice': 'required'}
    item = {**item, 'upstream_model': 'claude-sonnet-5-5'}
    try:
        source.messages_request(payload, item)
        raise AssertionError('expected forced-choice rejection')
    except source.cloud.CloudError as error:
        rows.append({'name': 'forced_unsupported', 'input': payload, 'item': item,
                     'error_param': error.param})
    return ''.join(json.dumps(row, ensure_ascii=False, sort_keys=True, separators=(',', ':')) + '\n'
                   for row in rows)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='ox-upstream-fixtures-') as home:
        os.environ.clear()
        os.environ.update(HOME=home, XDG_CONFIG_HOME=home, XDG_DATA_HOME=home, XDG_CACHE_HOME=home)
        with patch.object(socket, 'socket', side_effect=AssertionError('network forbidden')):
            data = record()
    target = Path(__file__).with_name('anthropic_requests.jsonl')
    if args.check:
        assert target.read_bytes() == data.encode(), 'Python fixture drift'
    else:
        target.write_bytes(data.encode())
