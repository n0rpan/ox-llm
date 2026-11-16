"""Build-tool tests only: installed commands are executed without a Python PATH."""
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
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
        self.write('packaging/native/oxc', (REPO / 'packaging/native/oxc').read_text())
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
                         {'oxa', 'oxe', 'oxe-server', 'oxc'})
        self.assertFalse(any(p.endswith(('.py', '.service', '.timer')) for p in names))
        self.assertFalse((self.payload / 'usr/share/ox-llm/oxc').exists())
        self.assertEqual(len(self.calls), 3, 'licenses collected for all shipped native targets')

    def test_code_and_plan_reach_native_oxa_without_python(self):
        wrapper = self.payload / 'usr/bin/oxc'
        for command, expected in [(['code', '-p', 'literal; value'], ['-p', 'literal; value']),
                                  (['plan', '-p', 'hello'], ['--tools', 'read,grep,find,ls', '-p', 'hello'])]:
            subprocess.run([wrapper, *command], env=self.env, check=True)
            self.assertEqual(self.capture.read_text().splitlines(), expected)
        self.assertEqual(list(self.home.iterdir()), [], 'no implicit user configuration changes')

    def test_python_features_are_disabled_with_one_line_and_no_process(self):
        wrapper = self.payload / 'usr/bin/oxc'
        for feature in ('panel', 'share', 'studio'):
            result = subprocess.run([wrapper, feature], env=self.env, text=True, capture_output=True)
            self.assertEqual(result.returncode, 2)
            self.assertEqual(len(result.stderr.splitlines()), 1)
            self.assertIn('disabled', result.stderr)
            self.assertFalse(self.capture.exists())

    def test_package_notice_is_one_line_without_python(self):
        for variant in ('ox-llm', 'ox-llm-bin'):
            notice = REPO / 'packaging/arch' / variant / 'native.install'
            result = subprocess.run(['/bin/sh', '-c', '. "$1"; post_install', 'notice', str(notice)],
                                    env=self.env, capture_output=True, text=True, check=True)
            self.assertEqual(len(result.stdout.splitlines()), 1)
            self.assertIn('disabled', result.stdout)

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
            result = subprocess.run(['/bin/bash', '-c', '. "$1"; printf "%s\\n" "${depends[@]}"',
                                     'recipe', str(recipe)], env=self.env,
                                    capture_output=True, text=True, check=True)
            dependencies = result.stdout.splitlines()
            self.assertNotIn('python', dependencies)
            self.assertIn('bash', dependencies)

    def test_fresh_destination_is_required(self):
        with self.assertRaises(ValueError):
            STAGE.stage(self.source, self.payload)


if __name__ == '__main__':
    unittest.main()
