"""The required gate covers every job/output; its caches save only on main, lane caches stay isolated.

Parsed line by line (no YAML library) so it runs with a bare `python3 -I`.
"""
import importlib.util
from pathlib import Path
import re
import subprocess
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


def lane_cache_problems(text):
    """Owner-authorized lane caches cannot write the required workflows' main keys."""
    found = []
    block = job_block(text, 'test')
    actions = re.findall(r'uses: ([^@\s]+)@([^\s]+)', text)
    if any(not name.startswith('actions/') or not re.fullmatch('[a-f0-9]{40}', sha)
           for name, sha in actions):
        found.append('lane actions must be GitHub-owned and SHA-pinned')
    condition = (
        "success() && github.event_name != 'merge_group' && github.ref != 'refs/heads/main' && "
        "(startsWith(github.ref, 'refs/heads/agent/') || startsWith(github.ref, 'refs/heads/codex/') || "
        "startsWith(github.ref, 'refs/heads/air/') || startsWith(github.ref, 'refs/heads/runner/')) && "
        "steps.cache.outputs.cache-hit != 'true'"
    )
    steps = re.findall(r'(?m)^      - uses: actions/cache/(restore|save)@[a-f0-9]{40}[^\n]*\n'
                       r'((?:(?:        .*|\s*)\n)*)', block + '\n')
    if sorted(kind for kind, _ in steps) != ['restore', 'save']:
        found.append('expected one explicit cache restore/save pair in lane test job')
    paths = ('          path: |\n'
             '            ${{ env.CARGO_HOME }}/registry\n'
             '            ${{ env.CARGO_HOME }}/git\n'
             '            ${{ env.CARGO_TARGET_DIR }}\n')
    for kind, step in steps:
        if paths not in step or '          key: lane-${{ steps.rust-key.outputs.key }}' not in step:
            found.append('lane cache paths/key must be explicit and lane-namespaced')
        if kind == 'save' and f'        if: {condition}\n' not in step:
            found.append('lane cache writes need success, a lane branch, and no main/queue access')
        if kind == 'restore' and '          restore-keys: lane-${{ steps.rust-key.outputs.prefix }}' not in step:
            found.append('lane cache fallback must stay in its namespace')
    return found


class Gate(unittest.TestCase):
    def setUp(self):
        self.checks = (WORKFLOWS / 'checks.yml').read_text()

    def test_gate_needs_every_job(self):
        block = job_block(self.checks, 'gate')
        needs = set(re.search(r'^    needs: \[(.*)\]', block, re.M).group(1).replace(' ', '').split(','))
        self.assertIn('if: always()', block)
        self.assertEqual(needs, set(jobs(self.checks)) - set(UNGATED))

    def test_openai_contract_runner_is_valid_shell_without_merge_markers(self):
        runner = WORKFLOWS.parents[1] / 'tools' / 'openai-contract' / 'run.sh'
        text = runner.read_text()
        self.assertNotRegex(text, r'(?m)^(<<<<<<<|=======|>>>>>>>)')
        subprocess.run(['bash', '-n', str(runner)], check=True)

    def test_secret_scan_declares_literal_receipt_sanitizer_fixtures(self):
        fixture = WORKFLOWS.parents[1] / 'oxc' / 'tools' / 'tests' / 'test_export_receipt.py'
        self.assertIn(":!oxc/tools/tests/test_export_receipt.py", self.checks)
        text = fixture.read_text()
        self.assertIn('sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', text)
        self.assertIn('-----BEGIN OPENSSH PRIVATE KEY-----', text)
        # This test's own literals are the same synthetic shapes, so the scan step excludes
        # this exact file as well (reason in checks.yml; T3 review, no string splitting).
        self.assertIn(':!tools/ci/tests/test_gate.py', self.checks)

    def test_merge_group_always_selects_complete_oxa_workspace(self):
        build = job_block(self.checks, 'build')
        self.assertIn("oxa: ${{ github.event_name == 'merge_group' || needs.changes.outputs.oxa == 'true' }}", build)
        reusable = (WORKFLOWS / 'build.yml').read_text()
        archive = job_block(reusable, 'oxa-test-build')
        shards = job_block(reusable, 'oxa-test-shards')
        self.assertIn('if: inputs.heavy && inputs.oxa', archive)
        self.assertIn('if: inputs.heavy && inputs.oxa', shards)
        self.assertIn('cargo nextest archive --profile ci --cargo-profile ci --locked --workspace', archive)
        self.assertIn('--partition hash:${{ matrix.shard }}/2', shards)
        self.assertNotIn('changed_paths', archive + shards)

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
                if path.name == 'lane.yml':
                    self.assertFalse(lane_cache_problems(text), lane_cache_problems(text))
                else:
                    self.assertIn("github.ref == 'refs/heads/main'", match.group(1), path.name)
                    self.assertIn("github.event_name != 'merge_group'", match.group(1), path.name)
            self.assertEqual(len(re.findall(r'actions/cache/save@', text)),
                             len(re.findall(r'actions/cache/restore@', text)), path.name)


    def test_lane_cache_cannot_bypass_required_main_cache_policy(self):
        text = (WORKFLOWS / 'lane.yml').read_text()
        self.assertFalse(lane_cache_problems(text))
        for changed in (
            text.replace("github.ref != 'refs/heads/main'", "true"),
            text.replace("github.event_name != 'merge_group'", "true"),
            text.replace("if: success() &&", "if: always() &&"),
            text.replace("startsWith(github.ref, 'refs/heads/agent/')", "true"),
            text.replace('key: lane-${{', 'key: rust-${{'),
            text.replace('restore-keys: lane-${{', 'restore-keys: rust-${{'),
            text.replace('${{ env.CARGO_HOME }}/registry', '~/.config'),
            text.replace('actions/cache/save@', 'other/cache/save@'),
        ):
            self.assertTrue(lane_cache_problems(changed))



class RetiredControllerJobs(unittest.TestCase):
    def test_python_controller_jobs_are_gone_and_the_gate_does_not_wait_for_them(self):
        checks = (WORKFLOWS / 'checks.yml').read_text()
        for name in ('lab-shards:', 'lab:', 'native-controller-cli:'):
            self.assertNotRegex(checks, r'(?m)^  ' + re.escape(name))
        self.assertNotIn('lab tests', checks)
        gate = job_block(checks, 'gate')
        for job in ('lab', 'lab-shards', 'native-controller-cli'):
            self.assertNotRegex(gate, r'[\[ ,]' + job + r'[,\]]')


class EngineNextest(unittest.TestCase):
    def setUp(self):
        build = (WORKFLOWS / 'build.yml').read_text()
        # Build/package, partitioned tests and the inventory are parallel jobs under one `build` result.
        self.engine = '\n'.join(job_block(build, name) for name in ('engine', 'engine-tests'))
        self.config = (WORKFLOWS.parents[1] / 'engine' / '.config' / 'nextest.toml').read_text()

    def test_engine_job_runs_nextest_doctests_and_counts(self):
        self.assertIn('tools/ci/install_nextest.sh', self.engine)
        self.assertIn('cargo nextest run --release --locked --no-fail-fast --workspace --exclude qwen4exp',
                      self.engine)
        self.assertIn('cargo test --release --locked --no-fail-fast --workspace --exclude qwen4exp --doc',
                      self.engine)
        self.assertIn('tools/ci/engine_test_count.py', self.engine)
        self.assertNotRegex(self.engine, r'cargo test --release --locked --no-fail-fast --workspace --exclude qwen4exp\s*$')

    def test_engine_tests_are_partitioned_and_flash_tests_are_split(self):
        build = (WORKFLOWS / 'build.yml').read_text()
        tests = job_block(build, 'engine-tests')
        self.assertIn('shard: [1, 2, 3]', tests)
        self.assertIn('--partition hash:${{ matrix.shard }}/3', tests)
        self.assertNotIn('nextest run', job_block(build, 'engine'))
        self.assertIn('tools/ci/engine_test_count.py', tests)
        flash = job_block(build, 'flash-tests')
        self.assertIn('part: [server, qwen4exp]', flash)
        self.assertIn('flash-m3 selftest', flash)
        self.assertNotIn('cargo test', job_block(build, 'flash'))

    def test_wall_clock_tests_run_alone_and_server_is_not_whole(self):
        self.assertRegex(self.config, r'(?m)^\[test-groups\.wall-clock\]\nmax-threads = 1$')
        override = re.search(r"\[\[profile\.default\.overrides\]\]\nfilter = \'\'\'(.*?)\'\'\'\ntest-group = 'wall-clock'\n"
                             r"threads-required = 'num-test-threads'", self.config, re.S)
        self.assertIsNotNone(override)
        flt = override.group(1)
        self.assertIn('binary_id(sched)', flt)
        # the server lib tests run in parallel except the named elapsed-time tests
        self.assertNotRegex(flt, r'binary_id\(server\)\s*(\||$)')
        self.assertIn('binary_id(server) & (', flt)


if __name__ == '__main__':
    unittest.main()
