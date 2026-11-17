"""Exercise the docs index's shell sequence without compiling or contacting an engine."""
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[3]


class DocsQuickstartTests(unittest.TestCase):
    def test_index_build_and_launch_from_project(self):
        page = (ROOT / 'oxa/docs/README.md').read_text()
        quickstart = page.split('## Quickstart\n', 1)[1]
        instructions, rest = quickstart.split('```console\n', 1)
        commands = rest.split('```', 1)[0]
        script = '\n'.join(line.removeprefix('$ ') for line in commands.splitlines())

        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            checkout = base / 'checkout'
            workspace = checkout / 'oxa'
            workspace.mkdir(parents=True)
            # Use the real manifests to distinguish the repository and workspace roots.
            (workspace / 'Cargo.toml').write_bytes((ROOT / 'oxa/Cargo.toml').read_bytes())
            (workspace / 'oxa-cli').mkdir()
            (workspace / 'oxa-cli/Cargo.toml').write_bytes(
                (ROOT / 'oxa/oxa-cli/Cargo.toml').read_bytes())
            home = base / 'home'
            project = home / 'src/my-project'
            project.mkdir(parents=True)
            (home / 'project').mkdir()
            bin_dir = base / 'bin'
            bin_dir.mkdir()
            cargo = bin_dir / 'cargo'
            # Stand in only for compilation: require the package's real location,
            # then leave an executable where Cargo would put the release binary.
            cargo.write_text('''#!/bin/sh
set -eu
test -f Cargo.toml && test -f oxa-cli/Cargo.toml || {
    echo "cargo build ran outside the oxa workspace" >&2
    exit 1
}
mkdir -p target/release
printf '#!/bin/sh\npwd\n' > target/release/oxa
chmod +x target/release/oxa
''')
            cargo.chmod(0o755)
            if 'repository root' in instructions:
                start = checkout
            else:
                self.assertIn('`oxa/` directory', instructions)
                start = workspace
            result = subprocess.run(
                ['/bin/sh', '-eu', '-c', script], cwd=start,
                env={'HOME': str(home), 'PATH': f'{bin_dir}:/usr/bin:/bin'},
                text=True, capture_output=True, check=False)
            self.assertEqual(result.returncode, 0, result.stderr)
            launched_from = Path(result.stdout.strip())
            self.assertTrue(launched_from.is_relative_to(home), result.stdout)
            self.assertNotEqual(launched_from, home)


if __name__ == '__main__':
    unittest.main()
