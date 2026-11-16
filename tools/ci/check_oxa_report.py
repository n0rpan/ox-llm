#!/usr/bin/env python3
"""Fail closed on missing partitions, failures, runtime skips or lost test coverage."""
import argparse
import json
from pathlib import Path
import xml.etree.ElementTree as ET

# Existing opt-in measurement/dump harnesses, not sandbox or correctness tests.
IGNORED = {
    ('oxa-core::v020_token_dump', 'dump_token_transcripts'),
    ('oxa-interactive::v020_arcade_anim', 'frame_cost'),
    ('oxa-interactive::v020_arcade_pixels', 'pixel_cost_and_previews'),
    # Owner-requested pre-S7 proof (#681). Remove with its #[ignore] when S7 lands.
    ('oxa-cli::no_python_in_path', 'print_uses_native_gateway_without_python_or_controller'),
}


def check(reports, expected=None):
    ran = set()
    for report in reports:
        count = 0
        for case in ET.parse(report).getroot().iter('testcase'):
            key = (case.get('classname'), case.get('name'))
            if not all(key):
                raise ValueError('test identity missing')
            if case.find('skipped') is not None:
                if key not in IGNORED:
                    raise ValueError(f'unexpected skipped test: {key}')
                continue
            if case.find('failure') is not None or case.find('error') is not None:
                raise ValueError(f'failed test: {key}')
            for output in ('system-out', 'system-err'):
                text = case.findtext(output, '')
                if any(line.lstrip().startswith('skipped:') for line in text.splitlines()):
                    raise ValueError(f'runtime skip: {key}')
            if key in ran:
                raise ValueError(f'test ran more than once: {key}')
            ran.add(key)
            count += 1
        if not count:
            raise ValueError('empty or missing partition report')
    if expected is not None:
        inventory = json.loads(Path(expected).read_text())
        wanted = set()
        for binary, suite in inventory['rust-suites'].items():
            if suite['status'] != 'listed':
                raise ValueError(f'unlisted test binary: {binary}')
            for name, test in suite['testcases'].items():
                key = (binary, name)
                if test['ignored']:
                    if key not in IGNORED:
                        raise ValueError(f'unexpected ignored test in inventory: {key}')
                else:
                    wanted.add(key)
        if not wanted or ran != wanted:
            raise ValueError(f'coverage mismatch: missing={sorted(wanted - ran)}, extra={sorted(ran - wanted)}')
    return len(ran)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--expected', type=Path)
    parser.add_argument('reports', type=Path, nargs='+')
    args = parser.parse_args()
    try:
        count = check(args.reports, args.expected)
    except (OSError, ValueError, KeyError, TypeError, ET.ParseError) as error:
        parser.exit(1, f'oxa report rejected: {error}\n')
    print(f'oxa: {count} tests passed; runtime skips: 0')


if __name__ == '__main__':
    main()
