#!/usr/bin/env python3
"""Record the runner queue delay from GitHub's job timestamps, once per started job."""
import argparse
from datetime import datetime
import json
import os
from pathlib import Path
import re
import urllib.request


def queue_seconds(job):
    created = datetime.fromisoformat(job['created_at'].replace('Z', '+00:00'))
    started = datetime.fromisoformat(job['started_at'].replace('Z', '+00:00'))
    seconds = (started - created).total_seconds()
    if seconds < 0:
        raise ValueError('job start precedes queue creation')
    return seconds


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--job-name', required=True)
    args = parser.parse_args()
    repository = os.environ['GITHUB_REPOSITORY']
    run = os.environ['GITHUB_RUN_ID']
    attempt = os.environ['GITHUB_RUN_ATTEMPT']
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repository) or not run.isdigit() or not attempt.isdigit():
        raise ValueError('invalid workflow identity')
    url = f'https://api.github.com/repos/{repository}/actions/runs/{run}/attempts/{attempt}/jobs?per_page=100'
    request = urllib.request.Request(url, headers={
        'Authorization': 'Bearer ' + os.environ['GH_TOKEN'],
        'Accept': 'application/vnd.github+json',
        'X-GitHub-Api-Version': '2022-11-28',
    })
    with urllib.request.urlopen(request, timeout=30) as response:
        jobs = json.load(response)['jobs']
    matches = [job for job in jobs if job['name'] == args.job_name]
    if len(matches) != 1:
        raise ValueError('current job is missing or ambiguous in the run')
    job = matches[0]
    delay = queue_seconds(job)
    # The workflow has fewer than 100 jobs; this is the exact current attempt's job,
    # not run.created_at (which would include dependency execution or prior attempts).
    with Path(os.environ['GITHUB_STEP_SUMMARY']).open('a') as summary:
        summary.write('## Runner queue wait\n\n')
        summary.write('| Run / attempt | Job | Queued UTC | Runner started UTC | Queue seconds |\n')
        summary.write('|---|---|---|---|---:|\n')
        summary.write(f'| {run} / {attempt} | {args.job_name} | {job["created_at"]} | {job["started_at"]} | {delay:.3f} |\n')
    print(f'Runner queue wait: {delay:.3f}s (job {job["id"]}, attempt {attempt})')


if __name__ == '__main__':
    main()
