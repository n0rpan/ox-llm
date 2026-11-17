"""The declared formatter uses a synthetic cargo; no compiler or user HOME is used."""
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[3]


class FmtCheck(unittest.TestCase):
    def test_done_check_invokes_the_helper(self):
        done = json.loads((ROOT / '.oxa/settings.json').read_text())['done']
        self.assertIn('in oxa: ../tools/ci/fmt_check.sh', done)
        self.assertNotIn('in oxa: cargo fmt --check', done)

    def test_root_member_and_linked_checkout_use_oxa_workspace(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            checkout = root / 'checkout'
            script = checkout / 'tools/ci/fmt_check.sh'
            script.parent.mkdir(parents=True)
            shutil.copyfile(ROOT / 'tools/ci/fmt_check.sh', script)
            workspace = checkout / 'oxa'
            member = workspace / 'member/src'
            member.mkdir(parents=True)
            (workspace / 'Cargo.toml').write_text('[workspace]\nmembers = []\n')
            (checkout / '.git').write_text('gitdir: synthetic-worktree\n')
            home = root / 'home'
            cargo_bin = home / 'cargo/bin'
            cargo_bin.mkdir(parents=True)
            log = root / 'invocation'
            cargo = cargo_bin / 'cargo'
            cargo.write_text('#!/bin/sh\nprintf "%s\\n" "$PWD" "$@" > "$FMT_TEST_LOG"\n')
            cargo.chmod(0o700)
            # Cargo is absent from PATH; only the isolated CARGO_HOME supplies it.
            path_bin = root / 'path-bin'
            path_bin.mkdir()
            for name in ('getent', 'id', 'cut', 'dirname', 'grep'):
                (path_bin / name).symlink_to(shutil.which(name))
            env = {'PATH': str(path_bin), 'HOME': str(home),
                   'CARGO_HOME': str(home / 'cargo'), 'FMT_TEST_LOG': str(log)}
            for cwd in (checkout, workspace, member):
                result = subprocess.run(['/bin/sh', str(script)], cwd=cwd, env=env,
                                        capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(log.read_text().splitlines(),
                                 [str(workspace), 'fmt', '--check'])
