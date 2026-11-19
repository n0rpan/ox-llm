"""Exercise packaging/macos/stage.sh with a fake Mach-O file; runs on Linux and macOS."""
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[3] / 'packaging/macos/stage.sh'
VERSION = 'v0.2.1-dev.abcdef0'


class MacosStage(unittest.TestCase):
    def run_stage(self, content, version=VERSION):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        binary = root / 'oxa'
        binary.write_bytes(content)
        binary.chmod(0o755)
        proc = subprocess.run(['bash', str(SCRIPT), version, str(binary), str(root / 'out')],
                              capture_output=True, text=True)
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

    def test_refuses_a_non_mach_o_binary(self):
        root, proc = self.run_stage(b'\x7fELF' + b'\0' * 64)
        self.assertNotEqual(proc.returncode, 0)
        self.assertFalse(list((root / 'out').glob('*')) if (root / 'out').exists() else [])

    def test_refuses_a_malformed_version(self):
        _, proc = self.run_stage(b'\xcf\xfa\xed\xfe', version='0.2.1')
        self.assertNotEqual(proc.returncode, 0)


if __name__ == '__main__':
    unittest.main()
