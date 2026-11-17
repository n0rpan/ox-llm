"""Gate controls: a shard cannot pass by dropping tests or printing a runtime skip."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location('report', Path(__file__).resolve().parents[1] / 'check_oxa_report.py')
REPORT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(REPORT)


class ReportControls(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.inventory = self.root / 'list.json'
        self.inventory.write_text(json.dumps({'rust-suites': {'tools': {
            'status': 'listed', 'testcases': {name: {'ignored': False} for name in ('first', 'second')}}}}))

    def report(self, name, cases):
        path = self.root / name
        path.write_text('<testsuites><testsuite>' + cases + '</testsuite></testsuites>')
        return path

    def case(self, name, content=''):
        return f'<testcase classname="tools" name="{name}">{content}</testcase>'

    def test_complete_partitions_pass(self):
        reports = [self.report('one.xml', self.case('first')), self.report('two.xml', self.case('second'))]
        self.assertEqual(REPORT.check(reports, self.inventory), 2)

    def test_missing_duplicate_and_extra_tests_fail(self):
        for cases in (self.case('first'), self.case('first') * 2,
                      self.case('first') + self.case('second') + self.case('unexpected')):
            with self.subTest(cases=cases), self.assertRaises(ValueError):
                REPORT.check([self.report('bad.xml', cases)], self.inventory)

    def test_failed_skipped_and_empty_reports_fail(self):
        for content in ('<failure/>', '<error/>', '<skipped/>',
                        '<system-err>  skipped: bwrap is unavailable</system-err>',
                        '<system-out>skipped: python3 missing</system-out>'):
            with self.subTest(content=content), self.assertRaises(ValueError):
                REPORT.check([self.report('bad.xml', self.case('first', content))])
        with self.assertRaises(ValueError):
            REPORT.check([self.report('empty.xml', '')])
        with self.assertRaises(OSError):
            REPORT.check([self.root / 'missing.xml'])

    def test_only_existing_measurement_ignores_are_allowed(self):
        binary, name = next(iter(REPORT.IGNORED))
        ignored = f'<testcase classname="{binary}" name="{name}"><skipped/></testcase>'
        self.assertEqual(REPORT.check([self.report('allowed.xml', self.case('first') + ignored)]), 1)
        inventory = json.loads(self.inventory.read_text())
        inventory['rust-suites']['tools']['testcases']['new_skip'] = {'ignored': True}
        self.inventory.write_text(json.dumps(inventory))
        with self.assertRaises(ValueError):
            REPORT.check([self.report('bad.xml', self.case('first') + self.case('second'))], self.inventory)

    def test_no_python_proof_is_active_and_cannot_be_skipped(self):
        binary = 'oxa-cli::no_python_in_path'
        name = 'print_uses_native_gateway_without_python_or_controller'
        inventory = json.loads(self.inventory.read_text())
        inventory['rust-suites'][binary] = {
            'status': 'listed', 'testcases': {name: {'ignored': False}}}
        self.inventory.write_text(json.dumps(inventory))
        ordinary = self.case('first') + self.case('second')
        proof = f'<testcase classname="{binary}" name="{name}"/>'
        self.assertEqual(REPORT.check([self.report('active.xml', ordinary + proof)], self.inventory), 3)
        skipped = f'<testcase classname="{binary}" name="{name}"><skipped/></testcase>'
        for ignored in (False, True):
            inventory['rust-suites'][binary]['testcases'][name]['ignored'] = ignored
            self.inventory.write_text(json.dumps(inventory))
            with self.subTest(ignored=ignored), self.assertRaises(ValueError):
                REPORT.check([self.report('skipped-proof.xml', ordinary + skipped)], self.inventory)
