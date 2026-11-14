"""v0.2.0 EG2, model-free: `model_limits[id].capabilities` per engine family (ctl_models.capabilities):
Rust MoE, Rust Flash (backend_kind oxe + an `oxe_flash` block), the C++ Flash adapter
(bigmoe-cpu-stream), the classic llama.cpp engine, and an unknown engine."""
import pathlib
import re
import unittest

from snapdragon_ai_lab.runtime import bigmoe_runtime, ctl_models, oxe_runtime

CONTEXT = {'cap': 32768, 'mode': 'fast', 'modes': [
    {'id': 'fast', 'cap': 32768, 'available': True}, {'id': 'deep', 'cap': 131072, 'available': True},
    {'id': 'max', 'cap': 262144, 'available': False}]}
FLAT = {'cap': 16384, 'mode': None, 'modes': []}
RUST_MOE = {'backend_kind': oxe_runtime.KIND, 'oxe_args': ['--model', 'x']}
RUST_FLASH = {'backend_kind': oxe_runtime.KIND, 'oxe_flash': {}, 'oxe_args': ['--engine', 'flash']}
CLASSIC_FLASH = {'backend_kind': bigmoe_runtime.KIND}
CLASSIC = {'backend_kind': 'llama', 'extra_args': ['--spec-type', 'draft-mtp']}
NO_HINTS = {'prefill_only': False, 'snapshot_hint': 0, 'turn_kind': False, 'hint_clear': False}
HINTS = {'prefill_only': True, 'snapshot_hint': 2, 'turn_kind': True, 'hint_clear': True}


class Capabilities(unittest.TestCase):
    def caps(self, config, context=CONTEXT):
        return ctl_models.capabilities(config, context)

    def test_families(self):
        self.assertEqual([ctl_models.engine_family(c) for c in (RUST_MOE, RUST_FLASH, CLASSIC_FLASH, CLASSIC, {},
                                                               {'backend_kind': 'other'})],
                         ['rust', 'rust-flash', 'classic-flash', 'classic', 'classic', 'unknown'])

    def test_rust_moe(self):
        self.assertEqual(self.caps(RUST_MOE), {**HINTS, 'thinking': 'toggle', 'preserve_thinking': True, 'mtp': None,
                                               'max_context': 131072, 'vision': False, 'slots': 1, 'park': 'ram+disk',
                                               'sampling': None})  # v0.2.0: the engine reports it once it runs

    def test_rust_flash(self):
        for args in (['--engine', 'flash'], ['--engine', 'flash', '--spec-type', 'draft-mtp'],
                     ['--engine', 'flash', '--flash-mtp-depth', '1'], ['--engine', 'flash', '--flash-mtp-depth', '0']):
            got = self.caps({**RUST_FLASH, 'oxe_args': args}, FLAT)
            self.assertEqual(got, {**HINTS, 'thinking': 'toggle', 'preserve_thinking': False, 'mtp': False,
                                   'max_context': 16384, 'vision': False, 'slots': 1, 'park': 'disk', 'sampling': None}, args)

    def test_classic_flash_claims_no_hint(self):
        self.assertEqual(self.caps(CLASSIC_FLASH, FLAT), {**NO_HINTS, 'thinking': 'toggle', 'preserve_thinking': False,
                                                          'mtp': False, 'max_context': 16384, 'vision': False,
                                                          'slots': 1, 'park': 'none', 'sampling': []})

    def test_classic_and_unknown_claim_no_hint(self):
        classic = self.caps(CLASSIC, FLAT)
        self.assertEqual({k: classic[k] for k in NO_HINTS}, NO_HINTS)
        self.assertEqual((classic['mtp'], classic['park'], classic['preserve_thinking']), (True, 'none', False))
        self.assertEqual(classic['sampling'], ['frequency_penalty', 'logit_bias', 'presence_penalty'])
        self.assertIs(self.caps({'backend_kind': 'llama', 'extra_args': 'bogus'}, FLAT)['mtp'], False)
        unknown = self.caps({'backend_kind': 'other', 'oxe_args': ['--mtp']}, FLAT)
        self.assertEqual({k: unknown[k] for k in NO_HINTS}, NO_HINTS)
        self.assertIs(unknown['mtp'], False)

    def test_rust_moe_mtp_from_the_profile_flags(self):
        for args, want in ((['--spec-type', 'draft-mtp'], True), (['--spec-type', 'none'], False),
                           (['--mtp'], True), (['--mtp', '--spec-type', 'none'], False), ([], None)):
            self.assertIs(self.caps({**RUST_MOE, 'oxe_args': args})['mtp'], want, args)

    def test_snapshot_hint_matches_the_engine_constant(self):
        root = pathlib.Path(__file__).resolve().parents[2] / 'engine' / 'crates' / 'server' / 'src' / 'cache.rs'
        if not root.exists():
            self.skipTest('engine tree not present')
        found = re.search(r'pub const MAX_SNAP: usize = (\d+);', root.read_text())
        self.assertEqual(int(found.group(1)), ctl_models.SNAPSHOT_KEEP)


if __name__ == '__main__':
    unittest.main()
