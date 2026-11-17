"""Build-tool tests only: installed commands are executed without a Python PATH."""
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import tarfile
import unittest

SPEC = importlib.util.spec_from_file_location('stage', Path(__file__).resolve().parents[1] / 'stage.py')
STAGE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(STAGE)
REPO = Path(__file__).resolve().parents[3]


class NativeStage(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.source = self.root / 'source'
        self.payload = self.root / 'payload'
        for product, names in [('engine', ('oxe', 'oxe-server')), ('oxa', ('oxa', 'local-ai-memlat-boot'))]:
            for name in names:
                self.write(f'{product}/target/release/{name}',
                           '#!/bin/sh\nprintf "%s\\n" "$@" > "$CAPTURE"\n', executable=True)
        self.write('engine/vendor/ggml-subset/LICENSE', 'synthetic license')
        self.write('LICENSE', 'synthetic license')
        self.write('docs/NATIVE-INSTALL.md', 'synthetic upgrade note')
        self.templates = ('05-local-ai-memlat-fallback.hook', '20-local-ai-memlat-fallback',
                          '85-local-ai-memlat-rebuild', '99-z-local-ai-memlat-rebuild.hook')
        for name in self.templates:
            self.write('oxc/packaging/memlat/' + name, 'synthetic template ' + name, executable=True)
        self.write('oxc/packaging/memlat/oxc-memlat-shutdown.service', 'retired unit')
        self.write('packaging/native/oxc', 'retired wrapper fixture')
        # A legacy source tree is present but must not leak into the installed payload.
        self.write('oxc/src/oxc/runtime/controller.py', 'legacy fixture')
        self.write('oxc/examples/runtime/units/oxc-chat.service', 'legacy fixture')
        self.write('oxc-rs/Cargo.toml', 'legacy parser fixture')
        self.calls = []
        STAGE.stage(self.source, self.payload, runner=lambda *a, **kw: self.calls.append((a, kw)))
        self.home = self.root / 'home'
        self.home.mkdir()
        self.capture = self.root / 'arguments'
        self.env = {'PATH': str(self.payload / 'usr/bin'), 'HOME': str(self.home),
                    'CAPTURE': str(self.capture)}

    def write(self, name, text, executable=False):
        path = self.source / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        if executable:
            path.chmod(0o755)

    def test_payload_has_no_python_controller_units_or_diagnostics(self):
        names = {p.relative_to(self.payload).as_posix() for p in self.payload.rglob('*')}
        self.assertEqual({p.name for p in (self.payload / 'usr/bin').iterdir()},
                         {'oxa', 'oxe-server', 'local-ai-memlat-boot'})
        self.assertFalse(any(p.endswith(('.py', '.service', '.timer')) for p in names))
        self.assertFalse((self.payload / 'usr/share/ox-llm/oxc').exists())
        self.assertEqual([call[0][0][-2] for call in self.calls], ['server', 'oxa-cli', 'oxc-memlat'], 'licenses collected for each shipped native target')

    def test_memlat_templates_are_exact_inert_data(self):
        directory = self.payload / 'usr/share/ox-llm/memlat'
        self.assertTrue(directory.is_dir(), 'qualified MEMLAT data must be shipped')
        self.assertEqual({p.name for p in directory.iterdir()}, set(self.templates))
        for name in self.templates:
            installed = directory / name
            self.assertEqual(installed.read_bytes(),
                             (self.source / 'oxc/packaging/memlat' / name).read_bytes())
            self.assertEqual(installed.stat().st_mode & 0o777, 0o644)
        self.assertFalse((self.payload / 'etc').exists(), 'templates must not be activated')
        self.assertFalse((self.payload / 'boot').exists(), 'staging must not populate an ESP')
        self.assertFalse((self.payload / 'usr/share/libalpm').exists(), 'no active pacman hooks')
        self.assertFalse((self.payload / 'usr/lib/systemd').exists(), 'no service activation')

    def test_native_files_are_unmodified_and_keep_literal_arguments(self):
        for workspace, name in [('oxa', 'oxa'), ('engine', 'oxe-server'), ('oxa', 'local-ai-memlat-boot')]:
            source = self.source / workspace / 'target/release' / name
            installed = self.payload / 'usr/bin' / name
            self.assertEqual(installed.read_bytes(), source.read_bytes())
            subprocess.run([installed, '--literal', 'value; untouched'], env=self.env, check=True)
            self.assertEqual(self.capture.read_text().splitlines(), ['--literal', 'value; untouched'])
        self.assertEqual(list(self.home.iterdir()), [], 'no implicit user configuration changes')

    def test_actual_tarball_contains_only_the_native_commands(self):
        archive = self.root / 'payload.tar.gz'
        subprocess.run(['/bin/bash', str(REPO / 'packaging/release/archive.sh'), str(self.payload), str(archive)],
                       env={'PATH': os.defpath, 'HOME': str(self.home), 'SOURCE_DATE_EPOCH': '1'}, check=True)
        with tarfile.open(archive) as tar:
            template_entries = {entry.name: entry for entry in tar if entry.isfile() and entry.name.startswith('usr/share/ox-llm/memlat/')}
            self.assertEqual({name.rsplit('/', 1)[-1] for name in template_entries}, set(self.templates))
            for name, entry in template_entries.items():
                self.assertEqual(entry.mode, 0o644)
                self.assertEqual(tar.extractfile(entry).read(), (self.source / 'oxc/packaging/memlat' / name.rsplit('/', 1)[-1]).read_bytes())
            commands = {entry.name.rsplit('/', 1)[-1] for entry in tar if entry.isfile() and entry.name.startswith('usr/bin/')}
        self.assertEqual(commands, {'oxa', 'oxe-server', 'local-ai-memlat-boot'})

    def test_package_notice_is_one_line_without_python(self):
        for variant in ('ox-llm', 'ox-llm-bin'):
            notice = REPO / 'packaging/arch' / variant / 'native.install'
            result = subprocess.run(['/bin/sh', '-c', '. "$1"; post_install', 'notice', str(notice)],
                                    env=self.env, capture_output=True, text=True, check=True)
            self.assertEqual(len(result.stdout.splitlines()), 1)
            self.assertIn('not included', result.stdout)

    def test_dangling_destination_link_is_not_a_fresh_directory(self):
        destination = self.root / 'link'
        outside = self.root / 'not-created'
        destination.symlink_to(outside)
        with self.assertRaises(ValueError):
            STAGE.stage(self.source, destination)
        self.assertFalse(outside.exists())

    def test_runtime_metadata_does_not_require_python(self):
        for variant in ('ox-llm', 'ox-llm-bin'):
            recipe = REPO / 'packaging/arch' / variant / 'PKGBUILD'
            result = subprocess.run(['/bin/bash', '-c', '. "$1"; printf "%s\\n" "${depends[@]}" --OPTIONS-- "${options[@]}"',
                                     'recipe', str(recipe)], env=self.env,
                                    capture_output=True, text=True, check=True)
            lines = result.stdout.splitlines()
            split = lines.index("--OPTIONS--")
            dependencies, options = lines[:split], lines[split + 1:]
            self.assertIn("docs", options, "the required native upgrade guide must survive package tidying")
            self.assertNotIn("!docs", options)
            self.assertNotIn('python', dependencies)
            self.assertIn('bash', dependencies)

    def test_fresh_destination_is_required(self):
        with self.assertRaises(ValueError):
            STAGE.stage(self.source, self.payload)


class ReleaseEnvironment(unittest.TestCase):
    def test_actual_build_home_toolchains_and_checkout_are_remapped(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            paths = {name: str(base / name.lower()) for name in
                     ('HOME', 'CARGO_HOME', 'RUSTUP_HOME', 'GITHUB_WORKSPACE')}
            result = subprocess.run(
                ['/bin/bash', '-c', '. "$1"; printf "%s\\n" "$RUSTFLAGS" "$CFLAGS" "$CXXFLAGS"',
                 'environment', str(REPO / 'packaging/release/environment.sh')],
                env={'PATH': os.defpath, 'SOURCE_DATE_EPOCH': '1', **paths},
                capture_output=True, text=True, check=True)
            rust, c, cxx = result.stdout.splitlines()
            for name, destination in [('HOME', 'build-home'), ('CARGO_HOME', 'cargo'),
                                      ('RUSTUP_HOME', 'rust'), ('GITHUB_WORKSPACE', 'checkout')]:
                self.assertIn(f'--remap-path-prefix={paths[name]}=/usr/src/{destination}', rust)
                self.assertIn(f'-ffile-prefix-map={paths[name]}=/usr/src/{destination}', c)
            self.assertEqual(c, cxx)


if __name__ == '__main__':
    unittest.main()
