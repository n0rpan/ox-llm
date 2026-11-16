#!/usr/bin/env python3
"""CycloneDX 1.6 inventory from Cargo's locked resolver, plus shipped file hashes.

Cargo metadata includes the full workspace resolution (including build/test-only
components); record that scope explicitly rather than claim a minimal runtime BOM.
"""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tomllib

root = Path(__file__).resolve().parents[2]
payload, output = map(Path, sys.argv[1:])
components = {}
for workspace in ('engine', 'oxa', 'oxc', 'oxc-rs'):
    if not (root / workspace / 'Cargo.toml').is_file():
        continue
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--locked', '--format-version', '1'], cwd=root / workspace))
    lock = tomllib.loads((root / workspace / 'Cargo.lock').read_text())
    checksums = {(p['name'], p['version'], p.get('source')): p.get('checksum') for p in lock['package']}
    for package in metadata['packages']:
        name, version = package['name'], package['version']
        source = package.get('source')
        reference = f'pkg:cargo/{name}@{version}'
        # Local workspaces can contain identically named packages. Keep source identity.
        key = (name, version, source)
        item = {'type': 'library', 'bom-ref': reference, 'name': name, 'version': version,
                'purl': reference, 'properties': [{'name': 'ox-llm:scope', 'value': 'workspace resolution; includes build and test dependencies'}]}
        if package.get('license'):
            item['licenses'] = [{'expression': package['license']}]
        checksum = checksums.get(key)
        if checksum:
            item['hashes'] = [{'alg': 'SHA-256', 'content': checksum}]
        components[key] = item
for path in sorted(payload.rglob('*')):
    if path.is_file():
        relative = path.relative_to(payload).as_posix()
        components[relative] = {'type': 'file', 'bom-ref': relative, 'name': relative,
                                'hashes': [{'alg': 'SHA-256', 'content': hashlib.sha256(path.read_bytes()).hexdigest()}]}
output.write_text(json.dumps({'bomFormat': 'CycloneDX', 'specVersion': '1.6', 'version': 1,
    'metadata': {'component': {'type': 'application', 'name': 'ox-llm', 'version': os.environ['VERSION'].removeprefix('v')}},
    'components': sorted(components.values(), key=lambda p: p['bom-ref'])}, indent=2) + '\n')
