"""Select every source or native peer explicitly; never silently mix peers."""
import argparse
import os
from pathlib import Path
import subprocess

CONFORMANCE = Path(__file__).resolve().parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--python', action='store_true', help='run only the Python reference')
    mode.add_argument('--native-dir', type=Path, help='directory containing all compiled contract examples')
    args = parser.parse_args()
    env = {name: value for name, value in os.environ.items()
           if not (name.startswith('OXC_') and name.endswith('_CONTRACT'))}
    if args.native_dir is not None:
        directory = args.native_dir.resolve()
        missing = []
        for module in sorted(CONFORMANCE.glob('test_*.py')):
            name = module.stem.removeprefix('test_')
            binary = directory / (name + '_contract')
            if not binary.is_file() or not os.access(binary, os.X_OK):
                missing.append(binary.name)
            else:
                env['OXC_' + name.upper() + '_CONTRACT'] = str(binary)
        if missing:
            parser.error('missing executable native contract drivers: ' + ', '.join(missing))
    return subprocess.call(['/usr/bin/python3', '-m', 'unittest', 'discover',
                            '-s', str(CONFORMANCE), '-v'], env=env)


if __name__ == '__main__':
    raise SystemExit(main())
