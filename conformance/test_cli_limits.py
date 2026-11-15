"""Python integer syntax and bounds through the actual public ledger command."""
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time
import unittest

from environment import ROOT, isolated_env, PRIMARY

GOLDENS = Path(__file__).with_name('goldens') / 'cli-limits.json'
CASES = {
    'ascii': '12',
    'arabic': '\u0661\u0662',
    'devanagari-underscore': '\u0967_\u0968',
    'signed-fullwidth': '+\uff11\uff12',
    'unicode-whitespace': '\u200312\u00a0',
    'negative-unicode': '-\u0663',
    'big-integer': '1234567890123456789012345678901234567890',
}


def capture(name):
    with tempfile.TemporaryDirectory(prefix='oxc-cli-limit-') as directory:
        home = Path(directory)
        env = isolated_env(home)
        ledger = home / '.local/state/local-ai/ledger'
        ledger.mkdir(parents=True, mode=0o700)
        path = ledger / ('requests-' + time.strftime('%Y%m%d') + '.jsonl')
        rows = [{'path': '/v1/chat/completions', 'model': PRIMARY, 't': index,
                 'status': 200, 'origin': 'lab', 'mode': 'fast', 'stream': False,
                 'prompt': 'Synthetic private content.'} for index in range(1, 202)]
        path.write_text(''.join(json.dumps(row) + '\n' for row in rows))
        path.chmod(0o600)
        before = path.read_bytes()
        executable = home / 'oxc'
        executable.symlink_to(ROOT / 'lab/src/snapdragon_ai_lab/runtime/local-ai')
        command = shlex.split(os.environ['OXC_COMMAND']) if 'OXC_COMMAND' in os.environ else [sys.executable, str(executable)]
        # Explicit form lets argparse distinguish a negative Unicode number
        # from an option, while still running its real type=int conversion.
        result = subprocess.run([*command, 'ledger', '--json', '--limit=' + CASES[name]],
                                env=env, cwd=home, capture_output=True, text=True, timeout=5)
        if result.returncode or result.stderr:
            raise AssertionError('Ledger limit invocation failed: ' + result.stderr)
        value = json.loads(result.stdout)
        if 'Synthetic private content.' in result.stdout or path.read_bytes() != before:
            raise AssertionError('Ledger reader exposed content or modified its input')
        rows = value['rows']
        if [row['t'] for row in rows] != list(range(201, 201 - len(rows), -1)):
            raise AssertionError('Ledger limit changed row ordering')
        return {'exit': result.returncode, 'stderr': result.stderr,
                'ledger_available': value['ledger_available'], 'count': len(rows),
                'first': rows[0], 'last': rows[-1]}


class CliLimitContract(unittest.TestCase):
    def test_unicode_and_large_ledger_limits(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(capture(name), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_COMMAND' in os.environ:
            raise SystemExit('Record goldens only from the Python reference')
        GOLDENS.write_text(json.dumps({name: capture(name) for name in CASES}, indent=2, sort_keys=True) + '\n')
    else:
        unittest.main()
