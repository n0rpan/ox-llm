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
        self.assertIn('needs: [version, build, macos]', dev)
        self.assertNotIn('contents: write', dev.split('  publish:', 1)[0])
        self.assertIn("github.ref == 'refs/heads/main'", dev.split('  publish:', 1)[1])
        self.assertIn('for arch in aarch64 x86_64', dev)
        release_commands = [line.strip() for line in dev.splitlines()
                            if 'gh release create ' in line or 'gh release edit ' in line]
        self.assertEqual(len(release_commands), 2)
        self.assertTrue(all(' dev ' in line for line in release_commands))
        self.assertEqual(dev.count('--prerelease --latest=false'), 2)
        self.assertIn('actions/attest-build-provenance@', dev)

    def test_dev_macos_asset_is_optional_checksummed_and_mach_o(self):
        dev = (W / 'dev-build.yml').read_text()
        macos = dev.split('  macos:', 1)[1].split('  publish:', 1)[0]
        publish = dev.split('  publish:', 1)[1]
        # A red macOS build never blocks the Linux release.
        self.assertIn('continue-on-error: true', macos)
        self.assertIn('runs-on: macos-15', macos)
        self.assertIn('bash packaging/macos/stage.sh', macos)
        self.assertIn('!cancelled() && needs.build.result == \'success\'', publish)
        self.assertNotIn('needs.macos.result', publish)
        # The archive is in dist/ before the checksums and the attestation cover it.
        copy = publish.index('macos-dist/ox-llm-$VERSION-macos-arm64.tar.gz')
        self.assertLess(copy, publish.index('sha256sum -- * > SHA256SUMS'))
        self.assertLess(copy, publish.index('actions/attest-build-provenance@'))
        # ELF is required for Linux archives; Mach-O only for the macOS one.
        self.assertIn("path.name.endswith('-macos-arm64.tar.gz')", publish)
        self.assertIn("b'\\xcf\\xfa\\xed\\xfe'", publish)
        self.assertIn("b'\\x7fELF'", publish)

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


if __name__ == '__main__':
    unittest.main()
