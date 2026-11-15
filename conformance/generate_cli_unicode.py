"""Record public Unicode decimal-zero facts for the dependency-free CLI parser."""
from pathlib import Path
import unicodedata

if __name__ == '__main__':
    zeroes = [point for point in range(0x110000) if unicodedata.decimal(chr(point), None) == 0]
    target = Path(__file__).resolve().parents[1] / 'oxc-rs/src/cli-decimal-zeroes.rs'
    target.write_text('// Generated from Python Unicode ' + unicodedata.unidata_version + '.\n[\n'
                      + ''.join(f'    0x{point:x},\n' for point in zeroes) + ']\n')
    print(f'Recorded {len(zeroes)} decimal zero code points from Unicode {unicodedata.unidata_version}')
    ranges = []
    start = end = None
    for point in range(0x110000):
        if chr(point).isprintable():
            if start is None:
                start = end = point
            elif point == end + 1:
                end = point
            else:
                ranges.append((start, end))
                start = end = point
    if start is not None:
        ranges.append((start, end))
    rows = [f'[{a:#x}, {b:#x}]' for a, b in ranges]
    target = target.with_name('cli-printable-ranges.rs')
    target.write_text('// Generated from Python Unicode ' + unicodedata.unidata_version + '.\n[\n'
                      + ''.join('    ' + ', '.join(rows[i:i+6]) + ',\n' for i in range(0, len(rows), 6)) + ']\n')
    print(f'Recorded {len(ranges)} printable ranges')
