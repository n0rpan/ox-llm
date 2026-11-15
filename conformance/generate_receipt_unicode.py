"""Record reference Unicode facts for native receipt matching.

Run deliberately when updating the Python receipt reference. These tables contain
Unicode character ranges only; no machine identifiers or private sample values.
"""
from pathlib import Path
import unicodedata


def character_class(predicate):
    spans = []
    start = end = None
    for point in range(0x110000):
        if predicate(chr(point)):
            if start is None:
                start = end = point
            elif point == end + 1:
                end = point
            else:
                spans.append((start, end))
                start = end = point
    if start is not None:
        spans.append((start, end))
    def escape(point):
        return '\\u{' + format(point, 'x') + '}'
    return '[' + ''.join(escape(a) + ('-' + escape(b) if b != a else '') for a, b in spans) + ']'


if __name__ == '__main__':
    target = Path(__file__).resolve().parents[1] / 'oxc-rs/src'
    for name, predicate in [('word-class', lambda c: c.isalnum() or c == '_'),
                            ('decimal-class', str.isdecimal), ('digit-pattern', str.isdigit)]:
        expression = character_class(predicate)
        if name == 'digit-pattern':
            expression = '^' + expression + '+\\z'
        (target / ('receipt-' + name + '.txt')).write_text(expression + '\n')
    print('Receipt Unicode tables recorded from Unicode ' + unicodedata.unidata_version)
