#!/usr/bin/env python3
"""Stage a standard /usr tree, including the source layout the bootstrap installer needs."""
import argparse
import os
from pathlib import Path
import shutil

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('destination', type=Path)
args = parser.parse_args()
root = Path(__file__).resolve().parents[2]
dest = args.destination.resolve()
if dest.exists():
    raise SystemExit('destination must be fresh')
(dest / 'usr/bin').mkdir(parents=True)
for workspace, names in [('engine', ('oxe', 'oxe-server')), ('oxa', ('oxa',))]:
    for name in names:
        shutil.copy2(root / workspace / 'target/release' / name, dest / 'usr/bin' / name)
for workspace in ('oxc', 'oxc-rs'):
    if (root / workspace / 'Cargo.toml').is_file():
        for binary in sorted((root / workspace / 'target/release').iterdir()):
            if binary.is_file() and os.access(binary, os.X_OK) and not binary.suffix:
                target = dest / 'usr/bin' / binary.name
                if target.exists():
                    raise SystemExit('duplicate binary: ' + binary.name)
                shutil.copy2(binary, target)
lab = dest / 'usr/share/ox-llm/lab'
shutil.copytree(root / 'lab', lab, ignore=shutil.ignore_patterns(
    '__pycache__', '*.pyc', '*.egg-info', '.venv', 'build', 'dist',
    'tests', 'tools', 'benchmarks', 'experiments'))
# Preserve the runtime script and sibling Python modules exactly as the bootstrap
# installer consumes them. Alias the command without renaming product modules.
wrapper = dest / 'usr/bin/oxc'
if not wrapper.exists():
    wrapper.write_text('#!/bin/sh\nexec /usr/bin/python3 -B /usr/share/ox-llm/lab/src/snapdragon_ai_lab/runtime/local-ai "$@"\n')
    wrapper.chmod(0o755)
wrapper = dest / 'usr/bin/snapdragon-ai-lab'
wrapper.write_text('#!/bin/sh\nPYTHONPATH=/usr/share/ox-llm/lab/src exec /usr/bin/python3 -B -m snapdragon_ai_lab "$@"\n')
wrapper.chmod(0o755)
licenses = dest / 'usr/share/licenses/ox-llm'
licenses.mkdir(parents=True)
for product in ('engine', 'oxa', 'lab'):
    for name in ('LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES', 'LICENSE-QuickJS.txt', 'LICENSE-TLS.txt', 'LICENSE-syntect.txt'):
        source = root / product / name
        if source.is_file():
            shutil.copy2(source, licenses / (product + '-' + name))
    for directory in ('third_party',):
        source = root / product / directory
        if source.is_dir():
            shutil.copytree(source, licenses / (product + '-' + directory))
shutil.copy2(root / 'LICENSE', licenses / 'LICENSE')
for path in dest.rglob('*'):
    if path.is_file() and not path.is_symlink():
        path.chmod(0o755 if path.parent == dest / 'usr/bin' else 0o644)
