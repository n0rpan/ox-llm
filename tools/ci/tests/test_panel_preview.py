"""The hosted widget qualifier must reject optional skips and missing THP coverage."""
import importlib.util
import io
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location(
    'panel_preview', Path(__file__).resolve().parents[1] / 'panel_preview.py')
PREVIEW = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PREVIEW)

THP = ('test_panel_v013_states.PanelV013States.'
       'test_v_cache_thp_shared_setting_default_toggle_and_failure')


def probe(outcome='pass', identity=THP):
    """Synthetic controls only; these never load Qt or execute a command."""
    class Probe(unittest.TestCase):
        def id(self):
            return identity

        def runTest(self):
            if outcome == 'skip':
                self.skipTest('synthetic missing Qt')
            if outcome == 'fail':
                self.fail('synthetic QML warning')
            if outcome == 'error':
                raise RuntimeError('synthetic renderer failure')
    return Probe()


class PanelPreview(unittest.TestCase):
    def qualify(self, *cases):
        return PREVIEW.qualify(unittest.TestSuite(cases), io.StringIO())

    def test_real_thp_control_and_other_preview_tests_must_run(self):
        self.assertTrue(self.qualify(probe(), probe(identity='other.preview')))

    def test_optional_skip_is_not_widget_evidence(self):
        self.assertFalse(self.qualify(probe('skip')))
        self.assertFalse(self.qualify(probe(), probe('skip', 'other.preview')))

    def test_qml_failure_or_renderer_error_is_not_evidence(self):
        for outcome in ('fail', 'error'):
            with self.subTest(outcome=outcome):
                self.assertFalse(self.qualify(probe(outcome)))

    def test_empty_missing_or_duplicate_inventory_is_not_evidence(self):
        self.assertFalse(self.qualify())
        self.assertFalse(self.qualify(probe(identity='other.preview')))
        self.assertFalse(self.qualify(probe(), probe()))


if __name__ == '__main__':
    unittest.main()
