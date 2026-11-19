import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('dev_version', Path(__file__).resolve().parents[1] / 'dev_version.py')
dev_version = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dev_version)


class DevVersion(unittest.TestCase):
    def test_bumps_past_latest_stable_tag(self):
        self.assertEqual(dev_version.bump(['v0.1.9', 'v0.2.0'], '0.2.0', 'abc1234'), 'v0.2.1-dev.abc1234')
        self.assertEqual(dev_version.bump(['v0.2.0', 'v0.2.1'], '0.2.0', 'abc1234'), 'v0.2.2-dev.abc1234')

    def test_semver_order_not_lexical_and_prereleases_ignored(self):
        tags = ['v0.2.9', 'v0.2.10', 'v0.3.0-rc.1', 'v0.2.0-dev.abc1234', 'dev', 'v1.0']
        self.assertEqual(dev_version.bump(tags, '0.2.0', 'abc1234'), 'v0.2.11-dev.abc1234')

    def test_missing_stable_tag_uses_workspace_version(self):
        self.assertEqual(dev_version.bump([], '0.2.0', 'abc1234'), 'v0.2.0-dev.abc1234')
        self.assertEqual(dev_version.bump(['dev', 'v0.3.0-rc.1'], '0.2.0', 'abc1234'), 'v0.2.0-dev.abc1234')

    def test_dev_is_newer_than_its_release(self):
        self.assertGreater(dev_version.latest_stable(['v0.2.1']), dev_version.latest_stable(['v0.2.0']))


if __name__ == '__main__':
    unittest.main()
