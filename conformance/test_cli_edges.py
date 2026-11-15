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


# Error rendering must preserve Python repr rather than emit raw control bytes.
for key, value in {
    'single-quote': "a'b", 'both-quotes': 'a\'"b', 'backslash': 'a\\b',
    'newline': 'a\nb', 'tab': '\t', 'control': '\x1c1',
    'nonbreaking-space': '\u00a0', 'format-character': '\u200d',
    'private-use': '\ue000', 'nonprintable-astral': '\U000e0001',
}.items():
    CASES['repr-limit-' + key] = ['--limit=' + value, '--version']
    CASES['repr-help-' + key] = ['--help=' + value]
CASES['repr-invalid-mode'] = ['--mode', "a'b", '--version']
CASES['repr-invalid-action'] = ["a'b", '--version']


CASES.update({
    'space-negative-limit': ['--limit', '- 1', '--version'],
    'space-negative-id': ['--id', '- 1', '--version'],
    'space-negative-mode': ['--mode', '- 1', '--version'],
    'space-negative-pref': ['--pref', '- 1', '--version'],
    'space-negative-action': ['--check', '- 1', '--version'],
    'negative-integer-action': ['--check', '-0', '--version'],
    'negative-fraction-action': ['--check', '-.1', '--version'],
    'known-option-with-space-remains-an-option': ['--id', '--help=hello world', '--version'],
    'version-before-later-ambiguity': ['--version', '--h'],
    'help-before-later-ambiguity': ['--help', '--s'],
    'invalid-choice-before-later-ambiguity': ['--mode', 'invalid', '--h'],
    'ambiguity-after-separator-is-positional': ['--version', '--', '--h'],
    'uninstall-help-before-later-ambiguity': ['uninstall', '--help', '--r'],
    'missing-value-before-later-ambiguity': ['--id', '--h'],
    # Arguments that are not valid UTF-8 reach Python as lone surrogates; the native
    # parser must not panic on them.
    'invalid-utf8-limit': ['--limit', b'\x855', '--version'],
    'invalid-utf8-mode': ['--mode', b'a\xffb', '--version'],
    'invalid-utf8-action': [b'\xc3(', '--version'],
})


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
