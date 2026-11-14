"""Classify changed paths for hosted checks. Unknown paths conservatively test all products."""
import json
import os
from pathlib import Path
import re
import subprocess


def classify(paths, full_event=False):
    paths = list(paths)
    docs = bool(paths) and all(Path(path).suffix.lower() in ('.md', '.rst') for path in paths)
    changed = {'oxa': False, 'engine': False, 'lab': False}
    for path in paths:
        if Path(path).suffix.lower() in ('.md', '.rst'):
            continue
        component = path.split('/', 1)[0]
        if component in changed:
            changed[component] = True
        else:
            changed = dict.fromkeys(changed, True)
    return {**changed, 'docs': docs, 'full': full_event and not docs}


def changed_paths(event, name):
    if event.get('deleted'):
        return []
    if name in ('schedule', 'workflow_dispatch'):
        return subprocess.check_output(['git', 'ls-files', '-z']).decode().split('\0')[:-1]
    if name == 'pull_request':
        base, head = event['pull_request']['base']['sha'], event['pull_request']['head']['sha']
        triple = True
    else:
        base, head = event['before'], event['after']
        triple = False
    if not all(isinstance(sha, str) and re.fullmatch('[0-9a-f]{40}', sha) for sha in (base, head)):
        raise ValueError('Invalid commit id in event')
    if base == '0' * 40:
        base = subprocess.check_output(['git', 'merge-base', 'origin/main', head], text=True).strip()
        if base == head:
            return subprocess.check_output(['git', 'ls-files', '-z']).decode().split('\0')[:-1]
    span = base + ('...' if triple else '..') + head
    return subprocess.check_output(['git', 'diff', '--name-only', '-z', span, '--']).decode().split('\0')[:-1]


def main():
    event = json.loads(Path(os.environ['GITHUB_EVENT_PATH']).read_text())
    name = os.environ['GITHUB_EVENT_NAME']
    full = name != 'push' or os.environ.get('GITHUB_REF') == 'refs/heads/main'
    result = classify(changed_paths(event, name), full)
    with open(os.environ['GITHUB_OUTPUT'], 'a') as stream:
        for key, value in result.items():
            stream.write(f'{key}={str(value).lower()}\n')


if __name__ == '__main__':
    main()
