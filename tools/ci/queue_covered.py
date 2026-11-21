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
  `.cargo/` configuration, the contract requirements, the toolchain setup script and the workflows
  that pin the compiler and flags). Main is what seeds the `<prefix>main` caches, and a queue run
  cannot write those. When a dependency key is new, the main run must happen to save it; skipping
  would make every later run rebuild dependencies from scratch.

Any doubt answers "not covered", so the full run happens: no git history for the pushed
range, an API failure, no merge-queue run at the commit, or an input we cannot classify.
Writes `covered=true|false` to GITHUB_OUTPUT. The caller keeps its unconditional floor (the
classifier and the security scans) and skips only the jobs this answer covers.

The queue classifies its own change, so it skips a path-selected job (the desktop's `desk`) that
main always runs. `--job NAME` adds `NAME_covered=true|false`: true only when the push is covered
and that queue run really ran the job `NAME` (its jobs are listed as `NAME / ...`) and all of them
passed. The caller dedupes such a job on this answer, not on `covered`.
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
# The cache key also covers the compiler version and the compilation flags, and these two define
# them: the toolchain setup script and the workflows that pin `RUST_VERSION` and the `CARGO_*`
# environment. Editing either makes a new key exactly as a lockfile edit does.
TOOLCHAIN_PATHS = ('engine/tools/ci/setup_toolchain.sh',)


def dependency_inputs(paths):
    """The changed paths that change a dependency cache key."""
    inputs = []
    for path in paths:
        parts = Path(path).parts
        if (parts and parts[-1] in DEPENDENCY_NAMES
                or '.cargo' in parts[:-1]
                or parts[:2] == ('.github', 'workflows')
                or '/'.join(parts) in TOOLCHAIN_PATHS):
            inputs.append(path)
    return inputs


def latest_run(runs, sha):
    """The newest completed merge-queue run at the commit, or None."""
    at_commit = [run for run in runs
                 if run.get('head_sha') == sha and run.get('status') == 'completed']
    return max(at_commit, default=None, key=lambda run: (run.get('created_at') or '',
                                                         run.get('run_attempt') or 1))


def covered(runs, sha, changed):
    """(covered, reason) for the pushed commit, the workflow's queue runs and the push's files.

    `runs` are the workflow's merge-queue runs (the API's records); `changed` is the push's
    changed paths, or None when they are unknown.
    """
    if changed is None:
        return False, 'the files this push changed are unknown'
    latest = latest_run(runs, sha)
    if latest is None:
        return False, 'no completed merge-queue run at this commit'
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


def job_ran(jobs, name):
    """Whether the run's jobs include the called workflow `name`, every one of them green.

    A job the queue skipped is listed as one skipped job called `name`, or not at all: both
    answer False, so main runs it.
    """
    mine = [job for job in jobs if (job.get('name') or '').startswith(name + ' / ')]
    return bool(mine) and all(job.get('conclusion') == 'success' for job in mine)


def run_jobs(repo, run, token):
    """Every job of the run's attempt (the list is paged)."""
    base = (f'https://api.github.com/repos/{repo}/actions/runs/{int(run["id"])}'
            f'/attempts/{int(run.get("run_attempt") or 1)}/jobs?per_page=100')
    jobs = []
    for page in range(1, 11):
        listed = github_json(f'{base}&page={page}', token).get('jobs') or []
        jobs += listed
        if len(listed) < 100:
            return jobs
    raise ValueError('more than 1000 jobs in the merge-queue run')


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
    parser.add_argument('--job', action='append', default=[], metavar='NAME',
                        help='also write NAME_covered: the queue run ran this called workflow')
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
    answers = {'covered': skip}
    if args.job:
        jobs = []
        if skip:
            try:
                jobs = run_jobs(args.repo, latest_run(runs, args.sha), os.environ['GH_TOKEN'])
            except (KeyError, TypeError, ValueError, OSError) as error:
                print(f'::warning::merge-queue job lookup failed: {error}', file=sys.stderr)
        for name in args.job:
            answers[f'{name}_covered'] = skip and job_ran(jobs, name)
            print(f'merge-queue coverage of {name}: {answers[f"{name}_covered"]}')
    output = os.environ.get('GITHUB_OUTPUT')
    if output:
        with open(output, 'a') as stream:
            for key, value in answers.items():
                stream.write(f'{key}={str(value).lower()}\n')


if __name__ == '__main__':
    main()
