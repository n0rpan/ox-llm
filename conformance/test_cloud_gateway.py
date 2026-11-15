"""Merged cloud gateway wire/file contracts; private network, synthetic providers."""
import fcntl
import json
import math
import os
from pathlib import Path
import re
import sys
import time
import unittest

from cli_harness import CANONICAL_HOME, invoke_case
from harness import Daemon
from test_cloud_catalog import mocks, normalize

GOLDENS = Path(__file__).with_name('goldens') / 'cloud-gateway.json'
CASES = ('router-json', 'router-sse', 'router-tools', 'router-privacy', 'router-no-usage',
         'router-error-redaction', 'router-disabled', 'router-offline', 'router-no-key',
         'router-key-mode', 'router-key-symlink', 'router-no-price', 'router-budget',
         'openai-chat', 'openai-chat-sse', 'openai-responses',
         'anthropic-json', 'anthropic-sse', 'anthropic-tools', 'anthropic-thinking',
         'router-cache', 'openai-cache', 'openai-responses-cache', 'anthropic-cache')
KEY = 'synthetic-conformance-provider-key'
PROMPT = 'Synthetic private cloud prompt.'


def private(path, data):
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), 'wb') as stream:
        stream.write(data)


def volatile(value, ledger=False):
    if isinstance(value, list):
        return [volatile(item, ledger) for item in value]
    if not isinstance(value, dict):
        return value
    result = {}
    for key, item in value.items():
        if key in ('created', 'created_at', 't') and type(item) in (int, float) and item > 1_000_000_000:
            if abs(item - time.time()) > 30:
                raise AssertionError('Invalid current timestamp')
            result[key] = '<current:unix-seconds>'
        elif ledger and key in ('queue_wait_ms', 'admit_ms', 'ttfb_ms', 'wall_ms', 'ctl_prep_ms') and item is not None:
            if type(item) not in (int, float) or not math.isfinite(item) or item < 0:
                raise AssertionError('Invalid measured timing')
            result[key] = '<measured:nonnegative-ms>'
        elif ledger and key == 'id' and isinstance(item, str) and re.fullmatch(r'[0-9a-f]{12}', item):
            result[key] = '<generated:ledger_12hex>'
        elif key in ('request_id', 'req_id') and isinstance(item, str) and re.fullmatch(r'req_[0-9a-f]{32}', item):
            result[key] = '<generated:req_32hex>'
        else:
            result[key] = volatile(item, ledger)
    return result


def capture(name):
    home = CANONICAL_HOME
    provider = 'anthropic' if name.startswith('anthropic') else 'openai' if name.startswith('openai') else 'openrouter'
    module = mocks()
    response = None
    cached = name.endswith('-cache')
    usage = {'prompt_tokens': 1000, 'completion_tokens': 4, 'total_tokens': 1004,
             'prompt_tokens_details': {'cached_tokens': 950, 'cache_write_tokens': 40}} if cached else None
    if name.startswith('openai-responses'):
        response = {'id': 'resp_synthetic', 'object': 'response', 'status': 'completed', 'created_at': 1,
                    'output': [{'id': 'msg_synthetic', 'type': 'message', 'role': 'assistant', 'status': 'completed',
                                'content': [{'type': 'output_text', 'text': 'Synthetic response reply.', 'annotations': []}]}],
                    'usage': {'input_tokens': 11, 'output_tokens': 4, 'total_tokens': 15}}
        if cached:
            response['usage'] = {'input_tokens': 1000, 'output_tokens': 4,
                                 'input_tokens_details': usage['prompt_tokens_details']}
    if provider == 'anthropic':
        message = None
        if name == 'anthropic-tools':
            message = {'id': 'msg_mock', 'type': 'message', 'role': 'assistant', 'model': 'claude-sonnet-5-5',
                       'content': [{'type': 'tool_use', 'id': 'tool_synthetic', 'name': 'synthetic_tool', 'input': {'value': 7}}],
                       'stop_reason': 'tool_use', 'stop_sequence': None, 'usage': {'input_tokens': 12, 'output_tokens': 5}}
        elif cached:
            message = {'id': 'msg_mock', 'type': 'message', 'role': 'assistant', 'model': 'claude-sonnet-5-5',
                       'content': [{'type': 'text', 'text': 'Synthetic cloud reply.'}], 'stop_reason': 'end_turn',
                       'stop_sequence': None, 'usage': {'input_tokens': 10, 'output_tokens': 4,
                       'cache_read_input_tokens': 950, 'cache_creation_input_tokens': 40}}
        upstream = module.MockAnthropic(message=message)
    else:
        upstream = module.MockOpenRouter(response=response, usage=usage, usage_missing=name == 'router-no-usage',
                    status=401 if name == 'router-error-redaction' else 200,
                    error='Synthetic provider refusal: ' + KEY if name == 'router-error-redaction' else None,
                    reply='Synthetic cloud reply.')
    try:
        secret = home / 'provider-secrets.json'
        if name != 'router-no-key':
            target = home / 'provider-secrets-target.json' if name == 'router-key-symlink' else secret
            private(target, json.dumps({provider: {'api_key': KEY}}).encode())
            if name == 'router-key-symlink':
                secret.symlink_to(target.name)
            if name == 'router-key-mode':
                secret.chmod(0o644)
        ledger = home / 'cloud-ledger'
        ledger.mkdir(mode=0o700)
        private(ledger / '.salt', b'0123456789abcdef0123456789abcdef')
        model = ('gpt-6-luna' if name.startswith('openai-responses') else 'gpt-4.1') if provider == 'openai' else (
                'claude-sonnet-5-5' if provider == 'anthropic' else 'synthetic/upstream')
        item = {'kind': 'remote/' + provider, 'provider': provider, 'base_url': upstream.base + ('' if provider == 'anthropic' else '/v1'),
                'upstream_model': model, 'context_window': 64000, 'max_output': 512,
                'pricing': {'input_per_million': 1.0, 'output_per_million': 2.0}}
        if cached:
            item['pricing'].update(cache_read_per_million=.1, cache_write_per_million=1.25)
        if name == 'router-no-price':
            del item['pricing']
        config = {'cloud_only': True, 'cloud': {'enabled': name != 'router-disabled',
                  'daily_budget_usd': 1e-9 if name == 'router-budget' else 1.0,
                  'secrets_file': str(secret), 'models': {'synthetic-cloud': item}}}
        environment = {'OXC_LEDGER_DIR': str(ledger)}
        if name == 'router-offline':
            environment['OXC_OFFLINE'] = '1'
        request = {'model': 'synthetic-cloud', 'messages': [{'role': 'user', 'content': PROMPT}],
                   'max_tokens': 32, 'stream': name.endswith('-sse')}
        if name.endswith('-tools'):
            request['tools'] = [{'type': 'function', 'function': {'name': 'synthetic_tool', 'description': 'Synthetic tool.',
                                'parameters': {'type': 'object', 'properties': {'value': {'type': 'integer'}}}}}]
        if name == 'router-privacy':
            request['provider'] = {'zdr': False, 'data_collection': 'allow'}
        if name == 'anthropic-thinking':
            request['reasoning_effort'] = 'high'
        lease = home / 'state/oxc/inference.lock'
        lease.parent.mkdir(parents=True, mode=0o700)
        with lease.open('xb') as owner:
            owner.write(b'synthetic-other-owner\n')
            owner.flush()
            fcntl.flock(owner, fcntl.LOCK_EX | fcntl.LOCK_NB)
            before = lease.stat()
            with Daemon(engine_free=True, home=home, config_changes=config, environ=environment) as daemon:
                original_config = daemon.config_path.read_bytes()
                reply = daemon.call('POST', '/v1/chat/completions', request, timeout=15)
                if cached or name in ('router-json', 'router-sse', 'router-tools', 'router-privacy', 'router-no-usage',
                            'openai-chat', 'openai-chat-sse', 'openai-responses',
                            'anthropic-json', 'anthropic-sse', 'anthropic-tools', 'anthropic-thinking'):
                    if reply['status'] != 200:
                        raise AssertionError('Positive provider contract did not forward: ' + repr(reply))
                deadline = time.monotonic() + 3
                rows = []
                while time.monotonic() < deadline:
                    rows = [json.loads(line) for path in sorted(ledger.glob('requests-*.jsonl')) for line in path.read_text().splitlines() if line]
                    if rows or name == 'router-disabled':
                        break
                    time.sleep(.01)
                public_ledger = daemon.call('GET', '/_local/ledger')['body']
                if cached:
                    if len(rows) != 1 or rows[0].get('cached_tokens') != 950 or rows[0].get('cache_write_tokens') != 40:
                        raise AssertionError('Cache usage was not recorded')
                    if not math.isclose(rows[0].get('cost_usd', -1), .000163, rel_tol=0, abs_tol=1e-12):
                        raise AssertionError('Cached prompt was priced as ordinary input')
                status = daemon.call('GET', '/_local/status')['body']
                if status['engine_mode'] != 'none' or status['loaded_model'] is not None or daemon.events():
                    raise AssertionError('Remote request touched local inference')
                if daemon.config_path.read_bytes() != original_config:
                    raise AssertionError('Gateway rewrote configuration')
                serialized = json.dumps(reply)
                if KEY in serialized or str(secret) in serialized:
                    raise AssertionError('Gateway exposed provider credentials')
                if PROMPT in json.dumps(rows) or 'Synthetic cloud reply.' in json.dumps(rows):
                    raise AssertionError('Cloud ledger contains conversation text')
                if len(upstream.requests) > 1:
                    raise AssertionError('Gateway retried a potentially paid provider request')
                observed = []
                for record in upstream.requests:
                    headers = {key: value for key, value in record['headers'].items()
                               if key in ('authorization', 'x-api-key', 'anthropic-version', 'anthropic-beta', 'content-type', 'accept')}
                    header = 'x-api-key' if provider == 'anthropic' else 'authorization'
                    expected = KEY if provider == 'anthropic' else 'Bearer ' + KEY
                    if headers.get(header) != expected:
                        raise AssertionError('Mock did not receive the synthetic provider credential')
                    headers[header] = '<synthetic:provider-key>'
                    observed.append({'method': record['method'], 'path': record['path'], 'body': record['body'], 'headers': headers})
                result = {'reply': reply, 'upstream': observed, 'rows': volatile(rows, True),
                          'public_ledger': volatile(public_ledger, True), 'cloud_status': status['cloud']}
            after = lease.stat()
            if (before.st_dev, before.st_ino, before.st_mtime_ns) != (after.st_dev, after.st_ino, after.st_mtime_ns):
                raise AssertionError('Gateway mutated a foreign inference lease')
            if lease.read_bytes() != b'synthetic-other-owner\n':
                raise AssertionError('Gateway changed foreign lease content')
        return normalize(volatile(result), home, daemon.port)
    finally:
        upstream.close()


class CloudGatewayContract(unittest.TestCase):
    def test_three_provider_wire_and_ledger(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(invoke_case(name, __file__, timeout=45), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_DAEMON_COMMAND' in os.environ:
            raise SystemExit('Record gateway contracts only from Python')
        GOLDENS.write_text(json.dumps({name: invoke_case(name, __file__, timeout=45) for name in CASES}, indent=2, sort_keys=True) + '\n')
    elif sys.argv[1:2] == ['--case']:
        print(json.dumps(capture(sys.argv[2])))
    else:
        unittest.main()
