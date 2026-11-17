"""The `cargo fmt --check` wrapper: `cargo fmt` looks for `cargo-fmt` on PATH (issue #983)."""
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
            # Records what the check would have seen: the arguments, the working directory, and
            # whether cargo can resolve its `fmt` external subcommand on PATH.
            'printf "cwd=%s\\nargs=%s\\nfmt=%s\\n" "$PWD" "$*" '
            '"$(command -v cargo-fmt 2>/dev/null || echo missing)" >> "$FMT_LOG"\n')

    def fake_cargo_fmt(self, directory):
        return self.executable(directory / 'cargo-fmt', 'exit 0\n')

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
