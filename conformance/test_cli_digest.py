"""Numbers-only digest CLI contracts, including private writes and stored-day interoperability."""
from environment import PYTHON
import datetime
import json
import os
from pathlib import Path
import shlex
import stat
import subprocess
import sys
import time
import unittest

from harness import Daemon, ROOT
from test_cli_runtime import CANONICAL_HOME, invoke_case

GOLDENS = Path(__file__).with_name('goldens') / 'cli-digest.json'
DAY = datetime.date(2026, 11, 1)
PRIVATE = 'SYNTHETIC-PRIVATE-DIGEST-TEXT'
CASES = ('empty-json', 'populated-json', 'populated-text', 'origin-filter', 'malformed',
         'symlink-ledger', 'public-ledger-metadata', 'status-empty', 'status-valid',
         'status-stale', 'status-invalid', 'status-symlink', 'write-private',
         'write-failure', 'soak-green', 'soak-idle-gap', 'soak-alert-stop',
         'today-version', 'today-unknown-version', 'stored-day', 'trends', 'iso-week-day',
         'numeric-rounding', 'nonfinite', 'date-not-padded', 'stored-write', 'round-trip')


def row(**changes):
    return {'t': DAY.strftime('%s'), 'path': '/v1/chat/completions', 'origin': 'organic',
            'model': 'ornith-35b-a3b-local', 'mode': 'fast', 'status': 200,
            'history_change': 'append', 'previous_reply': 'present', 'miss_class': 'append-ok',
            'cached_tokens': 8192, 'ctx_tokens': 8208, 'cache_n': 8192, 'prompt_n': 16,
            'prompt_ms': 8.0, 'predicted_n': 16, 'predicted_ms': 32.0, 'ttfb_ms': 20.0,
            'completeness': 'complete', 'prompt': PRIVATE, 'reply': PRIVATE, 'session': PRIVATE,
            **changes}


def capture(name):
    home = CANONICAL_HOME
    with Daemon(configured=True, home=home, port=8092) as daemon:
        entry = home / 'oxc'
        entry.symlink_to(ROOT / 'oxc/src/oxc/runtime/oxc')
        reference = [PYTHON, str(entry)]
        ledger, directory = home / 'digest-ledger', home / 'digest-output'
        ledger.mkdir(mode=0o700)
        directory.mkdir(mode=0o755)
        directory.chmod(0o755)
        engine_log = home / 'digest-engine.jsonl'
        environment = {**daemon.env, 'OXC_DIGEST_DIR': str(directory),
                       'OXC_LEDGER_DIR': str(ledger), 'OXC_ENGINE_LOG': str(engine_log), 'TZ': 'UTC'}
        when = datetime.date.today() if name.startswith('today-') else DAY
        path = ledger / ('requests-' + when.strftime('%Y%m%d') + '.jsonl')
        arguments = ['digest', '--day', when.isoformat(), '--json']
        rows = []
        if name in ('populated-json', 'populated-text', 'write-private', 'public-ledger-metadata'):
            rows = [row(), row(history_change='same', miss_class='retry', status=503,
                             error='backend_died', completeness='incomplete', queue_pos=1, queue_wait_ms=25),
                    row(origin='remote', history_rewrite=True, history_change='edited', fold_suspect=True,
                        rewrite_class='not-allowed-' + PRIVATE, turn_kind='compaction', wall_ms=100,
                        miss_class='unknown', resume_src='disk', resume_ms=6000, replay='cut-aborted'),
                    row(origin='lab', prompt=PRIVATE), row(origin='gate', status=500)]
            engine_log.write_text(json.dumps({'event': 'engine_died', 't': time.mktime(DAY.timetuple()) + 10,
                                             'guard_failure': 'cgroup_oom', 'private': PRIVATE}) + '\n' +
                                  json.dumps({'event': 'engine_died', 'private': PRIVATE}) + '\n')
        if name == 'populated-text':
            arguments.remove('--json')
        if name == 'origin-filter':
            rows = [row(origin='lab'), row(origin='gate'), row(origin=None), row(origin=PRIVATE),
                    row(origin='remote', model=PRIVATE, mode=PRIVATE, error=PRIVATE, completeness=PRIVATE,
                        rewrite_class=PRIVATE, miss_class=PRIVATE, replay=PRIVATE)]
        if name == 'numeric-rounding':
            rows = [row(ttfb_ms=value, resume_src='disk', resume_ms=value) for value in (2.675, 2.685, 2.695)]
        if name == 'nonfinite':
            rows = [row(origin=float('nan'), error=float('nan'), ttfb_ms=float('nan'),
                        predicted_ms=float('inf'), ctx_tokens=float('nan')),
                    row(origin='remote', status=float('-inf'), prompt_ms=float('-inf'))]
        if name == 'malformed':
            path.write_text('not JSON\n[]\n{\n' + 'x' * (1024**2 + 1) + '\n' + json.dumps(row()) + '\n')
        elif name == 'symlink-ledger':
            target = home / 'digest-target.jsonl'
            target.write_text(json.dumps(row()) + '\n')
            path.symlink_to(target)
        else:
            path.write_text(''.join(json.dumps(value) + '\n' for value in rows))
            path.chmod(0o644 if name == 'public-ledger-metadata' else 0o600)
        if name.startswith('status-'):
            arguments = ['digest', '--status', '--json', '--day', 'invalid']
            value = {'schema': 'local-ai-digest-status/1', 'day': DAY.isoformat(), 'state': 'attention',
                     'line': '1 thing to look at', 'alerts': ['stream_cut'], 'alert_text': ['answers cut off'],
                     'requests': 3, 'generated_at': int(time.time())}
            if name == 'status-stale':
                value['generated_at'] = 1
            if name == 'status-invalid':
                value['schema'] = 'synthetic wrong schema'
            if name == 'status-symlink':
                target = home / 'status-target.json'
                target.write_text(json.dumps(value))
                (directory / 'status.json').symlink_to(target)
            elif name != 'status-empty':
                (directory / 'status.json').write_text(json.dumps(value))
        if name.startswith('soak-'):
            arguments = ['digest', '--soak', '--day', DAY.isoformat(), '--json']
            for back in range(7):
                day = DAY - datetime.timedelta(days=back)
                value = {'schema': 'local-ai-digest/1', 'day': day.isoformat(), 'requests': 50,
                         'history': {'history_rewrites': 2}, 'alerts': []}
                if name == 'soak-idle-gap' and back == 1:
                    value['requests'] = 0
                if name == 'soak-alert-stop' and back == 2:
                    value['alerts'] = [{'id': 'stream_cut'}]
                (directory / ('digest-' + day.strftime('%Y%m%d') + '.json')).write_text(json.dumps(value))
        if name in ('write-private', 'write-failure'):
            arguments.append('--write')
            if name == 'write-failure':
                environment['OXC_DIGEST_DIR'] = str(home / 'not-a-directory' / 'digest')
                (home / 'not-a-directory').write_text('synthetic unrelated file')
        if name.startswith('today-'):
            agent = home / '.local/bin/oxa'
            agent.parent.mkdir(parents=True, exist_ok=True)
            agent.write_text('#!/bin/sh\nprintf "%s\\n" "' +
                             ('oxa 1.2.3-synthetic' if name == 'today-version' else PRIVATE) + '"\n')
            agent.chmod(0o700)
        if name == 'stored-day':
            (directory / ('digest-' + DAY.strftime('%Y%m%d') + '.json')).write_text(json.dumps({
                'schema': 'local-ai-digest/1', 'day': DAY.isoformat(), 'requests': 12, 'stored_marker': 'synthetic'}))
        if name in ('stored-write', 'round-trip'):
            initial = subprocess.run([*daemon.namespace, *reference, *arguments], cwd=home,
                                     env=environment, capture_output=True, text=True, timeout=15)
            if initial.returncode:
                raise AssertionError('Reference digest setup failed: ' + initial.stderr)
            value = json.loads(initial.stdout)
            if name == 'stored-write':
                value['generated_at'] = 1
                value['requests'] = 2
            (directory / ('digest-' + DAY.strftime('%Y%m%d') + '.json')).write_text(json.dumps(value))
            arguments.append('--write')
        if name == 'trends':
            # Three independently stored historical summaries establish a trend
            # only for this exact model/mode and depth band.
            path.write_text(''.join(json.dumps(row(ttfb_ms=100, prompt_ms=100, predicted_ms=1000,
                          turn_kind='compaction', wall_ms=1000)) + '\n' for _ in range(5)))
            for back in (1, 2, 3):
                day = DAY - datetime.timedelta(days=back)
                previous = {'schema': 'local-ai-digest/1', 'requests': 5, 'series': {
                    'ornith-35b-a3b-local/fast': {'ttft_hit_ms': {'p95': 10}, 'compaction_wall_ms_median': 10,
                        'decode_tok_s': {'8-32k': {'n': 5, 'p50': 100}},
                        'append_prefill_tok_s': {'8-32k': {'n': 5, 'p50': 1000}}}}}
                (directory / ('digest-' + day.strftime('%Y%m%d') + '.json')).write_text(json.dumps(previous))
        if name == 'iso-week-day':
            arguments[2] = '2026-W44-7'
        if name == 'date-not-padded':
            arguments[2] = '2026-1-1'
        command = shlex.split(os.environ['OXC_COMMAND']) if 'OXC_COMMAND' in os.environ else [PYTHON, str(entry)]
        result = subprocess.run([*daemon.namespace, *command, *arguments], cwd=home,
                                env=environment, capture_output=True, text=True, timeout=15)
        def normalize(value):
            if isinstance(value, list):
                return [normalize(item) for item in value]
            if not isinstance(value, dict):
                return value
            out = {key: normalize(item) for key, item in value.items()}
            at = out.get('generated_at')
            if at is not None and at != 1:
                if type(at) is not int or abs(time.time() - at) > 30:
                    raise AssertionError('Digest did not generate a recent timestamp')
                out['generated_at'] = '<validated:recent-unix-seconds>'
            if name.startswith('today-') and out.get('day') == when.isoformat():
                out['day'] = '<validated:today>'
            return out
        stdout = result.stdout
        if '--json' in arguments and result.returncode == 0:
            stdout = json.dumps(normalize(json.loads(stdout))) + '\n'
        files = {}
        if name in ('write-private', 'stored-write', 'round-trip'):
            for file in sorted(directory.iterdir()):
                if file.is_file():
                    files[file.name] = {'mode': oct(stat.S_IMODE(file.stat().st_mode)),
                                        'body': normalize(json.loads(file.read_text()))}
        output = {'exit': result.returncode, 'stdout': stdout, 'stderr': result.stderr,
                  'files': files, 'directory_mode': oct(stat.S_IMODE(directory.stat().st_mode)),
                  'engine_launches': len([r for r in daemon.events() if r['event'] == 'started'])}
        if name == 'round-trip':
            final = subprocess.run([*daemon.namespace, *reference, 'digest', '--status', '--json'], cwd=home,
                                   env=environment, capture_output=True, text=True, timeout=5)
            if final.returncode:
                raise AssertionError('Python could not read the written digest status')
            output['python_read'] = normalize(json.loads(final.stdout))
        public = json.dumps(output)
        if PRIVATE in public:
            raise AssertionError('Digest leaked private input text')
        return json.loads(public.replace(str(home), '<HOME>'))


class CliDigestContract(unittest.TestCase):
    def test_cli_digest_goldens(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(invoke_case(name, __file__), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_COMMAND' in os.environ or 'OXC_DAEMON_COMMAND' in os.environ:
            raise SystemExit('Goldens must be recorded from the Python reference')
        GOLDENS.write_text(json.dumps({name: invoke_case(name, __file__) for name in CASES}, indent=2, sort_keys=True) + '\n')
    elif sys.argv[1:2] == ['--case']:
        print(json.dumps(capture(sys.argv[2])))
    else:
        unittest.main()
