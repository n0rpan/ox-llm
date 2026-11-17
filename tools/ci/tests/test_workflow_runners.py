"""Every workflow job runs on a hosted ARM image or on the project's runner expression.

A job whose `runs-on` names a hosted x86_64 image directly (ubuntu-latest, ubuntu-24.04, ...)
bypasses the project's runners for good; this check keeps the x86_64/ARM split automatic.
Parsed line by line (no YAML library) so it runs with a bare `python3 -I`.
"""
from pathlib import Path
import re
import unittest

WORKFLOWS = Path(__file__).resolve().parents[3] / '.github' / 'workflows'

ARM_IMAGE = 'ubuntu-24.04-arm'
# The runner expression from checks.yml: the project's runners when the switch is on, otherwise
# (or for fork pull requests) the hosted x86_64 image.
RUNNER_CONDITION = (
    "vars.OXCI == 'on' && (github.event_name != 'pull_request' && 'oxci' || (vars.OXCI_PR == 'on' && "
    "github.event.pull_request.head.repo.full_name == github.repository) && 'oxci-pr') || 'ubuntu-24.04'"
)
RUNNER_EXPRESSION = '${{ ' + RUNNER_CONDITION + ' }}'

# Workflows allowed to name hosted images directly, with the reason.
EXCEPTIONS = {
    'release.yml': 'publishes releases; published binaries are never built on the project\'s own runners',
    'release-build.yml': 'builds the published binaries; they are never built on the project\'s own runners',
}

FIX = (
    "Run the job on the hosted ARM image ({arm}) or copy the runner expression from "
    ".github/workflows/checks.yml:\n    runs-on: {expr}"
)


def jobs_runs_on(text):
    """Yield (job, runs-on value, matrix values by key) for every job in a workflow."""
    jobs, in_jobs, job = {}, False, None
    for line in text.splitlines():
        if re.match(r'^jobs:\s*$', line):
            in_jobs = True
            continue
        if in_jobs and re.match(r'^\S', line) and not line.startswith('#'):
            in_jobs = False
        if not in_jobs:
            continue
        m = re.match(r'^  ([A-Za-z0-9_-]+):\s*$', line)
        if m:
            job = m.group(1)
            jobs[job] = {'runs_on': None, 'matrix': {}, 'runner_values': []}
            continue
        if job is None:
            continue
        m = re.match(r'^    runs-on:\s*(.+?)\s*$', line)
        if m:
            jobs[job]['runs_on'] = m.group(1).strip('"\'') if not m.group(1).startswith('${{') else m.group(1)
            continue
        scalar_runner = re.match(r'^\s+(?:-\s+)?runner:\s*(.+?)\s*$', line)
        if scalar_runner and not scalar_runner.group(1).startswith('['):
            jobs[job]['runner_values'].append(scalar_runner.group(1).strip(chr(34) + chr(39)))
        m = re.match(r'^\s+([A-Za-z0-9_-]+):\s*\[(.*)\]\s*$', line)
        if m:
            values = [v.strip().strip('"\'') for v in m.group(2).split(',') if v.strip()]
            jobs[job]['matrix'][m.group(1)] = values
    return jobs


def problems(name, text):
    found = []
    for job, info in jobs_runs_on(text).items():
        value = info['runs_on']
        if value is None:
            continue  # reusable-workflow call: no runner of its own
        # #795: the dedicated native ARM macOS qualification cannot use a Linux runner.
        if (name, job, value) == ('macos.yml', 'oxa-macos', 'macos-15'):

        # Owner lane CI: standard free Linux runners, including explicit matrix overrides.
        free = {'ubuntu-24.04', ARM_IMAGE}
        if ((name, job, value) == ('lane.yml', 'test', '${{ matrix.runner }}')
                and set(info['matrix'].get('runner', [])) == free
                and all(runner in free for runner in info['runner_values'])):
            continue
        if value == ARM_IMAGE or value == RUNNER_EXPRESSION:
            continue
        if value.startswith('${{'):
            refs = re.findall(r'matrix\.([A-Za-z0-9_-]+)', value)
            ok = bool(refs)
            for key in set(refs):
                for v in info['matrix'].get(key, [None]):
                    if v == ARM_IMAGE:
                        continue
                    # an x86 matrix entry must be routed through the runner expression
                    guard = f"matrix.{key} == '{v}' && ({RUNNER_CONDITION})"
                    if v is None or guard not in value:
                        ok = False
            if ok:
                continue
        found.append(f'{name}: job {job!r} has runs-on: {value}')
    return found


class WorkflowRunners(unittest.TestCase):
    def test_no_job_names_a_hosted_x86_image(self):
        checks = (WORKFLOWS / 'checks.yml').read_text()
        self.assertIn(RUNNER_EXPRESSION, checks, 'the runner expression in this test must match checks.yml')
        found = []
        files = sorted(WORKFLOWS.glob('*.y*ml'))
        self.assertTrue(files)
        for path in files:
            if path.name in EXCEPTIONS:
                continue
            found += problems(path.name, path.read_text())
        self.assertFalse(found, '\n'.join(found) + '\n' + FIX.format(arm=ARM_IMAGE, expr=RUNNER_EXPRESSION))

    def test_exceptions_exist(self):
        for name in EXCEPTIONS:
            self.assertTrue((WORKFLOWS / name).is_file(), name)

    def test_detects_hosted_images(self):
        for image in ('ubuntu-latest', 'ubuntu-24.04', 'ubuntu-22.04', '"ubuntu-latest"'):
            text = f'on: push\njobs:\n  a:\n    runs-on: {image}\n'
            self.assertTrue(problems('x.yml', text), image)
        text = 'jobs:\n  a:\n    strategy:\n      matrix:\n        r: [ubuntu-latest]\n    runs-on: ${{ matrix.r }}\n'
        self.assertTrue(problems('x.yml', text))
        expr = RUNNER_EXPRESSION.replace("|| 'ubuntu-24.04' }}", "|| 'ubuntu-22.04' }}")
        self.assertTrue(problems('x.yml', f'jobs:\n  a:\n    runs-on: {expr}\n'))

    def test_macos_arm_qualification_is_exact(self):
        text = 'jobs:\n  oxa-macos:\n    runs-on: macos-15\n'
        self.assertFalse(problems('macos.yml', text))
        self.assertTrue(problems('other.yml', text))
        self.assertTrue(problems('macos.yml', text.replace('oxa-macos:', 'other:')))
        for image in ('macos-13', 'macos-15-intel', 'ubuntu-latest', 'self-hosted'):
            self.assertTrue(problems('macos.yml', text.replace('macos-15', image)))

    def test_lane_matrix_accepts_only_its_fixed_free_runners(self):
        text = ('jobs:\n  test:\n    runs-on: ${{ matrix.runner }}\n'
                '    strategy:\n      matrix:\n        runner: [ubuntu-24.04, ubuntu-24.04-arm]\n')
        self.assertFalse(problems('lane.yml', text))
        self.assertTrue(problems('other.yml', text))
        self.assertTrue(problems('lane.yml', text.replace('test:', 'other:')))
        for runner in ('self-hosted', 'oxci', 'macos-15'):
            self.assertTrue(problems('lane.yml', text.replace('ubuntu-24.04,', runner + ',')))
            override = text + '        include:\n          - runner: ' + runner + '\n'
            self.assertTrue(problems('lane.yml', override))
        override = text + '        include:\n          - runner: ${{ inputs.runner }}\n'
        self.assertTrue(problems('lane.yml', override))

    def test_accepts_arm_and_expression(self):
        for value in (ARM_IMAGE, RUNNER_EXPRESSION):
            self.assertFalse(problems('x.yml', f'jobs:\n  a:\n    runs-on: {value}\n'))
        guarded = ("${{ matrix.r == 'ubuntu-latest' && (" + RUNNER_CONDITION + ") || matrix.r }}")
        text = f'jobs:\n  a:\n    strategy:\n      matrix:\n        r: [ubuntu-latest, {ARM_IMAGE}]\n    runs-on: {guarded}\n'
        self.assertFalse(problems('x.yml', text))


if __name__ == '__main__':
    unittest.main()
