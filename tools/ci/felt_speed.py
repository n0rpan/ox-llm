#!/usr/bin/env python3
"""Assemble the seven-row felt-speed scorecard from measurement files.

The scorecard keeps the two boundaries apart. `synthetic` is a hosted CI runner driving
mock or loopback peers; `laptop` is the maintainer's hardware. A boundary that was not
measured is reported as unmeasured with a reason and is never inferred from the other
boundary, so a synthetic number can never be read as a laptop number.

Every row records its unit, its target, its sample count, its distribution and its
validity. The scorecard records the exact source and binary identity, the cargo profile,
the enabled features and the runner it was produced on. An empty feature list means the
crate's default features.

Measurement input is a JSON file holding a list of
`{"row": "R1", "boundary": "synthetic", "samples": [12.0, ...]}` objects, either bare or
under a `measurements` key. Rows without a measurement are emitted as unmeasured.
"""
import argparse
import hashlib
import json
import math
import os
import platform
import sys
from pathlib import Path

SCHEMA = 'oxa-felt-speed-scorecard/1'
ROWS_SCHEMA = 'oxa-felt-speed-rows/1'
BOUNDARIES = ('synthetic', 'laptop')
STATISTICS = ('min', 'p50', 'p90', 'max')
MIN_SAMPLES = 5
PENDING = 'row definition pending the issue #885 row text'
UNMEASURED = {
    'synthetic': 'not measured on the synthetic boundary in this run',
    'laptop': 'the laptop boundary is measured on the maintainer hardware, not in CI',
}


def percentile(ordered, fraction):
    """Nearest-rank percentile of an already sorted, non-empty list."""
    return ordered[max(1, math.ceil(fraction * len(ordered))) - 1]


def distribution(samples):
    ordered = sorted(samples)
    return {
        'min': ordered[0],
        'p50': percentile(ordered, 0.5),
        'p90': percentile(ordered, 0.9),
        'max': ordered[-1],
    }


def check_target(identifier, boundary, target):
    if target.get('op') != '<=':
        raise ValueError(f'row {identifier} {boundary}: unsupported operator {target.get("op")!r}')
    if target.get('kind') == 'absolute':
        if not isinstance(target.get('value'), (int, float)) or isinstance(target.get('value'), bool):
            raise ValueError(f'row {identifier} {boundary}: an absolute target needs a number')
    elif target.get('kind') == 'relative':
        if not isinstance(target.get('percent'), (int, float)) or isinstance(target.get('percent'), bool):
            raise ValueError(f'row {identifier} {boundary}: a relative target needs a percent')
        baseline = target.get('baseline')
        if not isinstance(baseline, dict) or not baseline.get('row') or baseline.get('boundary') not in BOUNDARIES:
            raise ValueError(f'row {identifier} {boundary}: a relative target needs a baseline row and boundary')
    else:
        raise ValueError(f'row {identifier} {boundary}: unknown target kind {target.get("kind")!r}')


def load_rows(path):
    document = json.loads(Path(path).read_text())
    if document.get('schema') != ROWS_SCHEMA:
        raise ValueError(f'unknown rows schema: {document.get("schema")!r}')
    rows = document.get('rows')
    if not isinstance(rows, list) or len(rows) != 7:
        raise ValueError('the scorecard has exactly seven rows')
    seen = set()
    for row in rows:
        identifier = row.get('id')
        if not identifier or identifier in seen:
            raise ValueError(f'row id missing or repeated: {identifier!r}')
        seen.add(identifier)
        if row.get('name') is None:
            continue  # A pending row declares no unit, statistic or target.
        if not row.get('unit') or row.get('statistic') not in STATISTICS:
            raise ValueError(f'row {identifier} needs a unit and a known statistic')
        for boundary in BOUNDARIES:
            target = row.get(boundary)
            if target is not None:
                check_target(identifier, boundary, target)
    return rows


def load_measurements(paths):
    measurements = {}
    for path in paths:
        document = json.loads(Path(path).read_text())
        entries = document.get('measurements') if isinstance(document, dict) else document
        if not isinstance(entries, list):
            raise ValueError(f'{path}: expected a list of measurements')
        for entry in entries:
            key = (entry.get('row'), entry.get('boundary'))
            if key[1] not in BOUNDARIES:
                raise ValueError(f'{path}: unknown boundary {key[1]!r}')
            if key in measurements:
                raise ValueError(f'{path}: duplicate measurement for {key[0]} {key[1]}')
            samples = entry.get('samples')
            if not isinstance(samples, list) or not samples:
                raise ValueError(f'{path}: {key[0]} {key[1]} has no samples')
            for sample in samples:
                if isinstance(sample, bool) or not isinstance(sample, (int, float)) \
                        or not math.isfinite(sample) or sample < 0:
                    raise ValueError(f'{path}: {key[0]} {key[1]} has an invalid sample {sample!r}')
            measurements[key] = [float(sample) for sample in samples]
    return measurements


def unmeasured(reason):
    return {'measured': False, 'valid': False, 'samples': 0, 'distribution': None,
            'meets_target': None, 'target_note': None, 'unmeasured_reason': reason}


def build_row(row, measurements):
    identifier = row['id']
    built = {
        'id': identifier,
        'name': row.get('name'),
        'provisional': bool(row.get('provisional')),
        'unit': row.get('unit'),
        'statistic': row.get('statistic'),
        'targets': {boundary: row.get(boundary) for boundary in BOUNDARIES},
        'boundaries': {},
    }
    for boundary in BOUNDARIES:
        samples = measurements.get((identifier, boundary))
        if row.get('name') is None:
            built['boundaries'][boundary] = unmeasured(PENDING)
        elif samples is None:
            built['boundaries'][boundary] = unmeasured(UNMEASURED[boundary])
        elif len(samples) < MIN_SAMPLES:
            built['boundaries'][boundary] = unmeasured(
                f'only {len(samples)} samples; {MIN_SAMPLES} are required')
        else:
            built['boundaries'][boundary] = {
                'measured': True, 'valid': True, 'samples': len(samples),
                'distribution': distribution(samples), 'meets_target': None,
                'target_note': None, 'unmeasured_reason': None,
            }
    return built


def evaluate(scorecard):
    """Resolve relative targets and decide whether each measured boundary meets its target."""
    observed = {}
    for row in scorecard['rows']:
        for boundary, measured in row['boundaries'].items():
            if measured['measured']:
                observed[(row['id'], boundary)] = measured['distribution'][row['statistic']]
    for row in scorecard['rows']:
        for boundary, target in row['targets'].items():
            measured = row['boundaries'][boundary]
            if not measured['measured'] or target is None:
                continue
            if target['kind'] == 'absolute':
                limit = target['value']
            else:
                baseline = target['baseline']
                key = (baseline['row'], baseline['boundary'])
                if key not in observed:
                    measured['target_note'] = f'baseline {key[0]} {key[1]} was not measured'
                    continue
                limit = observed[key] * (1 + target['percent'] / 100)
            measured['meets_target'] = observed[(row['id'], boundary)] <= limit


def validate(scorecard):
    if scorecard.get('schema') != SCHEMA:
        raise ValueError('unknown scorecard schema')
    meta = scorecard.get('meta') or {}
    for field in ('source_sha', 'profile', 'runner'):
        if not meta.get(field):
            raise ValueError(f'scorecard metadata is missing {field}')
    if not isinstance(meta.get('features'), list):
        raise ValueError('scorecard metadata is missing features')
    rows = scorecard.get('rows')
    if not isinstance(rows, list) or len(rows) != 7:
        raise ValueError('the scorecard has exactly seven rows')
    for row in rows:
        if row.get('name') is not None and not row.get('unit'):
            raise ValueError(f'row {row["id"]} has no unit')
        for boundary in BOUNDARIES:
            measured = row['boundaries'][boundary]
            if measured['measured']:
                if not measured['valid'] or measured['samples'] < MIN_SAMPLES or not measured['distribution']:
                    raise ValueError(f'row {row["id"]} {boundary} is measured but not valid')
                if measured['unmeasured_reason'] is not None:
                    raise ValueError(f'row {row["id"]} {boundary} is measured and unmeasured')
            elif not measured['unmeasured_reason']:
                raise ValueError(f'row {row["id"]} {boundary} is unmeasured without a reason')
    return len(rows)


def collect_meta(args):
    meta = {
        'source_sha': args.source_sha,
        'profile': args.profile,
        'features': list(args.features),
        'runner': {
            'label': args.runner_label,
            'os': platform.system().lower(),
            'arch': platform.machine(),
            'cpu_count': os.cpu_count(),
        },
        'binary': None,
    }
    if args.binary:
        binary = Path(args.binary).resolve()
        meta['binary'] = {
            'path': str(binary),
            'sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
            'bytes': binary.stat().st_size,
        }
    return meta


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rows', type=Path, required=True)
    parser.add_argument('--measurements', type=Path, action='append', default=[])
    parser.add_argument('--source-sha', required=True)
    parser.add_argument('--profile', required=True)
    parser.add_argument('--features', action='append', default=[])
    parser.add_argument('--runner-label', required=True)
    parser.add_argument('--binary', type=Path)
    parser.add_argument('--out', type=Path)
    args = parser.parse_args()
    try:
        measurements = load_measurements(args.measurements)
        scorecard = {
            'schema': SCHEMA,
            'meta': collect_meta(args),
            'rows': [build_row(row, measurements) for row in load_rows(args.rows)],
        }
        evaluate(scorecard)
        count = validate(scorecard)
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.exit(1, f'felt-speed scorecard rejected: {error}\n')
    text = json.dumps(scorecard, indent=2, sort_keys=True) + '\n'
    if args.out:
        args.out.write_text(text)
    else:
        print(text, end='')
    measured = sum(1 for row in scorecard['rows'] for boundary in BOUNDARIES
                   if row['boundaries'][boundary]['measured'])
    print(f'felt-speed: {count} rows, {measured} measured boundaries', file=sys.stderr)


if __name__ == '__main__':
    main()
