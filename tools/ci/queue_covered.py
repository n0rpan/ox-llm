#!/usr/bin/env python3
"""Main-push dedupe: say when a green merge-queue run already covered this push (#2049).

The merge queue pushes the commit it tested to main, so the `merge_group` head SHA and the
main push SHA are the same commit: same tree, same Cargo inputs, same toolchain. A `checks`
run for that push would repeat work that already passed on that exact commit, so the workflow
may skip its jobs for it.

Two conditions, both required:

- the workflow's merge-queue run at this commit completed and succeeded (a failed, cancelled
  or still-running queue run never dedupes), and
- the push changed no dependency input (a Cargo manifest, a lockfile, `rust-toolchain*`,
  `.cargo/` configuration, the contract requirements). Main is what seeds the `<prefix>main`
  caches, and a queue run cannot write those. When a dependency key is new, the main run must
  happen to save it; skipping would make every later run rebuild dependencies from scratch.

Any doubt answers "not covered", so the full run happens: no git history for the pushed
range, an API failure, no merge-queue run at the commit, or an input we cannot classify.
Writes `covered=true|false` to GITHUB_OUTPUT. The caller keeps its unconditional floor (the
classifier and the security scans) and skips only the jobs this answer covers.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import urllib.error
import urllib.request

# Names whose content is a dependency cache key input (see rust_cache_key.py), plus the
# contract's pinned requirements, which key the x86_64 stub job's pip cache.
DEPENDENCY_NAMES = ('Cargo.toml', 'Cargo.lock', 'rust-toolchain', 'rust-toolchain.toml',
                    'requirements.txt')


def dependency_inputs(paths):
    """The changed paths that change a dependency cache key."""
    return [path for path in paths
            if Path(path).name in DEPENDENCY_NAMES or '.cargo' in Path(path).parts[:-1]]


def covered(runs, sha, changed):
    """(covered, reason) for the pushed commit, the workflow's queue runs and the push's files.

    `runs` are the workflow's merge-queue runs (the API's records); `changed` is the push's
    changed paths, or None when they are unknown.
    """
    if changed is None:
        return False, 'the files this push changed are unknown'
    at_commit = [run for run in runs
                 if run.get('head_sha') == sha and run.get('status') == 'completed']
    if not at_commit:
        return False, 'no completed merge-queue run at this commit'
    latest = max(at_commit, key=lambda run: (run.get('created_at') or '',
                                             run.get('run_attempt') or 1))
    if latest.get('conclusion') != 'success':
        return False, f'the merge-queue run at this commit is {latest.get("conclusion")}'
    inputs = dependency_inputs(changed)
    if inputs:
        return False, 'dependency inputs changed: ' + ', '.join(inputs)
    return True, f'merge-queue run {latest.get("html_url")} passed at this commit'


def github_json(url, token):
    request = urllib.request.Request(url, headers={
        'Accept': 'application/vnd.github+json',
        'Authorization': f'Bearer {token}',
        'X-GitHub-Api-Version': '2022-11-28',
    })
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


def queue_runs(repo, workflow, sha, token):
    """The workflow's recent merge-queue runs at `sha`."""
    base = f'https://api.github.com/repos/{repo}/actions/workflows/{workflow}/runs'
    runs = github_json(f'{base}?event=merge_group&head_sha={sha}&per_page=100',
                       token).get('workflow_runs') or []
    if not runs:
        # `head_sha` filtering is a convenience of the list endpoint; the run that matters is the
        # newest merge-queue run, so fall back to the recent list and filter on the SHA here.
        runs = github_json(f'{base}?event=merge_group&per_page=100',
                           token).get('workflow_runs') or []
    return runs


def changed_paths(base, sha):
    """The files the push changed, or None when the commit range is not available."""
    if not base or set(base) == {'0'}:
        return None
    try:
        listed = subprocess.run(['git', 'diff', '--name-only', '-z', base, sha],
                                check=True, capture_output=True)
    except (OSError, subprocess.CalledProcessError):
        return None
    return [path for path in listed.stdout.decode().split('\0')[:-1] if path]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', default=os.environ.get('GITHUB_REPOSITORY', ''))
    parser.add_argument('--sha', default=os.environ.get('GITHUB_SHA', ''))
    parser.add_argument('--base', default=os.environ.get('PUSH_BEFORE', ''))
    parser.add_argument('--workflow', default='checks.yml')
    args = parser.parse_args()
    if not args.repo or not args.sha:
        parser.error('--repo and --sha (or GITHUB_REPOSITORY and GITHUB_SHA) are required')

    changed = changed_paths(args.base, args.sha)
    try:
        runs = queue_runs(args.repo, args.workflow, args.sha, os.environ['GH_TOKEN'])
    except (KeyError, OSError, urllib.error.URLError, json.JSONDecodeError) as error:
        runs = []
        print(f'::warning::merge-queue run lookup failed: {error}', file=sys.stderr)
    skip, reason = covered(runs, args.sha, changed)
    print(f'merge-queue coverage: {skip} ({reason})')
    summary = os.environ.get('GITHUB_STEP_SUMMARY')
    if summary:
        with open(summary, 'a') as output:
            output.write(f'Main-push dedupe: skip checks jobs = **{str(skip).lower()}** '
                         f'({reason})\n')
    output = os.environ.get('GITHUB_OUTPUT')
    if output:
        with open(output, 'a') as stream:
            stream.write(f'covered={str(skip).lower()}\n')


if __name__ == '__main__':
    main()
