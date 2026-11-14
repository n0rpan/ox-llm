"""Context metadata, persisted modes and hash-bound RSS-proof gates, with fake models only."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import stat
import sys
import unittest

from harness import Daemon, PRIMARY, SECONDARY
from test_api import chat
from test_lifecycle import status, wait_until

GOLDENS = Path(__file__).with_name('goldens') / 'context-modes.json'
CASES = ('metadata', 'set-fast-deep', 'validation', 'proof-gates', 'max-load-cycle',
         'max-busy', 'config-symlink', 'metadata-edge-types')


def snapshot(daemon):
    state = status(daemon)
    return {'context': state['context'], 'model_limits': state['model_limits']}


def mode(daemon, value, model=PRIMARY):
    return daemon.call('POST', '/_local/mode', {'model': model, 'mode': value})


def proof(config, **extra):
    # A synthetic receipt for the synthetic engine, never a hardware measurement.
    return {'context': 262144, 'peak_rss_bytes': 26 * 1024**3,
            'engine_sha256': config['server_sha256'], 'date': '2026-01-01', **extra}


def run_case(name):
    with Daemon(configured=True, config_changes={'synthetic_extension': {'opaque': [7, 'synthetic']}}) as daemon:
        config = json.loads(daemon.config_path.read_text())
        if name in ('max-load-cycle', 'max-busy'):
            config['rss_proof'] = proof(config)
            config['oxe_args'] += ['--ctx-size', '{context}']
            config['profiles'][PRIMARY]['oxe_args'] = list(config['oxe_args'])
        if name == 'validation':
            config['profiles']['synthetic-no-modes'] = dict(config['profiles'][SECONDARY])
        daemon.config_path.write_text(json.dumps(config))
        output = []
        if name == 'metadata':
            output.append(snapshot(daemon))
            config['context_modes'] = {'default': 'deep', 'models': {PRIMARY: 'unknown-synthetic-mode'}}
            daemon.config_path.write_text(json.dumps(config))
            output.append(snapshot(daemon))
        elif name == 'metadata-edge-types':
            config['profiles'][PRIMARY]['context_size'] = 'synthetic-invalid-context'
            daemon.config_path.write_text(json.dumps(config))
            output.append(snapshot(daemon))
            config['selected_model'] = 'synthetic-no-modes'
            config['profiles']['synthetic-no-modes'] = {**config['profiles'][SECONDARY],
                                                       'context_mode': 'synthetic-ignored-mode'}
            daemon.config_path.write_text(json.dumps(config))
            output.append(snapshot(daemon))
        elif name in ('set-fast-deep', 'config-symlink'):
            if name == 'config-symlink':
                target = daemon.home / 'synthetic-context-config'
                daemon.config_path.rename(target)
                target.chmod(0o640)
                daemon.config_path.symlink_to(target)
            for chosen, model in (('deep', PRIMARY), ('fast', 'openai/' + PRIMARY), ('deep', SECONDARY)):
                output.append(mode(daemon, chosen, model))
                output.append(snapshot(daemon))
        elif name == 'validation':
            for body in ({}, {'model': PRIMARY}, {'model': PRIMARY, 'mode': 'fast', 'extra': True},
                         {'model': None, 'mode': 'fast'}, {'model': 7, 'mode': 'fast'},
                         {'model': PRIMARY, 'mode': 'unknown'}, {'model': PRIMARY, 'mode': None},
                         {'model': 'unknown-synthetic', 'mode': 'fast'},
                         {'model': 'synthetic-no-modes', 'mode': 'fast'},
                         {'model': SECONDARY, 'mode': 'max'}):
                output.append(daemon.call('POST', '/_local/mode', body))
        elif name == 'proof-gates':
            for receipt in (None, {'context': 262144}, proof(config, engine_sha256='f' * 64),
                            proof(config, context=196608), proof(config, peak_rss_bytes=27 * 1024**3 + 1),
                            proof(config, date='synthetic-invalid-date'), proof(config)):
                config = json.loads(daemon.config_path.read_text())
                if receipt is None:
                    config.pop('rss_proof', None)
                else:
                    config['rss_proof'] = receipt
                daemon.config_path.write_text(json.dumps(config))
                output.append(snapshot(daemon))
                output.append(mode(daemon, 'max'))
                output.append(mode(daemon, 'fast'))
        elif name == 'max-load-cycle':
            for chosen in (None, 'max', 'deep', 'fast'):
                if chosen is not None:
                    output.append(mode(daemon, chosen))
                    output.append({'state': status(daemon)['state']})
                output.append(daemon.call('POST', '/_local/load', {}))
            contexts = [event.get('context_size') for event in daemon.events() if event['event'] == 'started']
            if contexts != [32768, 262144, 32768]:
                raise AssertionError('Mode cap changes did not reach the engine launch: ' + repr(contexts))
            output.append({'launched_contexts': contexts})
            output.append(snapshot(daemon))
        elif name == 'max-busy':
            with ThreadPoolExecutor(max_workers=1) as pool:
                request = pool.submit(daemon.call, 'POST', '/v1/chat/completions', chat(
                    messages=[{'role': 'user', 'content': '[hold]'}]))
                try:
                    wait_until(lambda: any(event['event'] == 'held' for event in daemon.events()))
                    output.append(mode(daemon, 'max'))
                    output.append(mode(daemon, 'deep'))
                finally:
                    (daemon.home / 'engine-gate').touch()
                if request.result(timeout=5)['status'] != 200:
                    raise AssertionError('Held synthetic request did not finish')
            wait_until(lambda: status(daemon)['active_requests'] == 0)
            output.append(snapshot(daemon))
        saved = json.loads(daemon.config_path.read_text())
        output.append({'context_modes': saved.get('context_modes'), 'extension': saved['synthetic_extension'],
                       'config_mode': stat.S_IMODE(daemon.config_path.stat().st_mode),
                       'config_symlink': daemon.config_path.is_symlink()})
        if name not in ('max-load-cycle', 'max-busy') and any(event['event'] == 'started' for event in daemon.events()):
            raise AssertionError('Context metadata/control started an engine')
        return output


class ContextModeContract(unittest.TestCase):
    def test_context_mode_goldens(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(run_case(name), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_DAEMON_COMMAND' in os.environ:
            raise SystemExit('Goldens must be recorded from the Python reference')
        GOLDENS.write_text(json.dumps({name: run_case(name) for name in CASES}, indent=2, sort_keys=True) + '\n')
    else:
        unittest.main()
