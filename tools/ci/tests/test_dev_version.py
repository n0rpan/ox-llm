import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('dev_version', Path(__file__).resolve().parents[1] / 'dev_version.py')
dev_version = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dev_version)


class DevVersion(unittest.TestCase):
    def test_bumps_past_latest_stable_tag(self):
        self.assertEqual(dev_version.bump(['v0.1.9', 'v0.2.0'], '0.2.0', 'abc1234', 137), 'v0.2.1-dev.137+abc1234')
        self.assertEqual(dev_version.bump(['v0.2.0', 'v0.2.1'], '0.2.0', 'abc1234', 5), 'v0.2.2-dev.5+abc1234')

    def test_semver_order_not_lexical_and_prereleases_ignored(self):
        tags = ['v0.2.9', 'v0.2.10', 'v0.3.0-rc.1', 'v0.2.0-dev.abc1234', 'dev', 'v1.0']
        self.assertEqual(dev_version.bump(tags, '0.2.0', 'abc1234', 1), 'v0.2.11-dev.1+abc1234')

    def test_missing_stable_tag_uses_workspace_version(self):
        self.assertEqual(dev_version.bump([], '0.2.0', 'abc1234', 3), 'v0.2.0-dev.3+abc1234')
        self.assertEqual(dev_version.bump(['dev', 'v0.3.0-rc.1'], '0.2.0', 'abc1234', 3), 'v0.2.0-dev.3+abc1234')

    def test_dev_is_newer_than_its_release(self):
        self.assertGreater(dev_version.latest_stable(['v0.2.1']), dev_version.latest_stable(['v0.2.0']))

    def test_file_name_version_has_no_plus(self):
        self.assertEqual(dev_version.file_name_version('v0.2.2-dev.137+6955919'), 'v0.2.2-dev.137.6955919')

    def test_dev_versions_sort_by_semver(self):
        def key(version):
            core, pre = version.lstrip('v').split('+')[0].partition('-dev.')[::2]
            numbers = tuple(int(part) for part in core.split('.'))
            return (numbers, 0, int(pre)) if pre else (numbers, 1, 0)
        ordered = ['v0.2.2-dev.9+ffffff0', 'v0.2.2-dev.10+0000001', 'v0.2.2-dev.137+6955919', 'v0.2.2']
        self.assertEqual(sorted(ordered, key=key), ordered)

    def test_commit_count_since_stable_tag_in_a_real_repo(self):
        import subprocess, tempfile
        with tempfile.TemporaryDirectory() as tmp:
            def git(*args):
                subprocess.run(['git', '-C', tmp, '-c', 'user.name=t', '-c', 'user.email=t@t', '-c', 'commit.gpgsign=false',
                                '-c', 'core.hooksPath=/dev/null', *args], check=True, capture_output=True)
            git('init', '-q')
            for index in range(3):
                git('commit', '-q', '--allow-empty', '-m', str(index))
            self.assertEqual(dev_version.commit_count(tmp, []), 3)
            git('tag', 'v0.2.0')
            git('commit', '-q', '--allow-empty', '-m', 'after')
            git('commit', '-q', '--allow-empty', '-m', 'after2')
            self.assertEqual(dev_version.commit_count(tmp, ['v0.2.0']), 2)


if __name__ == '__main__':
    unittest.main()
