#!/usr/bin/env python3
"""Rules of the desktop workspace (oxa/desk/) that the compiler does not check.

    desk_contract.py            lint the tree; prints every problem and exits 1 when there is one
    desk_contract.py version    print the one version oxa and the desktop share

The lint keeps the desktop one product with oxa (build plan, sections 1.1 and 2.1):

- one version, the same lints and the same profiles as oxa/Cargo.toml, and for every crate that
  both lock files hold, a version in common;
- each desk crate depends only on what its layer may see: no GPUI in oxa-desk-core, no oxa crate in
  the views (the binary's `main.rs` is the oxa shim and the one exception);
- no path under oxa/desk/ names macos or darwin (such a path selects the 90-minute macOS build).
"""
from pathlib import Path
import re
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[2]
OXA = ROOT / 'oxa'
DESK = OXA / 'desk'

# What each crate may depend on (build plan 1.1). A new entry is an architect decision.
SHIM = {'oxa-cli', 'oxa-config', 'oxa-proto', 'oxa-web', 'oxc-local'}
ALLOWED = {
    'oxa-desk-core': {'oxa-proto', 'oxa-config', 'oxa-session', 'oxa-core', 'oxa-interactive', 'oxa-tui',
                      'oxa-rpc', 'oxa-http', 'oxa-cli', 'notify', 'libc', 'serde', 'serde_json'},
    'oxa-desk': {'oxa-desk-core', 'gpui', 'gpui-component'} | SHIM,
    'oxa-desk-probe': {'oxa-desk-core', 'oxa-session', 'oxa-fake'},
}
# Outside `main.rs`, the window crate reaches oxa through oxa-desk-core only, and starts no process
# and writes no file of its own.
VIEW_FORBIDDEN = tuple(name.replace('-', '_') + '::' for name in sorted(SHIM | {'oxa-core', 'oxa-session'})) + (
    'std::process::Command', 'std::fs::write')


def toml(path):
    return tomllib.loads(path.read_text())


def lock_versions(path):
    versions = {}
    for package in toml(path).get('package', []):
        versions.setdefault(package['name'], set()).add(package['version'])
    return versions


def shared_settings(oxa, desk):
    found = []
    for label, pick in (('[workspace.package]', lambda m: m['workspace'].get('package')),
                        ('[workspace.lints]', lambda m: m['workspace'].get('lints')),
                        ('[profile]', lambda m: m.get('profile'))):
        if pick(oxa) != pick(desk):
            found.append(f'oxa/desk/Cargo.toml: {label} differs from oxa/Cargo.toml (one product: copy it)')
    return found


def lock_problems(oxa, desk):
    """A crate in both lock files needs a version in common, so shared oxa code is the same build."""
    return [f'oxa/desk/Cargo.lock: {name} {", ".join(sorted(desk[name]))} but oxa/Cargo.lock has '
            f'{", ".join(sorted(oxa[name]))}' for name in sorted(oxa.keys() & desk.keys())
            if not oxa[name] & desk[name]]


def dependency_problems(crate, manifest):
    names = set()
    for table in [manifest] + list(manifest.get('target', {}).values()):
        for kind in ('dependencies', 'dev-dependencies', 'build-dependencies'):
            names |= set(table.get(kind, {}))
    return [f'oxa/desk/{crate}/Cargo.toml: {name} is not a dependency this crate may have'
            for name in sorted(names - ALLOWED[crate])]


def view_problems(path, text):
    return [f'{path}:{number}: {word} (the window reaches oxa through oxa-desk-core)'
            for number, line in enumerate(text.splitlines(), 1)
            for word in VIEW_FORBIDDEN if word in line]


def name_problems(paths):
    return [f'{path}: a desk path may not name macos or darwin (use mac)' for path in paths
            if 'macos' in path.lower() or 'darwin' in path.lower()]


def lint():
    found = []
    oxa, desk = toml(OXA / 'Cargo.toml'), toml(DESK / 'Cargo.toml')
    found += shared_settings(oxa, desk)
    members = set(desk['workspace']['members'])
    if members != set(ALLOWED):
        found.append(f'oxa/desk/Cargo.toml: members {sorted(members)}, expected {sorted(ALLOWED)}')
    if (DESK / 'Cargo.lock').is_file():
        found += lock_problems(lock_versions(OXA / 'Cargo.lock'), lock_versions(DESK / 'Cargo.lock'))
    else:
        found.append('oxa/desk/Cargo.lock is missing (the `desk check` job uploads one as `desk-cargo-lock`)')
    for crate in sorted(members & set(ALLOWED)):
        found += dependency_problems(crate, toml(DESK / crate / 'Cargo.toml'))
    files = [path for path in sorted(DESK.rglob('*'))
             if 'target' not in path.relative_to(DESK).parts[:1] and path.is_file()]
    found += name_problems([str(path.relative_to(ROOT)) for path in files])
    for path in files:
        relative = path.relative_to(DESK)
        if relative.parts[:2] == ('oxa-desk', 'src') and path.suffix == '.rs' and relative.parts[2:] != ('main.rs',):
            found += view_problems(path.relative_to(ROOT), path.read_text(errors='replace'))
    print('\n'.join(found) or f'desk contract ok ({len(files)} files)')
    return 1 if found else 0


def main(argv):
    if argv == ['version']:
        print(toml(OXA / 'Cargo.toml')['workspace']['package']['version'])
        return 0
    if argv:
        print(__doc__, file=sys.stderr)
        return 2
    return lint()


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
