"""OpenRouter protocol fixtures from Python, with private HOME/network and synthetic keys."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
GOLDENS = Path(__file__).with_name('openrouter.json')
KEY = 'synthetic-router-key'
CASES = ('request', 'request-fields', 'request-alias', 'request-huge', 'request-zero',
         'request-float', 'request-bool', 'request-tools', 'request-builtin', 'request-extra-tool',
         'request-null-tools', 'request-minimal', 'request-xhigh', 'request-unknown-effort',
         'request-bad-effort', 'request-thinking', 'request-null-effort', 'request-invalid',
         'json', 'json-reasoning', 'json-tools', 'json-details', 'json-redaction', 'json-scalar',
         'stream', 'stream-reasoning', 'stream-tools', 'stream-multiline', 'stream-eof',
         'stream-partial', 'stream-interleaved', 'stream-finish-tail', 'stream-opaque', 'stream-no-key')


def spec(name):
    value = {'op': 'request', 'payload': {'model': 'synthetic-cloud', 'messages': [
        {'role': 'user', 'content': 'Synthetic prompt.'}], 'max_tokens': 32}}
    payload = value['payload']
    if name.startswith('request'):
        if name == 'request-fields':
            payload.update(stream=True, stream_options={'include_usage': False}, provider={'zdr': False},
                           top_k=40, min_p=.1, cache_prompt=True, id_slot=1, stop=['x'],
                           temperature=.2, response_format={'type': 'json_object'}, seed=1,
                           presence_penalty=.1, frequency_penalty=.2, logit_bias={'1': 2})
        elif name == 'request-alias':
            payload.update(max_tokens=10, max_completion_tokens=20, n_predict=30)
        elif name in ('request-huge', 'request-zero', 'request-float', 'request-bool'):
            payload['max_tokens'] = {'request-huge': 10**40, 'request-zero': 0,
                                     'request-float': 12.0, 'request-bool': True}[name]
        elif name in ('request-tools', 'request-extra-tool', 'request-builtin'):
            payload['tools'] = [{'type': 'function', 'function': {'name': 'lookup', 'parameters': {'type': 'object'}}}]
            if name == 'request-extra-tool':
                payload['tools'][0]['extra'] = True
            elif name == 'request-builtin':
                payload['tools'] = [{'type': 'web_search'}]
        elif name == 'request-null-tools':
            payload['tools'] = None
        elif (name.startswith('request-') and 'effort' in name) or name in ('request-minimal', 'request-xhigh'):
            payload['reasoning_effort'] = {'request-minimal': 'minimal', 'request-xhigh': 'xhigh',
                'request-unknown-effort': 'synthetic', 'request-bad-effort': 7, 'request-null-effort': None}[name]
        elif name == 'request-thinking':
            payload['chat_template_kwargs'] = {'enable_thinking': True}
        elif name == 'request-invalid':
            value['payload'] = []
        return value
    message = {'role': 'assistant', 'content': 'Synthetic reply.'}
    usage = {'prompt_tokens': 11, 'completion_tokens': 4}
    reply = {'id': 'synthetic-id', 'model': 'upstream', 'choices': [
        {'index': 0, 'message': message, 'finish_reason': 'stop'}], 'usage': usage}
    if name.startswith('json'):
        if name == 'json-reasoning':
            message.update(content='éé', reasoning='想想')
            usage['completion_tokens'] = 5
        elif name == 'json-tools':
            message['tool_calls'] = [{'function': {'name': 'lookup', 'arguments': {'x': [1, 'é']}}},
                                     {'function': {'name': 'query'}}]
        elif name == 'json-details':
            usage.update(prompt_tokens_details={'cached_tokens': True, 'cache_write_tokens': 2},
                         completion_tokens_details={'reasoning_tokens': 3})
            reply['timings'] = {'cache_n': 7}
        elif name == 'json-redaction':
            message.update(content=KEY, tool_calls=[{'function': {'name': 'lookup', 'arguments': {KEY: KEY}}}])
            reply['id'] = KEY
        elif name == 'json-scalar':
            reply = 'synthetic opaque reply'
        return {'op': 'response', 'value': reply, 'key': name == 'json-redaction'}
    def frame(delta, index=0, finish=None):
        return {'choices': [{'index': index, 'delta': delta, 'finish_reason': finish}]}
    frames = [frame({'role': 'assistant', 'content': 'Synthetic reply.'}),
              frame({}, finish='stop'), {'choices': [], 'usage': usage}, '[DONE]']
    if name in ('stream-reasoning', 'stream-no-key'):
        frames[0] = frame({'reasoning': '想想'})
        frames.insert(1, frame({'content': 'éé'}))
    elif name == 'stream-tools':
        frames[0] = frame({'tool_calls': [{'function': {'name': 'lookup', 'arguments': {'x': [1, 'é']}}}]})
    elif name == 'stream-partial':
        frames = [frame({'content': KEY[:7]}), frame({'content': KEY[7:]}), frame({}, finish='stop'), '[DONE]']
    elif name == 'stream-interleaved':
        frames = [frame({'content': KEY[:7]}), frame({'content': 'second'}, index=1),
                  frame({}, index=1, finish='stop'), frame({'content': KEY[7:]}), frame({}, finish='stop'), '[DONE]']
    elif name == 'stream-finish-tail':
        frames = [frame({'content': 's'}, finish='stop'), '[DONE]']
        frames[0]['usage'] = usage
    wire = ''.join('data: ' + (f if isinstance(f, str) else json.dumps(f, ensure_ascii=False)) + '\n\n' for f in frames)
    if name == 'stream-multiline':
        wire = ':keepalive\r\nevent: update\r\ndata: {"choices":\r\ndata: []}\r\n\r\ndata: [DONE]\r\r'
    elif name == 'stream-eof':
        wire = 'event: final\rdata: ' + json.dumps({'choices': [], 'usage': usage})
    elif name == 'stream-opaque':
        wire = 'event: ' + KEY + '\ndata: opaque ' + KEY + '\n\ndata: [DONE]\n\n'
    data = wire.encode()
    return {'op': 'stream', 'chunks': [[byte] for byte in data], 'key': name != 'stream-no-key'}


def reference(value):
    from oxc.runtime import cloud
    if value['op'] == 'request':
        try:
            return {'body': cloud.upstream_payload(value['payload'], {'provider': 'openrouter',
                'upstream_model': 'synthetic/upstream', 'max_output': 512})}
        except cloud.CloudError as error:
            return {'status': error.status, 'message': str(error), 'code': error.code, 'param': error.param}
    key = KEY if value.get('key') else None
    if value['op'] == 'response':
        return cloud.normalize_response(cloud.redact_value(value['value'], key), 'synthetic-cloud')
    stream, raw = cloud.CloudStream('synthetic-cloud', key=key), cloud.StreamRedactor(key)
    out = b''.join(raw.feed(stream.feed(bytes(chunk))) for chunk in value['chunks'])
    out += raw.feed(stream.finish()) + raw.finish()
    return {'wire': out.decode()}


def invoke(name):
    with tempfile.TemporaryDirectory(prefix='oxc-router-contract-') as directory:
        home = '/home/oxc-router-contract'
        env = {'HOME': home, 'PATH': '/usr/bin:/bin', 'LANG': 'C.UTF-8',
               'PYTHONPATH': str(ROOT / 'oxc/src'), 'PYTHONDONTWRITEBYTECODE': '1'}
        if 'OXC_OPENROUTER_CONTRACT' in os.environ:
            env['OXC_OPENROUTER_CONTRACT'] = os.environ['OXC_OPENROUTER_CONTRACT']
        done = subprocess.run(['bwrap', '--die-with-parent', '--unshare-net', '--ro-bind', '/', '/',
            '--dev-bind', '/dev', '/dev', '--tmpfs', '/home', '--tmpfs', '/tmp', '--bind', directory, home,
            '--chdir', home, '--', '/usr/bin/python3', str(Path(__file__).resolve()), '--case', name],
            env=env, text=True, capture_output=True, timeout=10)
        if done.returncode:
            raise AssertionError(done.stderr)
        return json.loads(done.stdout)


class OpenRouterContract(unittest.TestCase):
    def test_reference_or_opt_in_protocol(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(invoke(name), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_OPENROUTER_CONTRACT' in os.environ:
            raise SystemExit('Record only Python protocol')
        rows = {name: invoke(name) for name in CASES}
        GOLDENS.write_text('{\n' + ',\n'.join('  ' + json.dumps(name) + ': ' +
            json.dumps(rows[name], sort_keys=True, ensure_ascii=False) for name in sorted(rows)) + '\n}\n')
    elif sys.argv[1:2] == ['--case']:
        value = spec(sys.argv[2])
        path = Path(os.environ['HOME']) / 'provider.json'
        if value.get('key'):
            path.write_text(json.dumps({'openrouter': {'api_key': KEY}}))
            path.chmod(0o600)
            value['key_path'] = str(path)
        native = os.environ.get('OXC_OPENROUTER_CONTRACT')
        if native:
            done = subprocess.run([native], input=json.dumps(value), text=True, capture_output=True,
                env={'HOME': os.environ['HOME'], 'PATH': '/usr/bin:/bin'}, timeout=5)
            if done.returncode:
                raise SystemExit(done.stderr)
            result = json.loads(done.stdout)
        else:
            result = reference(value)
        if value['op'] == 'stream':
            from oxc.runtime.cloud import SSEDecoder
            parser = SSEDecoder()
            events = parser.feed(result['wire'].encode()) + parser.finish()
            result = []
            for name, data in events:
                try:
                    data = json.loads(data)
                except ValueError:
                    data = data.decode()
                result.append({'event': name, 'data': data})
        if value.get('key') and KEY in json.dumps(result):
            raise AssertionError('Synthetic credential leaked')
        print(json.dumps(result))
    else:
        unittest.main()
