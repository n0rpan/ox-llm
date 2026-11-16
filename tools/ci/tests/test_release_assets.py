import re
import os
import subprocess
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

    def test_arch_recipes_select_the_rust_only_bundle_version(self):
        # Execute recipe metadata without invoking build/package or a package manager.
        for arch in ('aarch64', 'x86_64'):
            for variant in ('ox-llm', 'ox-llm-bin'):
                recipe = W.parents[1] / 'packaging/arch' / variant / 'PKGBUILD'
                result = subprocess.run(['/bin/bash', '-c', '. "$1"; printf "%s\\n" "$pkgver" "${source[0]}"',
                                         'metadata', str(recipe)], env={'PATH':os.defpath, 'CARCH':arch},
                                        capture_output=True, text=True, check=True)
                version, source = result.stdout.splitlines()
                self.assertEqual(version, '0.2.1')
                if variant == 'ox-llm-bin':
                    self.assertEqual(source, f'ox-llm-0.2.1-linux-{arch}.tar.gz')
                else:
                    self.assertEqual(source, 'ox-llm-' + '0' * 40 + '.tar.gz')


if __name__ == '__main__':
    unittest.main()
