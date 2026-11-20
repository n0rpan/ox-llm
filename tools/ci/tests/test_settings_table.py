"""The settings page's tables come from oxa-config's page sheet (#2042, R3).

The generator `tools/ci/settings_table.py` renders the section blocks of
`oxa/docs/pages/settings.md` from `oxa/oxa-config/src/descriptor/sheet/`, so a hand edit of a
row does not survive and a merge that keeps both sides of a row is fixed by rerunning the
generator. The check below is what the CI policy steps run.
"""

import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location(
    'settings_table', Path(__file__).resolve().parents[1] / 'settings_table.py')
TABLE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(TABLE)


class CommittedPage(unittest.TestCase):
    def test_tables_are_current(self):
        page = TABLE.PAGE.read_text()
        expected = TABLE.render(page)
        if page == expected:
            return
        was, now = page.splitlines(), expected.splitlines()
        number = next((index for index, (a, b) in enumerate(zip(was, now), 1) if a != b),
                      min(len(was), len(now)) + 1)
        first = was[number - 1] if number <= len(was) else ''
        self.fail(
            f'docs/pages/settings.md:{number} is stale: its tables come from oxa-config\'s page '
            f'sheet.\nRegenerate them with: {TABLE.REGENERATE}\n'
            f'page has: {first[:120]}\nsheet has: {now[number - 1][:120] if number <= len(now) else ""}')


class Regeneration(unittest.TestCase):
    def setUp(self):
        self.page = TABLE.PAGE.read_text()

    def test_a_hand_edited_row_is_replaced(self):
        lines = self.page.splitlines()
        start = lines.index('<!-- generated:interface -->')
        row = next(number for number in range(start, len(lines))
                   if lines[number].startswith('| `'))
        lines[row] = lines[row].replace(' | ', ' | stale: ', 1)
        edited = '\n'.join(lines) + '\n'
        self.assertNotEqual(edited, self.page)
        self.assertEqual(TABLE.render(edited), self.page)

    def test_a_missing_marker_names_the_section(self):
        broken = self.page.replace('<!-- generated:tools -->', '')
        with self.assertRaises(TABLE.Stale) as problem:
            TABLE.render(broken)
        self.assertIn('generated:tools', str(problem.exception))

    def test_a_marker_without_a_sheet_section_is_refused(self):
        broken = self.page.replace('<!-- /generated:other -->',
                                   '<!-- /generated:other -->\n<!-- generated:gone -->')
        with self.assertRaises(TABLE.Stale):
            TABLE.render(broken)


if __name__ == '__main__':
    unittest.main()
