"""v0.2.0 OpenAI v1 conformance for chat requests through the controller (O-133: one behaviour
for every client; nothing here looks at who the client is).

Pure functions and small stream translators; no I/O. The controller applies `chat_request` to a
chat body for the Rust engine (oxe) before `harness_compat.normalize`, and `ChatShim` to the
engine's reply (lab/docs/OPENAI-API.md, "Conformance").

- `reasoning_effort` is the standard thinking switch. The engines have an on/off switch and no
  budget levels, so the documented mapping is: `none` and `minimal` = thinking off; `low`,
  `medium`, `high` (and `xhigh`) = thinking on; absent or null = the server default (off). An
  explicit `chat_template_kwargs.enable_thinking` always wins. An unknown string is ignored
  (counted, reported in `X-Local-AI-Warning`); a non-string is a 400. The field itself is not
  forwarded (the engine would let `none` override an explicit `enable_thinking`).
- `response_format`: `text` is passed on. `json_object` and non-strict `json_schema` are best
  effort: an instruction (with the schema) is added to the end of the last user message, so the
  cached prefix of the conversation is kept, and a non-streamed reply is checked: a JSON value
  wrapped in prose or a code fence is cut out, anything still invalid is returned as the model
  wrote it with `X-Local-AI-Warning`. `json_schema` with `strict: true` is a 400 until
  grammar-constrained sampling exists: a client asking for a guarantee gets a clear refusal, not
  an unenforced promise.
- `presence_penalty`, `frequency_penalty`, `logit_bias`: always forwarded unchanged (the engine
  validates and applies them; the v0.2.0 Rust engine implements all three), so a request behaves
  the same through the controller as on the engine port. Only when the running engine reports
  that it lacks one (`capabilities.sampling`, from its /props) does a non-zero value get
  `X-Local-AI-Ignored: <fields>`.
- `usage`: every chat reply through the controller carries
  `prompt_tokens_details.cached_tokens` and `completion_tokens_details.reasoning_tokens`. The
  engine's own numbers win; until it reports reasoning tokens, the controller counts the
  streamed reasoning deltas (one per token) or, for a non-streamed reply, splits
  `completion_tokens` by the reasoning share of the generated text (an estimate, documented).
"""
import copy
import json

REASONING_EFFORT = {'none': False, 'minimal': False, 'low': True, 'medium': True, 'high': True, 'xhigh': True}
IGNORED_HEADER = 'X-Local-AI-Ignored'
WARNING_HEADER = 'X-Local-AI-Warning'
JSON_OBJECT_INSTRUCTION = ('Respond with one valid JSON object only: no code fences, no text before or after it.')
JSON_SCHEMA_INSTRUCTION = ('Respond with one valid JSON value only (no code fences, no text before or after it) '
                           'that matches this JSON Schema%s: %s')
SCHEMA_MAX_BYTES = 64 * 1024

if __package__:
    from .openai_api import BadParam
else:
    from openai_api import BadParam


class ChatMeta:
    """What the reply translation needs to know about the request (one per request)."""

    def __init__(self):
        self.stream = False
        self.json_mode = None      # None, 'object' or 'schema'
        self.schema = None
        self.ignored = []          # request fields accepted but without effect
        self.warnings = []

    def headers(self):
        out = []
        if self.ignored:
            out.append((IGNORED_HEADER, ', '.join(self.ignored)))
        if self.warnings:
            out.append((WARNING_HEADER, '; '.join(self.warnings)))
        return out


def _add_instruction(messages, text):
    """The instruction at the end of the last user message (the cached prefix of the conversation
    stays the same), or a new user message when the conversation does not end with one."""
    out = list(messages)
    last = out[-1] if out else None
    if isinstance(last, dict) and last.get('role') == 'user':
        content = last.get('content')
        if isinstance(content, str):
            out[-1] = {**last, 'content': content + '\n\n' + text}
            return out
        if isinstance(content, list):
            out[-1] = {**last, 'content': content + [{'type': 'text', 'text': '\n\n' + text}]}
            return out
    out.append({'role': 'user', 'content': text})
    return out


def _response_format(result, meta, notes):
    fmt = result.pop('response_format', None)
    if fmt is None:
        return
    if not isinstance(fmt, dict) or not isinstance(fmt.get('type'), str):
        raise BadParam('"response_format" must be an object with a "type".', 'response_format', 'invalid_value')
    kind = fmt['type']
    if kind == 'text':
        result['response_format'] = fmt
        return
    if kind == 'json_object':
        meta.json_mode, text = 'object', JSON_OBJECT_INSTRUCTION
    elif kind == 'json_schema':
        spec = fmt.get('json_schema')
        if not isinstance(spec, dict) or not isinstance(spec.get('schema', {}), dict):
            raise BadParam('"response_format.json_schema" must be an object with a "schema" object.',
                           'response_format.json_schema', 'invalid_value')
        if spec.get('strict') is True:
            raise BadParam('"response_format" json_schema with "strict": true is not supported: this local model '
                           'server has no grammar-constrained sampling, so it cannot guarantee the schema. Send '
                           '"strict": false (best effort: the schema is given to the model and the reply is '
                           'checked) or use tools.', 'response_format.json_schema.strict')
        meta.json_mode, meta.schema = 'schema', spec.get('schema')
        schema = json.dumps(spec.get('schema', {}), ensure_ascii=False, separators=(',', ':'), sort_keys=True)
        if len(schema) > SCHEMA_MAX_BYTES:
            raise BadParam('"response_format.json_schema.schema" is too large.', 'response_format.json_schema.schema',
                           'invalid_value')
        name = spec.get('name')
        text = JSON_SCHEMA_INSTRUCTION % ((' (%s)' % name) if isinstance(name, str) and name else '', schema)
    else:
        raise BadParam('Unknown "response_format" type %r (text, json_object or json_schema).' % kind[:40],
                       'response_format.type', 'invalid_value')
    if isinstance(result.get('messages'), list):
        result['messages'] = _add_instruction(result['messages'], text)
    notes.append('response_format_%s_best_effort' % kind)
    if meta.stream:
        meta.warnings.append('response_format is best effort; a streamed reply is not checked')


def _reasoning_effort(result, meta, notes, flash):
    if 'reasoning_effort' not in result:
        return
    effort = result.pop('reasoning_effort')
    if effort is None:
        return
    if not isinstance(effort, str):
        raise BadParam('"reasoning_effort" must be a string.', 'reasoning_effort', 'invalid_value')
    if effort not in REASONING_EFFORT:
        notes.append('reasoning_effort_unknown')
        meta.warnings.append('reasoning_effort %r is unknown and was ignored' % effort[:20])
        return
    if flash:
        notes.append('reasoning_effort_flash_ignored')
        meta.ignored.append('reasoning_effort')
        return
    kwargs = result.get('chat_template_kwargs')
    kwargs = dict(kwargs) if isinstance(kwargs, dict) else {}
    if 'enable_thinking' not in kwargs:  # an explicit template switch wins
        kwargs['enable_thinking'] = REASONING_EFFORT[effort]
        notes.append('reasoning_effort_%s' % ('on' if REASONING_EFFORT[effort] else 'off'))
    result['chat_template_kwargs'] = kwargs


def _nonzero_bias(bias):
    """True when a logit_bias has an effect (an all-zero bias is a no-op in the engine too)."""
    if isinstance(bias, dict):
        values = bias.values()
    elif isinstance(bias, list):
        values = [pair[1] for pair in bias if isinstance(pair, list) and len(pair) == 2]
    else:
        return bool(bias)
    return any(not (type(v) in (int, float) and v == 0) for v in values)


def _sampling(result, meta, supported):
    """Penalties and logit_bias are always forwarded unchanged, so the controller and the engine
    behave the same (the engine validates and applies them). `supported`: the controls the target
    engine reports (a set), or None when unknown (not loaded yet): then nothing is reported. A
    non-zero control the engine reports it does not implement is named in X-Local-AI-Ignored."""
    if supported is None:
        return
    for key in ('presence_penalty', 'frequency_penalty'):
        value = result.get(key)
        if type(value) in (int, float) and value != 0 and key not in supported:
            meta.ignored.append(key)
    if 'logit_bias' in result and _nonzero_bias(result['logit_bias']) and 'logit_bias' not in supported:
        meta.ignored.append('logit_bias')


def chat_request(payload, flash=False, sampling=None):
    """(payload, meta, notes) for one OpenAI chat body. Raises BadParam (400) for what is
    refused. The input is never modified. `sampling`: the sampling controls the target engine
    reports (see `_sampling`), None when unknown."""
    meta = ChatMeta()
    notes = []
    if not isinstance(payload, dict):
        return payload, meta, notes
    result = copy.copy(payload)
    meta.stream = result.get('stream') is True
    _reasoning_effort(result, meta, notes, flash)
    _response_format(result, meta, notes)
    _sampling(result, meta, sampling)
    return result, meta, notes


# ------------------------------------------------------------------ replies
def _json_value(text):
    """(parsed, exact text) of the JSON value in a reply: the whole text, else the text inside a
    code fence, else the outermost {...} / [...] span. None when there is none."""
    if not isinstance(text, str):
        return None
    candidates = [text.strip()]
    if '```' in text:
        inner = text.split('```', 2)
        if len(inner) >= 2:
            block = inner[1]
            if block.startswith(('json', 'JSON')):
                block = block[4:]
            candidates.append(block.strip())
    for opener, closer in (('{', '}'), ('[', ']')):
        start, end = text.find(opener), text.rfind(closer)
        if 0 <= start < end:
            candidates.append(text[start:end + 1])
    for candidate in candidates:
        try:
            return json.loads(candidate), candidate
        except ValueError:
            continue
    return None


def _schema_problem(value, schema):
    """A short reason when the value visibly misses the schema (top-level type and required
    keys only; this is a check, not a validator), else None."""
    if not isinstance(schema, dict):
        return None
    kind = schema.get('type')
    types = {'object': dict, 'array': list, 'string': str, 'boolean': bool}
    if kind in types and not isinstance(value, types[kind]):
        return 'the reply is not a JSON %s' % kind
    if isinstance(value, dict) and isinstance(schema.get('required'), list):
        missing = [k for k in schema['required'] if isinstance(k, str) and k not in value]
        if missing:
            return 'the reply misses required key(s) %s' % ', '.join(missing[:5])
    return None


def check_json(message, meta):
    """A non-streamed reply under response_format json_*: cut the JSON value out of surrounding
    prose or a fence; report what still does not fit in meta.warnings."""
    if meta.json_mode is None or not isinstance(message, dict):
        return
    if message.get('tool_calls'):
        return
    found = _json_value(message.get('content'))
    if found is None:
        meta.warnings.append('response_format: the reply is not valid JSON (best effort, not enforced)')
        return
    value, text = found
    if text != message.get('content'):
        message['content'] = text
    if meta.json_mode == 'object' and not isinstance(value, dict):
        meta.warnings.append('response_format: the reply is not a JSON object (best effort, not enforced)')
    elif meta.json_mode == 'schema':
        problem = _schema_problem(value, meta.schema)
        if problem:
            meta.warnings.append('response_format: %s (best effort, not enforced)' % problem)


def _text_len(value):
    return len(value) if isinstance(value, str) else 0


def reasoning_estimate(message, completion_tokens):
    """Reasoning tokens of a non-streamed reply: the engine does not report them yet, so the
    completion tokens are split by the reasoning share of the generated characters."""
    if not isinstance(message, dict) or type(completion_tokens) is not int or completion_tokens <= 0:
        return 0
    reasoning = _text_len(message.get('reasoning_content'))
    if not reasoning:
        return 0
    other = _text_len(message.get('content'))
    for call in message.get('tool_calls') or ():
        function = call.get('function') if isinstance(call, dict) else None
        if isinstance(function, dict):
            other += _text_len(function.get('arguments')) + _text_len(function.get('name'))
    return min(completion_tokens, round(completion_tokens * reasoning / (reasoning + other)))


def complete_usage(usage, reasoning_tokens, timings=None):
    """Fill the two standard detail objects of a chat usage block (the engine's values win)."""
    if not isinstance(usage, dict):
        return usage
    out = dict(usage)
    prompt = out.get('prompt_tokens_details')
    prompt = dict(prompt) if isinstance(prompt, dict) else {}
    if type(prompt.get('cached_tokens')) is not int:
        cached = timings.get('cache_n') if isinstance(timings, dict) else None
        prompt['cached_tokens'] = cached if type(cached) is int and cached >= 0 else 0
    out['prompt_tokens_details'] = prompt
    completion = out.get('completion_tokens_details')
    completion = dict(completion) if isinstance(completion, dict) else {}
    if type(completion.get('reasoning_tokens')) is not int:
        total = out.get('completion_tokens')
        value = reasoning_tokens if type(reasoning_tokens) is int else 0
        completion['reasoning_tokens'] = min(value, total) if type(total) is int else value
    out['completion_tokens_details'] = completion
    return out


def chat_response(value, meta):
    """A non-streamed chat completion (parsed JSON) made conformant in place; returns it."""
    if not isinstance(value, dict) or not isinstance(value.get('choices'), list):
        return value
    message = None
    for choice in value['choices']:
        if isinstance(choice, dict) and isinstance(choice.get('message'), dict):
            message = choice['message']
            check_json(message, meta)
            break
    if isinstance(value.get('usage'), dict):
        value['usage'] = complete_usage(value['usage'], reasoning_estimate(message, value['usage'].get('completion_tokens')),
                                        value.get('timings'))
    return value


class ChatStream:
    """Pass a chat SSE stream through, frame by frame: counts the reasoning deltas and completes
    the usage block of the final usage chunk. Every other frame is passed on byte for byte."""

    def __init__(self, meta):
        self.meta = meta
        self.buffer = b''
        self.reasoning = 0

    def _event(self, raw):
        if not raw.startswith(b'data:') or (b'reasoning_content' not in raw and b'"usage"' not in raw):
            return raw + b'\n\n'
        try:
            value = json.loads(raw[5:].strip())
        except ValueError:
            return raw + b'\n\n'
        if not isinstance(value, dict):
            return raw + b'\n\n'
        for choice in value.get('choices') or ():
            delta = choice.get('delta') if isinstance(choice, dict) else None
            if isinstance(delta, dict) and isinstance(delta.get('reasoning_content'), str) and delta['reasoning_content']:
                self.reasoning += 1
        if not isinstance(value.get('usage'), dict):
            return raw + b'\n\n'
        value['usage'] = complete_usage(value['usage'], self.reasoning, value.get('timings'))
        return b'data: ' + json.dumps(value, ensure_ascii=False).encode() + b'\n\n'

    def feed(self, chunk):
        self.buffer += chunk
        out = b''
        while b'\n\n' in self.buffer:
            raw, self.buffer = self.buffer.split(b'\n\n', 1)
            out += self._event(raw)
        return out

    def finish(self):
        out = self.buffer
        self.buffer = b''
        return out


class ChatShim:
    """The reply side for a chat request (ctl_proxy_stream): JSON body or stream translator."""

    def __init__(self, meta):
        self.meta = meta
        self.stream = meta.stream

    def body(self, data):
        try:
            value = json.loads(data)
        except ValueError:
            return data
        before = copy.deepcopy(value)
        value = chat_response(value, self.meta)
        if value == before:
            return data  # nothing to add: the engine's bytes unchanged
        return json.dumps(value, ensure_ascii=False).encode()

    def streamer(self):
        return ChatStream(self.meta)

    def headers(self):
        return self.meta.headers()
