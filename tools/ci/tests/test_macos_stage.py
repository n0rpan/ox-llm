"""Exercise packaging/macos/stage.sh with a fake Mach-O file; runs on Linux and macOS."""
import os
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[3] / 'packaging/macos/stage.sh'
VERSION = 'v0.2.1-dev.137.abcdef0'


class MacosStage(unittest.TestCase):
    def run_stage(self, content, version=VERSION, env=None):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        binary = root / 'oxa'
        binary.write_bytes(content)
        binary.chmod(0o755)
        proc = subprocess.run(['bash', str(SCRIPT), version, str(binary), str(root / 'out')],
                              capture_output=True, text=True, env=env)
        return root, proc

    def test_archive_has_exactly_the_layout_the_installer_extracts(self):
        root, proc = self.run_stage(b'\xcf\xfa\xed\xfe' + b'\0' * 64)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        archive = root / 'out' / f'ox-llm-{VERSION}-macos-arm64.tar.gz'
        with tarfile.open(archive) as tar:
            members = {m.name.rstrip('/'): m for m in tar.getmembers()}
        self.assertEqual(sorted(members), ['usr', 'usr/bin', 'usr/bin/oxa'])
        self.assertTrue(members['usr/bin/oxa'].isfile())
        self.assertEqual(members['usr/bin/oxa'].mode & 0o777, 0o755)
        self.assertEqual((members['usr/bin/oxa'].uid, members['usr/bin/oxa'].gid), (0, 0))

    def test_uses_bsdtar_numeric_ownership_flags_on_macos(self):
        with tempfile.TemporaryDirectory() as fake_dir:
            fake_tar = Path(fake_dir) / 'tar'
            real_tar = shutil.which('tar')
            self.assertIsNotNone(real_tar)
            fake_tar.write_text(f'''#!/usr/bin/env bash
if [[ $1 == --version ]]; then echo 'bsdtar 3.5.3'; exit 0; fi
args=()
while (($#)); do
  case $1 in
    --owner*|--group*|--numeric-owner) echo 'GNU ownership flag passed to bsdtar' >&2; exit 64 ;;
    --uid) args+=(--owner="$2"); shift 2 ;;
    --gid) args+=(--group="$2"); shift 2 ;;
    *) args+=("$1"); shift ;;
  esac
done
exec {real_tar} "${{args[@]}}"
''')
            fake_tar.chmod(0o755)
            env = os.environ.copy()
            env['PATH'] = fake_dir + os.pathsep + env['PATH']
            _, proc = self.run_stage(b'\xcf\xfa\xed\xfe' + b'\0' * 64, env=env)
            self.assertEqual(proc.returncode, 0, proc.stderr)

    def test_refuses_a_non_mach_o_binary(self):
        root, proc = self.run_stage(b'\x7fELF' + b'\0' * 64)
        self.assertNotEqual(proc.returncode, 0)
        self.assertFalse(list((root / 'out').glob('*')) if (root / 'out').exists() else [])

    def test_accepts_release_tags(self):
        for version in ('v0.2.2', 'v0.3.0-rc.1'):
            with self.subTest(version=version):
                root, proc = self.run_stage(b'\xcf\xfa\xed\xfe' + b'\0' * 64, version=version)
                self.assertEqual(proc.returncode, 0, proc.stderr)
                self.assertTrue((root / 'out' / f'ox-llm-{version}-macos-arm64.tar.gz').is_file())

    def test_refuses_a_malformed_version(self):
        for version in ('0.2.1', 'v0.2.1-dev.abcdef0', 'v0.2.1-dev.137+abcdef0', 'v0.2.2+meta',
                        'v0.2.2/../x'):
            with self.subTest(version=version):
                root, proc = self.run_stage(b'\xcf\xfa\xed\xfe', version=version)
                self.assertNotEqual(proc.returncode, 0)
                self.assertFalse((root / 'out').exists())


if __name__ == '__main__':
    unittest.main()
