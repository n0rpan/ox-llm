"""Classify changed paths for hosted checks. Unknown paths conservatively test all products."""
import json
import os
from pathlib import Path
import re
import subprocess


def documentation(path):
    """Only known documentation locations qualify; runtime Markdown is product source."""
    parts = Path(path).parts
    if Path(path).suffix.lower() not in ('.md', '.rst'):
        return False
    if len(parts) == 1:
        return parts[0] in ('README.md', 'CONTRIBUTING.md', 'CHANGELOG.md', 'AGENTS.md', 'SECURITY.md')
    if parts[0] == 'docs' or (len(parts) >= 3 and parts[0] in ('oxa', 'engine', 'lab')
                              and parts[1] == 'docs'):
        return True
    return len(parts) == 2 and parts[0] in ('oxa', 'engine', 'lab') and parts[1] in (
        'README.md', 'CONTRIBUTING.md', 'CHANGELOG.md', 'AGENTS.md', 'SECURITY.md')


def classify(paths, full_event=False):
    paths = list(paths)
    docs = bool(paths) and all(documentation(path) for path in paths)
    changed = {'oxa': False, 'engine': False, 'lab': False}
    for path in paths:
        if documentation(path):
            continue
        component = path.split('/', 1)[0]
        if component in changed:
            changed[component] = True
        else:
            changed = dict.fromkeys(changed, True)
    return {**changed, 'docs': docs, 'full': full_event}


def changed_paths(event, name):
    if event.get('deleted'):
        return []
    if name in ('schedule', 'workflow_dispatch', 'merge_group'):
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


def full_gate(name, ref):
    return name in ('merge_group', 'workflow_dispatch', 'schedule') or (
        name == 'push' and ref == 'refs/heads/main')


def main():
    event = json.loads(Path(os.environ['GITHUB_EVENT_PATH']).read_text())
    name = os.environ['GITHUB_EVENT_NAME']
    full = full_gate(name, os.environ.get('GITHUB_REF', ''))
    result = classify(changed_paths(event, name), full)
    with open(os.environ['GITHUB_OUTPUT'], 'a') as stream:
        for key, value in result.items():
            stream.write(f'{key}={str(value).lower()}\n')


if __name__ == '__main__':
    main()
