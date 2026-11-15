"""Pure money contracts from current Python; private HOME/network, no provider call."""
import json
import math
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
PYTHON = '/usr/bin/python3'
GOLDENS = Path(__file__).with_name('pricing.json')
CASES = ('router', 'openai', 'anthropic', 'anthropic-upstream', 'router-explicit',
         'openai-explicit', 'anthropic-explicit', 'one-hour-write', 'invalid-cache-sum',
         'invalid-cache-type', 'negative-cache', 'missing-price', 'free', 'free-bad-flat',
         'free-bad-tier', 'aliases', 'bad-rate-type', 'negative-rate', 'large-rate',
         'tier-low', 'tier-boundary', 'tier-high', 'tier-missing-final', 'tier-unordered',
         'tier-bool-bound', 'prompt-bound', 'prompt-over', 'output-bound', 'output-over',
         'invalid-prompt', 'decimal', 'tiny-rate', 'cache-rate-defaults',
         'settle-usage', 'settle-missing', 'settle-failed', 'settle-cache',
         'settle-invalid-sum', 'settle-invalid-type', 'settle-cancel-final',
         'settle-cancel-partial', 'settle-cancel-reasoning', 'settle-cancel-capped',
         'settle-reported', 'settle-repeated', 'settle-zero')


def inputs(name):
    provider = name if name in ('openai', 'anthropic') else 'openrouter'
    if name.startswith('openai-'):
        provider = 'openai'
    elif name.startswith('anthropic-'):
        provider = 'anthropic'
    price = {'input_per_million': 1.0, 'output_per_million': 2.0}
    model = {'pricing': price}
    value = {'provider': provider, 'upstream_model': 'synthetic/upstream', 'model': model,
             'prompt': 1000, 'output': 4, 'read': 950, 'write': 40,
             'context': 1000, 'output_limit': 32}
    if name == 'anthropic-upstream':
        value.update(provider='openrouter', upstream_model='anthropic/synthetic')
    if name.endswith('-explicit') or name.startswith('tier-') or name.startswith('invalid-cache') or name == 'negative-cache':
        price.update(cache_read_per_million=.1, cache_write_per_million=1.25)
    if name == 'one-hour-write':
        price.update(cache_read_per_million=.1, cache_write_per_million=2.0)
    if name == 'invalid-cache-sum':
        value.update(read=999, write=9)
    elif name == 'invalid-cache-type':
        value['read'] = True
    elif name == 'negative-cache':
        value['write'] = -1
    elif name in ('missing-price', 'free'):
        del model['pricing']
        model['free'] = name == 'free'
    elif name == 'free-bad-flat':
        price['input_per_million'] = 'synthetic-invalid'
        model['free'] = True
    elif name == 'free-bad-tier':
        price['tiers'] = []
        model['free'] = True
    elif name == 'aliases':
        model['pricing'] = {'input': 1, 'output': 2}
    elif name == 'bad-rate-type':
        price['input_per_million'] = True
    elif name == 'negative-rate':
        price['input_per_million'] = -.1
    elif name == 'large-rate':
        price['input_per_million'] = 1e12 + 1
    elif name.startswith('tier-'):
        price['tiers'] = [{'max_prompt_tokens': 100, 'input_per_million': 1.0},
                          {'max_prompt_tokens': None, 'input_per_million': 2.0}]
        value.update(prompt=99 if name == 'tier-low' else 100 if name == 'tier-boundary' else 101, read=90, write=0)
        if name == 'tier-missing-final':
            price['tiers'][-1]['max_prompt_tokens'] = 200
        elif name == 'tier-unordered':
            price['tiers'].insert(1, {'max_prompt_tokens': 50})
        elif name == 'tier-bool-bound':
            price['tiers'][0]['max_prompt_tokens'] = True
    elif name.startswith('prompt-'):
        value.update(prompt=100_000_000 + (name == 'prompt-over'), read=0, write=0)
    elif name.startswith('output-'):
        value.update(output=10_000_000 + (name == 'output-over'), read=0, write=0)
    elif name == 'invalid-prompt':
        value.update(prompt=True, read=0, write=0)
    elif name == 'decimal':
        price.update(input_per_million=.111, output_per_million=4.44,
                     cache_read_per_million=.0111, cache_write_per_million=.13875)
        value.update(prompt=979, output=83, read=950, write=20)
    elif name == 'tiny-rate':
        price.update(input_per_million=1e-20, output_per_million=3e-20)
    elif name == 'cache-rate-defaults':
        value.update(provider='anthropic', prompt=1000, read=950, write=0)
    elif name.startswith('settle-'):
        price.update(cache_read_per_million=.1, cache_write_per_million=1.25)
        usage = {'prompt_tokens': 11, 'completion_tokens': 4}
        value.update(frames=[{'usage': usage, 'choices': [{'finish_reason': 'stop'}]}],
                     outcome='complete', request_bytes=100, streamed_bytes=4, elapsed=.01,
                     usage_final=False)
        if name == 'settle-missing':
            value['frames'] = []
        elif name == 'settle-failed':
            value['outcome'] = 'failed'
        elif name in ('settle-cache', 'settle-invalid-sum', 'settle-invalid-type'):
            usage.update(prompt_tokens=1000, prompt_tokens_details={
                'cached_tokens': 999 if name == 'settle-invalid-sum' else
                                 True if name == 'settle-invalid-type' else 950,
                'cache_write_tokens': 40})
        elif name.startswith('settle-cancel-'):
            value['outcome'] = 'cancelled'
            if name == 'settle-cancel-final':
                value.update(usage_final=True, elapsed=5)
            elif name == 'settle-cancel-partial':
                usage['completion_tokens'] = 0
            else:
                value.update(frames=[], elapsed=.05 if name == 'settle-cancel-reasoning' else 1000)
                if name == 'settle-cancel-capped':
                    value['request_bytes'] = 100_000_001
        elif name in ('settle-reported', 'settle-repeated'):
            usage['cost'] = .002
            if name == 'settle-repeated':
                value['frames'].append({'usage': {'cost': .001}})
        elif name == 'settle-zero':
            usage.update(prompt_tokens=0, completion_tokens=0)
    return value


def reference(spec):
    sys.path.insert(0, str(ROOT / 'oxc/src'))
    from oxc.runtime import cloud
    price = cloud._pricing(spec['model'])
    item = {'provider': spec['provider'], 'upstream_model': spec['upstream_model'],
            'pricing': price, 'context_window': spec['context'], 'max_output': 512}
    if 'frames' in spec:
        meter = cloud.Meter()
        for frame in spec['frames']:
            meter._object(frame)
        hold = cloud.BudgetHold('synthetic-day', cloud.worst_case_cost(
            item, {'max_tokens': spec['output_limit']}), spec['output_limit'], started=0)
        row = meter.row(item)
        if spec['outcome'] == 'cancelled':
            return cloud.cancelled_cost(row, hold, item, spec['request_bytes'],
                                        spec['streamed_bytes'], elapsed=spec['elapsed'],
                                        usage_final=spec['usage_final'])
        return cloud.conservative_cost(row, hold, failed=spec['outcome'] == 'failed')
    actual = cloud.cost(item, spec['prompt'], spec['output'], cache_read=spec['read'], cache_write=spec['write'])
    reservation = cloud.worst_case_cost(item, {'max_tokens': spec['output_limit']})
    return {'pricing': price, 'actual': actual,
            'reservation': reservation if math.isfinite(reservation) else None}


def invoke(name):
    with tempfile.TemporaryDirectory(prefix='oxc-price-contract-') as directory:
        home = '/home/oxc-price-contract'
        env = {'HOME': home, 'PATH': '/usr/bin:/bin', 'LANG': 'C.UTF-8', 'LC_ALL': 'C.UTF-8'}
        if 'OXC_PRICING_CONTRACT' in os.environ:
            env['OXC_PRICING_CONTRACT'] = os.environ['OXC_PRICING_CONTRACT']
        done = subprocess.run(['bwrap', '--die-with-parent', '--unshare-net', '--ro-bind', '/', '/',
                               '--dev-bind', '/dev', '/dev', '--tmpfs', '/home', '--tmpfs', '/tmp',
                               '--ro-bind', str(ROOT), str(ROOT), '--bind', directory, home,
                               '--chdir', home, '--', PYTHON, str(Path(__file__).resolve()), '--case', name],
                              env=env, text=True, capture_output=True, timeout=10)
        if done.returncode:
            raise AssertionError(done.stderr)
        return json.loads(done.stdout)


class PricingContract(unittest.TestCase):
    def test_reference_or_opt_in_price_and_reservation_bound(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(invoke(name), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_PRICING_CONTRACT' in os.environ:
            raise SystemExit('Record only Python pricing')
        rows = {name: invoke(name) for name in CASES}
        GOLDENS.write_text('{\n' + ',\n'.join('  ' + json.dumps(name) + ': ' +
                          json.dumps(rows[name], sort_keys=True) for name in sorted(rows)) + '\n}\n')
    elif sys.argv[1:2] == ['--case']:
        spec = inputs(sys.argv[2])
        native = os.environ.get('OXC_PRICING_CONTRACT')
        if native:
            done = subprocess.run([native], input=json.dumps(spec), text=True, capture_output=True,
                                  env={'PATH': '/usr/bin:/bin', 'HOME': os.environ['HOME']}, timeout=5)
            if done.returncode:
                raise SystemExit(done.stderr)
            print(done.stdout)
        else:
            print(json.dumps(reference(spec)))
    else:
        unittest.main()
