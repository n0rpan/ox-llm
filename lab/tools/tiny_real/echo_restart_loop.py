#!/usr/bin/env python3
"""T1.5 tiny-real: cache / ECHO closed loop across an engine restart (loopback oxe-server only).

Phase `pre` (server up): an agent-shaped conversation driven by the model's ACTUAL replies:
  1. system + tools + user -> the model must call a tool (valid JSON arguments);
  2. history + the assistant turn WITHOUT its reasoning_content (as the v0.1.x client sends it) +
     a tool result -> answer; the engine's ECHO must restore the dropped reasoning, so the
     request must hit C3: cached >= previous prompt + previous completion - 1;
  3. one more user turn, same C3 rule.
  The conversation and the numbers go to STATE (JSON).
Phase `post` (after the caller restarted the server on the same --state-cache): one more user
  turn on the saved history must resume from disk (resume_src == "disk") and hit C3.

Prints one line `TINY-REAL LOOP <phase>: PASS|FAIL <why>` and exits 0 / 1. No prompt or reply
text is printed; STATE holds the conversation (keep it in a temporary directory).

  echo_restart_loop.py --base-url http://127.0.0.1:PORT --model tiny --state FILE --phase pre|post
"""
import argparse
import json
import sys
import urllib.parse
import urllib.request

TOOLS = [{'type': 'function', 'function': {
    'name': 'read_file', 'description': 'Read a text file from the workspace',
    'parameters': {'type': 'object', 'properties': {'path': {'type': 'string'}}, 'required': ['path']}}}]
SYSTEM = 'You are a coding assistant working in a small repository. Use the tools when needed. Be brief.'
USER = ['Read the file notes.txt and tell me what it says.',
        'Thanks. Now suggest a better title for that file, in one line.',
        'Summarise our conversation so far in two sentences.']
TOOL_RESULT = ('notes.txt\n1: TODO: port the parser to Rust\n2: TODO: add a cache for compiled regexes\n'
               '3: release checklist lives in RELEASE.md\n')


def post(base, model, session, messages, max_tokens, timeout):
    body = {'model': model, 'messages': messages, 'tools': TOOLS, 'max_tokens': max_tokens, 'temperature': 0,
            'chat_template_kwargs': {'enable_thinking': True}}
    req = urllib.request.Request(base.rstrip('/') + '/v1/chat/completions', json.dumps(body).encode(),
                                 {'Content-Type': 'application/json', 'X-Session-Id': session,
                                  'X-Local-AI-Origin': 'lab'})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def numbers(r):
    t = r.get('timings') or {}
    u = r.get('usage') or {}
    return {'prompt': u.get('prompt_tokens'), 'completion': u.get('completion_tokens'),
            'cached': (u.get('prompt_tokens_details') or {}).get('cached_tokens'),
            'resume_src': t.get('resume_src'), 'finish': r['choices'][0].get('finish_reason'),
            'draft_n': t.get('draft_n'), 'draft_accepted': t.get('draft_n_accepted'),
            'decode_tps': t.get('predicted_per_second')}


def as_history(msg):
    """The assistant message as a client sends it back: content + tool_calls, NO reasoning."""
    h = {'role': 'assistant', 'content': msg.get('content') or ''}
    if msg.get('tool_calls'):
        h['tool_calls'] = msg['tool_calls']
    return h


def c3_ok(prev, cur):
    return cur['cached'] is not None and cur['cached'] >= prev['prompt'] + prev['completion'] - 1


def run_pre(a):
    msgs = [{'role': 'system', 'content': SYSTEM}, {'role': 'user', 'content': USER[0]}]
    rows, fails = [], []
    r = post(a.base_url, a.model, a.session, msgs, a.max_tokens, a.timeout)
    m = r['choices'][0]['message']
    n = numbers(r)
    calls = m.get('tool_calls') or []
    valid = []
    for c in calls:
        try:
            json.loads(c['function']['arguments'])
            valid.append(True)
        except (KeyError, TypeError, ValueError):
            valid.append(False)
    n['tool_calls'] = len(calls)
    n['tool_json_valid'] = all(valid) if valid else None
    n['reasoning_chars'] = len(m.get('reasoning_content') or '')
    rows.append(n)
    if not calls:
        fails.append('turn 1: no tool call')
    elif not all(valid):
        fails.append('turn 1: invalid tool-call JSON')
    msgs.append(as_history(m))
    for c in calls:
        msgs.append({'role': 'tool', 'tool_call_id': c.get('id', ''), 'content': TOOL_RESULT})
    for turn, user in ((2, None), (3, USER[1])):
        if user:
            msgs.append({'role': 'user', 'content': user})
        prev = rows[-1]
        r = post(a.base_url, a.model, a.session, msgs, a.max_tokens, a.timeout)
        m = r['choices'][0]['message']
        n = numbers(r)
        n['reasoning_chars'] = len(m.get('reasoning_content') or '')
        rows.append(n)
        if not c3_ok(prev, n):
            fails.append('turn {}: cached {} < C3 {} (ECHO / cache miss)'.format(turn, n['cached'], prev['prompt'] + prev['completion'] - 1))
        msgs.append(as_history(m))
    with open(a.state, 'w') as f:
        json.dump({'messages': msgs, 'rows': rows, 'session': a.session}, f)
    return rows, fails


def run_post(a):
    st = json.load(open(a.state))
    msgs = st['messages'] + [{'role': 'user', 'content': USER[2]}]
    prev = st['rows'][-1]
    r = post(a.base_url, a.model, st['session'], msgs, a.max_tokens, a.timeout)
    n = numbers(r)
    fails = []
    if n['resume_src'] != 'disk':
        fails.append('post-restart resume_src {!r}, expected disk'.format(n['resume_src']))
    if not c3_ok(prev, n):
        fails.append('post-restart cached {} < C3 {}'.format(n['cached'], prev['prompt'] + prev['completion'] - 1))
    return [n], fails


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument('--base-url', required=True)
    ap.add_argument('--model', default='tiny')
    ap.add_argument('--state', required=True)
    ap.add_argument('--phase', choices=('pre', 'post'), required=True)
    ap.add_argument('--session', default='tiny-real-loop')
    ap.add_argument('--max-tokens', type=int, default=320)
    ap.add_argument('--timeout', type=float, default=300)
    ap.add_argument('--receipt', help='append the numbers (JSON line) here')
    a = ap.parse_args(argv)
    host = urllib.parse.urlparse(a.base_url).hostname
    if host not in ('127.0.0.1', 'localhost', '::1'):
        print('TINY-REAL LOOP {}: FAIL loopback endpoints only'.format(a.phase))
        return 1
    try:
        rows, fails = (run_pre if a.phase == 'pre' else run_post)(a)
    except Exception as e:  # report one line, never a traceback with request bodies
        print('TINY-REAL LOOP {}: FAIL {}: {}'.format(a.phase, type(e).__name__, str(e)[:200]))
        return 1
    if a.receipt:
        with open(a.receipt, 'a') as f:
            f.write(json.dumps({'phase': a.phase, 'rows': rows, 'fails': fails}) + '\n')
    for i, r in enumerate(rows):
        print('  {} turn {}: {}'.format(a.phase, i + 1, ' '.join('{}={}'.format(k, v) for k, v in r.items())))
    print('TINY-REAL LOOP {}: {}'.format(a.phase, 'FAIL ' + '; '.join(fails) if fails else 'PASS'))
    return 1 if fails else 0


if __name__ == '__main__':
    sys.exit(main())
