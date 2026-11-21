"""Render the settings.md setting tables from oxa-config's page sheet (#2042, R3).

`oxa/oxa-config/src/descriptor/sheet/` holds one module per section of
`oxa/docs/pages/settings.md`: the section title and one row per documented setting (key, type,
default, meaning), in page order. The page's prose and its other sections stay hand-written; only
the blocks between `<!-- generated:<id> -->` and `<!-- /generated:<id> -->` are written here, so a
settings-docs merge conflict is fixed by rerunning this generator instead of by hand:

    python3 tools/ci/settings_table.py           # check; names the command when the page is stale
    python3 tools/ci/settings_table.py --write   # rewrite the generated blocks

The guard test `tests/test_settings_table.py` runs the check, so the CI policy steps that run
`python3 -I -m unittest discover -s tools/ci/tests` fail while the page is stale.
"""

from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[2]
SHEET = ROOT / 'oxa/oxa-config/src/descriptor/sheet'
PAGE = ROOT / 'oxa/docs/pages/settings.md'

REGENERATE = 'python3 tools/ci/settings_table.py --write'

STRING = r'"((?:[^"\\]|\\.)*)"'
ROW = re.compile(r'\brow\(\s*' + STRING + r'\s*,\s*' + STRING + r'\s*,\s*'
                 + STRING + r'\s*,\s*' + STRING + r'\s*,?\s*\)')
TITLE = re.compile(r'const TITLE: &str = ' + STRING + ';')
ORDER = re.compile(r'const ALL: \[Page; \d+\] = \[(.*?)\];', re.S)
MARKER = '<!-- generated:{} -->'
END = '<!-- /generated:{} -->'


class Stale(Exception):
    """The page does not match the sheet."""


def unescape(literal):
    """A Rust string literal's body: the escapes the sheet uses, and no others."""
    def replace(match):
        char = match.group(1)
        if char in 'nrt':
            return {'n': '\n', 'r': '\r', 't': '\t'}[char]
        return char
    return re.sub(r'\\(.)', replace, literal)


def sections():
    """(id, title, rows) per page section, in the order `Page::ALL` declares."""
    mod = (SHEET / 'mod.rs').read_text()
    found = ORDER.search(mod)
    if found is None:
        raise Stale(f'{SHEET / "mod.rs"}: no `const ALL: [Page; N]` section list')
    order = [name.lower() for name in re.findall(r'Page::(\w+)', found.group(1))]
    out = []
    for name in order:
        source = SHEET / f'{name}.rs'
        if not source.is_file():
            raise Stale(f'{source}: missing module for section `{name}`')
        text = source.read_text()
        title = TITLE.search(text)
        if title is None:
            raise Stale(f'{source}: no TITLE')
        rows = [tuple(unescape(cell) for cell in row.groups()) for row in ROW.finditer(text)]
        if not rows:
            raise Stale(f'{source}: no rows')
        out.append((name, unescape(title.group(1)), rows))
    return out


def cell(text):
    """One table cell: a pipe would end it, so it is escaped."""
    return text.replace('|', '\\|')


def block(name, title, rows):
    lines = [MARKER.format(name), f'### {title}', '', '| Key | Type | Default | Meaning |',
             '|---|---|---|---|']
    for key, kind, default, text in rows:
        lines.append('| ' + ' | '.join(cell(part) for part in (f'`{key}`', kind, default, text)) + ' |')
    lines.append(END.format(name))
    return '\n'.join(lines)


def render(page_text):
    """The page with every generated block rewritten from the sheet."""
    found = sections()
    ids = {name for name, _, _ in found}
    for name in re.findall(r'<!-- generated:([a-z-]+) -->', page_text):
        if name not in ids:
            raise Stale(f'{PAGE}: marker for section `{name}`, which the sheet does not have')
    for name, title, rows in found:
        if MARKER.format(name) not in page_text:
            raise Stale(f'{PAGE}: no `{MARKER.format(name)}` marker for section `{name}`')
        pattern = re.compile(re.escape(MARKER.format(name)) + r'.*?' + re.escape(END.format(name)), re.S)
        page_text = pattern.sub(lambda _: block(name, title, rows), page_text)
    return page_text


def main(words):
    write = False
    for word in words:
        if word == '--write':
            write = True
        elif word == '--check':
            write = False
        elif word in ('-h', '--help'):
            print(__doc__.strip())
            return 0
        else:
            print(f'unknown option: {word}\nusage: python3 tools/ci/settings_table.py [--check | --write]',
                  file=sys.stderr)
            return 2
    page = PAGE.read_text()
    try:
        rendered = render(page)
    except Stale as problem:
        print(problem, file=sys.stderr)
        return 1
    if write:
        if rendered != page:
            PAGE.write_text(rendered)
            print(f'{PAGE.relative_to(ROOT)}: tables regenerated')
        return 0
    if rendered != page:
        print(f'{PAGE.relative_to(ROOT)} is stale: its tables come from oxa-config\'s page sheet.\n'
              f'Regenerate it with:\n\n    {REGENERATE}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    raise SystemExit(main(sys.argv[1:]))
