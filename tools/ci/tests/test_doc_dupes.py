"""Guard against keep-both-sides merge damage in the docs (#2042, R2).

A merge that keeps both sides adds a second section under a heading that already exists, or a
second table row with the same key. The detectors below flag exactly that shape, and the repo
test fails while any maintained docs page carries one, so the CI policy steps that run this
suite (`checks` and `lane`, `python3 -I -m unittest discover -s tools/ci/tests`) block it.
"""

import importlib.util
import os
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[3]

spec = importlib.util.spec_from_file_location('paths', Path(__file__).resolve().parents[1] / 'paths.py')
paths = importlib.util.module_from_spec(spec)
spec.loader.exec_module(paths)

HEADING = re.compile(r'^(#{1,6})\s+(.+?)\s*#*\s*$')
# A table delimiter row: pipes, dashes, colons, spaces.
DELIMITER = re.compile(r'^\s*\|?\s*:?-{2,}[-\s:|]*$')
CELL = re.compile(r'(?<!\\)\|')  # a cell separator, never an escaped pipe


def unfenced(lines):
    """Yield (line number, text) with fenced code blocks blanked out (None)."""
    fenced, marker = False, ''
    for number, line in enumerate(lines, 1):
        stripped = line.lstrip()
        if not fenced and stripped.startswith(('```', '~~~')):
            fenced, marker = True, stripped[:3]
            yield number, None
        elif fenced and stripped.startswith(marker):
            fenced = False
            yield number, None
        else:
            yield number, None if fenced else line


def duplicate_headings(lines):
    """(level, text, enclosing chain, line numbers) for headings repeated under the same chain.

    The chain is the enclosing heading texts, so a changelog may repeat a section heading under
    each release, while two sections with the same heading under the same parent are damage.
    """
    seen, chain = {}, {}
    for number, line in unfenced(lines):
        if line is None:
            continue
        match = HEADING.match(line)
        if match is None:
            continue
        level, text = len(match.group(1)), match.group(2).strip()
        parent = tuple(heading for lvl, heading in sorted(chain.items()) if lvl < level)
        chain = {lvl: heading for lvl, heading in chain.items() if lvl < level}
        chain[level] = text
        seen.setdefault((level, text, parent), []).append(number)
    return [(level, text, parent, at) for (level, text, parent), at in seen.items() if len(at) > 1]


def cells(line):
    row = line.strip()
    row = row[1:] if row.startswith('|') else row
    row = row[:-1] if row.endswith('|') else row
    return [cell.strip() for cell in CELL.split(row)]


def duplicate_rows(lines):
    """(header, key cell, line numbers) for table rows repeated under the same header.

    The first column is the row key, and the header row identifies the table: two rows keyed
    alike under the same header are the same entry documented twice, in one table or across two.
    A page may still repeat a first cell under a different header (measurement tables do).
    """
    stream, keyed = list(unfenced(lines)), {}
    for index, (_, line) in enumerate(stream):
        if line is None or not line.strip().startswith('|'):
            continue
        if index + 1 >= len(stream):
            break
        after = stream[index + 1][1]
        if after is None or not DELIMITER.match(after):
            continue
        header = tuple(cells(line))
        for number, row in stream[index + 2:]:
            if row is None or not row.strip().startswith('|'):
                break
            row_cells = cells(row)
            if row_cells and row_cells[0]:
                keyed.setdefault((header, row_cells[0]), []).append(number)
    return [(header, key, at) for (header, key), at in keyed.items() if len(at) > 1]


def docs_pages():
    """Every maintained docs page: the classifier's doc set, minus the oxc engineering notes.

    oxc/docs holds benchmark and research notes whose tables record one row per measurement,
    so a repeated first column there is their normal shape, not merge damage.
    """
    for directory, subdirectories, files in os.walk(ROOT):
        subdirectories[:] = [name for name in subdirectories
                             if name not in ('.git', 'target', '.oxa')]
        for name in files:
            page = Path(directory, name).relative_to(ROOT).as_posix()
            if paths.documentation(page) and not page.startswith('oxc/docs/'):
                yield page


SAMPLE = '''# Title

## Alpha

Prose.

### Same

Prose.

## Beta

### Same

Prose.

## Alpha

| Key | Value |
|---|---|
| one | first |
| two | second |
| one | first again |

| Key | Value |
|---|---|
| two | third |

| Other | Note |
|---|---|
| one | the same key cell under another header |
| other | a different entry |

```markdown
## Alpha

| Key | Value |
|---|---|
| one | inside a fence |
| one | inside a fence |
```
'''


class DuplicateDetection(unittest.TestCase):
    def test_heading_repeats_under_the_same_chain_only(self):
        # 'Same' repeats under different parents, the second '## Alpha' under the same one,
        # and the fenced '## Alpha' is an example, not a section.
        self.assertEqual([(2, 'Alpha', ('Title',), [3, 17])],
                         duplicate_headings(SAMPLE.splitlines()))

    def test_row_repeats_under_the_same_header_only(self):
        # 'one' repeats in one table and 'two' across two tables with the same header; the
        # same key cell under a different header is a different entry, and fenced rows do not count.
        rows = {(header, key): at for header, key, at in duplicate_rows(SAMPLE.splitlines())}
        self.assertEqual({(('Key', 'Value'), 'one'): [21, 23],
                          (('Key', 'Value'), 'two'): [22, 27]}, rows)


class DocsPages(unittest.TestCase):
    def test_no_duplicate_heading_or_keyed_row(self):
        failures = []
        for page in sorted(docs_pages()):
            lines = (ROOT / page).read_text(errors='replace').splitlines()
            for level, text, parent, at in duplicate_headings(lines):
                where = ' > '.join(parent) or 'the page top'
                failures.append(f'{page}: heading {text!r} (h{level}) under {where} repeats at lines {at}')
            for header, key, at in duplicate_rows(lines):
                failures.append(f'{page}: rows keyed {key!r} under header {header[0]!r} repeat at lines {at}')
        self.assertEqual([], failures)


if __name__ == '__main__':
    unittest.main()
