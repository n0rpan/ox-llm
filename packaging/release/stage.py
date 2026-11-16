#!/usr/bin/env python3
"""Build-time staging of the Rust-only installed payload; Python is not installed."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys


def stage(root, dest, runner=subprocess.run):
    root, dest = Path(root), Path(dest).absolute()
    if os.path.lexists(dest):
        raise ValueError('destination must be fresh')
    (dest / 'usr/bin').mkdir(parents=True)
    for workspace, names in [('engine', ('oxe-server',)), ('oxa', ('oxa',))]:
        for name in names:
            shutil.copy2(root / workspace / 'target/release' / name, dest / 'usr/bin' / name)
    # No controller tree, Python-backed unit, panel plugin or diagnostics wrapper is staged.
    licenses = dest / 'usr/share/licenses/ox-llm'
    licenses.mkdir(parents=True)
    for product in ('engine', 'oxa', 'oxc'):
        for name in ('LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES', 'LICENSE-QuickJS.txt', 'LICENSE-TLS.txt', 'LICENSE-syntect.txt'):
            source = root / product / name
            if source.is_file():
                shutil.copy2(source, licenses / (product + '-' + name))
        for directory in ('third_party',):
            source = root / product / directory
            if source.is_dir():
                shutil.copytree(source, licenses / (product + '-' + directory))
    shutil.copy2(root / 'LICENSE', licenses / 'LICENSE')
    guide = dest / 'usr/share/doc/ox-llm/NATIVE-INSTALL.md'
    guide.parent.mkdir(parents=True)
    shutil.copy2(root / 'docs/NATIVE-INSTALL.md', guide)
    # Use the repository's existing license collector for every linked Rust closure.
    collector = root / 'engine/tools/ci/crate_licenses.py'
    for workspace, packages in [('engine', ('server',)), ('oxa', ('oxa-cli',))]:
        for package in packages:
            runner([sys.executable, '-I', str(collector), package,
                    str(licenses / 'crates' / package)], cwd=root / workspace, check=True)
    shutil.copy2(root / 'engine/vendor/ggml-subset/LICENSE', licenses / 'ggml-subset-LICENSE')
    for path in dest.rglob('*'):
        if path.is_dir():
            path.chmod(0o755)
        elif path.is_file() and not path.is_symlink():
            path.chmod(0o755 if path.parent == dest / 'usr/bin' else 0o644)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('destination', type=Path)
    args = parser.parse_args()
    try:
        stage(Path(__file__).resolve().parents[2], args.destination)
    except ValueError as error:
        parser.exit(1, str(error) + '\n')


if __name__ == '__main__':
    main()
