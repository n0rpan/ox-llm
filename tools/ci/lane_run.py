#!/usr/bin/env python3
"""CI-only affected-crate tests with release features and a cleared runtime environment."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

ROOTS = {'oxa': ('oxa-cli', []), 'engine': ('server', []),
         'oxc-rs': ('oxc', ['native-controller'])}
NAME = re.compile(r'[A-Za-z0-9][A-Za-z0-9_-]*\Z')
FEATURE = re.compile(r'[A-Za-z0-9_][A-Za-z0-9_.+-]*\Z')


def release_features(workspace, packages, target):
    root, features = ROOTS[workspace]
    command = ['cargo', 'tree', '--locked', '-p', root, '--edges', 'normal,build',
               '--target', target, '--prefix', 'none', '--no-dedupe', '--format', '{p}|{f}']
    if features:
        command += ['--features', ','.join(features)]
    output = subprocess.check_output(command, cwd=workspace, text=True)
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--locked', '--format-version', '1', '--no-deps'], cwd=workspace, text=True))
    defaults = {p['name'] for p in metadata['packages'] if 'default' in p.get('features', {})}
    return parse_features(output, packages, root, defaults)


def parse_features(output, packages, root, defaults=()):
    found = {}
    for line in output.splitlines():
        if line.strip() in ('[build-dependencies]', '[dev-dependencies]'):
            continue
        identity, separator, enabled = line.rpartition('|')
        if not separator:
            raise ValueError('unexpected Cargo feature-tree output')
        name = identity.split()[0]
        if not NAME.fullmatch(name):
            raise ValueError('invalid package in Cargo feature tree')
        values = [value for value in enabled.strip().split(',') if value]
        if any(not FEATURE.fullmatch(value) for value in values):
            raise ValueError('invalid Cargo feature name')
        found.setdefault(name, set()).update(values)
    if root not in found:
        raise ValueError('release binary root is missing from feature tree')
    return [f'{package}/{feature}' for package in sorted(packages)
            for feature in sorted(found.get(package, {'default'} if package in defaults else set()))]


def cargo_arguments(packages, features):
    if not packages or any(not NAME.fullmatch(package) for package in packages):
        raise ValueError('expected a nonempty package-name list')
    arguments = ['--no-default-features'] + [argument for package in sorted(set(packages)) for argument in ('-p', package)]
    if features:
        arguments += ['--features', ','.join(features)]
    return arguments


def runtime_environment(root, home, temporary, workspace, fixtures):
    # Compiler/cache credentials never reach tests. These are the same explicit
    # synthetic HOME/XDG and mandatory-bwrap boundaries as the full workspace CI.
    allowed = ('PATH', 'CARGO_HOME', 'RUSTUP_HOME', 'RUSTUP_TOOLCHAIN')
    env = {name: os.environ[name] for name in allowed if name in os.environ}
    env.update(HOME=str(home), TMPDIR=str(temporary), CARGO_BUILD_JOBS='3',
               CARGO_INCREMENTAL='0', CARGO_NET_OFFLINE='true', OXA_REQUIRE_BWRAP='1',
               CARGO_TARGET_DIR=str(root / workspace / 'target'))
    for name, child in [('XDG_CONFIG_HOME', 'config'), ('XDG_DATA_HOME', 'data'),
                        ('XDG_STATE_HOME', 'state'), ('XDG_CACHE_HOME', 'cache'),
                        ('XDG_RUNTIME_DIR', 'runtime')]:
        env[name] = str(home / child)
    if fixtures:
        env['OXA_BIN'] = str(root / 'oxa/target/ci/oxa')
        env['OXA_FAKE_BIN'] = str(root / 'oxa/target/ci/oxa-fake')
    return env


def run(command, **kwargs):
    print('+ ' + ' '.join(command), flush=True)
    return subprocess.run(command, check=True, **kwargs)


def main():
    if os.environ.get('GITHUB_ACTIONS') != 'true':
        raise SystemExit('lane builds and tests run only in GitHub CI')
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--workspace', choices=ROOTS, required=True)
    parser.add_argument('--packages-json', required=True)
    args = parser.parse_args()
    root = Path.cwd()
    workspace = args.workspace
    packages = json.loads(args.packages_json)
    if not isinstance(packages, list) or any(not isinstance(p, str) for p in packages):
        parser.error('packages must be a JSON list of names')
    cargo_arguments(packages, [])
    host = subprocess.check_output(['rustc', '-vV'], text=True)
    target = next(line.removeprefix('host: ') for line in host.splitlines() if line.startswith('host: '))
    features = release_features(workspace, packages, target)
    arguments = cargo_arguments(packages, features)
    profile = 'ci' if workspace == 'oxa' else 'test'
    fixtures = workspace == 'oxa' and bool(set(packages) & {'oxa-cli', 'oxa-parity'})
    artifact = Path(os.environ['LANE_ARTIFACTS_DIR']) / workspace
    artifact.mkdir(parents=True, exist_ok=True)
    archive = artifact / 'tests.tar.zst'
    inventory = artifact / 'tests.json'
    if fixtures:
        run(['cargo', 'build', '--locked', '--profile', profile, '-p', 'oxa-cli', '-p', 'oxa-fake'], cwd=workspace)
    archive_profile = 'ci' if fixtures else 'default'
    run(['cargo', 'nextest', 'archive', '--locked', '--profile', archive_profile,
         '--cargo-profile', profile, *arguments, '--archive-file', str(archive)], cwd=workspace)
    with inventory.open('w') as output:
        run(['cargo', 'nextest', 'list', '--archive-file', str(archive), '--message-format', 'json'],
            cwd=workspace, stdout=output)
    # Only small synthetic fixtures use the hosted runner's disk-backed /tmp,
    # outside the account home required by the unchanged key-isolation guard.
    # All targets, caches and archives stay under the checkout or CI cache, never /tmp.
    with tempfile.TemporaryDirectory(prefix='lane-test-', dir='/tmp') as temporary:
        temporary = Path(temporary)
        home = temporary / 'home'
        scratch = temporary / 'tmp'
        home.mkdir(mode=0o700)
        scratch.mkdir(mode=0o700)
        env = runtime_environment(root, home, scratch, workspace, fixtures)
        nextest = str(Path(os.environ['LANE_TOOLS_DIR']) / 'cargo-nextest')
        if workspace == 'engine':
            libtest = artifact / 'libtest.txt'
            with libtest.open('w') as output:
                run(['cargo', 'test', '--locked', '--profile', profile, *arguments,
                     '--', '--list', '--format', 'terse'], cwd=workspace, env=env, stdout=output)
            run(['python3', '-I', str(root / 'tools/ci/engine_test_count.py'), str(libtest), str(inventory)])
        report = root / workspace / 'target/nextest/lane/junit.xml'
        report.unlink(missing_ok=True)
        result = subprocess.run([nextest, 'nextest', 'run', '--profile', 'lane',
            '--tool-config-file', 'lane:' + str(root / 'tools/ci/lane-nextest.toml'),
            '--archive-file', str(archive), '--workspace-remap', str(root / workspace),
            '--extract-to', str(root / workspace), '--extract-overwrite',
            '--no-tests', 'fail', '--test-threads', '4',
            '--success-output', 'immediate', '--failure-output', 'immediate-final'],
            cwd=workspace, env=env)
        # The existing guard rejects runtime skips, missing/duplicate cases and failures.
        if workspace != 'engine':
            run(['python3', '-I', str(root / 'tools/ci/check_oxa_report.py'),
                 '--expected', str(inventory), str(report)])
        if result.returncode:
            raise SystemExit(result.returncode)
        doctests = subprocess.run(
            ['cargo', 'test', '--locked', '--profile', profile, *arguments, '--doc', '--no-fail-fast'],
            cwd=workspace, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        print(doctests.stdout, end='', flush=True)
        (artifact / 'doctests.log').write_text(doctests.stdout)
        if any(line.startswith('skipped: ') for line in doctests.stdout.splitlines()):
            raise SystemExit('doctest skipped at runtime')
        if doctests.returncode:
            raise SystemExit(doctests.returncode)


if __name__ == '__main__':
    main()
