#!/usr/bin/env python3
"""Record the runner queue delay from GitHub's job timestamps, once per started job.

`--line` writes one line instead of the table, with the job's duration so far, and is best effort:
it never fails the job, and it works without a token (a pull request from a fork gets none).
"""
import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import urllib.request


def when(text):
    return datetime.fromisoformat(text.replace('Z', '+00:00'))


def queue_seconds(job):
    seconds = (when(job['started_at']) - when(job['created_at'])).total_seconds()
    if seconds < 0:
        raise ValueError('job start precedes queue creation')
    return seconds


def find_job(jobs, name):
    """The run's job `name`. A job of a called workflow is listed as `<caller> / <name>`."""
    matches = ([job for job in jobs if job['name'] == name]
               or [job for job in jobs if job['name'].endswith(' / ' + name)])
    if len(matches) != 1:
        raise ValueError('current job is missing or ambiguous in the run')
    return matches[0]


def line(name, job, now):
    return (f'{name}: {(now - when(job["started_at"])).total_seconds():.0f} s so far, '
            f'after {queue_seconds(job):.0f} s waiting for a runner')


def current_job(name, token):
    """(run, attempt, job) of this workflow run's job `name`."""
    repository = os.environ['GITHUB_REPOSITORY']
    run = os.environ['GITHUB_RUN_ID']
    attempt = os.environ['GITHUB_RUN_ATTEMPT']
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repository) or not run.isdigit() or not attempt.isdigit():
        raise ValueError('invalid workflow identity')
    url = f'https://api.github.com/repos/{repository}/actions/runs/{run}/attempts/{attempt}/jobs?per_page=100'
    headers = {'Accept': 'application/vnd.github+json', 'X-GitHub-Api-Version': '2022-11-28'}
    if token:
        headers['Authorization'] = 'Bearer ' + token
    with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=30) as response:
        jobs = json.load(response)['jobs']
    # The workflow has fewer than 100 jobs; this is the exact current attempt's job,
    # not run.created_at (which would include dependency execution or prior attempts).
    return run, attempt, find_job(jobs, name)


def best_effort_line(name):
    try:
        text = line(name, current_job(name, os.environ.get('GH_TOKEN'))[2], datetime.now(timezone.utc))
    except Exception as error:  # noqa: BLE001 - a summary line, never a reason to fail the job
        text = f'{name}: duration and runner wait unknown ({type(error).__name__}: {error})'
    print(text)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with Path(os.environ['GITHUB_STEP_SUMMARY']).open('a') as summary:
            summary.write(text + '\n')


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--job-name', required=True)
    parser.add_argument('--line', action='store_true',
                        help='one best-effort summary line with the duration so far')
    args = parser.parse_args(argv)
    if args.line:
        return best_effort_line(args.job_name)
    run, attempt, job = current_job(args.job_name, os.environ['GH_TOKEN'])
    delay = queue_seconds(job)
    with Path(os.environ['GITHUB_STEP_SUMMARY']).open('a') as summary:
        summary.write('## Runner queue wait\n\n')
        summary.write('| Run / attempt | Job | Queued UTC | Runner started UTC | Queue seconds |\n')
        summary.write('|---|---|---|---|---:|\n')
        summary.write(f'| {run} / {attempt} | {args.job_name} | {job["created_at"]} | {job["started_at"]} | {delay:.3f} |\n')
    print(f'Runner queue wait: {delay:.3f}s (job {job["id"]}, attempt {attempt})')


if __name__ == '__main__':
    main()
