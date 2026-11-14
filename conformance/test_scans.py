"""Receipt exporter/scrubber contracts using synthetic files and no scanner exclusions."""
import datetime
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest

from harness import ROOT, isolated_env

GOLDENS = Path(__file__).with_name('goldens') / 'scans.json'
CASES = ('scan-clean', 'scan-denied-word', 'scan-receipt-path', 'scan-tree-path', 'scan-binary',
         'scan-tree-binary', 'export-private-fields',
         'export-bad-digest', 'scrub-check', 'scrub-apply', 'rules')


def run_case(name):
    with tempfile.TemporaryDirectory(prefix='oxc-scan-conformance-') as directory:
        home = Path(directory)
        env = isolated_env(home)
        receipt = home / 'receipt.json'
        receipt.write_text(json.dumps({'prompt': 'Synthetic private prompt.', 'reply': 'Synthetic private reply.',
            'max_rss_kib': 1024, 'tokens_per_second': 2.5, 'n_ctx': 32768, 'threads': 1}))
        out = home / 'public.json'
        variable, script = 'OXC_EXPORT_RECEIPT_COMMAND', 'export_receipt.py'
        expected = 0
        args = []
        if name == 'scan-clean':
            clean = home / 'clean.json'
            clean.write_text('{"tokens_per_second":2.5}\n')
            args = ['scan', str(clean)]
        elif name == 'scan-denied-word':
            receipt.write_text('synthetic-forbidden-word\n')
            args = ['scan', str(receipt), '--deny-word', 'synthetic-forbidden-word']
            expected = 1
        elif name in ('scan-receipt-path', 'scan-tree-path', 'scan-binary', 'scan-tree-binary'):
            receipt.write_bytes(b'\x00synthetic placeholder\n' if 'binary' in name else b'/tmp/synthetic-private\n')
            args = ['scan', str(receipt)]
            if 'tree' in name:
                args += ['--profile', 'tree']
            else:
                expected = 1
        elif name.startswith('export-'):
            args = ['export', str(receipt), '--out', str(out), '--ticket', 'synthetic']
            if name == 'export-bad-digest':
                args += ['--binary-sha256', 'invalid']
                expected = 2
        elif name.startswith('scrub-'):
            variable, script = 'OXC_SCRUB_RECEIPTS_COMMAND', 'scrub_receipts.py'
            receipt.write_text('synthetic path: ' + str(home / 'private') + '\n')
            args = [str(receipt), '--home', str(home), '--user', '']
            if name == 'scrub-check':
                args.append('--check')
                expected = 1
        elif name == 'rules':
            args = ['rules']
        command = shlex.split(os.environ[variable]) if variable in os.environ else [
            sys.executable, str(ROOT / 'lab/tools' / script)]
        result = subprocess.run(command + args, env=env, cwd=home, capture_output=True, text=True, timeout=10)
        if result.returncode != expected:
            raise AssertionError(name + ': ' + repr((result.returncode, result.stdout, result.stderr)))
        value = {'exit': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr}
        if name.startswith('scrub-'):
            value['source_after'] = receipt.read_text()
        if out.exists():
            exported = json.loads(out.read_text())
            stamp = datetime.datetime.strptime(exported['created_utc'], '%Y-%m-%dT%H:%M:%SZ').replace(tzinfo=datetime.timezone.utc)
            if abs((datetime.datetime.now(datetime.timezone.utc) - stamp).total_seconds()) > 30:
                raise AssertionError('Invalid receipt timestamp')
            exported['created_utc'] = '<current:utc-seconds>'
            value['exported'] = exported
            encoded = json.dumps(exported)
            for private in ('Synthetic private prompt.', 'Synthetic private reply.'):
                if private in encoded:
                    raise AssertionError('Private input leaked into exported receipt')
        return json.loads(json.dumps(value).replace(str(home), '<HOME>'))


class ShippedScanContract(unittest.TestCase):
    def test_scan_goldens(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name in CASES:
            with self.subTest(case=name):
                self.assertEqual(run_case(name), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if any(key in os.environ for key in ('OXC_EXPORT_RECEIPT_COMMAND', 'OXC_SCRUB_RECEIPTS_COMMAND')):
            raise SystemExit('Goldens must be recorded from the Python reference')
        GOLDENS.write_text(json.dumps({name: run_case(name) for name in CASES},
                                     indent=2, sort_keys=True) + '\n')
    else:
        unittest.main()
