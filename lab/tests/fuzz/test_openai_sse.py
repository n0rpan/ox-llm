"""Task 61: SSE framing and stream-translation fuzzing for the controller (oxc).

The controller's two stream translators (`/v1/responses` and `/v1/completions`) parse an
untrusted engine/upstream SSE body. SSE lines end at CRLF, LF or a lone CR; an event ends at a
blank line. A malformed JSON chunk must not take the controller's request thread down, and an
unterminated giant line must not grow the buffer without bound.
"""
import json
import unittest
from pathlib import Path

from snapdragon_ai_lab.runtime import openai_api, openai_responses

try:  # the CI smoke installs hypothesis; the normal unittest suite skips these without it
    from hypothesis import HealthCheck, given, settings, strategies as st
except ImportError:  # pragma: no cover - exercised on a host without the optional dependency
    given = None

CORPUS = Path(__file__).with_name('corpus')
META = {'id': 'resp_test', 'created_at': 0, 'model': 'm', 'stream': True, 'custom': set(),
        'request': {'model': 'm'}}
COMPLETION_META = {'id': 'cmpl_test', 'created': 0, 'model': 'm', 'stream': True, 'echo': '',
                   'include_usage': False, 'logprobs': False, 'top_n': 0}


def corpus(name):
    return (CORPUS / name).read_bytes()


def events(body):
    """The `event:` names of an SSE response, in order."""
    return [line[6:].strip() for line in body.split(b'\n') if line.startswith(b'event:')]


class ResponseStreamFraming(unittest.TestCase):
    def test_crlf_stream_is_delivered_before_eof(self):
        stream = openai_responses.ResponseStream(META)
        out = stream.feed(corpus('response-crlf.sse'))
        # Every frame is complete once its blank line arrived: no waiting for the socket to close.
        self.assertIn(b'response.output_text.delta', out)
        self.assertIn(b'hello', out)
        self.assertIn(b'response.completed', out)
        self.assertTrue(stream.closed)
        self.assertEqual(stream.finish(), b'')

    def test_lone_cr_and_mixed_line_endings(self):
        for ending in (b'\r', b'\r\n', b'\n'):
            with self.subTest(ending=ending):
                body = corpus('response-split.sse').replace(b'\n', ending)
                out = openai_responses.ResponseStream(META).feed(body)
                self.assertIn(b'response.completed', out)
                self.assertIn(b'split!', out)

    def test_every_split_point_produces_the_same_events(self):
        body = corpus('response-split.sse')
        expected = events(openai_responses.ResponseStream(META).feed(body))
        self.assertTrue(expected)
        for cut in range(len(body) + 1):
            with self.subTest(cut=cut):
                stream = openai_responses.ResponseStream(META)
                out = stream.feed(body[:cut]) + stream.feed(body[cut:]) + stream.finish()
                self.assertEqual(events(out), expected)

    def test_comments_are_passed_through_and_do_not_complete_the_stream(self):
        stream = openai_responses.ResponseStream(META)
        out = stream.feed(corpus('comment-only.sse'))
        self.assertIn(b': queued', out)
        self.assertFalse(stream.closed)
        # A comment frame keeps exactly one blank line; no extra newline from the frame splitter.
        self.assertTrue(out.endswith(b'\n\n'))
        self.assertFalse(out.endswith(b'\n\n\n'))


class CompletionStreamFraming(unittest.TestCase):
    def test_crlf_native_completion_stream(self):
        stream = openai_api.CompletionStream(COMPLETION_META)
        out = stream.feed(corpus('completion-crlf.sse'))
        self.assertIn(b'"text": "hi"', out)
        self.assertIn(b'"text": " there"', out)
        self.assertIn(b'[DONE]', out)
        self.assertEqual(stream.finish(), b'')

    def test_a_comment_frame_keeps_one_blank_line(self):
        stream = openai_api.CompletionStream(COMPLETION_META)
        out = stream.feed(b': bye\n\n')
        self.assertEqual(out, b': bye\n\n')
        stream = openai_api.CompletionStream(COMPLETION_META)
        self.assertEqual(stream.feed(b': bye'), b'')
        self.assertEqual(stream.finish(), b': bye\n\n')


class MalformedChunks(unittest.TestCase):
    """A broken upstream chunk is ignored, not an exception on the controller thread."""

    def test_non_list_choices_are_ignored(self):
        stream = openai_responses.ResponseStream(META)
        out = stream.feed(b'data: {"choices":1}\n\n')
        self.assertIn(b'response.created', out)  # the stream started; the malformed choices are ignored
        out = stream.feed(b'data: {"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}],'
                          b'"usage":{"prompt_tokens":1,"completion_tokens":1}}\n\ndata: [DONE]\n\n')
        self.assertIn(b'response.completed', out)

    def test_unhashable_tool_call_index_is_treated_as_missing(self):
        stream = openai_responses.ResponseStream(META)
        chunk = {'choices': [{'delta': {'tool_calls': [
            {'index': [], 'id': 'call_1', 'function': {'name': 'read', 'arguments': '{"path":"x"}'}}]}}]}
        out = stream.feed(b'data: ' + json.dumps(chunk).encode() + b'\n\n')
        self.assertIn(b'response.function_call_arguments.delta', out)
        out += stream.feed(b'data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}],'
                           b'"usage":{"prompt_tokens":1,"completion_tokens":1}}\n\ndata: [DONE]\n\n')
        self.assertIn(b'response.completed', out)

    def test_unterminated_giant_frame_is_bounded(self):
        limit = getattr(openai_api, 'MAX_SSE_FRAME', 16 * 1024 ** 2)
        stream = openai_responses.ResponseStream(META)
        out = stream.feed(b'data: ' + b'x' * (limit + 1))
        self.assertLessEqual(len(stream.buffer), limit)
        self.assertTrue(stream.closed)
        self.assertIn(b'stream_too_large', out)
        self.assertEqual(stream.finish(), b'')


if given is not None:
    JSON = st.recursive(
        st.none() | st.booleans() | st.integers() | st.text(max_size=40),
        lambda children: st.lists(children, max_size=5) | st.dictionaries(st.text(max_size=12), children, max_size=5),
        max_leaves=30)

    class ChunkProperties(unittest.TestCase):
        @settings(max_examples=250, deadline=None, database=None,
                  suppress_health_check=[HealthCheck.too_slow])
        @given(JSON)
        def test_any_json_chunk_is_consumed_without_an_exception(self, value):
            stream = openai_responses.ResponseStream(META)
            try:
                stream.feed(b'data: ' + json.dumps(value, ensure_ascii=False).encode() + b'\n\n')
                stream.finish()
            except Exception as error:  # noqa: BLE001 - the property is "nothing escapes"
                self.fail(f'chunk {value!r} raised {type(error).__name__}: {error}')

        @settings(max_examples=250, deadline=None, database=None,
                  suppress_health_check=[HealthCheck.too_slow])
        @given(st.binary(max_size=512))
        def test_arbitrary_bytes_never_escape_the_translator(self, body):
            stream = openai_responses.ResponseStream(META)
            try:
                stream.feed(body)
                stream.finish()
            except Exception as error:  # noqa: BLE001
                self.fail(f'bytes {body!r} raised {type(error).__name__}: {error}')


if __name__ == '__main__':
    unittest.main()
