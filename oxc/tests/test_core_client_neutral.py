"""v0.1.4 core-dequirk (O-133/O-135) + v0.2.0 O-151: nothing ox-llm ships names a third-party coding client.

Every SHIPPED file is scanned (code, comments, docstrings, resources): the runtime (controller, CLI,
Studio), the Omarchy widget, the installers: the whole src/oxc package, plus the
examples the bootstrap copies. There is no allowlist and no per-file exception (P63);
client-specific test tooling lives under benchmarks/clients/ (not installed, not shipped).

FIT F15: the controller core does not special-case ox-llm's own agent either. Only the user-facing
edge that installs, launches or reports it may name `oxa`: the `oxc` CLI, `uninstall.py` and the
digest/update modules it loads (by role, listed below). Model-free."""
from pathlib import Path
import re
import unittest

LAB = Path(__file__).resolve().parents[1]
RUNTIME = LAB / 'src/oxc/runtime'
PACKAGE = LAB / 'src/oxc'
SHIPPED = (PACKAGE, LAB / 'examples')  # the whole installable package (runtime, widget, installers) + examples
# The CLI edge that installs, launches or reports ox-llm's agent (role-scoped, not an exception list).
AGENT_EDGE = frozenset({'oxc', 'uninstall.py', 'health_digest.py', 'release_update.py'})
OWN_AGENT = re.compile(r'(?i)\boxa\b')
# Coding clients and harnesses the lab has used or knows of. Word-bounded, case-insensitive.
CLIENT_NAMES = re.compile(
    r'(?i)(open[-_ ]?code|claude|codex|\baider\b|\bcline\b|roo[-_ ]?code|kilo[-_ ]?code|copilot|windsurf'
    r'|continue\.dev|\bdsh\b|deepseek[-_ ]harness|\bpi-ai\b|qwen[-_ ]code|gemini[-_ ]cli|cursor[-_ ]agent)')


def files_under(*roots):
    out = []
    for root in roots:
        out += [root] if root.is_file() else sorted(p for p in root.rglob('*') if p.is_file() and '__pycache__' not in p.parts)
    return out


def hits(files, pattern):
    found = []
    for path in files:
        try:
            text = path.read_text(encoding='utf-8')
        except UnicodeDecodeError:
            text = path.read_bytes().decode('latin-1')
        for number, line in enumerate(text.splitlines(), 1):
            match = pattern.search(line)
            if match:
                found.append(f'{path.relative_to(LAB)}:{number}: {match.group(0)}')
    return found


class CoreClientNeutral(unittest.TestCase):
    def test_no_client_name_in_the_core(self):
        files = files_under(RUNTIME)
        self.assertGreater(len(files), 20)
        found = hits(files, CLIENT_NAMES)
        self.assertEqual(found, [], 'client names in the core:\n' + '\n'.join(found))

    def test_no_client_name_in_any_shipped_path(self):
        files = files_under(*SHIPPED)
        self.assertGreater(len(files), 60)
        found = hits(files, CLIENT_NAMES)
        self.assertEqual(found, [], 'client names in shipped files (O-151):\n' + '\n'.join(found))
        self.assertFalse((PACKAGE / 'adapters').exists(), 'no client adapter ships (O-151)')

    def test_the_controller_core_does_not_special_case_its_own_agent(self):
        core = [p for p in files_under(RUNTIME) if p.name not in AGENT_EDGE]
        found = hits(core, OWN_AGENT)
        self.assertEqual(found, [], 'the controller names oxa (FIT F15):\n' + '\n'.join(found))
        self.assertTrue(hits([RUNTIME / 'oxc'], OWN_AGENT), 'the CLI edge launches oxa')

    def test_the_pattern_catches_what_it_should(self):
        for sample in ('OpenCode', 'opencode_exit', 'x-claude-code-session-id', 'Codex review', 'aider',
                       'Roo-Code', 'GitHub Copilot', 'dsh'):
            with self.subTest(sample=sample):
                self.assertRegex(sample, CLIENT_NAMES)
        for sample in ('cursor = source.tell()', 'optimized', 'pi-hole', 'continue'):
            with self.subTest(sample=sample):
                self.assertNotRegex(sample, CLIENT_NAMES)


if __name__ == '__main__':
    unittest.main()
