"""Verify the experimental parser cannot execute controller commands."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def main():
    if len(sys.argv) != 2:
        raise SystemExit('usage: check_native_scaffold.py ABSOLUTE_BINARY')
    binary = Path(sys.argv[1])
    if not binary.is_absolute() or not binary.is_file() or not os.access(binary, os.X_OK):
        raise SystemExit('expected an absolute path to the built native executable')
    commands = ([], ['status'], ['load'], ['cache', 'budget', 'off'],
                ['share', 'on', 'lan'], ['update'], ['uninstall', '--apply'])
    for words in commands:
        with tempfile.TemporaryDirectory(prefix='oxc-native-parser-') as directory:
            home = Path(directory)
            env = {'HOME': str(home), 'PATH': '/usr/bin:/bin', 'LANG': 'C.UTF-8', 'LC_ALL': 'C.UTF-8'}
            result = subprocess.run([str(binary), *words], cwd=home, env=env,
                                    text=True, capture_output=True, timeout=5)
            if result.returncode != 2 or result.stdout or result.stderr != (
                    'oxc-native: command execution is unavailable in this experimental parser build.\n'):
                raise AssertionError('Experimental parser did not refuse command execution: ' + repr(words))
            if list(home.iterdir()):
                raise AssertionError('Experimental parser wrote installation state')
    print(f'{len(commands)} native command-execution refusals passed')


if __name__ == '__main__':
    main()
