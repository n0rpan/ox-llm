#!/usr/bin/env python3
"""Compare shadow JSONL with local, read-only lab logs; never print raw log text."""
import argparse
from datetime import date, datetime, time, timedelta
import json
from pathlib import Path
import re
from zoneinfo import ZoneInfo

SOURCES = ('merge-flow.log', 'auto-approve.log', 'night-watch.log')


def action(text):
    text = text.lower()
    operation = re.match(r'^\d{2}:\d{2}\s+([\w-]+)\s+#\d+\b', text)
    if operation and operation[1] in ('enqueued', 're-enqueued', 'approved'):
        return 'ready'
    for words, name in ((('findings',), 'fix_findings'),
                        (('conflict',), 'resolve_conflict'),
                        (('ci failed', 'gate failed', 'queue rejected'), 'fix_required_ci'),
                        (('removes:',), 'add_removes'),
                        (('promoted',), 'promote_draft'),
                        (('review',), 'wait_review')):
        if any(word in text for word in words):
            return name
    return 'unclassified'


def lab_events(directory, last_day, timezone):
    """Logs have HH:MM only. --date names their final day; infer midnight backwards."""
    events = []
    for source in SOURCES:
        day, later = last_day, None
        for line in reversed((directory / source).read_text().splitlines()):
            stamp = re.match(r'^(\d{2}:\d{2})\s', line)
            if not stamp:
                continue
            clock = time.fromisoformat(stamp[1])
            minute = clock.hour * 60 + clock.minute
            if later is not None and minute > later:
                day -= timedelta(days=1)
            later = minute
            number = re.search(r'#(\d+)\b', line)
            if number:
                local = datetime.combine(day, clock, timezone)
                if local.utcoffset() != local.replace(fold=1).utcoffset():
                    raise ValueError('ambiguous or nonexistent local log time; use UTC logs')
                at = int(local.timestamp())
                events.append((at, int(number[1]), action(line), source))
    return sorted(events)


def differences(samples, events):
    for index, sample in enumerate(samples):
        if sample['schema_version'] != 1:
            raise ValueError('unsupported shadow schema')
        start = sample['observed_at']
        end = min(start + 300, samples[index + 1]['observed_at']) if index + 1 < len(samples) else start + 300
        if end <= start:
            raise ValueError('shadow timestamps must increase')
        predicted = {(row['number'], row['action']) for row in sample['would_take']}
        actual = {}
        for at, number, name, source in sorted(events):
            # A lab timestamp denotes [minute:00, minute:00 + 60s), not an instant.
            if at < end and at + 60 > start:
                actual.setdefault((number, name), source)
        for (number, name), source in sorted(actual.items()):
            if (number, name) not in predicted:
                yield dict(observed_at=start, difference='lab_only', number=number, action=name, source=source)
        observed = set(actual)
        for number, name in sorted(predicted - observed):
            yield dict(observed_at=start, difference='shadow_only', number=number, action=name)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('shadow', type=Path)
    parser.add_argument('--lab-dir', type=Path, required=True)
    parser.add_argument('--date', type=date.fromisoformat, required=True, help='final day in the supplied lab logs')
    parser.add_argument('--timezone', type=ZoneInfo, required=True, help='IANA timezone of lab HH:MM timestamps')
    args = parser.parse_args()
    try:
        samples = [json.loads(line) for line in args.shadow.read_text().splitlines() if line.strip()]
        if not samples:
            raise ValueError('shadow log is empty')
        rows = list(differences(samples, lab_events(args.lab_dir, args.date, args.timezone)))
    except (OSError, ValueError, KeyError, TypeError):
        parser.exit(2, 'comparison failed: check log files, timestamps and schema\n')
    for row in rows:
        print(json.dumps(row, sort_keys=True))
    print(json.dumps(dict(samples=len(samples), differences=len(rows)), sort_keys=True))


if __name__ == '__main__':
    main()
