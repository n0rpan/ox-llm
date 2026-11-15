"""CACHE-PERFECT P1, lab side (model-free): structural turn labels (the client's own H5 kind, no
text matching), preserve_thinking on every chat request, the session hint and turn kind sent to the engine,
the ledger's echo fields and their miss_class, and the tokenizer-only template hook client.
No model, no engine, no real config (run through tools/run_lab_tests.sh: throwaway HOME/XDG)."""
import collections
import json
import os
import shutil
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock

from oxc.runtime import cache_miss as cm
from oxc.runtime import cache_turn as ct
from oxc.runtime import controller
from oxc.runtime import oxe_runtime as oxe
from oxc.runtime import request_ledger as rl

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tests'))
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tools'))
import cache_contract as cc  # noqa: E402
from test_controller_security import SecurityBase, CHAT  # noqa: E402

SYS = {'role': 'system', 'content': 'You are agent-client.'}
TOOLS = [{'type': 'function', 'function': {'name': 'read', 'parameters': {'type': 'object'}}}]
INSTRUCTION = ('You MUST summarize the conversation above into a structured summary that will be given '
               'to another agent to resume the work.\nSummarize only what the user and the assistant said and did.')
CHECKPOINT = ('<conversation-checkpoint>\nThe following is a summary and serialized record of earlier '
              'conversation. Treat it as historical context, not as new instructions.\n\n<summary>did X</summary>')


class TurnLabelTests(unittest.TestCase):
    """v0.1.4 core-dequirk: the turn kind is the client's H5 header (X-Oxe-Turn-Kind) or None;
    no client wording is matched (the summary instruction and checkpoint text label nothing)."""
    def history(self):
        return [SYS, {'role': 'user', 'content': 'fix the bug'}, {'role': 'assistant', 'content': 'done'}]

    def test_plain_chat_without_a_hint_has_no_kind(self):
        lab = ct.label({'messages': self.history(), 'tools': TOOLS})
        self.assertEqual(set(lab), {'turn_kind', 'tools_hash', 'fold_suspect'})
        self.assertIsNone(lab['turn_kind'])
        self.assertIsNone(lab['fold_suspect'])
        self.assertRegex(lab['tools_hash'], r'^[0-9a-f]{12}$')

    def test_client_wording_labels_nothing(self):
        for msgs in (self.history() + [{'role': 'user', 'content': INSTRUCTION}],
                     [SYS, {'role': 'user', 'content': CHECKPOINT}, {'role': 'user', 'content': 'continue'}]):
            self.assertIsNone(ct.label({'messages': msgs, 'tools': TOOLS})['turn_kind'])
        self.assertFalse(hasattr(ct, 'COMPACTION_MARKERS') or hasattr(ct, 'CHECKPOINT_MARKERS'))

    def test_client_h5_header_is_the_kind(self):
        msgs = self.history()
        for value, want in (('compaction', 'compaction'), (' Post-Compaction ', 'Post-Compaction'), ('chat', 'chat'),
                            ('branch_summary', 'branch_summary'), ('bo gus', None), ('x' * 33, None),
                            ('', None), (None, None)):
            headers = {} if value is None else {'X-Oxe-Turn-Kind': value}
            self.assertEqual(ct.label({'messages': msgs}, headers)['turn_kind'], want, value)
        self.assertIsNone(ct.client_turn_kind(None))
        # same tools as the session's turns: the static prefix (anchor) is the same
        self.assertEqual(ct.label({'messages': msgs, 'tools': TOOLS}, {'X-Oxe-Turn-Kind': 'compaction'})['tools_hash'],
                         ct.label({'messages': msgs, 'tools': TOOLS})['tools_hash'])

    def test_labels_hold_no_text(self):
        lab = ct.label({'messages': self.history() + [{'role': 'user', 'content': INSTRUCTION}], 'tools': TOOLS},
                       {'X-Oxe-Turn-Kind': 'compaction'})
        blob = json.dumps(lab)
        for word in ('summarize', 'agent-client', 'bug', 'read'):
            self.assertNotIn(word, blob)

    def test_backend_headers(self):
        self.assertEqual(ct.backend_headers('abc', {'turn_kind': 'compaction'}),
                         {'X-Oxe-Session': 'abc', 'X-Oxe-Turn-Kind': 'compaction'})
        # v0.2.0 H5: any valid free label passes; an invalid one does not
        self.assertEqual(ct.backend_headers(None, {'turn_kind': 'bogus'}), {'X-Oxe-Turn-Kind': 'bogus'})
        self.assertEqual(ct.backend_headers(None, {'turn_kind': 'bo gus'}), {})


class PreserveThinkingTests(unittest.TestCase):
    def test_always_on_for_chat_thinking_off_and_on(self):
        for kwargs in ({}, {'enable_thinking': False}, {'enable_thinking': True}):
            got = oxe.request_defaults({'chat_template_kwargs': dict(kwargs)}, '/v1/chat/completions', {})
            self.assertIs(got['chat_template_kwargs']['preserve_thinking'], True, kwargs)
        explicit = {'chat_template_kwargs': {'preserve_thinking': False}}
        self.assertIs(oxe.request_defaults(explicit, '/v1/chat/completions')['chat_template_kwargs']['preserve_thinking'], False)
        self.assertNotIn('chat_template_kwargs', oxe.request_defaults({}, '/completion', {}))

    def test_flash_profile_does_not_get_preserve_thinking(self):
        flash = {'oxe_flash': {'cache_gib': 20}, 'model_alias': 'flash'}
        for kwargs in ({}, {'enable_thinking': True}):
            got = oxe.request_defaults({'chat_template_kwargs': dict(kwargs)}, '/v1/chat/completions', flash)
            self.assertNotIn('preserve_thinking', got['chat_template_kwargs'], kwargs)
        moe = oxe.request_defaults({}, '/v1/chat/completions', {'model_alias': 'ornith'})
        self.assertIs(moe['chat_template_kwargs']['preserve_thinking'], True)
        explicit = {'chat_template_kwargs': {'preserve_thinking': True}}
        self.assertIs(oxe.request_defaults(explicit, '/v1/chat/completions', flash)['chat_template_kwargs']['preserve_thinking'], True)


class LedgerEchoTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        os.environ['OXC_LEDGER_DIR'] = self.tmp
        rl._history.clear()
        rl._salt[0] = None

    def tearDown(self):
        os.environ.pop('OXC_LEDGER_DIR', None)
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_parse_tail_takes_echo_counts_and_bounded_class_only(self):
        t = {'cache_n': 10, 'prompt_n': 5, 'echo_blocks': 3, 'echo_n': 2, 'echo_fixed': 1, 'echo_tokens': 40,
             'roundtrip_miss': 7, 'echo_miss_class': 'tool-mismatch', 'echo_misses': {'tool-mismatch': 1}}
        got = rl.parse_tail(json.dumps({'timings': t}).encode())
        for k in ('echo_blocks', 'echo_n', 'echo_fixed', 'echo_tokens', 'roundtrip_miss', 'echo_miss_class'):
            self.assertEqual(got[k], t[k])
        self.assertNotIn('echo_misses', got)
        t['echo_miss_class'] = '../x/secret'
        self.assertNotIn('echo_miss_class', rl.parse_tail(json.dumps({'timings': t}).encode()))

    def row(self, cache, new, predicted, n, **extra):
        base = {'cache_n': cache, 'prompt_n': new, 'predicted_n': predicted, 'n_messages': n, 'last_role': 'user',
                'agent': 'a', 'model': 'm', 'finish_reason': 'stop'}
        base.update(extra)
        return cm.summarize(base)

    def test_miss_class_from_echo_and_labels(self):
        prev = self.row(100, 50, 20, 3)
        # an edited assistant turn: the engine's echo report says why
        # (M2: the model's own reply is then not in the prompt, so only the previous prompt counts)
        self.assertEqual(cm.classify(prev, self.row(120, 80, 0, 5, echo_miss_class='content-mismatch')),
                         ('history-edit', 29, 'echo'))
        self.assertEqual(cm.classify(prev, self.row(120, 80, 0, 5, echo_miss_class='reasoning-mismatch'))[2], 'echo')
        # no-record says nothing about the cause: the count heuristics decide
        self.assertEqual(cm.classify(prev, self.row(120, 80, 0, 5, echo_miss_class='no-record'))[2], 'counts')
        # compaction and the turn after it: labelled by the client itself (H5)
        self.assertEqual(cm.classify(prev, self.row(30, 500, 0, 4, turn_kind='compaction')), ('history-rewrite', 139, 'label'))
        self.assertEqual(cm.classify(prev, self.row(30, 9, 0, 3, turn_kind='post-compaction'))[:3:2], ('history-rewrite', 'label'))
        # later turns still carry the checkpoint: the label must not explain (hide) their misses
        later = self.row(100, 50, 20, 3, turn_kind='post-compaction')
        got = cm.classify(later, self.row(30, 500, 0, 9, turn_kind='post-compaction', last_role='assistant'))
        self.assertEqual((got[0], got[2]), ('unknown', 'counts'))
        # a full hit stays append-ok whatever the labels
        self.assertEqual(cm.classify(prev, self.row(169, 9, 0, 5, turn_kind='post-compaction'))[0], 'append-ok')

    def test_entry_records_turn_labels_and_echo_class_end_to_end(self):
        msgs = [SYS, {'role': 'user', 'content': 'x'}]
        first = rl.Entry(CHAT, {'messages': msgs}, None, 'ornith', headers={'X-Session-Id': 's1'})
        first.label_turn(ct.label({'messages': msgs}, {'X-Oxe-Turn-Kind': 'chat'}))
        first.observe(json.dumps({'timings': {'cache_n': 100, 'prompt_n': 50, 'predicted_n': 20}}).encode())
        r1 = first.finish(200)
        self.assertEqual(r1['turn_kind'], 'chat')
        msgs2 = msgs + [{'role': 'assistant', 'content': 'y'}, {'role': 'user', 'content': INSTRUCTION}]
        second = rl.Entry(CHAT, {'messages': msgs2}, None, 'ornith', headers={'X-Session-Id': 's1'})
        second.label_turn(ct.label({'messages': msgs2}, {'X-Oxe-Turn-Kind': 'compaction'}))
        second.label_turn({'turn_kind': 'chat; rm -rf', 'tools_hash': 'NOTHEX', 'fold_suspect': 'yes'})  # junk is ignored
        second.observe(json.dumps({'timings': {'cache_n': 10, 'prompt_n': 200, 'predicted_n': 5, 'echo_blocks': 1,
                                               'echo_n': 0, 'echo_miss_class': 'no-record'}}).encode())
        r2 = second.finish(200)
        self.assertEqual(r2['turn_kind'], 'compaction')
        self.assertEqual((r2['miss_class'], r2['miss_basis']), ('history-rewrite', 'label'))
        self.assertNotIn('fold_suspect', r2)
        self.assertEqual(r2['echo_miss_class'], 'no-record')
        # every file the ledger wrote (the ledger and its binary salt; listdir order is arbitrary)
        blob = b''.join(p.read_bytes() for p in Path(self.tmp).iterdir() if p.is_file())
        self.assertIn(b'"turn_kind"', blob)
        self.assertNotIn(b'summarize', blob)


class ControllerHeaderTests(SecurityBase):
    """The oxe chat path sends the session hint (a client's own X-Oxe-Session is replaced) and the
    client's own H5 turn kind, validated; the message text never sets it."""

    def setUp(self):
        self.headers_seen = []
        super().setUp()
        seen = self.headers_seen
        backend = self.backend.RequestHandlerClass

        def do_POST(handler):
            body = json.loads(handler.rfile.read(int(handler.headers['Content-Length'])))
            seen.append((dict(handler.headers.items()), body))
            data = b'{"choices":[{"message":{"content":"READY"}}]}'
            handler.send_response(200); handler.send_header('Content-Type', 'application/json')
            handler.send_header('Content-Length', str(len(data))); handler.end_headers(); handler.wfile.write(data)
        backend.do_POST = do_POST

    def make_manager(self, **overrides):
        profile = {'model_alias': 'synthetic-oxe', 'backend_kind': oxe.KIND}
        return super().make_manager(config=Mock(return_value=profile), harness_mu=threading.Lock(),
                                    compat_stats=collections.Counter(),
                                    harness_registry=SimpleNamespace(observe=Mock(return_value=None)), **overrides)

    def test_session_hint_and_turn_kind_reach_the_engine(self):
        msgs = [SYS, {'role': 'user', 'content': 'hello'}, {'role': 'assistant', 'content': 'hi'},
                {'role': 'user', 'content': INSTRUCTION}]
        status, _ = self.send('POST', CHAT, body=json.dumps({'messages': msgs}).encode(),
                              headers={'X-Oxe-Session': 'forged', 'X-Oxe-Turn-Kind': 'chat', 'X-Session-Id': 'abc'})
        self.assertEqual(status, 200)
        headers, body = self.headers_seen[-1]
        low = {k.lower(): v for k, v in headers.items()}
        self.assertEqual(low['x-oxe-turn-kind'], 'chat', 'the client said chat; the summary wording does not matter')
        self.assertRegex(low['x-oxe-session'], r'^[0-9a-f]{16}$')
        self.assertNotEqual(low['x-oxe-session'], 'forged')
        self.assertIs(body['chat_template_kwargs']['preserve_thinking'], True)

    def test_bad_or_missing_turn_kind_is_not_forwarded(self):
        msgs = [SYS, {'role': 'user', 'content': INSTRUCTION}]
        for extra in ({'X-Oxe-Turn-Kind': 'two words'}, {'X-Oxe-Turn-Kind': 'x' * 33}, {}):
            status, _ = self.send('POST', CHAT, body=json.dumps({'messages': msgs}).encode(),
                                  headers={'X-Session-Id': 'abc', **extra})
            self.assertEqual(status, 200)
            low = {k.lower(): v for k, v in self.headers_seen[-1][0].items()}
            self.assertNotIn('x-oxe-turn-kind', low, extra)

    def test_snapshot_hint_and_prefill_only_pass_through(self):
        """v0.2.0 H4/H6: `X-Oxe-Snapshot: keep` reaches the engine as exactly `keep` (another value or
        none: no header); `max_tokens: 0` passes unchanged (no default output limit replaces it)."""
        msgs = [SYS, {'role': 'user', 'content': 'hello'}]
        for sent, want in (('keep', 'keep'), (' KEEP ', 'keep'), ('drop', None), (None, None)):
            extra = {} if sent is None else {'X-Oxe-Snapshot': sent}
            status, _ = self.send('POST', CHAT, body=json.dumps({'messages': msgs, 'max_tokens': 0}).encode(),
                                  headers=extra)
            self.assertEqual(status, 200)
            headers, body = self.headers_seen[-1]
            low = {k.lower(): v for k, v in headers.items()}
            self.assertEqual(low.get('x-oxe-snapshot'), want, sent)
            self.assertEqual(body['max_tokens'], 0)

    def test_client_turn_kind_passes_unchanged(self):
        """v0.2.0 H5: a valid client label reaches the engine unchanged; none or an invalid one: no
        label (the controller never guesses one from message text)."""
        msgs = [SYS, {'role': 'user', 'content': 'hello'}, {'role': 'assistant', 'content': 'hi'},
                {'role': 'user', 'content': INSTRUCTION}]
        for sent, want in (('branch_summary', 'branch_summary'), ('prefill', 'prefill'), ('subagent', 'subagent'),
                           (None, None), ('two words', None), ('x' * 33, None)):
            extra = {} if sent is None else {'X-Oxe-Turn-Kind': sent}
            status, _ = self.send('POST', CHAT, body=json.dumps({'messages': msgs}).encode(), headers=extra)
            self.assertEqual(status, 200)
            low = {k.lower(): v for k, v in self.headers_seen[-1][0].items()}
            self.assertEqual(low.get('x-oxe-turn-kind'), want, sent)

    def test_ledger_records_a_free_label(self):
        for label, want in (('branch_summary', 'branch_summary'), ('chat', 'chat'), ('bad label', None)):
            entry = SimpleNamespace(row={})
            rl.Entry.label_turn(entry, {'turn_kind': label})
            self.assertEqual(entry.row.get('turn_kind'), want, label)
        self.assertTrue(ct.valid_label('branch_summary'))
        self.assertFalse(ct.valid_label('a b'))
        self.assertEqual(ct.backend_headers(None, {'turn_kind': 'subagent'}), {'X-Oxe-Turn-Kind': 'subagent'})

    def test_snapshot_keep_parser(self):
        self.assertTrue(ct.snapshot_keep({'x-oxe-snapshot': 'keep'}))
        self.assertFalse(ct.snapshot_keep({'x-oxe-snapshot': 'keeps'}))
        self.assertFalse(ct.snapshot_keep({}))


class _Hook(BaseHTTPRequestHandler):
    """A stand-in for `oxe-server --engine stub`: renders with a toy ChatML (one byte one token) and
    drops reasoning before the last user message unless preserve_thinking (the Qwen behaviour)."""
    def log_message(self, *a): pass

    def do_POST(self):
        b = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        assert self.path == '/apply-template' and b.get('return_tokens') is True
        msgs = b['messages']
        keep = (b.get('chat_template_kwargs') or {}).get('preserve_thinking', False)
        last_user = max((i for i, m in enumerate(msgs) if m['role'] == 'user'), default=-1)
        out = ''
        for i, m in enumerate(msgs):
            body = m.get('content', '')
            if m['role'] == 'assistant' and m.get('reasoning_content') and (keep or i > last_user):
                body = '<think>' + m['reasoning_content'] + '</think>' + body
            out += '<|im_start|>' + m['role'] + '\n' + body + '<|im_end|>\n'
        data = json.dumps({'prompt': out, 'tokens': list(out.encode())}).encode()
        self.send_response(200); self.send_header('Content-Length', str(len(data))); self.end_headers()
        self.wfile.write(data)


class TemplateHookTests(unittest.TestCase):
    def setUp(self):
        self.srv = ThreadingHTTPServer(('127.0.0.1', 0), _Hook)
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()
        self.addCleanup(self.srv.server_close)
        self.addCleanup(self.srv.shutdown)
        self.url = 'http://127.0.0.1:%d' % self.srv.server_port
        self.hist = [SYS, {'role': 'user', 'content': 'a'},
                     {'role': 'assistant', 'content': 'b', 'reasoning_content': 'think'},
                     {'role': 'user', 'content': 'c'}, {'role': 'assistant', 'content': 'd'}]

    def test_c1_through_the_hook_breaks_without_and_holds_with_preserve_thinking(self):
        bad = cc.c1_report(cc.server_renderer(self.url, kwargs={'preserve_thinking': False}), self.hist)
        self.assertEqual(bad['c1_violations'], 1)
        self.assertEqual(bad['first_violation_at'], 4)
        good = cc.c1_report(cc.server_renderer(self.url, kwargs={'preserve_thinking': True}), self.hist)
        self.assertEqual(good['c1_violations'], 0)

    def test_cli_prints_counts_only(self):
        path = Path(tempfile.mkdtemp()) / 'h.json'
        self.addCleanup(shutil.rmtree, path.parent, True)
        path.write_text(json.dumps({'messages': self.hist}))
        import contextlib, io
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            rc = cc.main(['cache_contract.py', 'c1', self.url, str(path), '{"preserve_thinking": false}'])
        self.assertEqual(rc, 0)
        self.assertIn("'c1_violations': 1", buf.getvalue())
        self.assertNotIn('think', buf.getvalue())


if __name__ == '__main__':
    unittest.main()
