import json
import os
import re
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
        # The same no-python scan covers the macOS archive, which may hold Mach-O only.
        self.assertLess(copy, publish.index('packaging/release/no-python.sh dist/*.tar.gz'))
        script = (W.parents[1] / 'packaging/release/no-python.sh').read_text()
        self.assertIn('*-macos-arm64.tar.gz) native=cffaedfe', script)

    def test_dev_version_extraction_handles_spaces_around_assignment(self):
        dev = (W / 'dev-build.yml').read_text()
        self.assertIn('split($0,a,"=");gsub(/[" ]/,"",a[2]);print a[2]', dev)
        command = r'''awk '/^\[workspace\.package\]/{p=1;next} /^\[/{p=0} p&&/^version[ ]*=/{split($0,a,"=");gsub(/[" ]/,"",a[2]);print a[2];exit}' oxa/Cargo.toml'''
        result = subprocess.run(['/bin/sh', '-c', command], cwd=W.parents[1],
                                text=True, capture_output=True, check=True)
        self.assertRegex(result.stdout.strip(), r'^\d+\.\d+\.\d+[0-9A-Za-z.+-]*$')

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

    def test_release_build_needs_no_python(self):
        root = W.parents[1]
        rb = (W / 'release-build.yml').read_text()
        for gone in ('sbom.py', 'smoke.py', 'python3 -B', 'python3 -I -'):
            self.assertNotIn(gone, rb)
        self.assertIn('packaging/release/sbom.sh /build/payload', rb)
        self.assertIn('packaging/release/smoke.sh:/smoke.sh:ro', rb)
        self.assertIn('bash packaging/release/no-python-installers.sh', rb)
        self.assertFalse((root / 'packaging/release/sbom.py').exists())
        self.assertFalse((root / 'packaging/release/smoke.py').exists())
        self.assertNotRegex((root / 'packaging/release/setup.sh').read_text(), r'\bpython')
        arch = (root / 'packaging/release/arch-test.sh').read_text()
        self.assertNotIn('python -B', arch)
        self.assertIn('bash /smoke.sh', arch)
        self.assertNotRegex(arch.split('pacman -Syu', 1)[1].splitlines()[0], r'\bpython\b')
        self.assertIn('(depend|makedepend|checkdepend) = python', arch)

    def test_sbom_preserves_sources_and_json_encodes_payload_names(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / 'source'
            scripts = root / 'packaging/release'
            scripts.mkdir(parents=True)
            shutil.copyfile(W.parents[1] / 'packaging/release/sbom.sh', scripts / 'sbom.sh')
            (scripts / 'sbom.sh').chmod(0o755)
            (root / 'engine').mkdir()
            (root / 'engine/Cargo.toml').write_text('[workspace]\n')
            (root / 'engine/Cargo.lock').write_text('''
[[package]]
name = "same"
version = "1.0.0"
source = "registry+https://example.invalid/index"
checksum = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

[[package]]
name = "same"
version = "1.0.0"
source = "git+https://example.invalid/repo#0123456789abcdef"
''')
            fakebin = root / 'fakebin'
            fakebin.mkdir()
            cargo = fakebin / 'cargo'
            cargo.write_text('''#!/bin/sh
cat <<EOF
{"packages":[
 {"name":"same","version":"1.0.0","source":"registry+https://example.invalid/index","manifest_path":"/cargo/one/Cargo.toml","license":"MIT"},
 {"name":"same","version":"1.0.0","source":"git+https://example.invalid/repo#0123456789abcdef","manifest_path":"/cargo/two/Cargo.toml","license":null},
 {"name":"not-a-feature","version":"2.0.0","source":null,"manifest_path":"$PWD/member/Cargo.toml","license":null}
]}
EOF
''')
            cargo.chmod(0o755)
            payload = root / 'payload'
            payload.mkdir()
            unusual = 'back\\slash "quote"'
            (payload / unusual).write_bytes(b'payload')
            output = root / 'sbom.json'
            env = os.environ.copy()
            env['VERSION'] = 'v9.8.7'
            env['PATH'] = f'{fakebin}:{env["PATH"]}'
            subprocess.run([scripts / 'sbom.sh', payload, output],
                           cwd=root, env=env, check=True)
            document = json.loads(output.read_text())

            self.assertEqual(document['metadata']['component']['version'], '9.8.7')
            files = [item for item in document['components'] if item['type'] == 'file']
            self.assertEqual([item['name'] for item in files], [unusual])
            self.assertRegex(files[0]['hashes'][0]['content'], r'^[0-9a-f]{64}$')
            libraries = [item for item in document['components'] if item['type'] == 'library']
            self.assertFalse(any(item['version'] == 'eature' for item in libraries))
            same = [item for item in libraries if item['name'] == 'same']
            self.assertEqual(len(same), 2)
            self.assertEqual(len({item['purl'] for item in same}), 2)
            registry = next(item for item in same if 'registry%2B' in item['purl'])
            self.assertEqual(registry['hashes'][0]['content'], 'a' * 64)
            self.assertEqual(len({item['bom-ref'] for item in document['components']}),
                             len(document['components']))

    def test_smoke_tracks_the_execed_server_process(self):
        smoke = (W.parents[1] / 'packaging/release/smoke.sh').read_text()
        self.assertRegex(smoke, r'run_server\(\) \{ exec env -i ')
        self.assertIn('run_server oxe-server --engine stub', smoke)

    def test_installer_check_flags_python_recipes_and_scripts(self):
        script = W.parents[1] / 'packaging/release/no-python-installers.sh'
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'packaging/release').mkdir(parents=True)
            (root / 'packaging/arch/x').mkdir(parents=True)
            copied = root / 'packaging/release/no-python-installers.sh'
            shutil.copyfile(script, copied)
            recipe = root / 'packaging/arch/x/PKGBUILD'
            helper = root / 'packaging/release/helper.sh'
            run = lambda: subprocess.run(['/bin/bash', str(copied)], text=True, capture_output=True)
            recipe.write_text("depends=('glibc')\nmakedepends=('cargo' 'cmake')\n")
            helper.write_text('# python3 only in a comment\nbash stage.sh\n')
            self.assertEqual(run().returncode, 0, run().stderr)
            recipe.write_text("depends=('glibc')\nmakedepends=('cargo'\n 'python')\n")
            self.assertEqual(run().returncode, 1)
            recipe.write_text("depends=('glibc')\n")
            helper.write_text('python3 -B tool.py\n')
            self.assertEqual(run().returncode, 1)

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
                'usr/share/ox-llm/hooks/plain.sh': '#!/bin/sh\nexec python "$@"\n',
                'usr/share/ox-llm/oxc/a': 'x\n',
            }
            for index, (name, text) in enumerate(cases.items()):
                bad = root / f'bad-{index}'
                shutil.copytree(root / 'good', bad)
                (bad / name).parent.mkdir(parents=True, exist_ok=True)
                (bad / name).write_text(text)
                self.assertEqual(run(str(bad)).returncode, 1, name)

            linked = root / 'linked'
            shutil.copytree(root / 'good', linked)
            (linked / 'usr/bin/oxa').unlink()
            (linked / 'usr/bin/oxa').symlink_to('/usr/bin/python3')
            result = run(str(linked))
            self.assertEqual(result.returncode, 1)
            self.assertIn('unsupported symlink', result.stderr)

    def test_no_python_script_accepts_mach_o_only_in_the_macos_archive(self):
        script = W.parents[1] / 'packaging/release/no-python.sh'
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)

            def archive(name, magic):
                tree = root / f'tree-{name}'
                (tree / 'usr/bin').mkdir(parents=True)
                (tree / 'usr/bin/oxa').write_bytes(magic + b'\0' * 16)
                path = root / name
                subprocess.run(['tar', '-czf', str(path), '-C', str(tree), 'usr'], check=True)
                return str(path)

            run = lambda path: subprocess.run(['/bin/sh', str(script), path], text=True, capture_output=True)
            mach_o, elf = b'\xcf\xfa\xed\xfe', b'\x7fELF'
            self.assertEqual(run(archive('ox-llm-v0-macos-arm64.tar.gz', mach_o)).returncode, 0)
            self.assertEqual(run(archive('ox-llm-v0-linux-aarch64.tar.gz', elf)).returncode, 0)
            self.assertEqual(run(archive('mixed-linux-aarch64.tar.gz', mach_o)).returncode, 1)
            self.assertEqual(run(archive('mixed-macos-arm64.tar.gz', elf)).returncode, 1)


if __name__ == '__main__':
    unittest.main()
