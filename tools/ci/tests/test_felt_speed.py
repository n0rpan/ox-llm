"""Scorecard controls: seven rows, both boundaries explicit, nothing inferred or invented."""
import argparse
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('felt_speed', ROOT / 'felt_speed.py')
FELT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FELT)

SHIPPED_ROWS = ROOT / 'felt_speed_rows.json'


def row(identifier, name='a row', unit='ms', statistic='p50', synthetic=None, laptop=None):
    return {'id': identifier, 'name': name, 'provisional': True, 'unit': unit,
            'statistic': statistic, 'synthetic': synthetic, 'laptop': laptop}


def seven(first=None, **overrides):
    rows = [first or row('R1')]
    rows += [row(f'R{index}') for index in range(2, 8)]
    for identifier, replacement in overrides.items():
        rows[int(identifier[1:]) - 1] = replacement
    return rows


class ScorecardControls(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def rows_file(self, rows, name='rows.json'):
        path = self.root / name
        path.write_text(json.dumps({'schema': FELT.ROWS_SCHEMA, 'rows': rows}))
        return path

    def measurements_file(self, entries, name='measurements.json'):
        path = self.root / name
        path.write_text(json.dumps({'measurements': entries}))
        return path

    def scorecard(self, rows, entries=()):
        measurements = FELT.load_measurements(
            [self.measurements_file(list(entries))] if entries else [])
        card = {'schema': FELT.SCHEMA, 'meta': self.meta(),
                'rows': [FELT.build_row(item, measurements) for item in rows]}
        FELT.evaluate(card)
        return card

    def meta(self):
        return {'source_sha': '0' * 40, 'profile': 'ci', 'features': [],
                'runner': {'label': 'hosted', 'os': 'linux', 'arch': 'aarch64', 'cpu_count': 4},
                'binary': None}

    def test_the_shipped_table_has_exactly_seven_rows(self):
        rows = FELT.load_rows(SHIPPED_ROWS)
        self.assertEqual([item['id'] for item in rows], [f'R{index}' for index in range(1, 8)])

    def test_a_table_that_is_not_seven_rows_is_rejected(self):
        for rows in (seven()[:6], seven() + [row('R8')], []):
            with self.subTest(count=len(rows)), self.assertRaises(ValueError):
                FELT.load_rows(self.rows_file(rows))

    def test_repeated_and_missing_row_ids_are_rejected(self):
        for rows in (seven(row('R1'), R2=row('R1')), seven(row(None))):
            with self.subTest(rows=rows), self.assertRaises(ValueError):
                FELT.load_rows(self.rows_file(rows))

    def test_a_defined_row_needs_a_unit_and_a_known_statistic(self):
        for broken in (row('R1', unit=None), row('R1', statistic='mean'), row('R1', statistic=None)):
            with self.subTest(broken=broken), self.assertRaises(ValueError):
                FELT.load_rows(self.rows_file(seven(broken)))

    def test_unknown_targets_are_rejected(self):
        for target in ({'kind': 'absolute', 'op': '>=', 'value': 1},
                       {'kind': 'absolute', 'op': '<=', 'value': 'fast'},
                       {'kind': 'relative', 'op': '<=', 'percent': 20},
                       {'kind': 'relative', 'op': '<=', 'percent': 20,
                        'baseline': {'row': 'R1', 'boundary': 'desktop'}},
                       {'kind': 'guess', 'op': '<='}):
            with self.subTest(target=target), self.assertRaises(ValueError):
                FELT.load_rows(self.rows_file(seven(row('R1', synthetic=target))))

    def test_every_unmeasured_boundary_carries_a_reason(self):
        card = self.scorecard(seven())
        self.assertEqual(FELT.validate(card), 7)
        for item in card['rows']:
            for boundary in FELT.BOUNDARIES:
                measured = item['boundaries'][boundary]
                self.assertFalse(measured['measured'])
                self.assertFalse(measured['valid'])
                self.assertEqual(measured['samples'], 0)
                self.assertIsNone(measured['distribution'])
                self.assertTrue(measured['unmeasured_reason'])

    def test_a_synthetic_number_is_never_read_as_a_laptop_number(self):
        card = self.scorecard(seven(), [{'row': 'R1', 'boundary': 'synthetic',
                                         'samples': [10.0, 11.0, 12.0, 13.0, 14.0]}])
        self.assertTrue(card['rows'][0]['boundaries']['synthetic']['measured'])
        laptop = card['rows'][0]['boundaries']['laptop']
        self.assertFalse(laptop['measured'])
        self.assertIn('maintainer hardware', laptop['unmeasured_reason'])

    def test_a_measured_boundary_records_samples_distribution_and_unit(self):
        card = self.scorecard(seven(), [{'row': 'R1', 'boundary': 'synthetic',
                                         'samples': [10.0, 20.0, 30.0, 40.0, 50.0]}])
        item = card['rows'][0]
        measured = item['boundaries']['synthetic']
        self.assertEqual(item['unit'], 'ms')
        self.assertEqual(measured['samples'], 5)
        self.assertEqual(measured['distribution'],
                         {'min': 10.0, 'p50': 30.0, 'p90': 50.0, 'max': 50.0})
        self.assertTrue(measured['valid'])
        self.assertIsNone(measured['unmeasured_reason'])

    def test_too_few_samples_stays_unmeasured(self):
        card = self.scorecard(seven(), [{'row': 'R1', 'boundary': 'synthetic',
                                         'samples': [1.0, 2.0, 3.0, 4.0]}])
        measured = card['rows'][0]['boundaries']['synthetic']
        self.assertFalse(measured['measured'])
        self.assertIn('4 samples', measured['unmeasured_reason'])

    def test_invalid_samples_are_rejected(self):
        for samples in ([float('nan')], [float('inf')], [-1.0], ['12'], [True], []):
            with self.subTest(samples=samples), self.assertRaises(ValueError):
                FELT.load_measurements([self.measurements_file(
                    [{'row': 'R1', 'boundary': 'synthetic', 'samples': samples}])])

    def test_unknown_boundaries_and_duplicates_are_rejected(self):
        with self.assertRaises(ValueError):
            FELT.load_measurements([self.measurements_file(
                [{'row': 'R1', 'boundary': 'desktop', 'samples': [1.0]}])])
        entry = {'row': 'R1', 'boundary': 'synthetic', 'samples': [1.0]}
        with self.assertRaises(ValueError):
            FELT.load_measurements([self.measurements_file([entry, dict(entry)])])

    def test_an_absolute_target_is_decided_against_the_declared_statistic(self):
        target = {'kind': 'absolute', 'op': '<=', 'value': 30}
        for samples, expected in (([10.0, 20.0, 30.0, 40.0, 50.0], True),
                                  ([10.0, 20.0, 31.0, 40.0, 50.0], False)):
            with self.subTest(samples=samples):
                card = self.scorecard(seven(row('R1', synthetic=target)),
                                      [{'row': 'R1', 'boundary': 'synthetic', 'samples': samples}])
                self.assertIs(card['rows'][0]['boundaries']['synthetic']['meets_target'], expected)

    def test_a_relative_target_uses_its_baseline_and_reports_a_missing_one(self):
        target = {'kind': 'relative', 'op': '<=', 'percent': 20,
                  'baseline': {'row': 'R1', 'boundary': 'laptop'}}
        rows = seven(row('R1', synthetic=target))
        card = self.scorecard(rows, [{'row': 'R1', 'boundary': 'synthetic',
                                      'samples': [10.0, 20.0, 30.0, 40.0, 50.0]}])
        synthetic = card['rows'][0]['boundaries']['synthetic']
        self.assertIsNone(synthetic['meets_target'])
        self.assertIn('baseline R1 laptop was not measured', synthetic['target_note'])
        card = self.scorecard(rows, [{'row': 'R1', 'boundary': 'synthetic',
                                      'samples': [10.0, 20.0, 30.0, 40.0, 50.0]},
                                     {'row': 'R1', 'boundary': 'laptop',
                                      'samples': [10.0, 20.0, 30.0, 40.0, 50.0]}])
        self.assertIs(card['rows'][0]['boundaries']['synthetic']['meets_target'], True)

    def test_a_pending_row_is_unmeasured_with_the_pending_reason(self):
        card = self.scorecard(seven(R2=row('R2', name=None, unit=None, statistic=None)))
        for boundary in FELT.BOUNDARIES:
            measured = card['rows'][1]['boundaries'][boundary]
            self.assertFalse(measured['measured'])
            self.assertEqual(measured['unmeasured_reason'], FELT.PENDING)
        self.assertEqual(FELT.validate(card), 7)

    def test_metadata_is_required_and_the_binary_identity_is_recorded(self):
        card = self.scorecard(seven())
        for field in ('source_sha', 'profile', 'runner', 'features'):
            with self.subTest(field=field):
                broken = dict(card, meta=dict(card['meta']))
                broken['meta'][field] = None
                with self.assertRaises(ValueError):
                    FELT.validate(broken)
        binary = self.root / 'oxa'
        binary.write_bytes(b'synthetic binary')
        meta = FELT.collect_meta(argparse.Namespace(
            source_sha='0' * 40, profile='ci', features=['share'], runner_label='hosted',
            binary=binary))
        self.assertEqual(meta['binary']['bytes'], len(b'synthetic binary'))
        self.assertEqual(len(meta['binary']['sha256']), 64)
        self.assertTrue(all(character in '0123456789abcdef' for character in meta['binary']['sha256']))
        self.assertEqual(meta['features'], ['share'])

    def test_a_row_cannot_be_measured_and_unmeasured_at_once(self):
        card = self.scorecard(seven(), [{'row': 'R1', 'boundary': 'synthetic',
                                         'samples': [1.0, 2.0, 3.0, 4.0, 5.0]}])
        card['rows'][0]['boundaries']['synthetic']['unmeasured_reason'] = 'contradiction'
        with self.assertRaises(ValueError):
            FELT.validate(card)
        card = self.scorecard(seven())
        card['rows'][0]['boundaries']['laptop']['unmeasured_reason'] = None
        with self.assertRaises(ValueError):
            FELT.validate(card)

    def test_a_measured_boundary_that_is_not_valid_is_rejected(self):
        card = self.scorecard(seven(), [{'row': 'R1', 'boundary': 'synthetic',
                                         'samples': [1.0, 2.0, 3.0, 4.0, 5.0]}])
        card['rows'][0]['boundaries']['synthetic']['valid'] = False
        with self.assertRaises(ValueError):
            FELT.validate(card)


if __name__ == '__main__':
    unittest.main()
