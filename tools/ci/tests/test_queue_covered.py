"""The main-push dedupe skips only a green merge-queue run at the pushed commit (#2049).

The queue pushes the commit it tested to main, so that run covers the same tree. It never
covers a run that did not pass, and never a push that changed a dependency cache key: main is
what seeds the `<prefix>main` caches.
"""
import contextlib
import importlib.util
import io
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location(
    'queue_covered', Path(__file__).resolve().parents[1] / 'queue_covered.py')
QUEUE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(QUEUE)

SHA = 'a' * 40
URL = 'https://github.invalid/oxstack/ox-llm/actions/runs/1'


def run(conclusion='success', sha=SHA, status='completed', attempt=1, created='2026-11-20T10:00:00Z'):
    return {'head_sha': sha, 'status': status, 'conclusion': conclusion,
            'run_attempt': attempt, 'created_at': created, 'html_url': URL}


class Covered(unittest.TestCase):
    def test_a_green_queue_run_at_the_pushed_commit_is_covered(self):
        skip, reason = QUEUE.covered([run()], SHA, ['oxa/src/lib.rs'])
        self.assertTrue(skip, reason)
        self.assertIn(URL, reason)

    def test_no_run_at_this_commit_is_not_covered(self):
        for runs in ([], [run(sha='b' * 40)], [run(conclusion=None, status='in_progress')],
                     [run(conclusion=None, status='queued')], [run(conclusion='cancelled')]):
            skip, reason = QUEUE.covered(runs, SHA, ['oxa/src/lib.rs'])
            self.assertFalse(skip, runs)
            self.assertTrue(reason)

    def test_the_latest_attempt_decides(self):
        failed_retry = run(conclusion='failure', attempt=2, created='2026-11-20T11:00:00Z')
        skip, _ = QUEUE.covered([run(), failed_retry], SHA, ['oxa/src/lib.rs'])
        self.assertFalse(skip)
        skip, _ = QUEUE.covered([failed_retry, run(attempt=3, created='2026-11-20T12:00:00Z')],
                                SHA, ['oxa/src/lib.rs'])
        self.assertTrue(skip)

    def test_a_dependency_input_change_must_run_on_main(self):
        for path in ('engine/Cargo.toml', 'engine/Cargo.lock', 'oxa/Cargo.toml', 'oxa/Cargo.lock',
                     'rust-toolchain.toml', '.cargo/config.toml', 'engine/.cargo/config.toml',
                     'tools/openai-contract/requirements.txt',
                     # the toolchain and flag definitions the cache key also covers
                     '.github/workflows/build.yml', '.github/workflows/checks.yml',
                     'engine/tools/ci/setup_toolchain.sh'):
            with self.subTest(path=path):
                skip, reason = QUEUE.covered([run()], SHA, ['oxa/src/lib.rs', path])
                self.assertFalse(skip)
                self.assertIn(path, reason)

    def test_unknown_push_files_never_skip(self):
        skip, reason = QUEUE.covered([run()], SHA, None)
        self.assertFalse(skip)
        self.assertIn('unknown', reason)

    def test_dependency_inputs_names_only_key_inputs(self):
        self.assertEqual(QUEUE.dependency_inputs(['oxa/src/lib.rs', 'engine/Cargo.lock', 'README.md',
                                                  '.cargo/config.toml']),
                         ['engine/Cargo.lock', '.cargo/config.toml'])
        self.assertEqual(QUEUE.dependency_inputs(['oxa/src/Cargo.toml.txt', 'engine/target/debug/x',
                                                  '.github/CODEOWNERS']), [])


class PathSelectedJob(unittest.TestCase):
    """The queue skips `desk` for a shared-crate change; main dedupes it only when the queue ran it."""

    @staticmethod
    def job(name, conclusion='success'):
        return {'name': name, 'conclusion': conclusion}

    def test_the_queue_ran_the_called_workflow_and_it_passed(self):
        self.assertTrue(QUEUE.job_ran([self.job('rustfmt (oxa)'), self.job('desk / desk check'),
                                       self.job('desk / desk test')], 'desk'))

    def test_a_job_the_queue_skipped_is_not_covered(self):
        for jobs in ([], [self.job('rustfmt (oxa)')],
                     # a skipped call is listed under the caller's name alone
                     [self.job('desk', 'skipped')], [self.job('desk')],
                     [self.job('desk / desk check'), self.job('desk / desk test', 'skipped')],
                     [self.job('desk / desk check', 'failure')],
                     [self.job('desktop / desk check')]):
            self.assertFalse(QUEUE.job_ran(jobs, 'desk'), jobs)

    def output(self, runs, jobs, changed=('oxa/oxa-cli/src/dispatch.rs',)):
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory) / 'out'
            env = {'GITHUB_OUTPUT': str(out), 'GH_TOKEN': 't'}
            with mock.patch.object(QUEUE, 'queue_runs', return_value=runs), \
                    mock.patch.object(QUEUE, 'changed_paths', return_value=list(changed)), \
                    mock.patch.object(QUEUE, 'github_json', side_effect=jobs), \
                    mock.patch.dict(os.environ, env, clear=True), \
                    mock.patch.object(sys, 'argv', ['queue_covered.py', '--repo', 'o/r', '--sha', SHA,
                                                    '--base', 'b' * 40, '--job', 'desk']), \
                    contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                QUEUE.main()
            return dict(line.split('=') for line in out.read_text().split())

    def test_main_push_after_a_queue_run_without_the_desk_job_still_runs_it(self):
        green = dict(run(), id=7)
        skipped = lambda url, token: {'jobs': [self.job('rustfmt (oxa)'), self.job('desk', 'skipped')]}
        self.assertEqual(self.output([green], skipped), {'covered': 'true', 'desk_covered': 'false'})
        ran = lambda url, token: {'jobs': [self.job('desk / desk check'), self.job('desk / desk test')]}
        self.assertEqual(self.output([green], ran), {'covered': 'true', 'desk_covered': 'true'})

    def test_any_doubt_runs_the_job(self):
        green = dict(run(), id=7)
        ran = lambda url, token: {'jobs': [self.job('desk / desk check')]}
        # the job list cannot be read
        self.assertEqual(self.output([green], OSError('down')), {'covered': 'true', 'desk_covered': 'false'})
        # the push is not covered at all: the job list is not even asked for
        self.assertEqual(self.output([green], AssertionError, changed=['oxa/Cargo.lock']),
                         {'covered': 'false', 'desk_covered': 'false'})
        self.assertEqual(self.output([], ran), {'covered': 'false', 'desk_covered': 'false'})


if __name__ == '__main__':
    unittest.main()
