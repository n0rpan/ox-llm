"""Interim adapters (counters, thermal/A-A, soak slope, same-PID, keep-warm policy fixture): every
positive and negative control behaves. Model-free; no sysfs, no processes."""
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tools/interim'))
import rehearse_interim as ri  # noqa: E402
import tool_counters as tc  # noqa: E402


class InterimRehearsalTests(unittest.TestCase):
    def test_every_control_behaves(self):
        r = ri.rehearse(with_sysfs=False)
        bad = [c['control'] for c in r['controls'] if not c['behaved']]
        self.assertEqual(bad, [])
        names = {c['control'] for c in r['controls']}
        for need in ('counters.valid', 'counters.repaired', 'counters.text_fallback', 'counters.malformed_reasons',
                     'thermal.start_pass', 'thermal.missing_soc_incomplete', 'aa.pass', 'aa.median_out_fails',
                     'soak.flat_pass', 'soak.noisy_pass', 'soak.growing_1GiB_h_fails', 'soak.missing_pss_incomplete',
                     'soak.restart_incomplete', 'keepwarm.same_pid_pass', 'keepwarm.pid_change_fails',
                     'policy.reference_pass', 'policy.missing_policy_blocked'):
            self.assertIn(need, names)

    def test_text_fallback_patterns(self):
        for text in ('<tool_call>{"name": "read"}</tool_call>', 'x <function=bash> y',
                     '{"name": "read", "arguments": {"path": "a"}}'):
            self.assertTrue(tc.classify_response(ri.TOOLS, {'content': text})[1], text)
        for text in ('The function reads a file.', 'name: read', ''):
            self.assertFalse(tc.classify_response(ri.TOOLS, {'content': text})[1], text)

    def test_structured_call_with_text_is_not_a_fallback(self):
        v, fb = tc.classify_response(ri.TOOLS, {'content': '<tool_call>', 'tool_calls': [ri.call('a', 'read', {'path': 'x'})]})
        self.assertFalse(fb)
        self.assertEqual(v, [('a', None)])



class AbRecorderCounterTests(unittest.TestCase):
    """The recording proxy classifies wire tool calls from a real-shaped SSE stream."""
    def setUp(self):
        import recording_proxy
        self.ab = recording_proxy

    def sse(self, *chunks):
        import json
        return b''.join(b'data: ' + json.dumps(c).encode() + b'\n\n' for c in chunks) + b'data: [DONE]\n\n'

    def test_streamed_valid_and_malformed_calls(self):
        buf = self.sse({'choices': [{'delta': {'tool_calls': [{'index': 0, 'id': 'a', 'function': {'name': 'read', 'arguments': '{"pa'}}]}}]},
                       {'choices': [{'delta': {'tool_calls': [{'index': 0, 'function': {'arguments': 'th": "x"}'}}]}}]},
                       {'choices': [{'delta': {'tool_calls': [{'index': 1, 'id': 'b', 'function': {'name': 'bash', 'arguments': '{"cmd": 1}'}}]},
                                     'finish_reason': 'tool_calls'}], 'usage': {'prompt_tokens': 10}})
        out = self.ab.parse_usage(buf, True, ri.TOOLS)
        self.assertEqual(out['wire_calls'], [['a', None], ['b', 'missing_required']])
        self.assertFalse(out['text_fallback'])

    def test_text_fallback_from_stream(self):
        buf = self.sse({'choices': [{'delta': {'content': '<tool_call>\n<function=read>'}, 'finish_reason': 'stop'}]})
        out = self.ab.parse_usage(buf, True, ri.TOOLS)
        self.assertTrue(out['text_fallback'])

    def test_listener_ownership(self):
        import socket
        s = socket.socket()
        s.bind(('127.0.0.1', 0))
        s.listen(1)
        port = s.getsockname()[1]
        try:
            import os
            self.assertTrue(self.ab.port_in_use(port))
            self.assertTrue(self.ab.pid_listens(os.getpid(), port))
            self.assertFalse(self.ab.pid_listens(1, port) if os.path.exists('/proc/1') else False)
        finally:
            s.close()
        self.assertFalse(self.ab.port_in_use(port))


if __name__ == '__main__':
    unittest.main()
