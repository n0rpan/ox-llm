"""Run an explicit review slice with no implicit Python fallback in native mode."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import unittest

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
OVERRIDES = {
    'OXC_COMMAND': (),
    'OXC_DAEMON_COMMAND': ('serve',),
    'OXC_BOOTSTRAP_COMMAND': ('__bootstrap',),
    'OXC_INSTALL_COMMAND': ('__install',),
    'OXC_EXPORT_RECEIPT_COMMAND': ('__export_receipt',),
    'OXC_SCRUB_RECEIPTS_COMMAND': ('__scrub_receipts',),
    'OXC_STUDIO_COMMAND': ('studio',),
    'OXC_CLOUD_INSTALL_COMMAND': ('__cloud_install',),
}


def cases(suite):
    for item in suite:
        if isinstance(item, unittest.TestSuite):
            yield from cases(item)
        else:
            yield item


def inventory(manifest):
    """Every currently captured method must belong to exactly one review slice."""
    result, owners = {}, {}
    loader = unittest.TestLoader()
    for name, selectors in manifest['slices'].items():
        tests = list(cases(loader.loadTestsFromNames(selectors)))
        if not tests:
            raise ValueError('Empty review slice: ' + name)
        for test in tests:
            if test.id() in owners:
                raise ValueError('Duplicate test in review slices: ' + test.id())
            owners[test.id()] = name
        result[name] = tests
    discovered = {test.id() for test in cases(loader.discover(str(HERE)))}
    if loader.errors:
        raise ValueError('Cannot load conformance contracts:\n' + '\n'.join(loader.errors))
    if set(owners) != discovered:
        missing = sorted(discovered - set(owners))
        extra = sorted(set(owners) - discovered)
        raise ValueError(f'Review inventory mismatch; unassigned={missing}, absent={extra}')
    for profile, slices in manifest['profiles'].items():
        if not slices or len(set(slices)) != len(slices) or set(slices) - set(result):
            raise ValueError('Invalid review profile: ' + profile)
    if set(manifest['blocked']) & (set(result) | set(manifest['profiles'])):
        raise ValueError('A blocked contract cannot also be runnable')
    return result


def main():
    manifest = json.loads(HERE.joinpath('slices.json').read_text())
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('suite', choices=sorted(set(manifest['slices']) | set(manifest['profiles']) | set(manifest['blocked'])))
    target = parser.add_mutually_exclusive_group()
    target.add_argument('--native', type=Path, metavar='ABSOLUTE_BINARY', help='set all seven native command overrides')
    target.add_argument('--python', action='store_true', help='run the Python reference (the default)')
    parser.add_argument('--list', action='store_true', help='audit and show the selected test methods without executing them')
    args = parser.parse_args()
    if args.suite in manifest['blocked']:
        parser.error(manifest['blocked'][args.suite])
    try:
        available = inventory(manifest)
    except ValueError as error:
        parser.error(str(error))
    selected = manifest['profiles'].get(args.suite, [args.suite])
    tests = [test for name in selected for test in available[name]]
    if args.list:
        for test in tests:
            print(test.id())
        print(f'{len(tests)} methods; review inventory covers all {sum(map(len, available.values()))} methods exactly once')
        return 0
    # A caller's stale overrides must not silently make a Python run native or
    # leave helpers on Python during a native run. Fixtures remain isolated.
    for key in OVERRIDES:
        os.environ.pop(key, None)
    if args.native:
        binary = args.native
        if not binary.is_absolute() or not binary.is_file() or not os.access(binary, os.X_OK):
            parser.error('--native requires an absolute path to an executable file')
        binary = binary.resolve()
        for key, words in OVERRIDES.items():
            os.environ[key] = shlex.join([str(binary), *words])
        print('native executable sha256 ' + hashlib.sha256(binary.read_bytes()).hexdigest(), flush=True)
    base = subprocess.run(['git', 'merge-base', 'HEAD', 'origin/main'], cwd=ROOT,
                          text=True, capture_output=True, check=True).stdout.strip()
    print(f'reference main {base}; suite {args.suite}; {len(tests)} methods', flush=True)
    outcome = unittest.TextTestRunner(verbosity=2).run(unittest.TestSuite(tests))
    return 0 if outcome.wasSuccessful() else 1


if __name__ == '__main__':
    raise SystemExit(main())
