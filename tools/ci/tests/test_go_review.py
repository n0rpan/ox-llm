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
    def run_review(self, *, pr=None, files=None, comments=None, answer=None, stale=False, models=None, argon=None):
        pr = copy.deepcopy(PR if pr is None else pr)
        files = FILES if files is None else files
        calls, posted, pulls = [], [], 0

        def request(url, headers, body=None, **kwargs):
            nonlocal pulls
            calls.append(url)
            if url == review.API + '/pulls/7':
                pulls += 1
                value = copy.deepcopy(pr)
                if stale and pulls == 3:
                    value['head']['sha'] = 'b' * 40
                return value
            if '/files?' in url:
                return files
            if '/comments?' in url:
                return comments or []
            if url == review.ARGON_URL:
                payload = json.loads(body)
                self.assertEqual(payload['generationConfig']['responseMimeType'], 'application/json')
                self.assertIn('Project guidelines:', payload['systemInstruction']['parts'][0]['text'])
                self.assertNotIn('synthetic-access', json.dumps(payload))
                return argon if argon is not None else {'candidates': [{'finishReason': 'STOP',
                    'content': {'parts': [{'text': response()['choices'][0]['message']['content']}]}}]}
            if url == review.MODEL_URL:
                payload = json.loads(body)
                self.assertEqual(payload['model'], 'deepseek-v4.1-flash')
                self.assertFalse(payload['stream'])
                self.assertIn('Project guidelines:', payload['messages'][0]['content'])
                self.assertIn('# Review rubric', payload['messages'][0]['content'])
                self.assertIn('"overall_verdict"', payload['messages'][0]['content'])
                self.assertNotIn('synthetic-access', payload['messages'][0]['content'])
                return answer or response()
            self.assertEqual(url, review.API + '/issues/7/comments')
            posted.append(json.loads(body)['body'])
            return {'id': 1}

        with tempfile.TemporaryDirectory() as tmp:
            event = Path(tmp) / 'event.json'
            event.write_text(json.dumps({'action': 'synchronize', 'pull_request': pr}))
            env = {'HOME': tmp, 'GITHUB_EVENT_PATH': str(event), 'GITHUB_EVENT_NAME': 'pull_request',
                   'GITHUB_REPOSITORY': review.REPO, 'GH_TOKEN': 'synthetic-github',
                   'ACTIONS_ID_TOKEN_REQUEST_TOKEN': 'synthetic-request'}
            if models is not None:
                env['GO_REVIEW_MODELS'] = models
            with patch.dict(os.environ, env, clear=True), patch.object(review, 'request', request), \
                    patch.object(review, 'access_token', return_value='synthetic-access') as auth:
                review.main()
                return calls, posted, auth.call_count

    def test_complete_review_posts_one_full_sha_and_diff_hash_receipt(self):
        calls, posted, auth = self.run_review(answer=response([finding(body='Do not emit synthetic-access')]))
        self.assertEqual(auth, 1)
        self.assertEqual(calls.count(review.MODEL_URL), 1)
        self.assertEqual(len(posted), 1)
        self.assertNotIn('synthetic-access', posted[0])
        marker = review.MARKER.findall(posted[0])
        self.assertEqual(len(marker), 1)
        self.assertEqual(json.loads(marker[0]),
                         {'sha': SHA, 'model': review.MODEL, 'verdict': 'REVISE',
                          'findings': 1, 'diff_hash': review.own_diff_hash(FILES)})

    def test_clean_comment_has_required_machine_readable_marker_on_its_own_line(self):
        _, posted, _ = self.run_review()
        marker_line = posted[0].splitlines()[-1]
        match = review.MARKER.fullmatch(marker_line)
        self.assertIsNotNone(match)
        receipt = json.loads(match[1])
        self.assertEqual(receipt, {'sha': SHA, 'model': review.MODEL, 'verdict': 'OK',
                                  'findings': 0, 'diff_hash': review.own_diff_hash(FILES)})
        comments = [{'user': {'login': 'github-actions[bot]'}, 'body': posted[0]}]
        self.assertEqual(self.run_review(comments=comments)[1:], ([], 0))

    def test_second_model_is_explicitly_enabled_and_scope_limited(self):
        models = json.dumps([review.MODEL, review.ARGON])
        for files, labels in ((FILES, [{'name': 'two-review'}]),
                              ([{**FILES[0], 'filename': 'oxa/oxa-core/src/lib.rs'}], []),
                              ([{**FILES[0], 'previous_filename': 'engine-perf/probe.rs'}], []),
                              ([{**FILES[0], 'filename': 'oxa-work-board/src/lib.rs'}], [])):
            pr = {**copy.deepcopy(PR), 'labels': labels}
            calls, posted, auth = self.run_review(pr=pr, files=files, models=models)
            self.assertEqual((calls.count(review.MODEL_URL), calls.count(review.ARGON_URL)), (1, 1))
            self.assertEqual((len(posted), auth), (1, 1))
            self.assertEqual(len(review.MARKER.findall(posted[0])), 1)
            self.assertIn(review.ARGON, posted[0])
        calls, _, _ = self.run_review(models=models)
        self.assertNotIn(review.ARGON_URL, calls)
        pr = {**copy.deepcopy(PR), 'labels': [{'name': 'two-review'}]}
        calls, _, _ = self.run_review(pr=pr)
        self.assertNotIn(review.ARGON_URL, calls)

    def test_invalid_model_configuration_never_authenticates(self):
        for models in ('not-json', '[]', '["unknown"]', '["gemini-4-argon"]',
                       '["deepseek-v4.1-flash","deepseek-v4.1-flash"]'):
            with self.subTest(models=models), self.assertRaises(review.Refused):
                self.run_review(models=models)

    def test_failed_second_review_posts_no_clear_receipt_or_fallback(self):
        pr = {**copy.deepcopy(PR), 'labels': [{'name': 'two-review'}]}
        for candidate in ({'finishReason': 'MAX_TOKENS', 'content': {'parts': [{'text': '{}'}]}},
                          {'finishReason': 'STOP', 'content': {'parts': [{'functionCall': {}}]}}):
            with self.subTest(candidate=candidate), self.assertRaises(review.Refused):
                self.run_review(pr=pr, models=json.dumps([review.MODEL, review.ARGON]),
                                argon={'candidates': [candidate]})

    def test_same_diff_on_a_different_head_spends_nothing(self):
        marker = {'sha': 'b' * 40, 'diff_hash': review.own_diff_hash(FILES), 'verdict': 'CLEAR'}
        comments = [{'user': {'login': 'github-actions[bot]'},
                     'body': '<!-- go-review:v1 ' + json.dumps(marker) + ' -->'}]
        calls, posted, auth = self.run_review(comments=comments)
        self.assertEqual((posted, auth), ([], 0))
        self.assertNotIn(review.MODEL_URL, calls)
        marker['diff_hash'] = '0' * 64
        marker['sha'] = SHA
        comments[0]['body'] = '<!-- go-review:v1 ' + json.dumps(marker) + ' -->'
        self.assertFalse(review.reviewed(comments, SHA, review.own_diff_hash(FILES)))
        comments[0]['user']['login'] = 'untrusted-author'
        self.assertFalse(review.reviewed(comments, SHA, marker['diff_hash']))

    def test_hash_ignores_commit_and_hunk_coordinates_but_keeps_content_and_context(self):
        moved = copy.deepcopy(FILES)
        moved[0]['patch'] = moved[0]['patch'].replace('@@ -1 +1 @@', '@@ -101 +201 @@')
        moved[0]['sha'] = 'c' * 40
        self.assertEqual(review.own_diff_hash(FILES), review.own_diff_hash(moved))
        moved[0]['patch'] += '\n context'
        self.assertNotEqual(review.own_diff_hash(FILES), review.own_diff_hash(moved))
        moved[0]['patch'] = '@@ -1 +1 @@\n-old\n+different'
        self.assertNotEqual(review.own_diff_hash(FILES), review.own_diff_hash(moved))

    def test_docs_unknown_authors_forks_and_drafts_spend_nothing(self):
        files = [{**FILES[0], 'filename': 'oxa/docs/pages/usage.md'}]
        self.assertEqual(self.run_review(files=files)[1:], ([], 0))
        for mutate in (lambda p: p.update(draft=True),
                       lambda p: p.update(author_association='NONE'),
                       lambda p: p['head']['repo'].update(full_name='outside/fork')):
            pr = copy.deepcopy(PR)
            mutate(pr)
            self.assertEqual(self.run_review(pr=pr), ([], [], 0))
        pr = {**copy.deepcopy(PR), 'author_association': 'NONE', 'labels': [{'name': 'ok-to-review'}]}
        self.assertEqual(self.run_review(pr=pr)[2], 1)

    def test_oversize_and_truncated_diff_never_authenticate(self):
        with self.assertRaises(review.Skipped):
            self.run_review(pr={**PR, 'additions': 6001})
        with self.assertRaises(review.Skipped):
            self.run_review(files=[{**FILES[0], 'patch': '@@ -1 +1,2 @@\n-old\n+new', 'additions': 2}])

    def test_stale_result_is_not_posted(self):
        calls, posted, auth = self.run_review(stale=True)
        self.assertEqual(auth, 1)
        self.assertIn(review.MODEL_URL, calls)
        self.assertEqual(posted, [])

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

    def test_oidc_audience_exchange_and_masking_are_process_only(self):
        calls = []

        def request(url, headers, body=None, **kwargs):
            calls.append((url, headers, body))
            if len(calls) == 1:
                return {'value': 'synthetic-oidc'}
            return {'access_token': 'synthetic-access', 'token_type': 'Bearer'}

        env = {'OPENCODE_CI_CLIENT_ID': 'synthetic-client',
               'ACTIONS_ID_TOKEN_REQUEST_URL': 'https://pipelines.actions.githubusercontent.com/token?a=1',
               'ACTIONS_ID_TOKEN_REQUEST_TOKEN': 'synthetic-request'}
        with patch.dict(os.environ, env, clear=True), patch.object(review, 'request', request), \
                contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(review.access_token(), 'synthetic-access')
            self.assertEqual(os.environ['OPENCODE_API_KEY'], 'synthetic-access')
        audience = urllib.parse.parse_qs(urllib.parse.urlsplit(calls[0][0]).query)
        self.assertEqual(audience['audience'], ['https://opencode.ai/console'])
        self.assertEqual(calls[1][0], review.TOKEN_URL)
        form = urllib.parse.parse_qs(calls[1][2].decode())
        self.assertEqual(form['subject_token'], ['synthetic-oidc'])
        self.assertEqual(form['client_id'], ['synthetic-client'])
        self.assertEqual(form['grant_type'], ['urn:ietf:params:oauth:grant-type:token-exchange'])
        self.assertEqual(form['subject_token_type'], ['urn:ietf:params:oauth:token-type:jwt'])
        self.assertEqual(output.getvalue().splitlines(),
                         ['::add-mask::synthetic-oidc', '::add-mask::synthetic-access'])

    def test_workflow_uses_trusted_base_and_narrow_permissions(self):
        text = (SOURCE.parents[2] / '.github/workflows/go-review.yml').read_text()
        self.assertIn('ref: ${{ github.event.pull_request.base.sha }}', text)
        self.assertIn('persist-credentials: false', text)
        self.assertIn('id-token: write', text)
        self.assertIn('GO_REVIEW_MODELS: ${{ vars.GO_REVIEW_MODELS }}', text)
        self.assertIn("github.event.label.name == 'two-review'", text)
        self.assertIn("vars.GO_REVIEW_ENABLED == 'true'", text)
        self.assertIn('pull-requests: write', text)
        self.assertNotIn('pull_request_target:', text)
        self.assertNotIn('secrets.', text)

    def test_transport_has_no_redirect_or_proxy_fallback_and_bounds_response(self):
        with patch.object(review.urllib.request, 'build_opener') as build:
            reply = build.return_value.open.return_value.__enter__.return_value
            reply.read.return_value = b'x' * 11
            with self.assertRaises(review.Refused):
                review.request('https://example.invalid', {}, limit=10)
            reply.read.assert_called_once_with(11)
            self.assertIsInstance(build.call_args.args[0], review.urllib.request.ProxyHandler)
            self.assertEqual(build.call_args.args[0].proxies, {})
            self.assertIsInstance(build.call_args.args[1], review.NoRedirect)
        self.assertIsNone(review.NoRedirect().redirect_request(None, None, 302, '', {}, 'https://example.invalid'))


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
