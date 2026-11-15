#!/usr/bin/env python3
"""Interim A/B tool-call counters, independent of every client (V020-INTERIM-OVERNIGHT-PLAN). MIT.

Counts, per arm, from the WIRE (what the model emitted, before any client repair) plus each
client's own record of what it executed:

  attempts            structured tool calls in model responses + tool-call syntax dumped as text
  valid               structured call: known tool name, arguments parse as a JSON object and
                      satisfy the declared schema (required keys, primitive types, enums)
  malformed           structured call failing that (reason: unknown_tool / args_not_json /
                      args_not_object / missing_required / wrong_type / bad_enum) + text_fallback
  text_fallback       a response without structured tool_calls whose content carries tool-call
                      syntax (<tool_call>, <function=..., a {"name":..,"arguments":..} object)
  repaired            a malformed wire call that the client nevertheless executed successfully
                      (matched by call id): the client repaired it
  repair_turns        model requests that directly follow a malformed structured call whose
                      client-side result was an error (linked by call id); null without client
                      outcomes. Text fallbacks have no call id to link and are counted on their own
  (any malformed / valid call id without a measured client outcome makes repaired /
   legit_tool_failures null: unmeasured, never 0; an empty client stream is no evidence)
  legit_tool_failures valid calls whose execution failed (not malformed)
  per_100             malformed / repaired / text_fallback per 100 attempts

A source that is absent gives null for what depends on it ("unmeasured"), never 0: no client
events -> repaired / legit_tool_failures = null; no wire capture -> everything null.

Inputs (all JSON lines):
  wire     one row per model request: {"request_tools": [...], "response": {"content": str,
           "tool_calls": [{"id","function":{"name","arguments"}}]}} (the recording proxy writes
           these; content may be dropped after classification: only verdicts are kept)
  oxa      oxa --mode rpc records (tool_execution_start / tool_execution_end: toolCallId, toolName,
           isError) -- the shapes of oxa 07c9f7f oxa-rpc/src/events.rs
  opencode `opencode run --format json` events (part.type == "tool", part.callID,
           part.state.status completed|error)
"""
import json
import re
import sys

TEXT_CALL = re.compile(r'<tool_call>|<function=[A-Za-z0-9_.-]+>|"name"\s*:\s*"[A-Za-z0-9_.-]+"\s*,\s*"(arguments|parameters)"\s*:')
JSON_TYPES = {'string': str, 'integer': int, 'number': (int, float), 'boolean': bool, 'object': dict, 'array': list}


def tool_schemas(request_tools):
    out = {}
    for t in request_tools or []:
        fn = (t or {}).get('function') or {}
        if fn.get('name'):
            out[fn['name']] = fn.get('parameters') or {}
    return out


def check_args(schema, args):
    """None when `args` satisfies the declared (subset of) JSON schema, else a reason."""
    if not isinstance(args, dict):
        return 'args_not_object'
    for key in schema.get('required') or []:
        if key not in args:
            return 'missing_required'
    for key, spec in (schema.get('properties') or {}).items():
        if key not in args or not isinstance(spec, dict):
            continue
        typ = spec.get('type')
        want = JSON_TYPES.get(typ) if isinstance(typ, str) else None
        val = args[key]
        if want is not None:
            if typ in ('integer', 'number') and isinstance(val, bool):
                return 'wrong_type'
            if not isinstance(val, want):
                return 'wrong_type'
        if 'enum' in spec and val not in spec['enum']:
            return 'bad_enum'
    return None


def classify_call(schemas, call):
    fn = (call or {}).get('function') or {}
    name, raw = fn.get('name'), fn.get('arguments')
    if name not in schemas:
        return 'unknown_tool'
    if isinstance(raw, dict):
        args = raw
    else:
        try:
            args = json.loads(raw if raw not in (None, '') else '{}')
        except (TypeError, ValueError):
            return 'args_not_json'
    return check_args(schemas[name], args)


def classify_response(request_tools, response):
    """[(call_id, None | reason)], text_fallback flag for one model response."""
    schemas = tool_schemas(request_tools)
    calls = (response or {}).get('tool_calls') or []
    verdicts = [(c.get('id'), classify_call(schemas, c)) for c in calls]
    content = (response or {}).get('content') or ''
    text_fallback = not calls and bool(TEXT_CALL.search(content if isinstance(content, str) else json.dumps(content)))
    return verdicts, text_fallback


def oxa_outcomes(records):
    """{call_id: True (ok) | False (error)} from oxa RPC tool_execution_end records; None when the
    stream holds no tool_execution_end at all (no client evidence, never 'zero repairs')."""
    if not any(r.get('type') == 'tool_execution_end' for r in records or []):
        return None
    out = {}
    for r in records:
        if r.get('type') == 'tool_execution_end' and r.get('toolCallId'):
            if not isinstance(r.get('isError'), bool):
                continue  # unmeasured, never assumed ok
            out[r['toolCallId']] = not r['isError']
    return out


def opencode_outcomes(events):
    if not any(((e or {}).get('part') or {}).get('type') == 'tool' for e in events or []):
        return None
    out = {}
    for e in events:
        part = (e or {}).get('part') or {}
        if part.get('type') != 'tool' or not part.get('callID'):
            continue
        status = (part.get('state') or {}).get('status')
        if status in ('completed', 'error'):
            out[part['callID']] = status == 'completed'
    return out


def count(wire_rows, outcomes=None):
    """The counters for one arm. `outcomes` None = no client-side source (repairs unmeasured)."""
    if wire_rows is None:
        return tally(None, outcomes)
    return tally([classify_response(r.get('request_tools'), r.get('response')) for r in wire_rows], outcomes)


def tally(classified, outcomes=None):
    """classified: [(verdicts, text_fallback)] per model request in order (a recorder may store
    these instead of any text). `outcomes` None = no client-side source."""
    if classified is None:
        return dict(measured=False, attempts=None, valid=None, malformed=None, text_fallback=None, repaired=None,
                    repair_turns=None, legit_tool_failures=None, malformed_reasons=None, per_100=None)
    attempts = valid = text_fb = 0
    reasons = {}
    malformed_ids, valid_ids = [], []
    repair_turns = 0
    pending_error = False
    for verdicts, fb in classified:
        if pending_error:
            # the request right after a malformed call whose client-side result was an ERROR
            # (linked by call id): the model's repair turn. A malformed call the client repaired
            # and executed successfully does not create one.
            repair_turns += 1
            pending_error = False
        if fb:
            attempts += 1
            text_fb += 1
            reasons['text_fallback'] = reasons.get('text_fallback', 0) + 1
        for call_id, reason in verdicts:
            attempts += 1
            if reason is None:
                valid += 1
                valid_ids.append(call_id)
            else:
                reasons[reason] = reasons.get(reason, 0) + 1
                malformed_ids.append(call_id)
                if outcomes is not None and outcomes.get(call_id) is False:
                    pending_error = True
    malformed = sum(reasons.values())
    # an id without a measured client outcome makes the dependent count unmeasured (null)
    if outcomes is None or any(i not in outcomes for i in malformed_ids):
        repaired = None
    else:
        repaired = sum(1 for i in malformed_ids if outcomes[i] is True)
    if outcomes is None or any(i not in outcomes for i in valid_ids):
        legit = None
    else:
        legit = sum(1 for i in valid_ids if outcomes[i] is False)
    if outcomes is None or repaired is None:
        repair_turns = None
    per = (lambda n: None if n is None or attempts == 0 else round(100.0 * n / attempts, 2))
    return dict(measured=True, attempts=attempts, valid=valid, malformed=malformed, text_fallback=text_fb,
                repaired=repaired, repair_turns=repair_turns, legit_tool_failures=legit, malformed_reasons=reasons,
                per_100=dict(malformed=per(malformed), repaired=per(repaired), text_fallback=per(text_fb)))


def read_jsonl(path):
    out = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line:
                out.append(json.loads(line))
    return out


def main(argv=None):
    import argparse
    ap = argparse.ArgumentParser(description='interim tool-call counters for one arm')
    ap.add_argument('--wire', required=True, help='JSON lines: request_tools + response per model request')
    ap.add_argument('--oxa-rpc', help='oxa --mode rpc record stream (JSON lines)')
    ap.add_argument('--opencode', help='opencode --format json event stream (JSON lines)')
    a = ap.parse_args(argv)
    outcomes = None
    if a.oxa_rpc:
        outcomes = oxa_outcomes(read_jsonl(a.oxa_rpc))
    elif a.opencode:
        outcomes = opencode_outcomes(read_jsonl(a.opencode))
    print(json.dumps(count(read_jsonl(a.wire), outcomes)))
    return 0


if __name__ == '__main__':
    sys.exit(main())
