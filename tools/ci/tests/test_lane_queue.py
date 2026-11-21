import contextlib
from datetime import datetime, timezone
import importlib.util
import io
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location('lane_queue', Path(__file__).resolve().parents[1] / 'lane_queue.py')
lane = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lane)


class QueueTiming(unittest.TestCase):
    def test_uses_job_queue_time_not_workflow_creation_or_previous_attempt(self):
        job = {'created_at': '2026-01-01T12:05:00Z', 'started_at': '2026-01-01T12:07:03Z',
               'run_created_at': '2025-12-31T10:00:00Z', 'run_attempt': 3}
        self.assertEqual(lane.queue_seconds(job), 123)

    def test_fractional_and_zero_queue_times(self):
        self.assertEqual(lane.queue_seconds({'created_at': '2026-01-01T12:00:00.125Z',
                                            'started_at': '2026-01-01T12:00:00.625Z'}), 0.5)
        self.assertEqual(lane.queue_seconds({'created_at': '2026-01-01T12:00:00Z',
                                            'started_at': '2026-01-01T12:00:00Z'}), 0)

    def test_missing_or_reversed_timestamps_do_not_report_fake_zero(self):
        with self.assertRaises(KeyError):
            lane.queue_seconds({'created_at': '2026-01-01T12:00:00Z'})
        with self.assertRaises(ValueError):
            lane.queue_seconds({'created_at': '2026-01-01T12:00:02Z',
                                'started_at': '2026-01-01T12:00:01Z'})


class CurrentJob(unittest.TestCase):
    def test_a_called_workflow_job_is_found_under_its_caller(self):
        jobs = [{'name': 'classify changed paths'}, {'name': 'desk / desk check'}, {'name': 'desk / desk test'}]
        self.assertEqual(lane.find_job(jobs, 'desk check'), jobs[1])
        # a dispatch of the workflow itself lists the plain name, and the exact name wins
        self.assertEqual(lane.find_job(jobs + [{'name': 'desk check'}], 'desk check'), {'name': 'desk check'})

    def test_a_missing_or_ambiguous_job_is_an_error(self):
        for jobs in ([], [{'name': 'not a desk check'}],
                     [{'name': 'a / desk check'}, {'name': 'b / desk check'}]):
            with self.assertRaises(ValueError):
                lane.find_job(jobs, 'desk check')


class SummaryLine(unittest.TestCase):
    def test_line_has_the_duration_so_far_and_the_runner_wait(self):
        job = {'created_at': '2026-01-01T12:00:00Z', 'started_at': '2026-01-01T12:00:40Z'}
        now = datetime(2026, 1, 1, 12, 6, 0, tzinfo=timezone.utc)
        self.assertEqual(lane.line('desk check', job, now),
                         'desk check: 320 s so far, after 40 s waiting for a runner')

    def run_line(self, env, answer):
        seen = {}

        def urlopen(request, timeout):
            seen['headers'] = dict(request.header_items())
            if isinstance(answer, Exception):
                raise answer
            return contextlib.closing(io.StringIO(answer))

        with tempfile.TemporaryDirectory() as directory:
            summary = Path(directory) / 'summary'
            env = {'GITHUB_REPOSITORY': 'o/r', 'GITHUB_RUN_ID': '1', 'GITHUB_RUN_ATTEMPT': '1',
                   'GITHUB_STEP_SUMMARY': str(summary), **env}
            with mock.patch.dict(os.environ, env, clear=True), \
                    mock.patch.object(lane.urllib.request, 'urlopen', urlopen), \
                    contextlib.redirect_stdout(io.StringIO()):
                lane.main(['--line', '--job-name', 'desk check'])
            return summary.read_text(), seen.get('headers', {})

    JOBS = ('{"jobs": [{"name": "desk / desk check", "created_at": "2026-01-01T12:00:00Z",'
            ' "started_at": "2026-01-01T12:00:40Z"}]}')

    def test_without_a_token_the_request_carries_no_credential(self):
        # A pull request from a fork gets no token (desk.yml); the line is still written.
        for env in ({}, {'GH_TOKEN': ''}):
            text, headers = self.run_line(env, self.JOBS)
            self.assertRegex(text, r'^desk check: \d+ s so far, after 40 s waiting for a runner\n$')
            self.assertNotIn('Authorization', headers)
        _, headers = self.run_line({'GH_TOKEN': 'secret'}, self.JOBS)
        self.assertEqual(headers['Authorization'], 'Bearer secret')

    def test_no_answer_is_written_down_and_never_fails_the_job(self):
        for answer in (OSError('rate limited'), '{"jobs": []}', 'not json'):
            text, _ = self.run_line({}, answer)
            self.assertIn('desk check: duration and runner wait unknown', text)
        text, _ = self.run_line({'GITHUB_RUN_ID': 'x'}, self.JOBS)
        self.assertIn('invalid workflow identity', text)

    def test_the_table_mode_still_needs_a_token(self):
        with mock.patch.dict(os.environ, {'GITHUB_REPOSITORY': 'o/r'}, clear=True):
            with self.assertRaises(KeyError):
                lane.main(['--job-name', 'lane plan'])


if __name__ == '__main__':
    unittest.main()
