"""The engine nextest run cannot silently lose a test that `cargo test` would have run."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location('count', Path(__file__).resolve().parents[1] / 'engine_test_count.py')
COUNT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(COUNT)

LIBTEST = """a::one: test
a::two: test
slow: test
src/lib.rs - Thing (line 4): test
src/lib.rs - Thing::new (line 9): test

3 tests, 0 benchmarks
"""


def inventory(*suites):
    return {'rust-suites': {f'bin{i}': {'testcases': {n: {'ignored': False} for n in names}}
                            for i, names in enumerate(suites)}}


class EngineTestCount(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        (self.root / 'libtest.txt').write_text(LIBTEST)

    def run_count(self, data):
        (self.root / 'nextest.json').write_text(json.dumps(data))
        return COUNT.compare((self.root / 'libtest.txt').read_text(), (self.root / 'nextest.json').read_text())

    def test_same_tests_pass_and_report_counts(self):
        self.assertEqual(self.run_count(inventory(['a::one', 'a::two'], ['slow'])), (3, 2, 3))

    def test_missing_test_fails(self):
        with self.assertRaisesRegex(ValueError, 'slow'):
            self.run_count(inventory(['a::one', 'a::two']))

    def test_extra_test_fails(self):
        with self.assertRaisesRegex(ValueError, 'extra'):
            self.run_count(inventory(['a::one', 'a::two', 'slow', 'extra']))

    def test_duplicate_names_count_per_binary(self):
        with self.assertRaisesRegex(ValueError, 'a::one'):
            self.run_count(inventory(['a::one', 'a::two', 'slow'], ['a::one']))

    def test_empty_listing_fails(self):
        with self.assertRaises(ValueError):
            COUNT.compare('', json.dumps(inventory()))


if __name__ == '__main__':
    unittest.main()
