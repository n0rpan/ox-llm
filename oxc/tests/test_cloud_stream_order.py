"""Stream redaction must preserve delta order and finish only the intended choice."""
import json
import unittest

from oxc.runtime import cloud
from test_cloud_remote import CloudCase, KEY
from mock_cloud import MockOpenRouter


class StreamOrder(unittest.TestCase):
    def transcode(self, events, key):
        stream = cloud.CloudStream('mock-model', key)
        wire = b''.join(b'data: ' + json.dumps(event).encode() + b'\n\n' for event in events)
        wire += b'data: [DONE]\n\n'
        out = b''.join(stream.feed(bytes([byte])) for byte in wire) + stream.finish()
        self.assertTrue(out.endswith(b'data: [DONE]\n\n'))
        return [json.loads(line[6:]) for line in out.splitlines() if line.startswith(b'data: {')]

    def test_tool_name_ending_in_key_prefix_is_not_split(self):
        # A name is atomic: holding its last character back made `ls` arrive as `l` then `s`.
        frames = self.transcode([{'choices': [{'index': 0, 'delta': {'tool_calls': [
            {'index': 0, 'id': 'c1', 'function': {'name': 'ls', 'arguments': ''}}]},
            'finish_reason': None}]}], 'sk-synthetic')
        names = [call['function']['name'] for frame in frames for choice in frame.get('choices', [])
                 for call in choice.get('delta', {}).get('tool_calls', []) if 'name' in call['function']]
        self.assertEqual(names, ['ls'])

    def test_tool_name_key_split_across_deltas_is_redacted(self):
        events = [{'choices': [{'index': 0, 'delta': {'tool_calls': [
            {'index': 0, 'id': 'c1', 'function': {'name': piece}}]}, 'finish_reason': None}]}
            for piece in ('sk-syn', 'thetic')]
        events.append({'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'tool_calls'}]})
        frames = self.transcode(events, 'sk-synthetic')
        names = [call['function']['name'] for frame in frames for choice in frame.get('choices', [])
                 for call in choice.get('delta', {}).get('tool_calls', []) if 'name' in call['function']]
        self.assertNotIn('sk-synthetic', ''.join(names))
        self.assertEqual(len(names), 1)

    def test_final_delta_precedes_its_buffered_suffix_and_finish(self):
        for field, text, key in (('content', 'hello S', 'SYNTHETIC_KEY'),
                                  ('reasoning_content', 'think S', 'SYNTHETIC_KEY'),
                                  ('refusal', 'refused S', 'SYNTHETIC_KEY'),
                                  ('arguments', '{"path":"x"}', '}SYNTHETIC_KEY')):
            with self.subTest(field=field):
                delta = ({'tool_calls': [{'index': 0, 'function': {field: text}}]}
                         if field in ('name', 'arguments') else {field: text})
                frames = self.transcode([{
                    'choices': [{'index': 0, 'delta': delta, 'finish_reason': 'stop'}],
                    'usage': {'prompt_tokens': 11, 'completion_tokens': 4}}], key)
                fragments, finishes = [], []
                for position, frame in enumerate(frames):
                    for choice in frame.get('choices', []):
                        part = choice.get('delta', {})
                        value = (''.join(call.get('function', {}).get(field, '')
                                         for call in part.get('tool_calls', []))
                                 if field in ('name', 'arguments') else part.get(field, ''))
                        if value:
                            fragments.append((position, value))
                        if choice.get('finish_reason') is not None:
                            finishes.append(position)
                self.assertEqual(''.join(value for _, value in fragments), text)
                self.assertEqual(len(finishes), 1)
                self.assertGreater(finishes[0], max(position for position, _ in fragments))
                self.assertEqual(sum('usage' in frame for frame in frames), 1)
                if field == 'arguments':
                    self.assertEqual(json.loads(''.join(value for _, value in fragments)), {'path': 'x'})

    def test_finishing_one_choice_does_not_flush_another_choices_key_prefix(self):
        key = 'SYNTHETIC_KEY'
        frames = self.transcode([
            {'choices': [{'index': 0, 'delta': {'content': key[:4]}, 'finish_reason': None}]},
            {'choices': [{'index': 1, 'delta': {'content': 'hello S'}, 'finish_reason': 'stop'}]},
            {'choices': [{'index': 0, 'delta': {'content': key[4:]}, 'finish_reason': 'stop'}]},
        ], key)
        text = {0: '', 1: ''}
        for frame in frames:
            for choice in frame.get('choices', []):
                text[choice['index']] += choice.get('delta', {}).get('content', '')
        self.assertEqual(text[0], '[redacted]')
        self.assertEqual(text[1], 'hello S')


class GatewayOrder(CloudCase):
    def test_combined_final_delta_survives_the_gateway_and_both_redaction_layers(self):
        final = {'choices': [{'index': 0, 'delta': {
            'content': 'hello ' + KEY[0],
            'tool_calls': [{'index': 0, 'id': 'call_1', 'type': 'function',
                            'function': {'name': 'ls', 'arguments': '{}'}}]},
            'finish_reason': 'tool_calls'}],
            'usage': {'prompt_tokens': 11, 'completion_tokens': 4}}
        upstream = MockOpenRouter(stream_frames=[final], usage_missing=True)
        self.addCleanup(upstream.close)
        self.start(mock=upstream)
        status, body = self.chat(stream=True)
        self.assertEqual(status, 200, body)
        frames = [json.loads(line[6:]) for line in body.splitlines() if line.startswith(b'data: {')]
        choices = [choice for frame in frames for choice in frame.get('choices', [])]
        self.assertEqual(''.join(c.get('delta', {}).get('content', '') for c in choices), 'hello ' + KEY[0])
        functions = [call.get('function', {}) for c in choices
                     for call in c.get('delta', {}).get('tool_calls', [])]
        self.assertEqual(''.join(f.get('name', '') for f in functions), 'ls')
        self.assertEqual(json.loads(''.join(f.get('arguments', '') for f in functions)), {})
        self.assertEqual(choices[-1]['finish_reason'], 'tool_calls')
        self.assertEqual(sum('usage' in frame for frame in frames), 1)
        row = self.ledger_rows()[-1]
        self.assertEqual((row['prompt_tokens'], row['completion_tokens']), (11, 4))
        self.assertEqual(row['settled'], 'usage')
