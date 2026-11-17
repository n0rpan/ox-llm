"""O-397/O-398: fake GitHub/OIDC/provider controls; no live credentials or cloud calls."""
import contextlib
import copy
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import urllib.parse

SOURCE = Path(__file__).resolve().parents[1] / 'go_review.py'
spec = importlib.util.spec_from_file_location('go_review', SOURCE)
review = importlib.util.module_from_spec(spec)
spec.loader.exec_module(review)
SHA = 'a' * 40
FILES = [{'filename': 'oxa/sample.rs', 'status': 'modified', 'additions': 1, 'deletions': 1,
          'patch': '@@ -1 +1 @@\n-old\n+new'}]
PR = {'number': 7, 'state': 'open', 'draft': False, 'author_association': 'MEMBER',
      'head': {'sha': SHA, 'repo': {'full_name': review.REPO}},
      'base': {'repo': {'full_name': review.REPO}}, 'labels': [],
      'additions': 1, 'deletions': 1, 'changed_files': 1}


def response(rows=None):
    rows = rows or []
    content = {'findings': rows, 'overall_verdict': 'incorrect' if rows else 'correct'}
    return {'choices': [{'finish_reason': 'stop', 'message': {'content': json.dumps(content)}}]}


def finding(**changes):
    row = {'title': '[P2] Preserve the value', 'body': 'A concrete failing path.', 'priority': 2,
           'confidence': 0.9, 'path': 'oxa/sample.rs', 'line_start': 1, 'line_end': 1}
    return {**row, **changes}


class GoReview(unittest.TestCase):
    def test_hash_ignores_commit_and_hunk_coordinates_but_keeps_content_and_context(self):
        moved = copy.deepcopy(FILES)
        moved[0]['patch'] = moved[0]['patch'].replace('@@ -1 +1 @@', '@@ -101 +201 @@')
        moved[0]['sha'] = 'c' * 40
        self.assertEqual(review.own_diff_hash(FILES), review.own_diff_hash(moved))
        moved[0]['patch'] += '\n context'
        self.assertNotEqual(review.own_diff_hash(FILES), review.own_diff_hash(moved))
        moved[0]['patch'] = '@@ -1 +1 @@\n-old\n+different'
        self.assertNotEqual(review.own_diff_hash(FILES), review.own_diff_hash(moved))

    def test_bad_model_paths_ranges_verdicts_and_incomplete_output_are_refused(self):
        allowed = review.changed_lines(FILES)
        for row in (finding(path='outside.rs'), finding(line_start=0), finding(line_end=30),
                    finding(priority=True), finding(confidence=float('nan'))):
            with self.assertRaises(review.Refused):
                review.findings(response([row]), allowed, [])
        incomplete = response()
        incomplete['choices'][0]['finish_reason'] = 'length'
        with self.assertRaises(review.Refused):
            review.findings(incomplete, allowed, [])
        bad = response()
        bad['choices'][0]['message']['content'] = '{"findings":[],"overall_verdict":"incorrect"}'
        with self.assertRaises(review.Refused):
            review.findings(bad, allowed, [])

    def test_cached_diff_requires_a_trusted_receipt(self):
        marker = {'sha': 'b' * 40, 'diff_hash': review.own_diff_hash(FILES), 'verdict': 'CLEAR'}
        comments = [{'user': {'login': 'github-actions[bot]'},
                     'body': '<!-- go-review:v1 ' + json.dumps(marker) + ' -->'}]
        self.assertTrue(review.reviewed(comments, SHA, marker['diff_hash']))
        comments[0]['user']['login'] = 'outside-author'
        self.assertFalse(review.reviewed(comments, SHA, marker['diff_hash']))

    def test_instruction_and_author_boundaries(self):
        self.assertTrue(review.eligible(PR, SHA))
        self.assertFalse(review.eligible({**PR, 'draft': True}, SHA))
        self.assertFalse(review.eligible({**PR, 'author_association': 'NONE'}, SHA))
        self.assertTrue(review.eligible({**PR, 'author_association': 'NONE',
                                       'labels': [{'name': 'ok-to-review'}]}, SHA))
        prompt = review.review_prompt()
        self.assertIn('Never expose credentials', prompt)
        self.assertIn('# Review rubric', prompt)
        self.assertIn('budget caps', prompt)
        self.assertIn('"overall_verdict"', prompt)


if __name__ == '__main__':
    unittest.main()
