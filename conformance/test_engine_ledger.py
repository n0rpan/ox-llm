"""Real child inheritance, numeric filtering and private advisory ledger storage."""
import json
import os
from pathlib import Path
import stat
import sys
import unittest

from harness import Daemon, PRIMARY
from test_api import chat
from test_conversation_stores import normalize
from test_lifecycle import status, wait_until

GOLDENS = Path(__file__).with_name('goldens') / 'engine-ledger.json'
CASES = ('enabled', 'invalid', 'chunked', 'tail', 'reload', 'explicit', 'explicit-equals',
         'inherited', 'log-symlink', 'log-hardlink', 'directory-symlink', 'rotation')


def run_case(name):
    environment = {'FAKE_ENGINE_LEDGER_CASE': name}
    if name == 'inherited':
        environment['OXC_ENGINE_LEDGER'] = '1'
    with Daemon(configured=True, environ=environment) as daemon:
        home = daemon.home
        config = json.loads(daemon.config_path.read_text())
        if name != 'inherited':
            config['environment'] = {**environment, 'OXC_ENGINE_LEDGER': '1'}
            config['profiles'][PRIMARY]['environment'] = config['environment']
        if name.startswith('explicit'):
            own = str(home / 'own-ledger.jsonl')
            extra = ['--ledger=' + own] if name.endswith('equals') else ['--ledger', own]
            config['oxe_args'] += extra
            config['profiles'][PRIMARY]['oxe_args'] = config['oxe_args']
        daemon.config_path.write_text(json.dumps(config))
        directory = home / 'state/oxc'
        path = directory / 'engine-ledger.jsonl'
        output = [{'before': status(daemon)['engine_ledger'], 'exists': path.exists()}]
        target = home / 'untouched-ledger-target'
        target.write_text('SYNTHETIC UNTOUCHED\n')
        directory.mkdir(mode=0o700, exist_ok=True)
        if name == 'directory-symlink':
            directory.rename(home / 'original-state')
            directory.symlink_to(home / 'original-state', target_is_directory=True)
        elif name == 'log-symlink':
            path.symlink_to(target)
        elif name == 'log-hardlink':
            os.link(target, path)
        elif name == 'rotation':
            path.write_bytes(b'x' * (1024**2 - 16))
            path.chmod(0o640)
        load = daemon.call('POST', '/_local/load', {})
        if load['status'] != 200:
            raise AssertionError(load)
        output.append(load)
        enabled = name not in ('explicit', 'explicit-equals', 'inherited', 'directory-symlink')
        output.append({'loaded': status(daemon)['engine_ledger']})
        generations = 2 if name == 'reload' else 1
        credentials = []
        for index in range(generations):
            credentials += [key.read_text().strip() for key in (home / 'run/oxc').glob('oxe-backend-*.key')]
            if not name.startswith('explicit'):
                reply = daemon.call('POST', '/v1/chat/completions', chat())
                if reply['status'] != 200:
                    raise AssertionError(reply)
                wait_until(lambda: status(daemon)['active_requests'] == 0)
            if enabled and name != 'tail':
                wait_until(lambda: status(daemon)['engine_ledger']['records'] >= 1)
            output.append({'after': status(daemon)['engine_ledger']})
            output.append(daemon.call('POST', '/_local/unload', {}))
            output.append({'unloaded': status(daemon)['engine_ledger']})
            if index + 1 < generations:
                output.append(daemon.call('POST', '/_local/load', {}))
        rows = []
        if path.is_file() and name not in ('log-symlink', 'log-hardlink', 'rotation'):
            rows = [json.loads(line) for line in path.read_text().splitlines()]
        elif name == 'rotation':
            rows = [json.loads(line) for line in path.read_text().splitlines()]
            backup = path.with_name(path.name + '.1')
            output.append({'backup_bytes': backup.stat().st_size, 'backup_mode': stat.S_IMODE(backup.stat().st_mode)})
        for row in rows:
            if row['engine_sha256'] != config['server_sha256']:
                raise AssertionError('Ledger did not identify the pinned executable')
            row['engine_sha256'] = '<validated:engine-sha256>'
        public = json.dumps([rows, output])
        if 'SYNTHETIC-PRIVATE-LEDGER-TEXT' in public:
            raise AssertionError('Ledger retained private text')
        if any(credential in public for credential in credentials):
            raise AssertionError('Ledger retained an engine credential')
        if any(row.get('n_prompt') == 999 for row in rows):
            raise AssertionError('Ledger captured ordinary stdout')
        output.append({'rows': rows, 'file_mode': stat.S_IMODE(path.stat().st_mode) if path.exists() else None,
                       'directory_mode': stat.S_IMODE(directory.stat().st_mode),
                       'target': target.read_text(),
                       'launches': [{key: value for key, value in event.items() if key != 'event'}
                                    for event in daemon.events() if event['event'] == 'ledger-started']})
        return normalize(output, home)


class EngineLedgerContract(unittest.TestCase):
    def test_engine_ledger_goldens(self):
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
