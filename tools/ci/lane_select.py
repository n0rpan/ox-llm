#!/usr/bin/env python3
"""Select changed Rust packages and their local reverse dependencies, without building."""
import argparse
import fnmatch
import json
import os
from pathlib import PurePosixPath
import posixpath
import re
import subprocess
import tomllib

WORKSPACES = ('oxa', 'engine', 'oxc-rs')
PORTABLE_ENGINE = {'server', 'gguf', 'tokenizer', 'sched', 'chat', 'oxe-cli'}
LANE_CONTRACT = {
    '.github/workflows/lane.yml', 'tools/ci/lane_select.py', 'tools/ci/lane_run.py',
    'tools/ci/lane_queue.py', 'tools/ci/lane-nextest.toml', 'tools/ci/rust_cache_key.py',
    'tools/ci/tests/test_lane_select.py', 'tools/ci/tests/test_lane_run.py',
    'tools/ci/tests/test_lane_queue.py',
}
NAME = re.compile(r'[A-Za-z0-9][A-Za-z0-9_-]*\Z')


def git(*args):
    return subprocess.check_output(['git', *args])


def member_matches(relative, pattern):
    # Cargo's single-component globs must not consume nested fixture manifests.
    wanted, actual = PurePosixPath(pattern).parts, PurePosixPath(relative).parts
    if '**' in wanted:
        raise ValueError('recursive workspace globs require explicit lane support')
    return len(wanted) == len(actual) and all(fnmatch.fnmatchcase(a, b) for a, b in zip(actual, wanted))


def manifests(ref):
    """Read tracked blobs only; manifests must never redirect reads through symlinks."""
    paths = git('ls-tree', '-r', '--name-only', '-z', ref).decode().split('\0')
    files = {p for p in paths if p.endswith('/Cargo.toml') or p == 'Cargo.toml'}
    packages = {}
    for workspace in WORKSPACES:
        manifest = f'{workspace}/Cargo.toml'
        if manifest not in files:
            continue
        root = tomllib.loads(git('show', f'{ref}:{manifest}').decode())
        members = root.get('workspace', {}).get('members', ['.'])
        exclusions = root.get('workspace', {}).get('exclude', [])
        for path in sorted(files):
            directory = str(PurePosixPath(path).parent)
            relative = posixpath.relpath(directory, workspace)
            if relative.startswith('../') or relative == '..':
                continue
            if not any(member_matches(relative, m) for m in members):
                continue
            if any(member_matches(relative, m) for m in exclusions):
                continue
            if path.startswith('engine/npu/') and not path.startswith('engine/npu/oxn/common/'):
                raise ValueError('lane metadata must not read device NPU material')
            data = tomllib.loads(git('show', f'{ref}:{path}').decode())
            name = data.get('package', {}).get('name')
            if not name or not NAME.fullmatch(name):
                raise ValueError(f'invalid package name in {path}')
            packages[directory] = {
                'name': name, 'workspace': workspace, 'manifest': data,
                'shared': root.get('workspace', {}).get('dependencies', {}),
            }
    return packages


def dependency_edges(packages):
    """Normal/build/dev consumers are affected, including renamed workspace dependencies."""
    edges = set()
    by_name = {p['name']: directory for directory, p in packages.items()}
    for directory, package in packages.items():
        tables = [package['manifest']]
        tables.extend(package['manifest'].get('target', {}).values())
        for table in tables:
            for kind in ('dependencies', 'build-dependencies', 'dev-dependencies'):
                for alias, value in table.get(kind, {}).items():
                    if not isinstance(value, dict):
                        continue
                    dependency = dict(value)
                    base = directory
                    if dependency.get('workspace'):
                        shared = package['shared'].get(alias)
                        if not isinstance(shared, dict):
                            continue
                        dependency = {**shared, **dependency}
                        base = package['workspace']
                    if 'path' not in dependency:
                        continue
                    target = posixpath.normpath(posixpath.join(base, dependency['path']))
                    if target in packages:
                        edges.add((packages[target]['name'], package['name']))
    # The parity suite executes these binaries rather than linking their crates.
    for fixture in ('oxa-cli', 'oxa-fake'):
        if fixture in by_name and 'oxa-parity' in by_name:
            edges.add((fixture, 'oxa-parity'))
    return edges


def select(current, previous, paths):
    packages = {p['name'] for p in current.values()}
    affected = set()
    reasons = []
    for path in paths:
        if path in LANE_CONTRACT or path in ('rust-toolchain', 'rust-toolchain.toml') or path.startswith('.cargo/'):
            affected.update(packages)
            reasons.append(f'shared lane/build contract: {path}')
            continue
        for workspace in WORKSPACES:
            if path in (f'{workspace}/Cargo.toml', f'{workspace}/Cargo.lock') or path.startswith(f'{workspace}/.cargo/'):
                affected.update(p['name'] for p in current.values() if p['workspace'] == workspace)
                reasons.append(f'shared workspace configuration: {path}')
        for mapping in (previous, current):
            owners = [(directory, p) for directory, p in mapping.items()
                      if path == directory or path.startswith(directory + '/')]
            if owners:
                _, owner = max(owners, key=lambda entry: len(entry[0]))
                affected.add(owner['name'])
        if path.startswith('engine/') and PurePosixPath(path).suffix not in ('.md', '.rst') and not any(
                path.startswith(directory + '/') for directory in (*current, *previous)):
            affected.update(p['name'] for p in current.values() if p['workspace'] == 'engine')
            reasons.append(f'shared engine input: {path}')
    edges = dependency_edges(previous) | dependency_edges(current)
    while True:
        expanded = affected | {consumer for dependency, consumer in edges if dependency in affected}
        if expanded == affected:
            break
        affected = expanded
    affected &= packages
    matrix = []
    deferred = []
    for workspace in WORKSPACES:
        selected = sorted(p['name'] for p in current.values()
                          if p['workspace'] == workspace and p['name'] in affected)
        if workspace == 'engine':
            deferred.extend(p for p in selected if p not in PORTABLE_ENGINE)
            selected = [p for p in selected if p in PORTABLE_ENGINE]
        if not selected:
            continue
        runners = ['ubuntu-24.04'] if workspace == 'engine' else ['ubuntu-24.04', 'ubuntu-24.04-arm']
        for runner in runners:
            matrix.append({'workspace': workspace, 'runner': runner, 'packages': selected})
    return {'include': matrix}, sorted(deferred), reasons


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--base', default='origin/main')
    args = parser.parse_args()
    base = git('merge-base', args.base, 'HEAD').decode().strip()
    # Treat renames as delete+add so both the old and new owner are considered.
    paths = git('diff', '--no-renames', '--name-only', '-z', base, 'HEAD').decode().split('\0')[:-1]
    matrix, deferred, reasons = select(manifests('HEAD'), manifests(base), paths)
    workspaces = list(dict.fromkeys(row['workspace'] for row in matrix['include']))
    package_map = {row['workspace']: row['packages'] for row in matrix['include']}
    result = {'matrix': matrix, 'has_tests': bool(matrix['include']), 'deferred_engine': deferred,
              'base': base, 'reasons': reasons}
    if output := os.environ.get('GITHUB_OUTPUT'):
        with open(output, 'a') as file:
            file.write(f'matrix={json.dumps(matrix, separators=(",", ":"))}\n')
            file.write(f'has_tests={str(result["has_tests"]).lower()}\n')
            file.write(f'workspaces={json.dumps(workspaces)}\n')
            file.write(f'packages={json.dumps(package_map)}\n')
    if summary := os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(summary, 'a') as file:
            file.write(f'## Affected crates\n\nDiff: `{base}...HEAD` (local reverse dependencies included).\n\n')
            for entry in matrix['include']:
                file.write(f'- {entry["workspace"]} / {entry["runner"]}: {", ".join(entry["packages"])}\n')
            if not matrix['include']:
                file.write('No affected portable Rust crates; CI policy checks still run.\n')
            if deferred:
                file.write(f'\nNative engine qualification stays on the existing Oryon runner: {", ".join(deferred)}.\n')
    print(json.dumps(result))


if __name__ == '__main__':
    main()
