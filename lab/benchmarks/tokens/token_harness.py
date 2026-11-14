#!/usr/bin/env python3
"""v0.2.0 opt L6: token-count harness (items #1, #2, #6, #7, #12 are counted with it). MIT.

Counts, with the engine's own tokenizer and chat template (`/apply-template` with `return_tokens`
on `oxe-server --engine stub`, no weights), for two kinds of input:

1. golden replay fixtures (`engine/crates/chat/tests/fixtures/*.jsonl`, one JSON object per line with
   `body`, `kwargs`, `expect.prompt`): the rendered prompt must equal the pinned golden string (exact
   check), and prompt bytes, tokens and tool-schema bytes/tokens are recorded;
2. transcripts (JSONL, one chat request per line, in order, one session per file): each line is
   `{"body": <chat request>, "reply": {"content": str, "tool_calls": [...]?}, "usage": {...}?}`.
   `tools/gates/hop_probe.py` writes this format from a tiny-real gate run (`transcript.jsonl`).

Per transcript turn i:
- prompt_tokens_i, prompt_bytes_i, tool_schema_bytes / tool_schema_tokens (the `tools` JSON, counted
  as its own string: an approximation of its share of the rendered prompt, labelled as such);
- reused_i = longest common token prefix of prompt_i with the previous turn's state, where the state is
  prompt_{i-1} + the tokens of its reply (the cache the engine would keep if it kept the reply exactly);
  appended_i = prompt_i - reused_i. A reply the client edits or re-renders shows up as a short reuse;
- output_tokens_i: `usage.completion_tokens` when the transcript has it, else the reply text and tool
  calls tokenised (marked `output_tokens_source`);
- tool outputs new in this turn: count, bytes and tokens of each new `role: tool` message.

Output: one JSON receipt (stdout or `--out`). Pure helpers (`common_prefix_len`, `turn_metrics`,
`summarize_turns`, `golden_check`) are unit-tested on synthetic data in lab/tests (ci-lab).

usage: token_harness.py --server-bin engine/target/release/oxe-server --model <gguf>
       [--golden engine/crates/chat/tests/fixtures/ornith.jsonl ...] [--transcript t.jsonl ...] [--out r.json]
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / 'req_path'))
import stub_bench as sb  # noqa: E402

SCHEMA = 'v020-token-harness/v1'
DEFAULT_MODEL = sb.DEFAULT_MODEL


# ---- pure helpers ----------------------------------------------------------------------------------

def common_prefix_len(a, b):
    n = min(len(a), len(b))
    i = 0
    while i < n and a[i] == b[i]:
        i += 1
    return i


def json_bytes(obj):
    return len(json.dumps(obj, ensure_ascii=False, separators=(',', ':'), sort_keys=False).encode('utf-8'))


def tool_messages(messages):
    return [m for m in messages if isinstance(m, dict) and m.get('role') == 'tool']


def new_tool_messages(prev_messages, messages):
    """Tool messages of `messages` that were not in the previous request. By content identity, not
    position: after a compaction the list shrinks and a positional slice would miss new tool results."""
    def key(m):
        return json.dumps(m, sort_keys=True, ensure_ascii=False)
    seen = {}
    for m in prev_messages or []:
        if isinstance(m, dict) and m.get('role') == 'tool':
            seen[key(m)] = seen.get(key(m), 0) + 1
    out = []
    for m in messages:
        if isinstance(m, dict) and m.get('role') == 'tool':
            k = key(m)
            if seen.get(k):
                seen[k] -= 1
            else:
                out.append(m)
    return out


def turn_metrics(prev_state, cur_tokens):
    """reused / appended for one turn. prev_state None means the first turn (all appended)."""
    reused = common_prefix_len(prev_state, cur_tokens) if prev_state else 0
    return {'prompt_tokens': len(cur_tokens), 'reused_tokens': reused, 'appended_tokens': len(cur_tokens) - reused,
            'reuse_estimate': True}


def median(values):
    vals = [float(v) for v in values if v is not None]
    if not vals:
        return None
    vals.sort()
    m = len(vals) // 2
    return vals[m] if len(vals) % 2 else (vals[m - 1] + vals[m]) / 2


REUSE_KIND = ('estimate: text-level reconstruction render(prompt_i-1) + tokenize(reasoning + content + '
              'tool_calls JSON); excludes the template tags around reasoning, the exact tool-call syntax '
              'and turn terminators. NOT measured engine reuse (use the engine cache_n for that)')


def summarize_turns(turns):
    keys = ('prompt_tokens', 'reused_tokens', 'appended_tokens', 'output_tokens', 'prompt_bytes',
            'tool_schema_bytes', 'tool_schema_tokens', 'new_tool_output_bytes', 'new_tool_output_tokens')
    tot = {k: sum(t.get(k) or 0 for t in turns) for k in keys}
    med = {k: median([t.get(k) for t in turns]) for k in keys}
    reuse = [t['reused_tokens'] / t['prompt_tokens'] for t in turns if t.get('prompt_tokens') and 'reused_tokens' in t and t.get('prev_state_present')]
    return {'turns': len(turns), 'totals': tot, 'medians': med,
            'median_reuse_fraction': median(reuse),
            'reuse_kind': REUSE_KIND}


def golden_check(expect_prompt, rendered_prompt):
    """Exact comparison against the pinned golden string; the first differing character on mismatch."""
    if expect_prompt == rendered_prompt:
        return {'match': True}
    i = common_prefix_len(expect_prompt, rendered_prompt)
    return {'match': False, 'first_diff_char': i, 'expect_len': len(expect_prompt), 'got_len': len(rendered_prompt)}


def reply_text(reply):
    """Text of the generated reply for the prior-state ESTIMATE: reasoning, content, tool-call JSON.
    Not the exact generated token state (template tags around reasoning, tool-call syntax and
    terminators differ); reuse figures built on it are labelled estimates."""
    if not reply:
        return ''
    text = (reply.get('reasoning_content') or '') + (reply.get('content') or '')
    if reply.get('tool_calls'):
        text += json.dumps(reply['tool_calls'], ensure_ascii=False, separators=(',', ':'))
    return text


# ---- server-backed counting ------------------------------------------------------------------------

class Counter:
    def __init__(self, port):
        self.port = port

    def render(self, body):
        req = {'messages': body.get('messages', []), 'return_tokens': True}
        if body.get('tools'):
            req['tools'] = body['tools']
        if body.get('kwargs') or body.get('chat_template_kwargs'):
            req['chat_template_kwargs'] = body.get('kwargs') or body.get('chat_template_kwargs')
        if body.get('add_generation_prompt') is not None:
            req['add_generation_prompt'] = body['add_generation_prompt']
        status, j, _, _ = sb.post_json(self.port, '/apply-template', req)
        if status != 200 or not isinstance(j, dict) or 'tokens' not in j:
            raise RuntimeError('/apply-template failed: HTTP %s' % status)
        return j['prompt'], j['tokens']

    def tokenize(self, text):
        if not text:
            return []
        status, j, _, _ = sb.post_json(self.port, '/tokenize', {'content': text, 'add_special': False})
        if status != 200:
            raise RuntimeError('/tokenize failed: HTTP %s' % status)
        return j['tokens']


def count_golden(counter, path):
    rows, mismatches = [], 0
    for line in Path(path).read_text().splitlines():
        if not line.strip():
            continue
        rec = json.loads(line)
        body = rec['body']
        kw = rec.get('kwargs') or {}
        rb = {'messages': body.get('messages', []), 'tools': body.get('tools'), 'kwargs': kw,
              'add_generation_prompt': body.get('add_generation_prompt')}
        prompt, tokens = counter.render(rb)
        expect = (rec.get('expect') or {}).get('prompt')
        chk = golden_check(expect, prompt) if expect is not None else {'match': None}
        if chk.get('match') is False:
            mismatches += 1
        rows.append({'name': rec.get('name'), 'prompt_bytes': len(prompt.encode()), 'prompt_tokens': len(tokens),
                     'tool_schema_bytes': json_bytes(body.get('tools') or []),
                     'tool_schema_tokens': len(counter.tokenize(json.dumps(body.get('tools') or [], ensure_ascii=False))),
                     'golden': chk})
    return {'file': os.path.basename(path), 'cases': len(rows), 'golden_mismatches': mismatches, 'rows': rows}


def count_transcript(counter, path):
    turns, prev_msgs, prev_state = [], None, None
    for line in Path(path).read_text().splitlines():
        if not line.strip():
            continue
        rec = json.loads(line)
        body, reply, usage = rec['body'], rec.get('reply') or {}, rec.get('usage') or {}
        msgs = body.get('messages', [])
        prompt, tokens = counter.render({'messages': msgs, 'tools': body.get('tools'),
                                         'kwargs': body.get('chat_template_kwargs') or {},
                                         'add_generation_prompt': body.get('add_generation_prompt')})
        m = turn_metrics(prev_state, tokens)
        m['prev_state_present'] = prev_state is not None
        m['prompt_bytes'] = len(prompt.encode())
        m['tool_schema_bytes'] = json_bytes(body.get('tools') or [])
        m['tool_schema_tokens'] = len(counter.tokenize(json.dumps(body.get('tools') or [], ensure_ascii=False)))
        new_tools = new_tool_messages(prev_msgs, msgs)
        m['new_tool_outputs'] = len(new_tools)
        m['new_tool_output_bytes'] = sum(len(str(t.get('content') or '').encode()) for t in new_tools)
        m['new_tool_output_tokens'] = sum(len(counter.tokenize(str(t.get('content') or ''))) for t in new_tools)
        rtext = reply_text(reply)
        rtoks = counter.tokenize(rtext) if rtext else []
        if 'completion_tokens' in usage:
            m['output_tokens'], m['output_tokens_source'] = int(usage['completion_tokens']), 'usage'
        else:
            m['output_tokens'], m['output_tokens_source'] = len(rtoks), 'tokenized'
        turns.append(m)
        prev_msgs = msgs
        prev_state = tokens + rtoks
    return {'file': os.path.basename(path), 'summary': summarize_turns(turns), 'turns': turns}


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    ap.add_argument('--server-bin', default=str(Path(__file__).resolve().parents[3] / 'engine/target/release/oxe-server'))
    ap.add_argument('--model', default=os.environ.get('TINY_GGUF', DEFAULT_MODEL))
    ap.add_argument('--golden', action='append', default=[])
    ap.add_argument('--transcript', action='append', default=[])
    ap.add_argument('--port', type=int, default=18796)
    ap.add_argument('--out', default='')
    a = ap.parse_args(argv)
    if a.port in sb.REFUSED_PORTS:
        ap.error('port %d is production' % a.port)
    if not (a.golden or a.transcript):
        ap.error('give --golden and/or --transcript')

    work = tempfile.mkdtemp(prefix='token-harness-')
    env = dict(os.environ, HOME=os.path.join(work, 'home'), XDG_CONFIG_HOME=os.path.join(work, 'cfg'),
               XDG_STATE_HOME=os.path.join(work, 'state'), XDG_RUNTIME_DIR=os.path.join(work, 'run'),
               XDG_CACHE_HOME=os.path.join(work, 'cache'))
    for d in ('home', 'cfg', 'state', 'run', 'cache'):
        os.makedirs(os.path.join(work, d), mode=0o700, exist_ok=True)
    env.pop('LOCAL_LLM_CONFIG', None)
    env.pop('OXE_API_KEY_FILE', None)
    proc, log = None, None
    try:
        proc, log = sb.start_server(a.server_bin, a.model, a.port, 'on', 65536, env, os.path.join(work, 'server.log'))
        sb.wait_health(a.port, proc)
        counter = Counter(a.port)
        result = {'schema': SCHEMA, 'engine': 'stub', 'model': os.path.basename(a.model),
                  'golden': [count_golden(counter, g) for g in a.golden],
                  'transcripts': [count_transcript(counter, t) for t in a.transcript]}
        text = json.dumps(result, indent=2, sort_keys=True)
        if a.out:
            Path(a.out).write_text(text + '\n')
        print(text)
        return 0
    finally:
        if proc is not None:
            sb.stop_server(proc, log)
        shutil.rmtree(work, ignore_errors=True)


if __name__ == '__main__':
    sys.exit(main())
