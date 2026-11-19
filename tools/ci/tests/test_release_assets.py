import re
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

W = Path(__file__).resolve().parents[3] / '.github' / 'workflows'


class ReleaseAssets(unittest.TestCase):
    def test_release_attaches_combined_archives(self):
        rel = (W / 'release.yml').read_text()
        self.assertIn('release-build.yml', rel)
        self.assertIn('for a in aarch64 x86_64', rel)
        self.assertIn('ox-llm-$TAG-linux-$a.tar.gz', rel)
        self.assertIn('ox-llm-$TAG-linux-$a.sbom.cdx.json', rel)
        
        self.assertIn('dist/*.sbom.cdx.json', rel)

    def test_release_build_is_reusable_and_versioned(self):
        rb = (W / 'release-build.yml').read_text()
        self.assertIn('workflow_call:', rb)
        self.assertIsNone(re.search(r'ox-llm-0\.2\.0-linux', rb))

    def test_release_build_does_not_cancel_called_runs(self):
        rb = (W / 'release-build.yml').read_text()
        self.assertIsNone(re.search(r'cancel-in-progress:\s*true', rb))
        self.assertIn("cancel-in-progress: ${{ github.event_name == 'pull_request' }}", rb)
        self.assertIn('release-build-${{ github.workflow }}-${{ github.ref }}', rb)

    def test_dev_uses_the_full_recipe_and_cannot_be_a_stable_release(self):
        dev = (W / 'dev-build.yml').read_text()
        self.assertRegex(dev, r'on:\s*\n  push:\s*\n    branches: \[main\]')
        self.assertNotIn('pull_request', dev)
        # A running dev build finishes; only the newest pending push is kept.
        self.assertIn('group: dev-build\n  cancel-in-progress: false', dev)
        # Publication moves only forward, checked before any create or upload.
        publish = dev.split('  publish:', 1)[1]
        self.assertLess(publish.index('compare/$GITHUB_SHA...main'), publish.index('gh release create dev'))
        self.assertLess(publish.index('compare/$(cat dev-sha)...$GITHUB_SHA'), publish.index('gh release upload dev'))
        self.assertIn('uses: ./.github/workflows/release-build.yml', dev)
        self.assertIn('needs: [version, build]', dev)
        self.assertNotIn('contents: write', dev.split('  publish:', 1)[0])
        self.assertIn("github.ref == 'refs/heads/main'", dev.split('  publish:', 1)[1])
        self.assertIn('for arch in aarch64 x86_64', dev)
        release_commands = [line.strip() for line in dev.splitlines()
                            if 'gh release create ' in line or 'gh release edit ' in line]
        self.assertEqual(len(release_commands), 2)
        self.assertTrue(all(' dev ' in line for line in release_commands))
        self.assertEqual(dev.count('--prerelease --latest=false'), 2)
        self.assertIn('actions/attest-build-provenance@', dev)

    def test_release_environment_resolves_itself_when_sourced_by_a_workflow(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / 'source'
            scripts = root / 'packaging/release'
            scripts.mkdir(parents=True)
            shutil.copyfile(W.parents[1] / 'packaging/release/environment.sh', scripts / 'environment.sh')
            caller = scripts / 'build.sh'
            caller.write_text('source "$(dirname "$0")/environment.sh"\n' + 'printf "%s" "$PWD"\n')
            env = {'PATH': os.defpath, 'HOME': temporary, 'CARGO_HOME':temporary+'/cargo',
                   'RUSTUP_HOME':temporary+'/rustup', 'SOURCE_DATE_EPOCH':'1'}
            for command in (["/bin/bash", str(caller)],
                            ["/bin/bash", "-c", 'source "$1"; printf "%s" "$PWD"',
                             "workflow-step", str(scripts / 'environment.sh')]):
                result = subprocess.run(command, cwd=temporary, env=env,
                                        text=True, capture_output=True, check=True)
                self.assertEqual(result.stdout, str(root))

    def test_arch_consumers_accept_the_versioned_archive(self):
        # release-build names the archive ox-llm-v<version>-linux-<arch>; the PKGBUILD wants the
        # unprefixed pkgver name, so arch-test.sh must map one to the other.
        t = (W.parents[1] / 'packaging/release/arch-test.sh').read_text()
        self.assertIn('ox-llm-v*-linux-$(uname -m).tar.gz', t)
        self.assertIn('ox-llm-0.2.0-linux-$(uname -m).tar.gz', t)

    def test_no_python_check_is_wired_into_every_release_path(self):
        script = (W.parents[1] / 'packaging/release/no-python.sh').read_text()
        self.assertTrue(script.startswith('#!/bin/sh\n'))
        for rule in ('python shebang', 'non-native executable', 'controller payload', 'python invocation'):
            self.assertIn(rule, script)
        self.assertIn('packaging/release/no-python.sh "$artifact"',
                      (W / 'release-build.yml').read_text())
        self.assertIn('packaging/release/no-python.sh:/no-python.sh:ro',
                      (W / 'release-build.yml').read_text())
        self.assertIn('packaging/release/no-python.sh dist/*.tar.gz', (W / 'release.yml').read_text())
        dev = (W / 'dev-build.yml').read_text()
        publish = dev.split('  publish:', 1)[1]
        self.assertLess(publish.index('packaging/release/no-python.sh dist/*.tar.gz'),
                        publish.index('gh release upload dev'))
        arch = (W.parents[1] / 'packaging/release/arch-test.sh').read_text()
        self.assertIn('sh /no-python.sh --list /tmp/installed-files', arch)
        for name in ('dev-build.yml', 'release-build.yml'):
            self.assertNotIn('python3 -I - <<', (W / name).read_text())
            self.assertNotIn('tomllib', (W / name).read_text())

    def test_no_python_script_flags_python_and_passes_native_trees(self):
        script = W.parents[1] / 'packaging/release/no-python.sh'
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            good = root / 'good/usr/bin'
            good.mkdir(parents=True)
            (good / 'oxa').write_bytes(b'\x7fELF\x02\x01')
            (root / 'good/usr/share/licenses').mkdir(parents=True)
            (root / 'good/usr/share/licenses/LICENSE').write_text('python3 is mentioned in a licence\n')
            run = lambda *a: subprocess.run(['/bin/sh', str(script), *a], text=True, capture_output=True)
            self.assertEqual(run(str(root / 'good')).returncode, 0)
            cases = {
                'usr/share/x/tool.py': 'print(1)\n',
                'usr/bin/script': '#!/usr/bin/env python3\n',
                'usr/share/ox-llm/memlat/hook.sh': '#!/bin/sh\nexec /usr/bin/python3 x\n',
                'usr/share/ox-llm/oxc/a': 'x\n',
            }
            for name, text in cases.items():
                bad = root / ('bad-' + str(len(name)))
                shutil.copytree(root / 'good', bad)
                (bad / name).parent.mkdir(parents=True, exist_ok=True)
                (bad / name).write_text(text)
                self.assertEqual(run(str(bad)).returncode, 1, name)


if __name__ == '__main__':
    unittest.main()
