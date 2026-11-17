"""The formatter helper picks the workspace from its caller; a synthetic cargo records the run."""
import shutil
import subprocess
import tempfile
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[3]
HELPER = ROOT / 'tools/ci/fmt_check.sh'


class FmtCheck(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def checkout(self, name='checkout'):
        """A synthetic checkout: the helper at tools/ci and cargo only in an isolated CARGO_HOME."""
        checkout = self.root / name
        script = checkout / 'tools/ci/fmt_check.sh'
        script.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(HELPER, script)
        home = checkout / 'home'
        cargo_bin = home / 'cargo/bin'
        cargo_bin.mkdir(parents=True, exist_ok=True)
        log = self.root / f'{name}-invocation'
        cargo = cargo_bin / 'cargo'
        cargo.write_text('#!/bin/sh\nprintf "%s\\n" "$PWD" "$@" > "$FMT_TEST_LOG"\n')
        cargo.chmod(0o700)
        # Cargo is absent from PATH; only the isolated CARGO_HOME supplies it.
        path_bin = checkout / 'path-bin'
        path_bin.mkdir(exist_ok=True)
        for tool in ('getent', 'id', 'cut', 'dirname', 'grep'):
            (path_bin / tool).symlink_to(shutil.which(tool))
        env = {'PATH': str(path_bin), 'HOME': str(home),
               'CARGO_HOME': str(home / 'cargo'), 'FMT_TEST_LOG': str(log)}
        return checkout, script, env, log

    def workspace(self, directory):
        directory.mkdir(parents=True, exist_ok=True)
        (directory / 'Cargo.toml').write_text('[workspace]\nmembers = []\n')
        return directory

    def run_helper(self, script, cwd, env):
        return subprocess.run(['/bin/sh', str(script)], cwd=cwd, env=env,
                              capture_output=True, text=True, timeout=10)

    def checked(self, script, cwd, env, log):
        result = self.run_helper(script, cwd, env)
        self.assertEqual(result.returncode, 0, result.stderr)
        ran = log.read_text().splitlines()
        self.assertEqual(ran[1:], ['fmt', '--check'], 'the check itself must not change')
        return Path(ran[0])

    def test_member_directory_checks_the_workspace_above_it(self):
        checkout, script, env, log = self.checkout()
        workspace = self.workspace(checkout / 'rust')
        member = workspace / 'crates/app/src'
        member.mkdir(parents=True)
        self.assertEqual(self.checked(script, member, env, log), workspace)

    def test_checkout_root_falls_back_to_the_only_workspace(self):
        # The root of a repository holds no workspace itself, and the workspace folder is not
        # required to be named after this one.
        checkout, script, env, log = self.checkout()
        workspace = self.workspace(checkout / 'rust')
        self.assertEqual(self.checked(script, checkout, env, log), workspace)

    def test_caller_workspace_wins_over_the_helper_checkout(self):
        # The defect: a helper that resolves the workspace beside itself checks the checkout it
        # was copied from instead of the tree the command runs in.
        helper, script, env, log = self.checkout('helper')
        self.workspace(helper / 'oxa')
        caller, _, _, _ = self.checkout('caller')
        workspace = self.workspace(caller / 'oxa')
        member = workspace / 'crates/app/src'
        member.mkdir(parents=True)
        self.assertEqual(self.checked(script, member, env, log), workspace)

    def test_root_prefers_the_workspace_beside_the_helper(self):
        # The repository root holds several workspaces; the helper's own checkout names the one
        # the formatter is for.
        checkout, script, env, log = self.checkout()
        self.workspace(checkout / 'engine')
        workspace = self.workspace(checkout / 'oxa')
        self.assertEqual(self.checked(script, checkout, env, log), workspace)

    def test_no_workspace_is_refused_without_running_cargo(self):
        checkout, script, env, log = self.checkout()
        result = self.run_helper(script, checkout, env)
        self.assertEqual(result.returncode, 1)
        self.assertIn('workspace', result.stderr)
        self.assertFalse(log.exists(), 'cargo must not run without a workspace')

    def test_repository_helper_checks_this_checkout(self):
        checkout, _, env, log = self.checkout()
        self.assertEqual(self.checked(HELPER, ROOT, env, log), ROOT / 'oxa')


if __name__ == '__main__':
    unittest.main()
