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
