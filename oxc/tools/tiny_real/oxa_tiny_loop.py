#!/usr/bin/env python3
"""T1.5 tiny-real step 5: one real oxa loop against oxe + the tiny model (OXA_TINY_LOOP driver).

  oxa_tiny_loop.py BASE_URL MODEL WORKDIR        (the gate's OXA_TINY_LOOP contract; exit 0 = PASS)
  env OXA_BIN: the oxa binary (required); OXA_LOOP_SOFT_TOKENS (default 3000)

Drives `oxa --mode rpc` (pi's RPC records on stdin/stdout) in a scratch git repository under
WORKDIR, with a private HOME / agent dir / session dir there. Steps, each with its own verdict:

  tools        (fail-closed: isError must be false AND the content must match)
               read: a full read of calc.py and notes.txt returns exactly the file text;
               edit: succeeds and calc.py equals the original with exactly the one-line fix;
               bash: a `python3 calc.py` command (no echo/printf/cat/redirect) succeeds and prints
               the check value derived from a per-run random seed and the fixed add(); the
               driver reruns its own pinned copy of the fixed calc.py (never the live file; only
               after a valid bash record and only while the live file is byte-identical to it)
               and must get the same line
  compaction   a small soft limit (`oxa.compaction.softTokens`) must trigger >= 1 compaction
  subagent     every `subagent` call (>= 1) succeeds (isError false) with non-empty text
  cancel       `abort` after the first streamed delta settles the run as aborted, and the
               next prompt still works
  cache        every model request after the first of its prefix epoch hits C3:
               cached >= previous prompt + previous completion - 1 (pi usage: prompt =
               input + cacheRead, cached = cacheRead); no `cache_miss` event
  epochs       no prefix rebase: prefix_epoch events <= 1 + compactions
  post_compaction_prefix  the first request after a compaction reuses the static prefix
               (cached > 0; needs the server's --prefix-cache, as production runs it)

Prints one line per step and `OXA LOOP: PASS` or `OXA LOOP: FAIL <steps>`; the RPC record log
(no prompt text beyond the scripted one) stays in WORKDIR/oxa-rpc.jsonl for the caller.
Loopback endpoints only. Standard library only.
"""
import hashlib
import json
import os
from pathlib import Path
import queue
import re
import secrets
import subprocess
import tempfile
import sys
import threading
import time
import urllib.parse

TIMEOUT = float(os.environ.get('OXA_LOOP_TIMEOUT', '300'))
# calc.py prints a check value derived from a per-run random seed (in .seed) and add(2, 3):
# the expected line cannot be produced without running the FIXED code (or hashing by hand).
CALC = ('import hashlib\nimport pathlib\n\n\ndef add(a, b):\n    return a - b\n\n\nif __name__ == "__main__":\n'
        '    seed = pathlib.Path(__file__).with_name(".seed").read_text().strip()\n'
        '    r = add(2, 3)\n'
        '    print("add(2, 3) =", r, "check", hashlib.sha256((seed + ":" + str(r)).encode()).hexdigest()[:12])\n')
RUN_CALC = re.compile(r'(^|[;&|(]\s*|\s)(python3?|\S*/python3?)\s+(\./)?calc\.py(\s|$)')
FORGE = re.compile(r'\b(echo|printf|cat|tee|print\()|<<|>\s*\S')


def bash_expect(seed):
    """The line the fixed calc.py prints for this seed."""
    return 'add(2, 3) = 5 check ' + hashlib.sha256((seed + ':5').encode()).hexdigest()[:12]
NOTES = ('Project notes\n- the calculator lives in calc.py\n- add() has a bug: it subtracts\n'
         '- release checklist: run calc.py, then tag\n') + ''.join('- filler line {} for context growth\n'.format(i) for i in range(40))
AGENT = '---\nname: helper\ndescription: Reads files and answers short questions about them\ntools: read\n---\nYou are a careful helper. Answer in one or two sentences.\n'
PROMPTS = {
    'read': 'Use the read tool to read notes.txt and calc.py, then say in one sentence what the bug is.',
    'edit': 'Fix the bug in calc.py with the edit tool so that add returns a + b. Change nothing else.',
    'bash': 'Run `python3 calc.py` with the bash tool and tell me the exact output.',
    'subagent': 'Use the subagent tool with agent "helper" and the task "Read notes.txt and list the release checklist steps." Then repeat its answer.',
    'cancel': 'Write a long, detailed essay (at least 1500 words) about ownership and borrowing in Rust.',
    'after': 'Reply with the single word: done.',
}


KEEP_ENV = ('OXA_REAL', 'HOP_RPC_OUT')
STEPS = ('tools.read', 'tools.edit', 'tools.bash', 'compaction', 'post_compaction_prefix', 'subagent', 'cancel',
         'after_cancel', 'cache', 'epochs')


BULKY_KEYS = ('messages', 'assistantMessageEvent')


def log_line(r):
    """One COMPLETE JSON line per RPC record (never cut: a cut line is invalid evidence). The bulky
    transcript fields are replaced by their sha256 + byte length; the full records stay in memory
    (Rpc.records) and in oxa's own session file."""
    slim = {k: v for k, v in r.items() if k not in BULKY_KEYS}
    omitted = {}
    for k in BULKY_KEYS:
        if k in r:
            raw = json.dumps(r[k], sort_keys=True).encode()
            omitted[k] = {'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}
    if omitted:
        slim['_omitted'] = omitted
    return json.dumps(slim)


class Rpc:
    def __init__(self, argv, env, cwd, log):
        self.p = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(log + '.stderr', 'wb'),
                                  env=env, cwd=cwd, start_new_session=True)
        self.q = queue.Queue()
        self.log = open(log, 'w')
        self.records = []
        self.n = 0
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.p.stdout:
            try:
                r = json.loads(line)
            except ValueError:
                r = {'type': '_unparsed', 'text': line[:200].decode('utf-8', 'replace')}
            self.q.put(r)
        self.q.put({'type': '_eof'})

    def send(self, cmd):
        self.n += 1
        cmd = dict(cmd, id='c{}'.format(self.n))
        self.p.stdin.write((json.dumps(cmd) + '\n').encode())
        self.p.stdin.flush()
        return cmd['id']

    def next(self, timeout):
        r = self.q.get(timeout=timeout)
        self.records.append(r)
        self.log.write(log_line(r) + '\n')
        self.log.flush()
        return r

    def get_state(self, timeout=60):
        """One RPC get_state; returns its data dict (records seen meanwhile are kept in
        self.records). Raises on failure, timeout or oxa exit."""
        rid = self.send({'type': 'get_state'})
        deadline = time.time() + timeout
        while True:
            left = deadline - time.time()
            if left <= 0:
                raise TimeoutError('no get_state reply in {} s'.format(timeout))
            r = self.next(left)
            if r.get('type') == '_eof':
                raise RuntimeError('oxa exited (see oxa-rpc.jsonl.stderr)')
            if r.get('type') == 'response' and r.get('id') == rid:
                if not r.get('success'):
                    raise RuntimeError('get_state failed: {}'.format(r.get('error')))
                return r.get('data') or {}

    def wait_idle(self, timeout=None):
        """Poll get_state until the session neither streams nor compacts (oxa compacts after a
        reply, W5); returns the event records seen meanwhile."""
        seen, deadline = [], time.time() + (timeout or TIMEOUT)
        while True:
            rid = self.send({'type': 'get_state'})
            state = None
            while state is None:
                left = deadline - time.time()
                if left <= 0:
                    raise TimeoutError('session not idle in {} s'.format(timeout or TIMEOUT))
                r = self.next(left)
                if r.get('type') == '_eof':
                    raise RuntimeError('oxa exited (see oxa-rpc.jsonl.stderr)')
                if r.get('type') == 'response' and r.get('id') == rid:
                    if not r.get('success'):
                        raise RuntimeError('get_state failed: {}'.format(r.get('error')))
                    state = r.get('data') or {}
                else:
                    seen.append(r)
            if not state.get('isStreaming') and not state.get('isCompacting'):
                return seen
            time.sleep(0.5)

    def run_prompt(self, text, on_record=None):
        """Send a prompt; collect records until the run settles. Returns (records, response)."""
        rid = self.send({'type': 'prompt', 'message': text})
        out, resp, deadline = [], None, time.time() + TIMEOUT
        while True:
            left = deadline - time.time()
            if left <= 0:
                raise TimeoutError('prompt did not settle in {} s'.format(TIMEOUT))
            r = self.next(left)
            out.append(r)
            if r.get('type') == '_eof':
                raise RuntimeError('oxa exited (see oxa-rpc.jsonl.stderr)')
            if r.get('type') == 'response' and r.get('id') == rid:
                resp = r
                if not r.get('success'):
                    return out, resp
            if on_record:
                on_record(r)
            if r.get('type') in ('agent_settled',) or (r.get('type') == 'agent_end' and not r.get('willRetry')):
                # drain until agent_settled when it exists (pi emits agent_end, oxa adds agent_settled)
                if r.get('type') == 'agent_settled':
                    return out, resp
                try:
                    while True:
                        r2 = self.next(5)
                        out.append(r2)
                        if r2.get('type') == 'agent_settled':
                            return out, resp
                except queue.Empty:
                    return out, resp

    def close(self):
        try:
            self.p.stdin.close()
            self.p.wait(timeout=20)
        except Exception:
            os.killpg(self.p.pid, 15)
            try:
                self.p.wait(timeout=10)
            except Exception:
                os.killpg(self.p.pid, 9)


def tool_calls(recs, name):
    return [r for r in recs if r.get('type') == 'tool_execution_start' and r.get('toolName') == name]


def tool_ends(recs, name):
    return [r for r in recs if r.get('type') == 'tool_execution_end' and r.get('toolName') == name]


def result_text(end):
    """The text of a tool_execution_end record (pi: result.content[*].text)."""
    res = end.get('result') or {}
    return ''.join(c.get('text', '') for c in res.get('content') or [] if isinstance(c, dict))


def ok_ends(recs, name):
    """Successful executions only: isError must be present and false (fail-closed)."""
    return [e for e in tool_ends(recs, name) if e.get('isError') is False]


def _args_by_call(recs):
    return {r.get('toolCallId'): (r.get('args') or {}) for r in recs if r.get('type') == 'tool_execution_start'}


def check_read(recs):
    """At least one successful full read of each file whose text is exactly that file's content."""
    args = _args_by_call(recs)
    want = {'calc.py': CALC, 'notes.txt': NOTES}
    got = {}
    for e in ok_ends(recs, 'read'):
        a = args.get(e.get('toolCallId'), {})
        path = str(a.get('path') or a.get('file_path') or '')
        name = path.rsplit('/', 1)[-1]
        if name in want and a.get('offset') in (None, 0, 1) and a.get('limit') is None:
            got[name] = got.get(name) or result_text(e).rstrip('\n') == want[name].rstrip('\n')
    missing = [n for n in want if not got.get(n)]
    return not missing, 'exact full reads missing/wrong: {}'.format(missing) if missing else 'calc.py + notes.txt read exactly'


def check_edit(recs, calc_now):
    """A successful edit and calc.py now equal to the original with exactly the one fix."""
    want = CALC.replace('return a - b', 'return a + b')
    ok = bool(ok_ends(recs, 'edit'))
    if not ok:
        return False, 'no successful edit'
    if calc_now != want:
        return False, 'calc.py differs from the exact one-line fix'
    return True, 'one-line fix exact'


def check_bash(recs, expect, rerun=None):
    """A successful bash command that runs `python3 calc.py` (no echo / printf / cat / heredoc /
    redirect that could forge output) whose output contains the seed-derived line `expect`;
    `rerun` (the driver's own run of calc.py afterwards) must print the same line."""
    args = _args_by_call(recs)
    for e in ok_ends(recs, 'bash'):
        cmd = str(args.get(e.get('toolCallId'), {}).get('command', ''))
        if RUN_CALC.search(cmd) and not FORGE.search(re.sub(r'\d?>&\d', '', cmd)) and expect in result_text(e):
            if rerun is not None and expect not in rerun:
                return False, 'bash output matched but an independent run of calc.py does not print it'
            return True, 'python3 calc.py printed the seed check'
    return False, 'no successful `python3 calc.py` run printing the seed check'


FIXED = CALC.replace('return a - b', 'return a + b')


def independent_rerun(repo, seed):
    """Run the driver's OWN pinned copy of the fixed calc.py (never the live, model-writable
    file) in a fresh temporary directory, and only when the live file is still byte-identical
    to it. Returns (stdout or None, note)."""
    live = Path(repo, 'calc.py').read_bytes()
    if hashlib.sha256(live).digest() != hashlib.sha256(FIXED.encode()).digest():
        return None, 'calc.py changed after the edit check; not executed'
    with tempfile.TemporaryDirectory(prefix='oxa-loop-rerun-') as d:
        Path(d, 'calc.py').write_text(FIXED)
        Path(d, '.seed').write_text(seed + '\n')
        env = {'PATH': '/usr/bin:/bin', 'HOME': d, 'LANG': 'C.UTF-8'}
        out = subprocess.run([sys.executable, '-I', 'calc.py'], cwd=d, env=env, capture_output=True, text=True,
                             timeout=30, stdin=subprocess.DEVNULL).stdout
    return out, 'pinned copy rerun'


def bash_verdict(recs, repo, seed):
    """check_bash on the records first; the independent rerun happens only after a valid,
    successful bash record, and only on the pinned copy (see independent_rerun)."""
    expect = bash_expect(seed)
    ok, why = check_bash(recs, expect)
    if not ok:
        return False, why
    rerun, note = independent_rerun(repo, seed)
    if rerun is None:
        return False, note
    return check_bash(recs, expect, rerun)


def check_subagent(recs):
    ends = tool_ends(recs, 'subagent')
    good = [e for e in ends if e.get('isError') is False and result_text(e).strip()]
    if not ends:
        return False, 'the model did not call the subagent tool'
    return (bool(good) and len(good) == len(ends)), '{} of {} subagent calls succeeded with text'.format(len(good), len(ends))


def requests(recs):
    """Model requests in order: assistant message_end records with usage."""
    out = []
    for r in recs:
        if r.get('type') == 'message_end':
            m = r.get('message') or {}
            u = m.get('usage') or {}
            if m.get('role') == 'assistant' and u:
                inp, cr, outp = u.get('input', 0), u.get('cacheRead', 0), u.get('output', 0)
                out.append({'prompt': inp + cr, 'cached': cr, 'completion': outp, 'stop': m.get('stopReason')})
        elif r.get('type') in ('prefix_epoch', 'compaction_end'):
            out.append({'mark': r['type']})
    return out


def client_env(base, home, agent, sess, engine):
    """The oxa child's environment. O170: BRAVE_* never inherited. OXA_* stripped (isolation) EXCEPT
    the hop probe's tee contract (tools/gates/oxa_rpc_tee.py: OXA_BIN may be the tee, which needs
    OXA_REAL + HOP_RPC_OUT). oxa itself runs every bash tool call in its bwrap sandbox."""
    env = {k: v for k, v in base.items() if k in KEEP_ENV or not k.startswith(('OXA_', 'OXC', 'XDG_', 'BRAVE_'))}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / '.config'), XDG_STATE_HOME=str(home / '.local/state'),
               XDG_CACHE_HOME=str(home / '.cache'), XDG_RUNTIME_DIR=str(home / 'run'), OXA_AGENT_DIR=str(agent),
               OXA_SESSION_DIR=str(sess), OXA_ENGINE_URL=engine, OXA_OFFLINE='1', OXA_SKIP_VERSION_CHECK='1',
               OXA_TELEMETRY='0')
    return env


def main(argv):
    if len(argv) != 4:
        print(__doc__)
        return 2
    base, model, work = argv[1], argv[2], Path(argv[3])
    host = urllib.parse.urlparse(base).hostname
    if host not in ('127.0.0.1', 'localhost', '::1'):
        print('OXA LOOP: FAIL loopback endpoints only')
        return 1
    oxa = os.environ.get('OXA_BIN', '')
    if not os.access(oxa, os.X_OK):
        print('OXA LOOP: FAIL OXA_BIN not set or not executable')
        return 1
    engine = base[:-3] if base.rstrip('/').endswith('/v1') else base
    engine = engine.rstrip('/')
    home, agent, repo, sess = work / 'home', work / 'home' / '.oxa' / 'agent', work / 'repo', work / 'sessions'
    for d in (agent / 'agents', repo, sess, home / 'run'):
        d.mkdir(parents=True, exist_ok=True)
    soft = int(os.environ.get('OXA_LOOP_SOFT_TOKENS', '3000'))
    (agent / 'settings.json').write_text(json.dumps({
        'compaction': {'enabled': True, 'keepRecentTokens': 800},
        'oxa': {'compaction': {'softTokens': soft}, 'subagent': {'enabled': True, 'smallContext': True, 'softTokens': soft}}}, indent=1))
    (agent / 'agents' / 'helper.md').write_text(AGENT)
    (repo / 'calc.py').write_text(CALC)
    seed = secrets.token_hex(8)
    (repo / '.seed').write_text(seed + '\n')
    (repo / 'notes.txt').write_text(NOTES)
    subprocess.run(['git', 'init', '-q'], cwd=repo, check=True)
    subprocess.run(['git', '-c', 'user.name=t', '-c', 'user.email=t@t', 'add', '.'], cwd=repo, check=True)
    subprocess.run(['git', '-c', 'user.name=t', '-c', 'user.email=t@t', 'commit', '-qm', 'scratch'], cwd=repo, check=True)
    env = client_env(os.environ, home, agent, sess, engine)
    os.chmod(home / 'run', 0o700)
    argv_oxa = [oxa, '--mode', 'rpc', '--model', model, '--exclude-tools', 'web_search', '--no-context-files', '--no-skills', '--no-prompt-templates',
                '--no-themes', '--no-mcp', '--permission-mode', 'workspace', '--approve']
    rpc = Rpc(argv_oxa, env, str(repo), str(work / 'oxa-rpc.jsonl'))
    verdict, notes = {}, {}
    all_recs = []
    try:
        def step(name, prompt, on_record=None):
            all_recs.extend(rpc.wait_idle())
            recs, resp = rpc.run_prompt(prompt, on_record)
            all_recs.extend(recs)
            if resp is not None and not resp.get('success'):
                raise RuntimeError('{}: prompt refused: {}'.format(name, resp.get('error')))
            return recs

        r = step('read', PROMPTS['read'])
        verdict['tools.read'], notes['read'] = check_read(r)
        r = step('edit', PROMPTS['edit'])
        verdict['tools.edit'], notes['edit'] = check_edit(r, (repo / 'calc.py').read_text())
        r = step('bash', PROMPTS['bash'])
        verdict['tools.bash'], notes['bash'] = bash_verdict(r, repo, seed)
        r = step('subagent', PROMPTS['subagent'])
        verdict['subagent'], notes['subagent'] = check_subagent(r)
        # cancel mid-stream
        seen = {'delta': False, 'aborted_sent': False}

        def on_rec(rec):
            if rec.get('type') == 'message_update' and not seen['aborted_sent']:
                seen['delta'] = True
                seen['aborted_sent'] = True
                rpc.send({'type': 'abort'})
        t0 = time.time()
        r = step('cancel', PROMPTS['cancel'], on_rec)
        settled = [x for x in r if x.get('type') == 'agent_settled']
        verdict['cancel'] = seen['aborted_sent'] and bool(settled) and bool(settled[-1].get('aborted'))
        notes['cancel_settle_s'] = round(time.time() - t0, 1)
        r = step('after', PROMPTS['after'])
        verdict['after_cancel'] = any(x.get('type') == 'message_end' and (x.get('message') or {}).get('role') == 'assistant'
                                      and (x.get('message') or {}).get('stopReason') not in ('error', 'aborted') for x in r)
        all_recs.extend(rpc.wait_idle())  # a compaction after the last reply finishes first
        comps = [x for x in all_recs if x.get('type') == 'compaction_end']
        if not comps:  # the soft limit did not fire within the scripted turns: one more growth turn
            r = step('grow', 'Read notes.txt again and quote its last five lines.')
            comps = [x for x in all_recs if x.get('type') == 'compaction_end']
        verdict['compaction'] = bool(comps) and not any(x.get('errorMessage') or x.get('aborted') for x in comps)
        notes['compactions'] = len(comps)
        # cache + epochs
        reqs = requests(all_recs)
        misses = [x for x in all_recs if x.get('type') == 'cache_miss']
        epochs = [x for x in all_recs if x.get('type') == 'prefix_epoch']
        bad, prev, checked, exact, post = [], None, 0, 0, []
        after_comp = False
        for q in reqs:
            if 'mark' in q:
                prev = None  # a new epoch / compaction restarts the chain
                after_comp = after_comp or q['mark'] == 'compaction_end'
                continue
            if after_comp:
                post.append((q['prompt'], q['cached']))
                after_comp = False
            if prev is not None and prev['stop'] != 'aborted':
                need = prev['prompt'] + prev['completion'] - 1
                checked += 1
                exact += q['cached'] == need
                if q['cached'] < need:
                    bad.append('cached {} < {}'.format(q['cached'], need))
            prev = q
        verdict['cache'] = checked > 0 and not bad and not misses
        notes['cache'] = '{} appends checked, {} exactly C3, misses {}, events cache_miss {}'.format(
            checked, exact, bad[:3], [(m.get('expected'), m.get('cached'), m.get('cause')) for m in misses][:3])
        verdict['epochs'] = len(epochs) <= 1 + len(comps)
        # the first request after a compaction must reuse at least the static prefix
        verdict['post_compaction_prefix'] = bool(post) and all(c > 0 for _, c in post)
        notes['post_compaction'] = ['prompt {} cached {}'.format(p, c) for p, c in post]
        notes['epochs'] = [(e.get('reason'), e.get('reprefillTokens')) for e in epochs]
    except Exception as e:
        notes['error'] = '{}: {}'.format(type(e).__name__, str(e)[:300])
    finally:
        rpc.close()
    for k in STEPS:
        print('  oxa {}: {}'.format(k, 'PASS' if verdict.get(k) else 'FAIL' if k in verdict else 'NOT RUN'))
    for k, v in notes.items():
        print('  note {}: {}'.format(k, v))
    failed = [k for k in STEPS
              if not verdict.get(k)]
    print('OXA LOOP: ' + ('FAIL ' + ', '.join(failed) if failed else 'PASS'))
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main(sys.argv))
