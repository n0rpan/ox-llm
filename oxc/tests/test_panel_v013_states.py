"""Every panel state renders offscreen from a stub status file, on every tab, with a clean QML log.

The plugin's real Panel.qml runs against stand-ins for Omarchy's kit (tests/panel_preview/qs) and
for Quickshell's Process (tests/panel_preview/Quickshell), which "prints" what the scenario file in
tests/fixtures/panel/v013 says. No command is executed, no shell is installed into or restarted, no
model is loaded, and nothing outside a temporary directory is written (set
OXC_PANEL_PREVIEW_DIR to keep the PNGs). This proves loading, layout, texts and state logic;
the look under the real kit and theme still needs one check in the running shell.
"""
from pathlib import Path
import json
import os
import re
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
RENDER = ROOT / 'tests/panel_preview/render_panel.sh'
SCENARIOS = ROOT / 'tests/fixtures/panel/v013'
CATALOG = json.loads((ROOT.parent / 'oxa/omarchy-panel/i18n/en.json').read_text(encoding='utf-8'))
TABS = ('models', 'activity', 'storage')
QUIET = ('qml: Qt ', 'qml: Using built-in', 'qml: PANEL-TEXTS ')
RAW_KEY = re.compile(r'^(now|mode|bar|model|notice|activity|storage|cache|clear|flash|harness|launch|unit|stat|tab|footer|chip|ram|share|empty)'
                     r'\.[A-Za-z0-9_.-]+$')
REASON = 'Not enough free RAM to load Ornith: 19.2 GiB needed, 6.0 GiB free.'


@unittest.skipUnless(shutil.which('qml6'), 'qml6 is not installed')
class PanelV013States(unittest.TestCase):
    def render(self, name, label=None, **changes):
        """Runs one scenario; returns {tab: probe} after checking the log, the exit and the images."""
        scenario = json.loads((SCENARIOS / f'{name}.json').read_text(encoding='utf-8'))
        scenario.update(changes)
        label = label or name
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / 'scenario.json'
            source.write_text(json.dumps(scenario), encoding='utf-8')
            prefix = Path(directory) / 'panel'
            result = subprocess.run([str(RENDER), str(source), str(prefix)], capture_output=True, text=True, timeout=90)
            lines = [line for line in result.stderr.splitlines() if line]
            noise = [line for line in lines if not line.startswith(QUIET)]
            self.assertEqual(noise, [], f'{label}: QML warnings or errors')
            self.assertEqual(result.returncode, 0, label)
            probes = [json.loads(line.split('PANEL-TEXTS ', 1)[1]) for line in lines if 'PANEL-TEXTS ' in line]
            tabs = TABS + (('sharing',) if scenario.get('share') else ())  # the Sharing tab exists with a share state
            self.assertEqual(sorted(probe['view'] for probe in probes), sorted(tabs), label)
            start = scenario.get('view', 'models')
            self.assertEqual(probes[0]['view'], start, f'{label}: the panel opens on its starting tab')
            keep = os.environ.get('OXC_PANEL_PREVIEW_DIR')
            for tab in tabs:
                image = Path(f'{prefix}-{tab}.png')
                self.assertGreater(image.stat().st_size, 10000, f'{label}: {tab} image')
                if keep:
                    Path(keep).mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(image, Path(keep) / f'{label}-{tab}.png')
        for probe in probes:
            where = f"{label}/{probe['view']}"
            self.assertGreater(len(probe['texts']), 8, f'{where}: the content was built')
            for text in probe['texts'] + probe['tips']:
                self.assertFalse(text in CATALOG or RAW_KEY.match(text), f'{where}: raw text key {text!r}')
                self.assertNotRegex(text, r'undefined|NaN|\[object', where)
            self.assertIn('Local AI', probe['texts'], where)
            bar = [tip for tip in probe['tips'] if tip.startswith('Local AI: ')]
            self.assertEqual(len(bar), 1, f'{where}: one bar tooltip')
            self.assertRegex(bar[0], r'^Local AI: \S.*\S$')
            self.assertNotRegex(bar[0], r'  |: $')
        return {probe['view']: probe for probe in probes}

    def texts(self, name, label=None, **changes):
        return self.render(name, label, **changes)['models']

    def assertShows(self, probe, *wanted):
        for text in wanted:
            self.assertTrue(any(text in shown for shown in probe['texts']), f'{text!r} not in {probe["texts"]}')

    def assertHides(self, probe, *unwanted):
        for text in unwanted:
            self.assertFalse(any(text in shown for shown in probe['texts']), f'{text!r} is shown')

    # ---- every state, every tab ----------------------------------------------------------------

    def test_idle_shows_the_mode_selector_with_fast_default_and_max_not_offered(self):
        probe = self.texts('idle')
        self.assertEqual((probe['phase'], probe['notice']), ('sleeping', ''))
        self.assertShows(probe, 'Nothing loaded', 'Ornith loads on the first request.', 'Load Ornith',
                         'Context', 'Fast', 'Deep', 'Max')
        tips = '\n'.join(probe['tips'])
        self.assertIn('Keeps about 48K tokens before it summarises.', tips)
        self.assertIn('Keeps about 100K tokens before it summarises.', tips)
        self.assertIn('Fast is the default', tips)  # O-105
        self.assertIn('Not offered yet: its memory use at this size has not been measured on this laptop.', tips)
        self.assertHides(probe, 'Resuming last session', 'Session ready', REASON)

    def test_loading(self):
        probe = self.texts('loading')
        self.assertEqual(probe['phase'], 'loading')
        self.assertShows(probe, 'Ornith', 'Loading…')
        self.assertHides(probe, 'Load Ornith', 'Unload')
        self.assertIn('Local AI: loading Ornith', probe['tips'])

    def test_ready_and_answering(self):
        ready = self.texts('ready')
        self.assertEqual(ready['phase'], 'ready')
        self.assertShows(ready, 'Ornith', 'Ready · unloads in 9:4', 'Unload')
        self.assertHides(ready, 'Session ready', 'Resuming')
        answering = self.texts('answering')
        self.assertShows(answering, 'Answering · 2 requests waiting')

    def test_preload_running_done_and_quiet_otherwise(self):
        running = self.texts('preload_running')
        self.assertShows(running, 'Resuming last session…', 'Unload')
        self.assertHides(running, 'Session ready', 'Ready · unloads')
        self.assertTrue(any('being restored from disk' in tip for tip in running['tips']))
        done = self.texts('preload_done')
        self.assertShows(done, 'Session ready · unloads in 9:4')
        self.assertHides(done, 'Resuming')
        self.assertTrue(any('restored from disk in 3.2 s' in tip for tip in done['tips']))
        for name in ('preload_cancelled', 'preload_unavailable'):
            with self.subTest(name):
                quiet = self.texts(name)
                self.assertShows(quiet, 'Ready · unloads in 9:4')
                self.assertHides(quiet, 'Session ready', 'Resuming')

    def test_each_mode_renders_and_max_is_offered_once_available(self):
        for mode in ('fast', 'deep', 'max'):
            with self.subTest(mode):
                probe = self.texts('mode_' + mode)
                self.assertShows(probe, 'Context', 'Fast', 'Deep', 'Max')
                tips = '\n'.join(probe['tips'])
                self.assertEqual('Not offered yet' in tips, mode != 'max')
                self.assertIn('Keeps about 200K tokens before it summarises.', tips)
                self.assertIn('Context window: 262K tokens.', tips)

    def test_max_unavailable_keeps_its_reason_and_other_reasons_pass_through(self):
        probe = self.texts('max_unavailable')
        self.assertShows(probe, 'Max')
        self.assertTrue(any('Rarely summarises' in tip and 'Not offered yet: its memory use' in tip for tip in probe['tips']))
        scenario = json.loads((SCENARIOS / 'max_unavailable.json').read_text(encoding='utf-8'))
        for reason, words in (('Ornith only', 'only Ornith has it'), ('a reason added later', 'a reason added later')):
            scenario['status']['context']['modes'][2]['reason'] = reason
            other = self.texts('max_unavailable', 'max_unavailable_' + reason.split()[0].lower(), status=scenario['status'])
            self.assertTrue(any(f'Not offered yet: {words}.' in tip for tip in other['tips']))

    def test_choosing_a_mode_calls_the_mode_command_once(self):
        probe = self.texts('mode_request')
        self.assertEqual([call for call in probe['calls'] if call.startswith('mode')], ['mode --mode fast'])
        self.assertEqual(probe['notice'], '')

    def test_stopping_offline_and_error_are_three_calm_states_with_reasons(self):
        stopping = self.texts('stopping')
        self.assertEqual(stopping['phase'], 'stopping')
        self.assertShows(stopping, 'Stopping…', 'Waiting for the engine to exit.')
        self.assertTrue(any('Still stopping: engine pid 4242 has not exited' in tip for tip in stopping['tips']))
        self.assertIn('Local AI: stopping… (engine pid 4242 has not exited: still running)', stopping['tips'])
        offline = self.texts('offline')
        self.assertEqual(offline['phase'], 'offline')
        self.assertShows(offline, 'Not running', 'The local AI service is stopped. Chat or Code starts it.')
        self.assertIn('Local AI: service not running', offline['tips'])
        for name, reason in (('error_command', 'The oxc command failed (exit 2).'),
                             ('error_unreadable', 'The status answer could not be read.'),
                             ('error_missing', 'The oxc command did not start.')):
            with self.subTest(name):
                error = self.texts(name)
                self.assertEqual((error['phase'], error['notice']), ('error', ''))
                self.assertShows(error, 'No status', reason)
                self.assertHides(error, 'Not running', 'Unload', 'Load ')
                self.assertIn('Local AI: no status', error['tips'])
        for calm in (stopping, offline):
            self.assertHides(calm, 'Unload', 'Load ', 'No status')

    def test_own_action_failure_shows_its_reason(self):
        probe = self.texts('own_failure')
        self.assertEqual(probe['notice'], 'failure')
        self.assertShows(probe, REASON)
        self.assertEqual([call for call in probe['calls'] if call == 'load'], ['load'])

    def test_own_action_failure_without_a_reason_text_names_the_model(self):
        """The CLI's flat error_at / error_origin keys carry no reason: the panel still says what failed."""
        probe = self.texts('own_failure_flat')
        self.assertEqual(probe['notice'], 'failure')
        self.assertShows(probe, 'Ornith did not start. Check free RAM, then load again.')

    def test_foreign_failures_stay_hidden(self):
        for name in ('foreign_failure_lab', 'foreign_failure_older', 'foreign_failure_no_action', 'foreign_failure_flat'):
            with self.subTest(name):
                probes = self.render(name)
                for tab in TABS:
                    self.assertHides(probes[tab], REASON, 'did not start', 'That did not work')
                self.assertEqual(probes['models']['notice'], '')

    def test_a_model_without_a_name_is_still_labelled(self):
        probe = self.texts('unlisted_model')
        self.assertShows(probe, 'Unlisted model', 'Unload')
        self.assertIn('Local AI: a model loaded', probe['tips'])
        self.assertHides(probe, 'Context')

    def test_sharing_has_its_own_tab_only_when_the_command_reports_it(self):
        for tab, probe in self.render('ready').items():   # no share state: no tab, no status fact
            self.assertHides(probe, 'Sharing', 'Tailscale', 'Not shared', 'Shared ·')
        off = self.render('share_off')
        self.assertShows(off['sharing'], 'Sharing', 'Off', 'Tailscale', 'LAN', 'Only this laptop can use the models.')
        self.assertHides(off['sharing'], 'Shared at', 'not running', 'Plain HTTP', 'from other devices')
        self.assertTrue(any(tip.startswith('Recommended.') for tip in off['sharing']['tips']))
        for tab in TABS:                                  # the tab and the status fact are on every tab
            self.assertShows(off[tab], 'Sharing', 'Not shared')
            self.assertHides(off[tab], 'Tailscale', 'Shared at')
        on = self.render('share_tailscale')
        self.assertShows(on['sharing'], 'Shared at http://laptop.example.ts.net:8443/v1',
                         '3 requests from other devices · one running now', 'Key sha256:0123456789ab.')
        self.assertHides(on['sharing'], 'Plain HTTP')
        self.assertTrue(any('Key sha256:0123456789ab.' in tip and '2 requests refused' in tip for tip in on['sharing']['tips']))
        for tab in TABS:
            self.assertShows(on[tab], 'Shared · Tailscale')
            self.assertHides(on[tab], 'Shared at', 'Key sha256')
        lan = self.render('share_lan')
        self.assertShows(lan['sharing'], 'Shared at http://laptop.example.lan:8443/v1', 'Plain HTTP: anyone on this network can read')
        self.assertShows(lan['models'], 'Shared · LAN')
        self.assertTrue(any(tip.startswith('Plain HTTP:') for tip in lan['models']['tips']))  # the warning rides on the fact
        down = self.render('share_not_active')
        self.assertShows(down['sharing'], 'Sharing is asked for but not running', 'No private network address was found.')
        self.assertHides(down['sharing'], 'Plain HTTP')
        self.assertTrue(any('No private network address was found.' in tip for tip in down['sharing']['tips']))
        self.assertShows(down['models'], 'Sharing is not running')
        asked = self.texts('share_request')
        self.assertEqual([call for call in asked['calls'] if call.startswith('share o')], ['share on tailscale'])

    def test_the_status_line_names_mode_cache_and_sharing_and_stays_quiet_without_them(self):
        ready = self.texts('share_tailscale')
        self.assertShows(ready, 'Context · ', 'Cache · 18 of 64 GB', 'Shared · Tailscale')
        tips = '\n'.join(ready['tips'])
        self.assertIn('Change it on its card under Models.', tips)
        self.assertIn('Change it under Storage.', tips)
        self.assertIn('Change it under Sharing.', tips)
        self.assertEqual(len([call for call in ready['calls'] if call == 'cache status --json']) >= 1, True)  # read on open
        self.assertHides(self.texts('unlisted_model'), 'Context · ')      # no mode without a model that has one
        for name in ('mode_fast', 'mode_deep', 'mode_max'):
            with self.subTest(name):
                self.assertShows(self.texts(name), 'Context · ' + name.split('_')[1].capitalize())

    def test_a_small_screen_scrolls_the_tab_body_and_keeps_the_launch_row(self):
        tall = self.render('share_lan', 'roomy')
        self.assertFalse(any(probe['scrolls'] for probe in tall.values()))
        small = self.render('share_lan', 'small_screen', card_height=520)
        for tab, probe in small.items():
            with self.subTest(tab):
                self.assertLessEqual(probe['height'], 520)
                self.assertShows(probe, 'Chat', 'Code', 'Studio', 'Models', 'Activity', 'Storage', 'Sharing')
        self.assertTrue(small['models']['scrolls'])
        self.assertTrue(small['storage']['scrolls'])

    def test_empty_and_unavailable_tabs_say_so_in_words(self):
        for name in ('error_missing', 'offline'):
            with self.subTest(name):
                probes = self.render(name)
                self.assertShows(probes['storage'], 'Conversation cache')
                self.assertShows(probes['activity'], 'Recent requests')

    def test_the_panel_draws_in_another_theme_with_rounded_corners(self):
        """Nothing is tied to one palette: the same states render clean with other theme colours."""
        theme = {'foreground': '#3c3836', 'background': '#fbf1c7', 'accent': '#b57614', 'urgent': '#cc241d',
                 'muted': '#a89984', 'radius': 8}
        for name in ('share_lan', 'own_failure', 'idle'):
            with self.subTest(name):
                self.render(name, name + '_light', theme=theme)

    def test_turning_sharing_on_asks_first_and_turning_it_off_does_not(self):
        def run(base, label, *steps):
            probe = self.render(base, label, share_steps=list(steps))['sharing']
            return probe, [call for call in probe['calls'] if call.startswith('share o')]
        tail_text = ('Share chat, completions and the model list with the devices on your tailnet? '
                     'Each device needs the share key. The connection is encrypted.')
        asked, calls = run('share_off', 'share_ask_tailscale', 'pick:tailscale')
        self.assertEqual(calls, [])                       # nothing is shared before the yes
        self.assertShows(asked, tail_text, 'Share', 'Cancel')
        lan, calls = run('share_off', 'share_ask_lan', 'pick:lan')
        self.assertEqual(calls, [])
        self.assertShows(lan, 'with everyone on this network?', 'Each device needs the share key',
                         'the traffic is not encrypted', 'Tailscale is the safer choice')
        for mode in ('tailscale', 'lan'):
            with self.subTest(mode):
                yes, calls = run('share_off', 'share_yes_' + mode, 'pick:' + mode, 'confirm')
                self.assertEqual(calls, ['share on ' + mode])
                self.assertHides(yes, 'Share chat, completions')
        no, calls = run('share_off', 'share_cancelled', 'pick:lan', 'cancel', 'confirm')
        self.assertEqual(calls, [])
        self.assertHides(no, 'Share chat, completions')
        off, calls = run('share_tailscale', 'share_turned_off', 'pick:off')
        self.assertEqual(calls, ['share off'])            # off needs no confirm
        self.assertHides(off, 'Share chat, completions')
        switch, calls = run('share_tailscale', 'share_switch_to_lan', 'pick:lan')
        self.assertEqual(calls, [])                       # widening from tailnet to LAN asks too
        self.assertShows(switch, 'with everyone on this network?')
        same, calls = run('share_tailscale', 'share_same_mode', 'pick:lan', 'pick:tailscale', 'confirm')
        self.assertEqual(calls, [])                       # picking the current mode drops the question
        quiet = self.render('share_off')['sharing']
        self.assertHides(quiet, 'Share chat, completions')

    def test_the_panel_opens_on_any_starting_tab(self):
        """WR-5: a panel whose first tab is not Models builds its content like any other."""
        for view in ('activity', 'storage'):
            with self.subTest(view):
                probes = self.render('idle', 'idle_starts_on_' + view, view=view)
                self.assertShows(probes[view], 'Nothing loaded', 'Models', 'Activity', 'Storage')
        sharing = self.render('share_off', 'share_starts_on_sharing', view='sharing')['sharing']
        self.assertShows(sharing, 'Only this laptop can use the models.')
        self.assertShows(self.render('ready', 'ready_starts_on_activity', view='activity')['activity'], 'requests', 'first token')
        self.assertShows(self.render('ready', 'ready_starts_on_storage', view='storage')['storage'], 'Conversation cache', 'on disk')


if __name__ == '__main__':
    unittest.main()
