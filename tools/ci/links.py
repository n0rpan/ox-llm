"""Check local Markdown link targets in changed docs; never contact external services."""
import json
import os
from pathlib import Path
import re
import urllib.parse

from paths import changed_paths

ROOT = Path(__file__).resolve().parents[2]
LINK = re.compile(r'!?\[[^\]\n]*\]\(([^)\n]+)\)')


def broken_links(source):
    fenced = False
    for number, line in enumerate(source.read_text().splitlines(), 1):
        if line.lstrip().startswith(('```', '~~~')):
            fenced = not fenced
        if fenced:
            continue
        for match in LINK.finditer(line):
            target = match.group(1).strip().split(' "', 1)[0].strip('<>')
            parts = urllib.parse.urlsplit(target)
            if parts.scheme or parts.netloc or not parts.path:
                continue
            path = urllib.parse.unquote(parts.path)
            destination = ROOT / path.lstrip('/') if path.startswith('/') else source.parent / path
            if not destination.exists():
                yield f'{source.relative_to(ROOT)}:{number}: missing {target}'


def main():
    event = json.loads(Path(os.environ['GITHUB_EVENT_PATH']).read_text())
    changed = changed_paths(event, os.environ['GITHUB_EVENT_NAME'])
    failures = []
    for path in changed:
        source = ROOT / path
        if source.is_file() and source.suffix.lower() == '.md':
            failures.extend(broken_links(source))
    print('\n'.join(failures) if failures else 'Local Markdown links are valid.')
    return bool(failures)


if __name__ == '__main__':
    raise SystemExit(main())
