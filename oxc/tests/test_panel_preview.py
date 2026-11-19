"""The panel's view components load and lay out offscreen against stand-ins for Omarchy's kit.

This catches QML load errors, missing properties and binding warnings. It does not prove the
look under the real kit and theme; that needs the running shell.
"""
from pathlib import Path
import json
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
RENDER = ROOT / 'tests/panel_preview/render.sh'
FIXTURES = ROOT / 'tests/fixtures/panel'


@unittest.skipUnless(shutil.which('qml6'), 'qml6 is not installed')
class PanelPreview(unittest.TestCase):
    def test_views_render_without_qml_warnings(self):
        for name in ('status_production', 'status_full', 'preview_ready', 'preview_sleeping_flash'):
            with self.subTest(name), tempfile.TemporaryDirectory() as directory:
                image = Path(directory) / 'out.png'
                result = subprocess.run([str(RENDER), str(FIXTURES / f'{name}.json'), str(image)],
                                        capture_output=True, text=True, timeout=60)
                noise = [line for line in result.stderr.splitlines()
                         if line and not line.startswith(('qml: Qt ', 'qml: Using built-in'))]
                self.assertEqual(noise, [])
                self.assertEqual(result.returncode, 0)
                self.assertGreater(image.stat().st_size, 10000)

    def test_flash_cache_control_renders_when_profile_enables_it(self):
        value = json.loads((FIXTURES / 'preview_ready.json').read_text())
        value.update(chat_state='ready', active_requests=0, loaded_model='flash', flash_available=True)
        value['models'] = [{'key': 'flash', 'engine': 'rust', 'flash_cache': {
            'pref': '20', 'preference_control': True, 'tier_gib': 18,
            'reason': 'preferred-swap-tolerant', 'estimated_working_set_gib': 26.325}}]
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / 'status.json'; image = Path(directory) / 'out.png'
            source.write_text(json.dumps(value))
            result = subprocess.run([str(RENDER), str(source), str(image)],
                                    capture_output=True, text=True, timeout=60)
            noise = [line for line in result.stderr.splitlines()
                     if line and not line.startswith(('qml: Qt ', 'qml: Using built-in'))]
            self.assertEqual(noise, [])
            self.assertEqual(result.returncode, 0)
            self.assertGreater(image.stat().st_size, 10000)

    def test_plugin_qml_has_no_rich_text_sink_or_hex_colour(self):
        import re
        for path in sorted((ROOT.parent / 'oxa/omarchy-panel').glob('*.qml')):
            text = path.read_text()
            with self.subTest(path.name):
                self.assertEqual(len(re.findall(r'^\s*(?:Ui\.)?Text \{', text, re.M)),
                                 text.count('textFormat: Text.PlainText'))
                self.assertIsNone(re.search(r'"#[0-9a-fA-F]{3,8}"', text))
                self.assertIsNone(re.search(r'color: *"(?!transparent")', text))        # no named colours either
                self.assertIsNone(re.search(r'Qt\.(rgba|hsla|hsva)\(\s*[0-9.]', text))    # nor one built from numbers
                self.assertNotIn('/home/', text)

    def test_every_string_the_surviving_panel_asks_for_is_in_the_catalog(self):
        import re
        plugin = ROOT.parent / 'oxa/omarchy-panel'
        catalog = json.loads((plugin / 'i18n/en.json').read_text(encoding='utf-8'))
        source = '\n'.join(path.read_text() for path in sorted(plugin.glob('*.qml')))
        literal = set(re.findall(r'\b(?:t|tn|word)\("([a-z][A-Za-z0-9_.-]*)"', source))
        self.assertEqual(sorted(key for key in literal if key not in catalog and not key.endswith('.')), [])
        # Keys built at run time ("model." + key + ".short") are matched by their fixed prefix.
        prefixes = set(re.findall(r'"([a-z][A-Za-z0-9_.-]*\.)" *\+', source))
        built = [key for key in catalog if any(key.startswith(prefix) for prefix in prefixes)]
        asked = (literal - {key for key in literal if key.endswith('.')}) | set(built)
        self.assertGreater(len(asked), 10)  # a bad path or regex must not make this check vacuous
        self.assertEqual(sorted(key for key in asked if key not in catalog), [])

        # P0 deletes the Python-controller views, not the translations reserved for the oxa
        # replacements in P5-P9.  Those strings are deliberately unused during the staged move;
        # still validate the retained catalog so a later view cannot receive a blank label.
        for key, value in catalog.items():
            with self.subTest(key=key):
                texts = [value] if isinstance(value, str) else list(value.values())
                self.assertTrue(texts)
                self.assertTrue(all(isinstance(text, str) and text.strip() for text in texts))
