"""OpenAI wire contracts with the real daemon, lifecycle and synthetic engine."""
import json
import os
from pathlib import Path
import sys
import unittest

from harness import Daemon, PRIMARY, SECONDARY

GOLDENS = Path(__file__).with_name('goldens') / 'http-configured.json'
MESSAGES = [{'role': 'user', 'content': 'Synthetic prompt.'}]
TOOLS = [{'type': 'function', 'function': {'name': name, 'description': 'Synthetic tool.',
          'parameters': {'type': 'object', 'properties': {'value': {'type': 'integer'}}}}}
         for name in ('first_tool', 'second_tool')]


def chat(**extra):
    return {'model': PRIMARY, 'messages': MESSAGES, **extra}


def completion(**extra):
    return {'model': PRIMARY, 'prompt': 'Synthetic prompt.', **extra}


# A declared status prevents recording a broken launch as successful inference.
# Bodies and backend requests are exact JSON goldens, not reconstructed assertions
# using implementation helpers.
CASES = {
    'models-before-load': ('GET', '/v1/models', None, 200),
    'model-before-load': ('GET', '/v1/models/' + PRIMARY, None, 200),
    'unknown-model': ('GET', '/v1/models/not-installed', None, 404),
    'unknown-route': ('GET', '/v1/not-a-route', None, 404),
    'unsupported-embeddings': ('POST', '/v1/embeddings', {'input': 'synthetic'}, 404),
    'unknown-chat-model': ('POST', '/v1/chat/completions', chat(model='not-installed'), 404),
    'chat': ('POST', '/v1/chat/completions', chat(), 200),
    'chat-without-model': ('POST', '/v1/chat/completions', {'messages': MESSAGES}, 200),
    'chat-local-default': ('POST', '/v1/chat/completions', chat(model='local-default'), 200),
    'chat-openai-alias': ('POST', '/v1/chat/completions', chat(model='openai/' + PRIMARY), 200),
    'chat-secondary': ('POST', '/v1/chat/completions', chat(model=SECONDARY), 200),
    'chat-stream': ('POST', '/v1/chat/completions', chat(stream=True), 200),
    'chat-stream-usage': ('POST', '/v1/chat/completions', chat(stream=True, stream_options={'include_usage': True}), 200),
    'chat-tools': ('POST', '/v1/chat/completions', chat(tools=TOOLS), 200),
    'chat-tools-stream': ('POST', '/v1/chat/completions', chat(tools=TOOLS, stream=True,
                                                        stream_options={'include_usage': True}), 200),
    'chat-tools-no-parallel': ('POST', '/v1/chat/completions', chat(tools=TOOLS, parallel_tool_calls=False), 200),
    'chat-tools-none': ('POST', '/v1/chat/completions', chat(tools=TOOLS, tool_choice='none'), 200),
    'chat-reasoning': ('POST', '/v1/chat/completions', chat(reasoning_effort='high'), 200),
    'chat-reasoning-stream': ('POST', '/v1/chat/completions', chat(reasoning_effort='low', stream=True,
                                                            stream_options={'include_usage': True}), 200),
    'chat-max-tokens': ('POST', '/v1/chat/completions', chat(max_tokens=2), 200),
    'chat-max-completion-tokens': ('POST', '/v1/chat/completions', chat(max_completion_tokens=1), 200),
    'chat-stop': ('POST', '/v1/chat/completions', chat(stop=[' engine']), 200),
    'chat-n': ('POST', '/v1/chat/completions', chat(n=2), 400),
    'chat-n-stream': ('POST', '/v1/chat/completions', chat(n=2, stream=True,
                                                    stream_options={'include_usage': True}), 400),
    'chat-invalid-n': ('POST', '/v1/chat/completions', chat(n=0), 400),
    'chat-logprobs': ('POST', '/v1/chat/completions', chat(logprobs=True, top_logprobs=2), 200),
    'chat-logprobs-stream': ('POST', '/v1/chat/completions', chat(logprobs=True, top_logprobs=2, stream=True), 200),
    'chat-engine-error': ('POST', '/v1/chat/completions', chat(messages=[{'role': 'user', 'content': '[upstream-error]'}]), 422),
    'chat-text-format': ('POST', '/v1/chat/completions', chat(response_format={'type': 'text'}), 200),
    'chat-json-format': ('POST', '/v1/chat/completions', chat(response_format={'type': 'json_object'}), 200),
    'chat-json-format-stream': ('POST', '/v1/chat/completions', chat(response_format={'type': 'json_object'}, stream=True), 200),
    'chat-strict-schema-refused': ('POST', '/v1/chat/completions', chat(response_format={
        'type': 'json_schema', 'json_schema': {'name': 'synthetic', 'strict': True, 'schema': {'type': 'object'}}}), 400),
    'chat-penalties': ('POST', '/v1/chat/completions', chat(presence_penalty=0.5, frequency_penalty=0.25,
                                                       logit_bias={'1': -1}), 200),
    'chat-engine-validates-penalty': ('POST', '/v1/chat/completions', chat(presence_penalty=5), 200),
    'chat-trailing-slash': ('POST', '/v1/chat/completions/', chat(), 200),
    'chat-native-alias': ('POST', '/chat/completions', chat(), 200),
    'completion': ('POST', '/v1/completions', completion(), 200),
    'completion-stream': ('POST', '/v1/completions', completion(stream=True), 200),
    'completion-stream-usage': ('POST', '/v1/completions', completion(stream=True,
                                                                 stream_options={'include_usage': True}), 200),
    'completion-logprobs': ('POST', '/v1/completions', completion(logprobs=2), 200),
    'completion-logprobs-stream': ('POST', '/v1/completions', completion(logprobs=2, stream=True), 200),
    'completion-echo': ('POST', '/v1/completions', completion(echo=True), 200),
    'completion-max-tokens': ('POST', '/v1/completions', completion(max_tokens=1), 200),
    'completion-n': ('POST', '/v1/completions', completion(n=2), 400),
    'completion-multiple-prompts': ('POST', '/v1/completions', completion(prompt=['first', 'second']), 400),
    'completion-suffix-refused': ('POST', '/v1/completions', completion(suffix='synthetic'), 400),
    'completion-token-prompt': ('POST', '/v1/completions', completion(prompt=[1, 2]), 200),
    'responses': ('POST', '/v1/responses', {'model': PRIMARY, 'input': 'Synthetic prompt.'}, 200),
    'responses-stream': ('POST', '/v1/responses', {'model': PRIMARY, 'input': 'Synthetic prompt.', 'stream': True}, 200),
    'responses-store-ignored': ('POST', '/v1/responses', {'model': PRIMARY, 'input': 'synthetic', 'store': True}, 200),
}


def run_case(daemon, name):
    method, path, body, expected_status = CASES[name]
    before = len(daemon.events())
    reply = daemon.call(method, path, body)
    if reply['status'] != expected_status:
        raise AssertionError(name + ': ' + repr(reply))
    backend = [{'path': row['path'], 'body': row['body']} for row in daemon.events()[before:]
               if row['event'] == 'request' and row['method'] == 'POST']
    return {'reply': reply, 'backend': backend}


class ConfiguredApiContract(unittest.TestCase):
    def test_wire_goldens(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        with Daemon(configured=True) as daemon:
            for name in CASES:
                with self.subTest(case=name):
                    self.assertEqual(run_case(daemon, name), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_DAEMON_COMMAND' in os.environ:
            raise SystemExit('Goldens must be recorded from the Python reference')
        with Daemon(configured=True) as daemon:
            GOLDENS.write_text(json.dumps({name: run_case(daemon, name) for name in CASES},
                                         indent=2, sort_keys=True) + '\n')
    else:
        unittest.main()
