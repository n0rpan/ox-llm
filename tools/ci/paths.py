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
    if parts[0] == 'docs' or (len(parts) >= 3 and parts[0] in ('oxa', 'engine', 'oxc')
                              and parts[1] == 'docs'):
        return True
    return len(parts) == 2 and parts[0] in ('oxa', 'engine', 'oxc') and parts[1] in (
        'README.md', 'CONTRIBUTING.md', 'CHANGELOG.md', 'AGENTS.md', 'SECURITY.md')


# Top-level directories and the outputs (job groups) they select. `gateway` is the in-process
# library; gateway changes exercise the oxa workspace. The Python controller suite and the
# oxc-rs CLI are gone, so oxc/ selects only the gateway job.
COMPONENTS = {'engine': {'engine'}, 'oxa': {'oxa'}, 'oxc': {'gateway'}}
FUZZ = {'fuzz/oxa': {'oxa'}, 'fuzz/oxe': {'engine'}}
OUTPUTS = ('engine', 'oxa', 'gateway')
# Anything else (.github/, tools/, packaging/, root files, new directories) marks every component.

# `macos` selects the 90-minute native macOS build on ready PRs. It is not one of OUTPUTS: an
# unknown path does not select it, only macOS packaging, the two workflows that build macOS, the
# oxa dependency set, any path naming macos/darwin, and the oxa sources that hold macOS-only code
# (cfg target_os = "macos" or not(target_os = "linux")). Every other change is built on macOS by
# dev-build.yml after it lands on main, which files a p1 issue when that build breaks.
MACOS_PREFIXES = ('packaging/macos/',)
MACOS_FILES = frozenset((
    '.github/workflows/macos.yml', '.github/workflows/dev-build.yml',
    'oxa/Cargo.toml', 'oxa/Cargo.lock',
    'oxa/oxa-cli/src/report_cmd.rs', 'oxa/oxa-cli/src/suite_status/sessions.rs',
    'oxa/oxa-cli/src/workers/proc.rs',
    'oxa/oxa-core/src/exec.rs', 'oxa/oxa-core/src/owner_confirm.rs',
    'oxa/oxa-interactive/src/driver.rs', 'oxa/oxa-interactive/src/external.rs',
    'oxa/oxa-mcp/src/oauth.rs', 'oxa/oxa-mcp/tests/stdio_without_prctl.rs',
    'oxa/oxa-proto/src/exec.rs', 'oxa/oxa-proto/tests/mode_text.rs',
    'oxa/oxa-rpc/src/server.rs',
    'oxa/oxa-sandbox/src/bwrap.rs', 'oxa/oxa-sandbox/src/lib.rs', 'oxa/oxa-sandbox/src/run.rs',
    'oxa/oxa-sandbox/tests/unsandboxed_portable.rs',
    'oxa/oxa-session/src/inbox.rs', 'oxa/oxa-session/src/lock.rs',
    'oxa/oxa-session/tests/inbox_poll.rs',
    'oxa/oxa-tools/src/checked.rs', 'oxa/oxa-tools/src/edit.rs',
    'oxa/oxc-gateway/src/openai/chatgpt_tokens.rs', 'oxa/oxc-gateway/src/secrets.rs',
    'oxa/oxc-local/src/lib.rs',
))


MACOS_CFG = re.compile(r'target_os\s*=\s*"macos"|not\((any\()?target_os\s*=\s*"linux"')
REPO = Path(__file__).resolve().parents[2]


def macos_code(path):
    # A changed Rust file that holds macOS-only cfg selects the build without being listed anywhere,
    # so adding macOS code to a new file never needs a list edit (MACOS_FILES stays as a fast path).
    if not path.endswith('.rs'):
        return False
    try:
        return bool(MACOS_CFG.search((REPO / path).read_text(errors='replace')))
    except OSError:
        return False


def macos(path):
    lower = path.lower()
    return (path in MACOS_FILES or path.startswith(MACOS_PREFIXES)
            or 'macos' in lower or 'darwin' in lower or macos_code(path))


def classify(paths, full_event=False):
    paths = list(paths)
    docs = bool(paths) and all(documentation(path) for path in paths)
    changed = dict.fromkeys(OUTPUTS, False)
    mac = False
    for path in paths:
        if documentation(path):
            continue
        mac = mac or macos(path)
        if path.startswith(('oxa/oxc-gateway/', 'oxa/oxa-engine/', 'oxa/oxa-proto/', 'oxa/oxa-cli/')) or path in ('oxa/Cargo.toml', 'oxa/Cargo.lock'):
            components = {'oxa', 'gateway'}
        else:
            components = COMPONENTS.get(path.split('/', 1)[0]) or FUZZ.get('/'.join(path.split('/', 2)[:2]))
        for component in components or OUTPUTS:
            changed[component] = True
    md = any(Path(path).suffix.lower() in ('.md', '.rst') for path in paths)
    return {**changed, 'macos': mac, 'docs': docs, 'md': md, 'full': full_event}


def changed_paths(event, name):
    if event.get('deleted'):
        return []
    if name in ('schedule', 'workflow_dispatch'):
        return subprocess.check_output(['git', 'ls-files', '-z']).decode().split('\0')[:-1]
    if name == 'merge_group':
        # Only what the group changes on top of its base; an empty diff cannot be classified,
        # so it conservatively counts as every file.
        group = event.get('merge_group') or {}
        base, head = group.get('base_sha'), group.get('head_sha')
        triple = False
    elif name == 'pull_request':
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
    changed = subprocess.check_output(['git', 'diff', '--name-only', '-z', span, '--']).decode().split('\0')[:-1]
    if name == 'merge_group' and not changed:
        return subprocess.check_output(['git', 'ls-files', '-z']).decode().split('\0')[:-1]
    return changed


def full_gate(name, ref):
    return name in ('merge_group', 'workflow_dispatch', 'schedule') or (
        name == 'push' and ref == 'refs/heads/main')


def main():
    event = json.loads(Path(os.environ['GITHUB_EVENT_PATH']).read_text())
    name = os.environ['GITHUB_EVENT_NAME']
    full = full_gate(name, os.environ.get('GITHUB_REF', ''))
    result = classify(changed_paths(event, name), full)
    if full:
        if name != 'merge_group':
            # Main, nightly and manual runs test every component.
            result.update(dict.fromkeys(OUTPUTS + ('macos',), True))
    with open(os.environ['GITHUB_OUTPUT'], 'a') as stream:
        for key, value in result.items():
            stream.write(f'{key}={str(value).lower()}\n')


if __name__ == '__main__':
    main()
