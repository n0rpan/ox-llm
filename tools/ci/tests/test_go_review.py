"""O-397/O-398: fake GitHub/OIDC/provider controls; no live credentials or cloud calls."""
import contextlib
import copy
from datetime import UTC, datetime, timedelta
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
MODEL = 'deepseek-v4.1-flash'
GLM = 'glm-5.3'
ARGON = 'gemini-4-argon'
MODEL_URL = review.MODEL_URL
ARGON_URL = review.GOOGLE_BASE + ARGON + ':generateContent'
SHA = 'a' * 40
FILES = [{'filename': 'oxa/sample.rs', 'status': 'modified', 'additions': 1, 'deletions': 1,
          'patch': '@@ -1 +1 @@\n-old\n+new'}]
PR = {'number': 7, 'state': 'open', 'draft': False, 'author_association': 'MEMBER',
      'head': {'sha': SHA, 'repo': {'full_name': review.REPO}},
      'base': {'ref': 'main', 'repo': {'full_name': review.REPO}}, 'labels': [],
      'additions': 1, 'deletions': 1, 'changed_files': 1}


def quota_comment(comment_id=11, verdict='OK', *, yesterday=False, login='github-actions[bot]'):
    date = datetime.now(UTC) - timedelta(days=int(yesterday))
    receipt = {'sha': 'b' * 40, 'diff_hash': 'c' * 64, 'verdict': verdict}
    return {'id': comment_id, 'user': {'login': login}, 'created_at': date.strftime('%Y-%m-%dT%H:%M:%SZ'),
            'body': '<!-- go-review:v1 ' + json.dumps(receipt) + ' -->'}


def response(rows=None):
    rows = rows or []
    content = {'findings': rows, 'overall_verdict': 'incorrect' if rows else 'correct'}
    return {'choices': [{'finish_reason': 'stop', 'message': {'content': json.dumps(content)}}]}


def finding(**changes):
    row = {'title': '[P2] Preserve the value', 'body': 'A concrete failing path.', 'priority': 2,
           'confidence': 0.9, 'path': 'oxa/sample.rs', 'line_start': 1, 'line_end': 1}
    return {**row, **changes}


class GoReview(unittest.TestCase):
    def run_review(self, *, pr=None, files=None, comments=None, answer=None, stale=False, models=None,
                   argon=None, current_base=None, daily_comments=None, max_per_day=None):
        pr = copy.deepcopy(PR if pr is None else pr)
        files = FILES if files is None else files
        calls, posted, pulls = [], [], 0

        def request(url, headers, body=None, **kwargs):
            nonlocal pulls
            calls.append(url)
            if url == review.API + '/pulls/7':
                pulls += 1
                value = copy.deepcopy(pr)
                if current_base is not None:
                    value['base']['ref'] = current_base
                if stale and pulls == 3:
                    value['head']['sha'] = 'b' * 40
                return value
            if '/files?' in url:
                return files
            if url.startswith(review.API + '/issues/comments?'):
                return daily_comments or []
            if '/comments?' in url:
                return comments or []
            if url.startswith(review.GOOGLE_BASE):
                self.assertIn('"verdict":"RESERVED"', posted[0])
                payload = json.loads(body)
                self.assertEqual(payload['generationConfig']['responseMimeType'], 'application/json')
                self.assertEqual(payload['generationConfig']['maxOutputTokens'], 8192)
                self.assertIn('Project guidelines:', payload['systemInstruction']['parts'][0]['text'])
                self.assertNotIn('synthetic-access', json.dumps(payload))
                return argon if argon is not None else {'candidates': [{'finishReason': 'STOP',
                    'content': {'parts': [{'text': response()['choices'][0]['message']['content']}]}}]}
            if url == MODEL_URL:
                self.assertIn('"verdict":"RESERVED"', posted[0])
                payload = json.loads(body)
                self.assertFalse(payload['model'].startswith('gemini-'))
                self.assertEqual(payload['max_tokens'], review.max_output(payload['model']))
                self.assertFalse(payload['stream'])
                self.assertIn('Project guidelines:', payload['messages'][0]['content'])
                self.assertIn('# Review rubric', payload['messages'][0]['content'])
                self.assertIn('"overall_verdict"', payload['messages'][0]['content'])
                self.assertNotIn('synthetic-access', payload['messages'][0]['content'])
                return answer or response()
            body = json.loads(body)['body']
            if kwargs.get('method') == 'PATCH':
                self.assertEqual(url, review.API + '/issues/comments/1')
                self.assertEqual(len(posted), 1)
                posted[0] = body
                return {'id': 1}
            self.assertEqual(url, review.API + '/issues/7/comments')
            self.assertEqual(posted, [])
            posted.append(body)
            return {**quota_comment(1), 'issue_url': review.API + '/issues/7', 'body': body}

        with tempfile.TemporaryDirectory() as tmp:
            event = Path(tmp) / 'event.json'
            event.write_text(json.dumps({'action': 'synchronize', 'pull_request': pr}))
            env = {'HOME': tmp, 'GITHUB_EVENT_PATH': str(event), 'GITHUB_EVENT_NAME': 'pull_request',
                   'GITHUB_REPOSITORY': review.REPO, 'GH_TOKEN': 'synthetic-github',
                   'ACTIONS_ID_TOKEN_REQUEST_TOKEN': 'synthetic-request'}
            if models is not None:
                env['GO_REVIEW_MODELS'] = models
            if max_per_day is not None:
                env['GO_REVIEW_MAX_PER_DAY'] = max_per_day
            with patch.dict(os.environ, env, clear=True), patch.object(review, 'request', request), \
                    patch.object(review, 'access_token', return_value='synthetic-access') as auth:
                review.main()
                return calls, posted, auth.call_count

    def test_complete_review_posts_one_full_sha_and_diff_hash_receipt(self):
        calls, posted, auth = self.run_review(answer=response([finding(body='Do not emit synthetic-access')]))
        self.assertEqual(auth, 1)
        self.assertEqual(calls.count(MODEL_URL), 1)
        self.assertEqual(len(posted), 1)
        self.assertNotIn('synthetic-access', posted[0])
        marker = review.MARKER.findall(posted[0])
        self.assertEqual(len(marker), 1)
        self.assertEqual(json.loads(marker[0]),
                         {'sha': SHA, 'model': MODEL, 'verdict': 'REVISE',
                          'findings': 1, 'diff_hash': review.own_diff_hash(FILES)})

    def test_full_daily_quota_refuses_before_authentication(self):
        with self.assertRaises(review.Skipped):
            self.run_review(daily_comments=[quota_comment()], max_per_day='1')

    def test_default_cap_includes_other_prs_in_flight_and_ignores_yesterday_and_spoofs(self):
        comments = [quota_comment(i) for i in range(1, 40)]
        with self.assertRaises(review.Skipped):
            self.run_review(daily_comments=comments + [quota_comment(40, 'RESERVED')])
        comments += [quota_comment(40, yesterday=True), quota_comment(41, login='untrusted')]
        self.assertEqual(self.run_review(daily_comments=comments)[2], 1)

    def test_unverifiable_daily_counter_refuses(self):
        for listing in ({'message': 'unavailable'},
                        [{**quota_comment(), 'created_at': 'unknown'}]):
            with self.subTest(listing_type=type(listing)), self.assertRaises(review.Refused):
                self.run_review(daily_comments=listing)
        with self.assertRaises(review.Refused):
            self.run_review(max_per_day='unknown')

    def test_provider_quota_refusal_is_skipped_after_one_attempt(self):
        for url in (MODEL_URL, review.GOOGLE_BASE):
            for status in (402, 429):
                with self.subTest(url=url, status=status), \
                        patch.object(review.urllib.request, 'build_opener') as build:
                    opener = build.return_value
                    opener.open.side_effect = review.urllib.error.HTTPError(url, status, '', {}, None)
                    with self.assertRaises(review.Skipped):
                        review.request(url, {}, b'{}')
                    self.assertEqual(opener.open.call_count, 1)

    def test_oversized_review_input_never_reaches_provider(self):
        with patch.object(review, 'request') as send, self.assertRaises(review.Skipped):
            review.model_response(MODEL, 'synthetic', 'prompt', 'é' * review.MAX_INPUT_BYTES, 'session')
        send.assert_not_called()

    def test_clean_comment_has_required_machine_readable_marker_on_its_own_line(self):
        _, posted, _ = self.run_review()
        marker_line = posted[0].splitlines()[-1]
        match = review.MARKER.fullmatch(marker_line)
        self.assertIsNotNone(match)
        receipt = json.loads(match[1])
        self.assertEqual(receipt, {'sha': SHA, 'model': MODEL, 'verdict': 'OK',
                                  'findings': 0, 'diff_hash': review.own_diff_hash(FILES)})
        comments = [{'user': {'login': 'github-actions[bot]'}, 'body': posted[0]}]
        self.assertEqual(self.run_review(comments=comments)[1:], ([], 0))

    def test_second_model_is_explicitly_enabled_and_scope_limited(self):
        models = json.dumps([MODEL, ARGON])
        for files, labels in ((FILES, [{'name': 'two-review'}]),
                              ([{**FILES[0], 'filename': 'oxa/oxa-core/src/lib.rs'}], []),
                              ([{**FILES[0], 'previous_filename': 'engine-perf/probe.rs'}], []),
                              ([{**FILES[0], 'filename': 'oxa-work-board/src/lib.rs'}], [])):
            pr = {**copy.deepcopy(PR), 'labels': labels}
            calls, posted, auth = self.run_review(pr=pr, files=files, models=models)
            self.assertEqual((calls.count(MODEL_URL), calls.count(ARGON_URL)), (1, 1))
            self.assertEqual((len(posted), auth), (1, 1))
            self.assertEqual(len(review.MARKER.findall(posted[0])), 1)
            self.assertIn(ARGON, posted[0])
        calls, _, _ = self.run_review(models=models)
        self.assertFalse(any(c.startswith(review.GOOGLE_BASE) for c in calls))
        pr = {**copy.deepcopy(PR), 'labels': [{'name': 'two-review'}]}
        calls, _, _ = self.run_review(pr=pr)
        self.assertFalse(any(c.startswith(review.GOOGLE_BASE) for c in calls))

    def test_invalid_model_configuration_never_authenticates(self):
        for models in ('not-json', '[]', '["gemini-4-argon","glm-5.3","deepseek-v4.1-flash"]',
                       '["deepseek-v4.1-flash","deepseek-v4.1-flash"]'):
            with self.subTest(models=models), self.assertRaises(review.Refused):
                self.run_review(models=models)

    def test_any_valid_go_model_id_is_accepted(self):
        calls, posted, auth = self.run_review(models=json.dumps(['glm-5.3']))
        self.assertEqual((calls.count(MODEL_URL), auth, len(posted)), (1, 1, 1))
        self.assertEqual(json.loads(review.MARKER.findall(posted[0])[0])['model'], 'glm-5.3')
        pr = {**copy.deepcopy(PR), 'labels': [{'name': 'two-review'}]}
        calls, posted, auth = self.run_review(pr=pr, models=json.dumps(['kimi-k3', 'glm-5.3']))
        self.assertEqual((calls.count(MODEL_URL), auth, len(posted)), (2, 1, 1))
        self.assertEqual(json.loads(review.MARKER.findall(posted[0])[0])['model'], 'kimi-k3,glm-5.3')

    def test_invalid_id_is_refused(self):
        for models in ('["glm 5.3"]', '["glm/5.3"]', '["GLM-5.3"]', '["' + 'a' * 65 + '"]', '[""]',
                       '["../x"]', '["glm-5.3","glm-5.3"]', '["glm-5.3","kimi-k3","x"]', '[1]'):
            with self.subTest(models=models), self.assertRaises(review.Refused):
                self.run_review(models=models)

    def test_gemini_ids_use_google_endpoint(self):
        calls, _, _ = self.run_review(models=json.dumps(['gemini-4-argon']))
        self.assertEqual((calls.count(ARGON_URL), calls.count(MODEL_URL)), (1, 0))
        calls, _, _ = self.run_review(models=json.dumps(['gemini-5-flash']))
        self.assertIn(review.GOOGLE_BASE + 'gemini-5-flash:generateContent', calls)
        self.assertNotIn(MODEL_URL, calls)

    def test_others_use_openai_endpoint(self):
        calls, _, _ = self.run_review(models=json.dumps(['kimi-k3']))
        self.assertEqual((calls.count(MODEL_URL), calls.count(ARGON_URL)), (1, 0))
        self.assertFalse(any(c.startswith(review.GOOGLE_BASE) for c in calls))

    def test_default_when_unset(self):
        self.assertEqual(review.review_models(PR, FILES), [MODEL])
        self.assertEqual(review.DEFAULT_MODELS, [MODEL])
        pr = {**copy.deepcopy(PR), 'labels': [{'name': 'two-review'}]}
        calls, posted, _ = self.run_review(pr=pr)
        self.assertEqual((calls.count(MODEL_URL), calls.count(ARGON_URL)), (1, 0))
        self.assertEqual(json.loads(review.MARKER.findall(posted[0])[0])['model'], MODEL)

    def test_two_models_any_order_accepted(self):
        pr = {**copy.deepcopy(PR), 'labels': [{'name': 'two-review'}]}
        for order in ([ARGON, GLM], [GLM, MODEL], [GLM, ARGON], [ARGON, MODEL]):
            with self.subTest(order=order):
                calls, posted, auth = self.run_review(pr=pr, models=json.dumps(order))
                self.assertEqual(auth, 1)
                self.assertEqual(len(posted), 1)
                marker = json.loads(review.MARKER.findall(posted[0])[0])
                self.assertEqual(marker['model'], ','.join(order))
                self.assertEqual(calls.count(ARGON_URL), order.count(ARGON))
                self.assertEqual(calls.count(MODEL_URL), len(order) - order.count(ARGON))

    def test_two_review_label_does_not_consume_a_second_slot_for_the_same_diff(self):
        _, first, _ = self.run_review()
        comments = [{'user': {'login': 'github-actions[bot]'}, 'body': first[0]}]
        pr = {**copy.deepcopy(PR), 'labels': [{'name': 'two-review'}]}
        models = json.dumps([MODEL, ARGON])
        calls, posted, auth = self.run_review(pr=pr, comments=comments, models=models)
        self.assertEqual((calls.count(MODEL_URL), calls.count(ARGON_URL)), (0, 0))
        self.assertEqual((posted, auth), ([], 0))

    def test_gemini_text_metadata_and_thoughts_preserve_the_final_review(self):
        pr = {**copy.deepcopy(PR), 'labels': [{'name': 'two-review'}]}
        final_text = response([finding(title='[P2] Argon finding')])['choices'][0]['message']['content']
        argon = {'candidates': [{'finishReason': 'STOP', 'content': {'parts': [
            {'text': 'Thought text must stay out of JSON.', 'thought': True, 'thoughtSignature': 'c2ln'},
            {'text': final_text, 'thoughtSignature': 'c2ln', 'partMetadata': {'source': 'synthetic'}},
        ]}}]}
        models = json.dumps([MODEL, ARGON])
        _, posted, _ = self.run_review(pr=pr, models=models, argon=argon)
        self.assertEqual(len(posted), 1)
        self.assertIn('[P2] Argon finding', posted[0])
        self.assertNotIn('Thought text', posted[0])

    def test_failed_second_review_posts_no_clear_receipt_or_fallback(self):
        pr = {**copy.deepcopy(PR), 'labels': [{'name': 'two-review'}]}
        for candidate in ({'finishReason': 'MAX_TOKENS', 'content': {'parts': [{'text': '{}'}]}},
                          {'finishReason': 'STOP', 'content': {'parts': [{'functionCall': {}}]}},
                          {'finishReason': 'STOP', 'content': {'parts': [
                              {'text': response()['choices'][0]['message']['content'], 'functionCall': {}}
                          ]}}):
            with self.subTest(candidate=candidate), self.assertRaises(review.Refused):
                self.run_review(pr=pr, models=json.dumps([MODEL, ARGON]),
                                argon={'candidates': [candidate]})

    def test_same_diff_on_a_different_head_spends_nothing(self):
        marker = {'sha': 'b' * 40, 'diff_hash': review.own_diff_hash(FILES), 'verdict': 'CLEAR'}
        comments = [{'user': {'login': 'github-actions[bot]'},
                     'body': '<!-- go-review:v1 ' + json.dumps(marker) + ' -->'}]
        calls, posted, auth = self.run_review(comments=comments)
        self.assertEqual((posted, auth), ([], 0))
        self.assertNotIn(MODEL_URL, calls)
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

    def test_stale_result_leaves_only_the_non_retryable_reservation(self):
        calls, posted, auth = self.run_review(stale=True)
        self.assertEqual(auth, 1)
        self.assertIn(MODEL_URL, calls)
        self.assertEqual(len(posted), 1)
        self.assertIn('"verdict":"RESERVED"', posted[0])
        comments = [{'user': {'login': 'github-actions[bot]'}, 'body': posted[0]}]
        self.assertEqual(self.run_review(comments=comments)[1:], ([], 0))

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
        self.assertRegex(text, r'(?m)^    branches: \[main\]$')
        self.assertIn("github.event.repository.default_branch == 'main'", text)
        self.assertIn("github.event.pull_request.base.ref == 'main'", text)
        self.assertIn('ref: ${{ github.event.pull_request.base.sha }}', text)
        self.assertIn('persist-credentials: false', text)
        self.assertIn('id-token: write', text)
        self.assertIn('GO_REVIEW_MODELS: ${{ vars.GO_REVIEW_MODELS }}', text)
        self.assertIn('GO_REVIEW_MAX_PER_DAY: ${{ vars.GO_REVIEW_MAX_PER_DAY }}', text)
        self.assertRegex(text, r'(?m)^    concurrency:\n      group: go-review-daily-quota\n      queue: max$')
        self.assertIn("github.event.label.name == 'two-review'", text)
        self.assertIn("vars.GO_REVIEW_ENABLED == 'true'", text)
        self.assertIn('pull-requests: write', text)
        self.assertNotIn('pull_request_target:', text)
        self.assertNotIn('secrets.', text)

    def test_review_refuses_non_main_base_before_authentication(self):
        other = {**PR, 'base': {**PR['base'], 'ref': 'topic-base'}}
        self.assertEqual(self.run_review(pr=other), ([], [], 0))
        calls, posted, auth = self.run_review(current_base='topic-base')
        self.assertEqual(calls, [review.API + '/pulls/7'])
        self.assertEqual((posted, auth), ([], 0))

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

    def test_incomplete_history_defers_instead_of_failing_open(self):
        # Busy repositories exceed the comment-listing bound. An uncountable quota or an
        # unreadable same-diff history must defer the review: no reservation, no provider call.
        busy = [{'id': i, 'user': {'login': 'someone'}, 'body': 'chat'} for i in range(1, 101)]
        with self.assertRaisesRegex(review.Skipped, 'per-PR'):
            self.run_review(comments=busy)
        with self.assertRaisesRegex(review.Skipped, 'daily'):
            self.run_review(daily_comments=busy)
        # The same bound with a configured quota set to zero still refuses: nothing counted,
        # nothing spent.
        with self.assertRaisesRegex(review.Skipped, 'quota'):
            self.run_review(daily_comments=busy, max_per_day='0')

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
