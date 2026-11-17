import importlib.util
from pathlib import Path
import unittest

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


if __name__ == '__main__':
    unittest.main()
