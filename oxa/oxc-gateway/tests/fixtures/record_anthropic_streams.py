"""Record synthetic events through the Python stream translator (no transport)."""
import argparse
import json
import os
from pathlib import Path
import socket
import sys
import tempfile
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[4]
sys.path.insert(0, str(ROOT / 'oxc' / 'src'))
CREATED = 1700000000


def record():
    from oxc.runtime import cloud_anthropic as source

    def start(usage=None):
        return {'type': 'message_start', 'message': {'usage': usage or {
            'input_tokens': 10, 'output_tokens': 0, 'cache_read_input_tokens': 3,
            'cache_creation_input_tokens': 2}}}

    def block(index, kind, **fields):
        return {'type': 'content_block_start', 'index': index,
                'content_block': {'type': kind, **fields}}

    def delta(index, kind, **fields):
        return {'type': 'content_block_delta', 'index': index, 'delta': {'type': kind, **fields}}

    def stop(index):
        return {'type': 'content_block_stop', 'index': index}

    def end(reason, usage=None):
        return [{'type': 'message_delta', 'delta': {'stop_reason': reason},
                 'usage': {'output_tokens': 7} if usage is None else usage}, {'type': 'message_stop'}]

    text = [start(), block(0, 'text', text=''), delta(0, 'text_delta', text='Synthetic λ'), stop(0)]
    cases = [(reason, text + end(reason), True) for reason in source.STOP_REASONS]
    cases += [
        ('no_usage_requested', text + end('end_turn'), False),
        ('thinking_tools', [start(), {'type': 'ping'}, block(0, 'thinking'),
            delta(0, 'thinking_delta', thinking='Synthetic thought'),
            delta(0, 'signature_delta', signature='synthetic-signature'), stop(0),
            block(1, 'redacted_thinking', data='synthetic-opaque-state'), stop(1),
            block(2, 'tool_use', id='call_1', name='read', input={}),
            delta(2, 'input_json_delta', partial_json='{"path":'),
            delta(2, 'input_json_delta', partial_json='"λ"}'), stop(2),
            block(3, 'tool_use', id='call_2', name='other', input={}),
            delta(3, 'input_json_delta', partial_json='{}'), stop(3)] + end('tool_use'), True),
        ('invalid_json_fallback', [start(), block(0, 'tool_use', id='call_1', name='read', input={}),
            delta(0, 'input_json_delta', partial_json='invalid'), stop(0)] + end('tool_use'), True),
        ('invalid_usage', text + end('end_turn', {'output_tokens': True}), True),
        ('unknown_stop', text + end('future_stop_reason'), True),
        ('empty_text', [start(), block(0, 'text', text=''), delta(0, 'text_delta', text=''), stop(0)]
            + end('end_turn'), True),
        ('repeated_signature', [start(), block(0, 'thinking'),
            delta(0, 'signature_delta', signature='opaque-first'),
            delta(0, 'signature_delta', signature='opaque-replacement'), stop(0)] + end('end_turn'), True),
        ('redaction', [start(), block(0, 'text', text=''),
            delta(0, 'text_delta', text='Synthetic λ synthetic-redact-'),
            delta(0, 'text_delta', text='marker synthetic-'), stop(0), block(1, 'thinking'),
            delta(1, 'thinking_delta', thinking='synthetic-redact-'),
            delta(1, 'thinking_delta', thinking='marker synthetic-'),
            delta(1, 'signature_delta', signature='opaque-signature'), stop(1),
            block(2, 'tool_use', id='call_1', name='read_synthetic-redact-marker', input={}),
            delta(2, 'input_json_delta', partial_json='{"note":"synthetic-redact-'),
            delta(2, 'input_json_delta', partial_json='marker"}'), stop(2)] + end('tool_use'), True),
    ]
    rows = []
    for name, events, include_usage in cases:
        marker = 'synthetic-redact-marker' if name == 'redaction' else None
        stream = source._Stream({'id': 'synthetic-model', 'upstream_model': 'synthetic'}, include_usage, key=marker)
        chunks = []
        with patch.object(source.time, 'time', return_value=CREATED):
            for event in events:
                chunks.extend(source.cloud.redact_value(chunk, marker) for chunk in stream.event(event))
        rows.append({'name': name, 'events': events, 'include_usage': include_usage,
                     'expected': chunks, 'native_blocks': stream.blocks, 'redact_marker': marker,
                     'final_usage': stream.usage if stream.usage_final else None})
    return ''.join(json.dumps(row, ensure_ascii=False, sort_keys=True, separators=(',', ':')) + '\n'
                   for row in rows)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='ox-upstream-stream-') as home:
        os.environ.clear()
        os.environ.update(HOME=home, XDG_CONFIG_HOME=home, XDG_DATA_HOME=home, XDG_CACHE_HOME=home)
        with patch.object(socket, 'socket', side_effect=AssertionError('network forbidden')):
            data = record()
    target = Path(__file__).with_name('anthropic_streams.jsonl')
    if args.check:
        assert target.read_bytes() == data.encode(), 'Python fixture drift'
    else:
        target.write_bytes(data.encode())
