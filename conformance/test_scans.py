"""Receipt exporter/scrubber contracts using synthetic files and no scanner exclusions."""
from environment import PYTHON
import datetime
import hashlib
import json
import os
from pathlib import Path
import shlex
import secrets
import random
import subprocess
import sys
import tempfile
import unittest
import uuid

from harness import ROOT, isolated_env

GOLDENS = Path(__file__).with_name('goldens') / 'scans.json'
CASES = ('scan-clean', 'scan-denied-word', 'scan-receipt-path', 'scan-tree-path', 'scan-binary',
         'scan-tree-binary', 'export-private-fields',
         'export-bad-digest', 'scrub-check', 'scrub-apply', 'rules',
         'scan-rule-corpus', 'scan-trivial-mac', 'scan-allow-file', 'scan-exact-word',
         'export-structured', 'export-no-hashes', 'export-allow-text', 'export-hardware',
         'scrub-layout', 'scrub-binary', 'scan-allow-summary', 'scan-deny-file',
         'scan-line-limit', 'export-refusal', 'export-directories', 'export-numbers',
         'export-shell-argv', 'export-generated-trees', 'export-virtual-numbers',
         'export-help', 'scan-help', 'scrub-help', 'scan-unicode-boundaries',
         'scan-unicode-deny', 'scan-system-word', 'scan-system-numeric-word',
         'export-terminal-newline')


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
        generated_private = []
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
        elif name == 'scan-rule-corpus':
            from test_share_network import generate_certificate
            _, key = generate_certificate(home, 'synthetic', '127.0.0.1')
            credential = 'sk-' + secrets.token_urlsafe(32)
            generated_private.append(credential)
            receipt.write_text('\n'.join((
                '/home/synthetic-profile/private', '~/synthetic-private', '/tmp/synthetic-private',
                '/run/user/12345/bus user-12345.slice user@12345.service', '02:01:02:03:04:05',
                'device_serial=' + secrets.token_hex(12), str(uuid.uuid4()),
                'synthetic@example.invalid', '/mnt/synthetic-volume/file', '192.168.50.2',
                key.read_text().splitlines()[0], credential,
                json.dumps({'authorization': secrets.token_hex(12)}), '12.5 USD')) + '\n')
            args = ['scan', str(receipt), '--no-system-denylist']
            expected = 1
        elif name == 'scan-trivial-mac':
            receipt.write_text('00:00:00:00:00:00 ff-ff-ff-ff-ff-ff\n')
            args = ['scan', str(receipt), '--no-system-denylist']
        elif name == 'scan-allow-file':
            receipt.write_text('/tmp/synthetic-private\n')
            allow = home / 'allow.tsv'
            allow.write_text('receipt.json\tabs-path\tsynthetic test exemption\n')
            args = ['scan', str(receipt), '--allow-file', str(allow), '--base', str(home), '--no-system-denylist']
        elif name == 'scan-exact-word':
            receipt.write_text('SyntheticForbiddenWord\n')
            args = ['scan', str(receipt), '--deny-word', 'syntheticforbiddenword',
                    '--allow-exact', 'SyntheticForbiddenWord', '--no-system-denylist']
        elif name == 'scan-allow-summary':
            receipt.write_text('/tmp/synthetic-private\n~/synthetic-private\n')
            allow = home / 'allow.tsv'
            allow.write_text('receipt.json\tabs-path\tsynthetic test exemption\n'
                             'missing.json\tabs-path\tsynthetic unused exemption\n')
            args = ['scan', str(receipt), '--allow-file', str(allow), '--base', str(home),
                    '--no-system-denylist', '--summary']
            expected = 1
        elif name == 'scan-deny-file':
            receipt.write_text('SyntheticForbiddenWord abcSyntheticForbiddenWorddef\n')
            deny = home / 'deny.txt'
            deny.write_text('# synthetic comment\nxx\nsyntheticforbiddenword\n')
            args = ['scan', str(receipt), '--deny-file', str(deny), '--no-system-denylist']
            expected = 1
        elif name == 'scan-line-limit':
            receipt.write_text('µ' * 199999 + '~/private\n' + 'µ' * 199997 + '~/private\n')
            args = ['scan', str(receipt), '--no-system-denylist']
            expected = 1
        elif name == 'scan-unicode-boundaries':
            credential = 'sk-' + secrets.token_urlsafe(32)
            generated_private.append(credential)
            receipt.write_text('\n'.join((
                'devıce_serıal=' + secrets.token_hex(12),
                'authorızation=' + secrets.token_hex(12),
                '\u0301' + credential, 'api_key=\u001f' + secrets.token_hex(12),
                '$HOME\u0301/synthetic', '\u0301user-12345.slice',
                '²' + credential)) + '\n')
            args = ['scan', str(receipt), '--no-system-denylist']
            expected = 1
        elif name == 'scan-unicode-deny':
            receipt.write_text('synthetıcidentıty\n')
            args = ['scan', str(receipt), '--deny-word', 'syntheticidentity', '--no-system-denylist']
            expected = 1
        elif name == 'scan-system-word':
            env['RECEIPT_DENY_WORDS'] = 'synthetic-system-identity'
            receipt.write_text('synthetic-system-identity\n')
            args = ['scan', str(receipt)]
            expected = 1
        elif name == 'scan-system-numeric-word':
            env['USER'] = 'ⅫⅫⅫ'
            receipt.write_text('ⅫⅫⅫ\n')
            args = ['scan', str(receipt)]
            expected = 1
        elif name in ('export-help', 'scan-help', 'scrub-help'):
            if name == 'scrub-help':
                variable, script = 'OXC_SCRUB_RECEIPTS_COMMAND', 'scrub_receipts.py'
                args = ['--help']
            else:
                args = [name.removesuffix('-help'), '--help']
        elif name.startswith('export-'):
            args = ['export', str(receipt), '--out', str(out), '--ticket', 'synthetic']
            if name == 'export-bad-digest':
                args += ['--binary-sha256', 'invalid']
                expected = 2
            elif name == 'export-structured':
                credential = 'sk-' + secrets.token_urlsafe(32)
                generated_private.append(credential)
                receipt.write_text(json.dumps({
                    'argv': ['/tmp/synthetic-engine', '--threads', '4', '--prompt',
                             'SYNTHETIC-PRIVATE-PROMPT', '--api-key', credential],
                    'environment': {'OMP_NUM_THREADS': '4', 'UNRECOGNIZED_CREDENTIAL': credential},
                    'token_ids': [1, 2, 3], 'stdout': 'SYNTHETIC-PRIVATE-OUTPUT',
                    'thermal_zone0/temp': 65000, 'thermal_zone1/temp': 55,
                    'model_path': '/tmp/synthetic-model.gguf', 'engine_path': '/tmp/synthetic-engine',
                    'threads': 4, 'n_ctx': 8192, 'pid': 12345}))
                generated_private += ['SYNTHETIC-PRIVATE-PROMPT', 'SYNTHETIC-PRIVATE-OUTPUT']
                args.append('--no-system-denylist')
            elif name == 'export-no-hashes':
                args += ['--no-prompt-hash', '--no-source-hash', '--no-system-denylist']
            elif name == 'export-allow-text':
                receipt.write_text(json.dumps({'custom_note': 'synthetic safe note',
                                               'custom_path': '/tmp/synthetic-private'}))
                args += ['--allow-text-key', 'custom_note', '--no-system-denylist']
            elif name == 'export-hardware':
                hardware = home / 'hardware.json'
                hardware.write_text(json.dumps({'chip': 'synthetic chip', 'memory_gb': 16,
                                                'serial': 'SYNTHETIC-PRIVATE-SERIAL', 'price': '12 USD'}))
                args += ['--hardware', str(hardware), '--no-system-denylist']
            elif name == 'export-refusal':
                refused = home / 'synthetic-forbidden-word.json'
                refused.write_bytes(receipt.read_bytes())
                args[1] = str(refused)
                args += ['--deny-word', 'synthetic-forbidden-word', '--no-system-denylist']
                expected = 3
            elif name == 'export-directories':
                inputs = home / 'receipts'
                inputs.mkdir()
                (inputs / 'a.json').write_text(json.dumps({'threads': 2, 'engine_commit': 'abcdef1',
                    'model': 'synthetic-model', 'quant': 'Q4', 'n_ctx': 8192}))
                (inputs / 'b.json').write_text(json.dumps({'engine_commit': 'abcdef123456', 'threads': 2}))
                (inputs / 'invalid.json').write_text('synthetic invalid JSON\n')
                (inputs / 'not-a-model.gguf').write_bytes(b'\0synthetic unused file\n')
                args[1] = str(inputs)
                args += ['--title', 'synthetic report', '--evidence-class', 'S', '--n', '3',
                         '--quality-class', 'lossless', '--note', 'synthetic note', '--no-system-denylist']
            elif name == 'export-numbers':
                receipt.write_text(json.dumps({'finite': 1e-7, 'large_float': 1e17,
                    'big_integer': 10 ** 100, 'non_finite': float('nan'), 'infinite': float('inf'),
                    'environment': {'OMP_BOOLEAN': True}, 'thermal_zone0/temp': 1250,
                    'thermal_zone1/temp': '65.05', 'count': True, 'prompt': [1, 2.5, float('nan')]}))
                args.append('--no-system-denylist')
            elif name == 'export-shell-argv':
                receipt.write_text(json.dumps({'command': "OMP_NUM_THREADS=2 /tmp/synthetic-engine "
                    "--prompt 'synthetic private prompt' --threads=4 -n -1 --api-key=<redacted>"}))
                args.append('--no-system-denylist')
            elif name == 'export-generated-trees':
                rng = random.Random(20261115)
                keys = ['meta', 'note', 'verdict', 'label', 'name', 'data', 'x', 'run', 'env', 'argv',
                        'model', 'kind', 'model_path', 'title', 'output', 'status', 'items', 'config',
                        'args', 'command', 'environment', 'prompt', 'token_ids', '/tmp/synthetic-key']
                values = [0, 1, True, None, 'ok', 'synthetic-forbidden-word', '/tmp/synthetic-private',
                          '~/synthetic-private', '/run/user/12345', 'pre synthetic-forbidden-word post',
                          '13/13 checks, 44.1 tok/s (108 GB/s)']
                def tree(depth):
                    roll = rng.random()
                    if depth > 3 or roll < .25:
                        return rng.choice(values + [rng.random(), rng.randint(0, 9)])
                    if roll < .5:
                        return [tree(depth + 1) for _ in range(rng.randint(0, 4))]
                    return {rng.choice(keys): tree(depth + 1) for _ in range(rng.randint(0, 5))}
                receipt.write_text(json.dumps([tree(0) for _ in range(48)]))
                args += ['--deny-word', 'synthetic-forbidden-word', '--no-system-denylist']
            elif name == 'export-virtual-numbers':
                receipt.write_text(json.dumps({'prompt': float('nan'), 'output': float('inf'),
                    'environment': {'OMP_VALUE': float('nan')}, 'argv': [float('inf'), True, None],
                    'thermal_zone0/temp': True, 'underflow': 1e-200, 'negative_zero': -0.0,
                    'prompt_ids': [1, 10 ** 100], 'nested': [float('nan'), None, 2.0]}))
                args.append('--no-system-denylist')
            elif name == 'export-terminal-newline':
                receipt.write_text(json.dumps({'x': '123\n', 'other': 'ok\n', 'path': 'fast\n',
                    'command': ['synthetic-engine\n', '--threads\n', '4']}))
                args.append('--no-system-denylist')
        elif name.startswith('scrub-'):
            variable, script = 'OXC_SCRUB_RECEIPTS_COMMAND', 'scrub_receipts.py'
            receipt.write_text('synthetic path: ' + str(home / 'private') + '\n')
            args = [str(receipt), '--home', str(home), '--user', '']
            if name == 'scrub-check':
                args.append('--check')
                expected = 1
            elif name == 'scrub-layout':
                receipt.write_bytes((str(home / 'Work/local-ai') + '\r\n' +
                    str(home / '.local/share/ai-stack') + '\r\n' +
                    str(home / '.local/state/oxc') + '\r\n' +
                    '/run/user/12345 user-12345.slice user@12345.service\r\n' +
                    str(uuid.uuid4()) + '\r\n' + 'numbers 1.25 32768\r\n').encode())
            elif name == 'scrub-binary':
                receipt.write_bytes(b'\0synthetic unchanged binary fixture\n')
        elif name == 'rules':
            args = ['rules']
        command = shlex.split(os.environ[variable]) if variable in os.environ else [
            PYTHON, str(ROOT / 'oxc/tools' / script)]
        result = subprocess.run(command + args, env=env, cwd=home, capture_output=True, text=True, timeout=10)
        if result.returncode != expected:
            raise AssertionError(name + ': ' + repr((result.returncode, result.stdout, result.stderr)))
        value = {'exit': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr}
        if name.startswith('scrub-') and name != 'scrub-help':
            value['source_after'] = receipt.read_bytes().decode()
        if out.exists():
            exported = json.loads(out.read_text())
            stamp = datetime.datetime.strptime(exported['created_utc'], '%Y-%m-%dT%H:%M:%SZ').replace(tzinfo=datetime.timezone.utc)
            if abs((datetime.datetime.now(datetime.timezone.utc) - stamp).total_seconds()) > 30:
                raise AssertionError('Invalid receipt timestamp')
            exported['created_utc'] = '<current:utc-seconds>'
            if name == 'export-structured':
                source = exported['sources'][0]
                if source['private_file_sha256'] != hashlib.sha256(receipt.read_bytes()).hexdigest():
                    raise AssertionError('Invalid private source digest')
                source['private_file_sha256'] = '<validated:private-file-sha256>'
            value['exported'] = exported
            encoded = json.dumps(exported)
            for private in ('Synthetic private prompt.', 'Synthetic private reply.', *generated_private):
                if private in encoded:
                    raise AssertionError('Private input leaked into exported receipt')
        for private in generated_private:
            if private in result.stdout or private in result.stderr:
                raise AssertionError('Private input leaked into diagnostics')
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
