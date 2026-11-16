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
        for product, names in [('engine', ('oxe', 'oxe-server')), ('oxa', ('oxa',))]:
            for name in names:
                self.write(f'{product}/target/release/{name}',
                           '#!/bin/sh\nprintf "%s\\n" "$@" > "$CAPTURE"\n', executable=True)
        self.write('engine/vendor/ggml-subset/LICENSE', 'synthetic license')
        self.write('LICENSE', 'synthetic license')
        self.write('docs/NATIVE-INSTALL.md', 'synthetic upgrade note')
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
                         {'oxa', 'oxe-server'})
        self.assertFalse(any(p.endswith(('.py', '.service', '.timer')) for p in names))
        self.assertFalse((self.payload / 'usr/share/ox-llm/oxc').exists())
        self.assertEqual([call[0][0][-2] for call in self.calls], ['server', 'oxa-cli'], 'licenses collected for each shipped native target')

    def test_native_files_are_unmodified_and_keep_literal_arguments(self):
        for workspace, name in [('oxa', 'oxa'), ('engine', 'oxe-server')]:
            source = self.source / workspace / 'target/release' / name
            installed = self.payload / 'usr/bin' / name
            self.assertEqual(installed.read_bytes(), source.read_bytes())
            subprocess.run([installed, '--literal', 'value; untouched'], env=self.env, check=True)
            self.assertEqual(self.capture.read_text().splitlines(), ['--literal', 'value; untouched'])
        self.assertEqual(list(self.home.iterdir()), [], 'no implicit user configuration changes')

    def test_actual_tarball_contains_only_the_two_native_commands(self):
        archive = self.root / 'payload.tar.gz'
        subprocess.run(['/bin/bash', str(REPO / 'packaging/release/archive.sh'), str(self.payload), str(archive)],
                       env={'PATH': os.defpath, 'HOME': str(self.home), 'SOURCE_DATE_EPOCH': '1'}, check=True)
        with tarfile.open(archive) as tar:
            commands = {entry.name.rsplit('/', 1)[-1] for entry in tar if entry.isfile() and entry.name.startswith('usr/bin/')}
        self.assertEqual(commands, {'oxa', 'oxe-server'})

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


if __name__ == '__main__':
    unittest.main()
