"""Cache correctness controls: incompatible compilers/flags/manifests never share a prefix."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location('cache_key', Path(__file__).resolve().parents[1] / 'rust_cache_key.py')
CACHE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CACHE)


class CacheControls(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.files = ['Cargo.toml', 'Cargo.lock', 'lib.rs']
        for name in self.files:
            (self.root / name).write_text('synthetic ' + name)

    def key(self, compiler='rustc pinned host', flags=None, variant='ubuntu-ci'):
        return CACHE.keys(self.root, self.files, variant, compiler, flags or {})

    def test_identical_inputs_are_stable_and_source_changes_keep_dependency_prefix(self):
        original = self.key()
        self.assertEqual(self.key(), original)
        (self.root / 'lib.rs').write_text('changed code')
        changed = self.key()
        self.assertNotEqual(changed[0], original[0])
        self.assertEqual(changed[1], original[1])

    def test_compiler_flags_and_variant_cannot_restore_incompatible_cache(self):
        original = self.key()
        for changed in (self.key(compiler='new rustc or host'), self.key(flags={'RUSTFLAGS': 'new flag'}),
                        self.key(variant='fedora-flash')):
            self.assertNotEqual(changed[1], original[1])

    def test_manifest_and_lock_changes_invalidate_dependency_prefix(self):
        original = self.key()
        for name in ('Cargo.toml', 'Cargo.lock'):
            (self.root / name).write_text('changed dependency graph')
            self.assertNotEqual(self.key()[1], original[1])

    def test_source_archive_checkout_needs_no_git_metadata_and_omits_build_outputs(self):
        (self.root / 'oxa/target/ci').mkdir(parents=True)
        (self.root / 'oxa/src').mkdir()
        (self.root / 'oxa/Cargo.toml').write_text('manifest')
        (self.root / 'oxa/src/lib.rs').write_text('source')
        (self.root / 'oxa/target/ci/build-output').write_text('must not hash generated files')
        self.assertEqual(sorted(CACHE.source_files(self.root, ['oxa'])),
                         ['oxa/Cargo.toml', 'oxa/src/lib.rs'])
