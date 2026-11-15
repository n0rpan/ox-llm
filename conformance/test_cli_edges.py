"""Additional argparse contracts shared by the native parser review slice."""
import json
import os
from pathlib import Path
import sys
import unittest

from test_cli import invoke

GOLDENS = Path(__file__).with_name('goldens') / 'cli-edges.json'
CASES = {
    'help-explicit-long': ['--help=synthetic'],
    'help-explicit-abbreviated': ['--he=synthetic'],
    'help-explicit-short': ['-h=synthetic'],
    'help-attached-short': ['-hsynthetic'],
    'help-short-cluster': ['-hh'],
    'help-empty-explicit': ['--help='],
    'help-wins-over-unknown': ['--synthetic-unknown', '--help'],
    'help-wins-over-extra-positionals': ['status', 'extra', '--help'],
    'ambiguous-s': ['--s'],
    'ambiguous-h': ['--h'],
    'ambiguous-help-value': ['--h=synthetic'],
    'invalid-short-option': ['-synthetic'],
    'missing-id': ['--id'],
    'missing-limit-before-help': ['--limit', '--help'],
    'invalid-abbreviated-mode': ['--mo', 'synthetic'],
    'invalid-negative-fraction': ['--limit', '-1.5'],
    'invalid-negative-dot-fraction': ['--limit', '-.5'],
    'negative-exponent-is-an-option': ['--limit', '-1e2'],
    'invalid-double-underscore': ['--limit', '1__2', '--version'],
    'invalid-leading-underscore': ['--limit', '_12', '--version'],
    'invalid-trailing-underscore': ['--limit', '12_', '--version'],
    'valid-underscores': ['--limit', '1_2', '--version'],
    'valid-arabic-decimal': ['--limit', '\u0661\u0662', '--version'],
    'valid-devanagari-underscore': ['--limit', '\u0967_\u0968', '--version'],
    'valid-fullwidth-signed': ['--limit', '+\uff11\uff12', '--version'],
    'valid-surrounding-whitespace': ['--limit', ' \t12\n', '--version'],
    'valid-unicode-whitespace': ['--limit', '\u200312\u00a0', '--version'],
    'valid-big-integer': ['--limit', '1234567890123456789012345678901234567890', '--version'],
    'valid-interpreter-digit-limit': ['--limit', '1' * 4300, '--version'],
    'invalid-interpreter-digit-limit': ['--limit', '1' * 4301, '--version'],
    'valid-negative-unicode': ['--limit', '-\u0661\u0662', '--version'],
    'valid-explicit-negative-underscore': ['--limit=-1_2', '--version'],
    'separate-negative-underscore-is-an-option': ['--limit', '-1_2', '--version'],
    'superscript-is-not-decimal': ['--limit', '\u00b2', '--version'],
    'uninstall-help-explicit': ['uninstall', '--help=synthetic'],
    'uninstall-short-help-attached': ['uninstall', '-hsynthetic'],
    'uninstall-missing-record': ['uninstall', '--record'],
    'uninstall-ambiguous-r': ['uninstall', '--r'],
    'uninstall-unknown-then-help': ['uninstall', '--synthetic-unknown', '--help'],
}


class CliEdgeContract(unittest.TestCase):
    def test_parser_edge_goldens(self):
        expected = json.loads(GOLDENS.read_text())
        self.assertEqual(set(expected), set(CASES))
        for name, words in CASES.items():
            with self.subTest(case=name):
                self.assertEqual(invoke(words), expected[name])


if __name__ == '__main__':
    if sys.argv[1:] == ['--record-python']:
        if 'OXC_COMMAND' in os.environ:
            raise SystemExit('Record goldens only from the Python reference')
        GOLDENS.write_text(json.dumps({name: invoke(words) for name, words in CASES.items()}, indent=2, sort_keys=True) + '\n')
    else:
        unittest.main()
