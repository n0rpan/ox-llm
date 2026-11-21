#!/usr/bin/env python3
"""Rules of the desktop workspace (oxa/desk/) that the compiler does not check.

    desk_contract.py            lint the tree; prints every problem and exits 1 when there is one
    desk_contract.py version    print the one version oxa and the desktop share
    desk_contract.py timing JOB append the job's duration and runner wait to the job summary

The lint keeps the desktop one product with oxa (build plan, sections 1.1 and 2.1):

- one version, the same lints and the same profiles as oxa/Cargo.toml, and for every crate that
  both lock files hold, a version in common;
- each desk crate depends only on what its layer may see: no GPUI in oxa-desk-core, no oxa crate in
  the views (the binary's `main.rs` is the oxa shim and the one exception);
- no path under oxa/desk/ names macos or darwin (such a path selects the 90-minute macOS build).
"""
import datetime
import json
import os
from pathlib import Path
import re
import sys
import tomllib
import urllib.request

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


def timing(job):
    """Best effort: a missing answer from the API is written down, it never fails the job."""
    def when(text):
        return datetime.datetime.fromisoformat(text.replace('Z', '+00:00'))

    now = datetime.datetime.now(datetime.timezone.utc)
    try:
        env = os.environ
        url = (f"{env['GITHUB_API_URL']}/repos/{env['GITHUB_REPOSITORY']}/actions/runs/{env['GITHUB_RUN_ID']}"
               f"/attempts/{env.get('GITHUB_RUN_ATTEMPT', '1')}/jobs?per_page=100")
        if not url.startswith('https://'):
            raise ValueError('unexpected API address')
        headers = {'Accept': 'application/vnd.github+json'}
        if env.get('GH_TOKEN'):  # empty on a PR from a fork: the workflow gives that run no token
            headers['Authorization'] = f"Bearer {env['GH_TOKEN']}"
        request = urllib.request.Request(url, headers=headers)
        with urllib.request.urlopen(request, timeout=20) as response:
            jobs = json.load(response)['jobs']
        mine = [entry for entry in jobs if entry.get('runner_name') == env['RUNNER_NAME']
                and entry.get('status') == 'in_progress' and entry['name'].endswith(job)]
        if len(mine) != 1:
            raise ValueError(f'{len(mine)} running jobs named {job!r} on this runner')
        created, started = when(mine[0]['created_at']), when(mine[0]['started_at'])
        line = (f'{job}: {(now - started).total_seconds():.0f} s so far, '
                f'after {(started - created).total_seconds():.0f} s waiting for a runner')
    except Exception as error:  # noqa: BLE001 - a summary line, never a reason to fail the job
        line = f'{job}: duration and runner wait unknown ({type(error).__name__}: {error})'
    print(line)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as summary:
            summary.write(line + '\n')
    return 0


def main(argv):
    if argv == ['version']:
        print(toml(OXA / 'Cargo.toml')['workspace']['package']['version'])
        return 0
    if len(argv) == 2 and argv[0] == 'timing':
        return timing(argv[1])
    if argv:
        print(__doc__, file=sys.stderr)
        return 2
    return lint()


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
