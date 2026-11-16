"""The `gate` job in checks.yml covers every job and every classifier output; caches save only on main.

Parsed line by line (no YAML library) so it runs with a bare `python3 -I`.
"""
import importlib.util
from pathlib import Path
import re
import tomllib
import unittest

WORKFLOWS = Path(__file__).resolve().parents[3] / '.github' / 'workflows'
SPEC = importlib.util.spec_from_file_location('paths', Path(__file__).resolve().parents[1] / 'paths.py')
PATHS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PATHS)

# Jobs outside the gate, with the reason.
UNGATED = {'gate': 'the gate itself', 'oxa-nightly-arm': 'nightly and manual opt-in only; never on PRs or the queue'}


def jobs(text):
    return re.findall(r'^  ([A-Za-z0-9_-]+):\s*$', text.split('\njobs:\n', 1)[1], re.M)


def job_block(text, name):
    body = text.split('\njobs:\n', 1)[1]
    return re.search(rf'^  {re.escape(name)}:\s*\n((?:(?:    .*|\s*)\n)*)', body + '\n', re.M).group(1)


class Gate(unittest.TestCase):
    def setUp(self):
        self.checks = (WORKFLOWS / 'checks.yml').read_text()

    def test_gate_needs_every_job(self):
        block = job_block(self.checks, 'gate')
        needs = set(re.search(r'^    needs: \[(.*)\]', block, re.M).group(1).replace(' ', '').split(','))
        self.assertIn('if: always()', block)
        self.assertEqual(needs, set(jobs(self.checks)) - set(UNGATED))

    def test_gate_checks_every_classifier_output(self):
        outputs = set(re.findall(r'^      ([a-z]+): \$\{\{ steps\.paths\.outputs\.\1 \}\}',
                                 job_block(self.checks, 'changes'), re.M))
        produced = set(PATHS.classify(['oxa/src/a.rs']))
        self.assertEqual(outputs, produced)
        checked = set(re.search(r"for k in \(([^)]*)\):\n\s*if flags", job_block(self.checks, 'gate'))
                      .group(1).replace("'", '').replace(' ', '').split(','))
        self.assertEqual(checked, produced)

    def test_scans_and_classifier_are_never_conditional(self):
        for name in ('changes', 'scans'):
            self.assertNotRegex(job_block(self.checks, name), r'(?m)^    if:', name)

    def test_gateway_runs_isolated_native_tests_as_a_workspace_member(self):
        root = WORKFLOWS.parents[1]
        block = job_block(self.checks, 'gateway')
        manifests = re.findall(r'--manifest-path (\S+)', block)
        self.assertTrue(manifests)
        self.assertEqual(set(manifests), {'oxa/Cargo.toml'})
        self.assertIn('cargo test --locked --offline --manifest-path oxa/Cargo.toml -p oxc-gateway --all-features', block)
        self.assertIn('cargo test --locked --offline --manifest-path oxa/Cargo.toml -p oxc-gateway --all-features -p oxa-engine', block)
        self.assertIn('cargo test --locked --offline --manifest-path oxa/Cargo.toml -p oxa-cli --test in_process_cloud', block)
        self.assertIn('bwrap --unshare-user --unshare-net --cap-drop ALL', block)
        self.assertIn('env -i ', block)
        self.assertIn('sudo chown 65534:65534 "$gateway_owner_fixture"', block)
        self.assertIn('OXC_SECRET_OWNER_PATH="$gateway_owner_fixture"', block)
        self.assertNotIn('unittest discover', block)
        self.assertNotIn('OXC_POLICY_CONTRACT', block)
        workspace = tomllib.loads((root / 'oxa/Cargo.toml').read_text())['workspace']
        self.assertIn('oxc-gateway', workspace['members'])
        package = tomllib.loads((root / 'oxa/oxc-gateway/Cargo.toml').read_text())
        self.assertNotIn('workspace', package)
        self.assertFalse((root / 'oxa/oxc-gateway/Cargo.lock').exists())

    def test_caches_restore_only_and_save_only_on_main(self):
        for path in sorted(WORKFLOWS.glob('*.y*ml')):
            text = path.read_text()
            self.assertNotRegex(text, r'actions/cache@', path.name)
            for match in re.finditer(r'actions/cache/save@.*\n(?:\s+#.*\n)?\s+if: (.*)', text):
                self.assertIn("github.ref == 'refs/heads/main'", match.group(1), path.name)
                self.assertIn("github.event_name != 'merge_group'", match.group(1), path.name)
            self.assertEqual(len(re.findall(r'actions/cache/save@', text)),
                             len(re.findall(r'actions/cache/restore@', text)), path.name)


class LabShards(unittest.TestCase):
    def setUp(self):
        self.checks = (WORKFLOWS / 'checks.yml').read_text()

    def test_three_shards_and_an_aggregate_with_the_old_name(self):
        shards = job_block(self.checks, 'lab-shards')
        self.assertRegex(shards, r'(?m)^        shard: \[1, 2, 3\]$')
        self.assertIn('name: lab tests shard ${{ matrix.shard }}', shards)
        # ci_test_shard.py numbers shards from 0; the check names count from 1
        self.assertIn('LAB_TEST_SHARD="$((${{ matrix.shard }} - 1))/3"', shards)
        self.assertIn('fail-fast: false', shards)
        lab = job_block(self.checks, 'lab')
        self.assertIn('name: lab tests, CLI, receipt scrub (x86_64)', lab)
        self.assertRegex(lab, r'(?m)^    needs: \[changes, lab-shards\]$')
        self.assertRegex(lab, r"(?m)^    if: always\(\) && needs\.changes\.outputs\.lab == 'true'")
        self.assertIn("test '${{ needs.lab-shards.result }}' = success", lab)

    def test_non_unit_steps_run_once(self):
        shards, lab = job_block(self.checks, 'lab-shards'), job_block(self.checks, 'lab')
        for step in ('CLI help (isolated HOME)', 'Report skipped tests'):
            self.assertEqual((shards + lab).count(f'name: {step}'), 1, step)
        self.assertIn('name: Report skipped tests', lab)
        self.assertRegex(shards, r'(?m)CLI help.*\n        if: matrix\.shard == 1$')
        self.assertRegex(shards, r"actions/cache/save@.*\n(?:\s+#.*\n)?\s+if: .*matrix\.shard == 1")


class EngineNextest(unittest.TestCase):
    def setUp(self):
        self.engine = job_block((WORKFLOWS / 'build.yml').read_text(), 'engine')
        self.config = (WORKFLOWS.parents[1] / 'engine' / '.config' / 'nextest.toml').read_text()

    def test_engine_job_runs_nextest_doctests_and_counts(self):
        self.assertIn('tools/ci/install_nextest.sh', self.engine)
        self.assertIn('cargo nextest run --release --locked --no-fail-fast --workspace --exclude qwen4exp',
                      self.engine)
        self.assertIn('cargo test --release --locked --no-fail-fast --workspace --exclude qwen4exp --doc',
                      self.engine)
        self.assertIn('tools/ci/engine_test_count.py', self.engine)
        self.assertNotRegex(self.engine, r'cargo test --release --locked --no-fail-fast --workspace --exclude qwen4exp\s*$')

    def test_wall_clock_binaries_run_alone(self):
        self.assertRegex(self.config, r'(?m)^\[test-groups\.wall-clock\]\nmax-threads = 1$')
        override = re.search(r"\[\[profile\.default\.overrides\]\]\nfilter = '(.*)'\ntest-group = 'wall-clock'\n"
                             r"threads-required = 'num-test-threads'", self.config)
        self.assertIsNotNone(override)
        for binary in ('binary_id(sched)', 'binary_id(server)'):
            self.assertIn(binary, override.group(1))


if __name__ == '__main__':
    unittest.main()
