"""The `cargo fmt --check` wrapper: `cargo fmt` looks for `cargo-fmt` on PATH (issue #983)."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'fmt_check.sh'


class FmtCheck(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.repo = self.root / 'repo'
        self.workspace = self.repo / 'oxa'
        (self.workspace / 'oxa-core' / 'src').mkdir(parents=True)
        (self.workspace / 'Cargo.toml').write_text('[workspace]\nmembers = ["oxa-core"]\n')
        (self.workspace / 'oxa-core' / 'Cargo.toml').write_text('[package]\nname = "oxa-core"\n')
        self.log = self.root / 'calls.log'
        # A confined PATH keeps the account-home lookup (getent, id) out of the test, so a
        # synthetic layout is the only toolchain the wrapper can see.
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        for tool in ('sh', 'grep', 'cut'):
            (self.bin / tool).symlink_to(shutil.which(tool))

    def executable(self, path, body):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text('#!/bin/sh\n' + body)
        path.chmod(0o755)
        return path

    def fake_cargo(self, directory):
        return self.executable(
            directory / 'cargo',
            'printf "cwd=%s\\nargs=%s\\nfmt=%s\\ncargo=%s\\n" "$PWD" "$*" '
            '"$(command -v cargo-fmt 2>/dev/null || echo missing)" "$0" >> "$FMT_LOG"\n'
            'exec cargo-fmt "$@"\n')

    def fake_cargo_fmt(self, directory, rustfmt=True):
        if rustfmt:
            self.executable(directory / 'rustfmt', 'exit 0\n')
        return self.executable(directory / 'cargo-fmt', 'exec rustfmt "$@"\n')

    def account(self, home):
        # A synthetic account home: no literal paths, and the real machine's toolchains are
        # outside the confined PATH this test runs with.
        self.executable(self.bin / 'id', 'echo 1000\n')
        self.executable(self.bin / 'getent',
                        'printf "u:x:1000:1000::%s:/bin/sh\\n" "{}"\n'.format(home))

    def run_check(self, cwd, path=None):
        env = {'PATH': os.pathsep.join(str(p) for p in (path or [self.bin])),
               'HOME': str(self.root / 'home'), 'FMT_LOG': str(self.log), 'LANG': 'C'}
        return subprocess.run(['/bin/sh', str(SCRIPT)], cwd=str(cwd), env=env,
                              text=True, capture_output=True)

    def calls(self):
        return self.log.read_text() if self.log.exists() else ''

    def test_cargo_fmt_is_put_on_path_when_it_is_not_beside_cargo(self):
        # The toolchain that supplies cargo and the one that supplies cargo-fmt differ: the
        # external subcommand is found through PATH, not beside the cargo binary.
        only_cargo = self.root / 'cargo-only'
        self.fake_cargo(only_cargo)
        toolchain = self.root / 'home' / '.rustup' / 'toolchains' / 'pinned' / 'bin'
        self.fake_cargo_fmt(toolchain)

        done = self.run_check(self.workspace, path=[self.bin, only_cargo])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn('args=fmt --check', self.calls())
        self.assertIn('fmt=' + str(toolchain / 'cargo-fmt'), self.calls())

    def test_missing_cargo_fmt_names_the_external_subcommand(self):
        self.fake_cargo(self.bin)
        done = self.run_check(self.workspace)
        self.assertEqual(done.returncode, 127, done.stderr)
        self.assertIn('cargo-fmt', done.stderr)
        self.assertIn('PATH', done.stderr)
        self.assertEqual(self.calls(), '', 'cargo must not run without its fmt subcommand')

    def test_runs_the_check_from_the_declaring_workspace_root(self):
        self.fake_cargo(self.bin)
        self.fake_cargo_fmt(self.bin)
        done = self.run_check(self.workspace / 'oxa-core')
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn('cwd=' + str(self.workspace), self.calls())
        self.assertIn('args=fmt --check', self.calls())

    def test_account_home_toolchain_is_searched_when_the_sandbox_home_has_none(self):
        # A session sandbox points HOME at a directory of its own; the toolchain that rustup
        # installed lives in the account home from the passwd database.
        self.account(self.root / 'account')
        toolchain = self.root / 'account' / '.rustup' / 'toolchains' / 'pinned' / 'bin'
        self.fake_cargo(toolchain)
        self.fake_cargo_fmt(toolchain)

        done = self.run_check(self.workspace)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn('args=fmt --check', self.calls())

    def test_missing_rustfmt_is_reported_before_cargo_runs(self):
        self.fake_cargo(self.bin)
        self.fake_cargo_fmt(self.bin, rustfmt=False)
        done = self.run_check(self.workspace)
        self.assertEqual(done.returncode, 127, done.stderr)
        self.assertIn('rustfmt', done.stderr)
        self.assertEqual(self.calls(), '')

    def test_finding_companions_does_not_replace_cargo_already_on_path(self):
        active = self.fake_cargo(self.bin)
        tools = self.root / 'home' / '.rustup' / 'toolchains' / 'other' / 'bin'
        self.fake_cargo_fmt(tools)
        self.executable(tools / 'cargo', 'echo wrong-cargo >&2\nexit 73\n')
        done = self.run_check(self.workspace)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn('cargo=' + str(active), self.calls())

    def test_requested_toolchain_precedes_lexicographic_install_order(self):
        root = self.root / 'home' / '.rustup'
        self.fake_cargo(root / 'toolchains' / 'aaa-old' / 'bin')
        self.fake_cargo_fmt(root / 'toolchains' / 'aaa-old' / 'bin')
        selected = root / 'toolchains' / 'selected' / 'bin'
        self.fake_cargo(selected)
        self.fake_cargo_fmt(selected)
        env = {'PATH': str(self.bin), 'HOME': str(self.root / 'home'),
               'RUSTUP_TOOLCHAIN': 'selected', 'FMT_LOG': str(self.log)}
        done = subprocess.run(['/bin/sh', str(SCRIPT)], cwd=self.workspace, env=env,
                              text=True, capture_output=True, timeout=10)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn('cargo=' + str(selected / 'cargo'), self.calls())

    def test_default_toolchain_precedes_lexicographic_install_order(self):
        root = self.root / 'home' / '.rustup'
        self.fake_cargo(root / 'toolchains' / 'aaa-old' / 'bin')
        self.fake_cargo_fmt(root / 'toolchains' / 'aaa-old' / 'bin')
        selected = root / 'toolchains' / 'selected' / 'bin'
        self.fake_cargo(selected)
        self.fake_cargo_fmt(selected)
        (root / 'settings.toml').write_text('default_toolchain = "selected"\n')
        done = self.run_check(self.workspace)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn('cargo=' + str(selected / 'cargo'), self.calls())

ROOT = Path(__file__).resolve().parents[3]
HELPER = ROOT / 'tools/ci/fmt_check.sh'


class CallerWorkspace(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def checkout(self, name='checkout'):
        """A synthetic checkout: the helper at tools/ci and cargo only in an isolated CARGO_HOME."""
        checkout = self.root / name
        script = checkout / 'tools/ci/fmt_check.sh'
        script.parent.mkdir(parents=True, exist_ok=True)
        (checkout / '.git').write_text('gitdir: synthetic-worktree\n')
        shutil.copyfile(HELPER, script)
        home = checkout / 'home'
        cargo_bin = home / 'cargo/bin'
        cargo_bin.mkdir(parents=True, exist_ok=True)
        log = self.root / f'{name}-invocation'
        cargo = cargo_bin / 'cargo'
        cargo.write_text('#!/bin/sh\nprintf "%s\\n" "$PWD" "$@" > "$FMT_TEST_LOG"\n')
        cargo.chmod(0o700)
        for companion in ('cargo-fmt', 'rustfmt'):
            tool = cargo_bin / companion
            tool.write_text('#!/bin/sh\nexit 0\n')
            tool.chmod(0o700)
        # Cargo is absent from PATH; only the isolated CARGO_HOME supplies it.
        path_bin = checkout / 'path-bin'
        path_bin.mkdir(exist_ok=True)
        for tool in ('id', 'cut', 'dirname', 'grep'):
            (path_bin / tool).symlink_to(shutil.which(tool))
        getent = path_bin / 'getent'
        getent.write_text('#!/bin/sh\necho "u:x:1000:1000::/nonexistent:/bin/sh"\n')
        getent.chmod(0o700)
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


    def test_symlink_caller_uses_the_physical_workspace(self):
        checkout, script, env, log = self.checkout()
        self.workspace(checkout / 'oxa')
        workspace = self.workspace(checkout / 'rust')
        member = workspace / 'member' / 'src'
        member.mkdir(parents=True)
        link = checkout / 'link'
        link.symlink_to(member, target_is_directory=True)
        result = subprocess.run(['/bin/sh', '-c', 'cd "$1" && exec /bin/sh "$2"',
                                 'sh', str(link), str(script)], cwd=checkout, env=env,
                                capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(Path(log.read_text().splitlines()[0]), workspace)

    def test_formatter_failure_exit_status_is_preserved(self):
        checkout, script, env, log = self.checkout()
        workspace = self.workspace(checkout / 'oxa')
        cargo = Path(env['CARGO_HOME']) / 'bin' / 'cargo'
        cargo.write_text('#!/bin/sh\nexit 37\n')
        self.assertEqual(self.run_helper(script, workspace, env).returncode, 37)


class DoneWiring(unittest.TestCase):
    def test_done_check_invokes_the_helper(self):
        done = json.loads((ROOT / '.oxa/settings.json').read_text())['done']
        self.assertIn('in oxa: ../tools/ci/fmt_check.sh', done)
        self.assertNotIn('in oxa: cargo fmt --check', done)

    def test_repo_done_assertion_names_the_declared_entry(self):
        done = json.loads((ROOT / '.oxa/settings.json').read_text())['done']
        self.assertTrue(done, done)
        source = (ROOT / 'oxa/oxa-config/tests/repo_done_checks.rs').read_text()
        for entry in done:
            self.assertIn(entry, source)


if __name__ == '__main__':
    unittest.main()
