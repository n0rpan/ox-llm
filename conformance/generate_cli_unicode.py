"""Record public Unicode decimal-zero facts for the dependency-free CLI parser."""
from pathlib import Path
import unicodedata

if __name__ == '__main__':
    zeroes = [point for point in range(0x110000) if unicodedata.decimal(chr(point), None) == 0]
    target = Path(__file__).resolve().parents[1] / 'oxc-rs/src/cli-decimal-zeroes.rs'
    target.write_text('// Generated from Python Unicode ' + unicodedata.unidata_version + '.\n[\n'
                      + ''.join(f'    0x{point:x},\n' for point in zeroes) + ']\n')
    print(f'Recorded {len(zeroes)} decimal zero code points from Unicode {unicodedata.unidata_version}')
