"""D6: the Anthropic Messages upstream for a remote catalog entry.

This is a native Messages API client (the official ``anthropic`` Python SDK), not an
OpenAI-compatible shim: the controller translates OpenAI v1 chat requests and replies in both
directions, including tools, streamed tool calls, stop reasons (``refusal`` included), adaptive
thinking plus ``output_config.effort`` and cache-control breakpoints on the frozen prefix and
the latest turn.

The key is passed to the SDK per request; it is never logged and every error string is
redacted before it reaches a client or the ledger.
"""
import hashlib
import json
from collections import OrderedDict
import threading
import time
if __package__:
    from . import cloud, openai_api
else:
    import cloud
    import openai_api

MAX_BODY = 64 * 1024 * 1024
# Thinking blocks are opaque provider state: the signature (or a redacted_thinking ``data``
# blob) is bound to the conversation and must be replayed byte for byte with the assistant
# turn that produced it.  The OpenAI wire has no field for that state, so keep it in memory
# keyed by the exact conversation prefix and the assistant turn the client echoes back.
_THINKING_LOCK = threading.Lock()
_THINKING = OrderedDict()
_THINKING_ENTRIES = 4096
_THINKING_BYTES = 64 * 1024 * 1024
_THINKING_SIZE = [0]
THINKING_TYPES = ('thinking', 'redacted_thinking')
# The current 5.5 Opus/Sonnet Messages models reject forced tool use on every request. These
# family markers are model-neutral: the shipped core never names a vendor or client.
FORCED_TOOL_UNSUPPORTED = ('-opus-5-5', '-sonnet-5-5')
STOP_REASONS = {
    'end_turn': 'stop',
    'stop_sequence': 'stop',
    'max_tokens': 'length',
    'tool_use': 'tool_calls',
    'refusal': 'content_filter',
    'pause_turn': 'stop',
    'model_context_window_exceeded': 'length',
}
EFFORT = {'low': 'low', 'medium': 'medium', 'high': 'high', 'xhigh': 'xhigh',
          'minimal': 'low', 'none': 'low'}


def _cache_control():
    return {'type': 'ephemeral'}


def _effort(payload):
    """(thinking, output_config) from the OpenAI effort or the local enable_thinking switch.

    The current 5.5-family models reject ``thinking: {"type": "disabled"}`` and sampling
    parameters. Depth is controlled by adaptive thinking plus ``output_config.effort`` only;
    ``none`` and ``minimal`` map to the lowest effort rather than a disabled thinking mode.
    """
    effort = payload.get('reasoning_effort')
    kwargs = payload.get('chat_template_kwargs') if isinstance(payload.get('chat_template_kwargs'), dict) else {}
    if effort is None and isinstance(kwargs.get('enable_thinking'), bool):
        effort = 'high' if kwargs['enable_thinking'] else 'none'
    if effort is None:
        return None, None
    if not isinstance(effort, str):
        raise cloud.CloudError('"reasoning_effort" must be a string.', 400, 'invalid_value', 'reasoning_effort')
    if effort not in EFFORT:
        return None, None
    return {'type': 'adaptive'}, {'effort': EFFORT[effort]}


def _text_parts(content):
    if isinstance(content, str):
        return [{'type': 'text', 'text': content}]
    out = []
    if isinstance(content, list):
        for part in content:
            if isinstance(part, dict) and part.get('type') == 'text' and isinstance(part.get('text'), str):
                out.append({'type': 'text', 'text': part['text']})
            elif isinstance(part, dict) and part.get('type') in ('image_url', 'input_image'):
                out.append({'type': 'text', 'text': '[image omitted: the cloud text model reads text only]'})
    return out


def _tool_use(call):
    function = call.get('function') if isinstance(call, dict) else None
    if not isinstance(function, dict):
        return None
    args = function.get('arguments')
    if isinstance(args, str):
        try:
            args = json.loads(args)
        except ValueError:
            args = {'input': args}
    if not isinstance(args, dict):
        args = {'input': args}
    return {'type': 'tool_use', 'id': call.get('id') or 'toolu_unknown',
            'name': function.get('name') or 'tool', 'input': args}


def _assistant_key(message):
    """Canonical assistant turn the client echoes: content and tool calls, not reasoning text."""
    if not isinstance(message, dict):
        return 'null'
    content = message.get('content')
    if isinstance(content, list):
        content = ''.join(part.get('text', '') for part in content
                          if isinstance(part, dict) and part.get('type') == 'text')
    calls = []
    for call in message.get('tool_calls') or ():
        if not isinstance(call, dict):
            continue
        function = call.get('function') if isinstance(call.get('function'), dict) else {}
        args = function.get('arguments')
        if isinstance(args, str):
            try:
                args = json.loads(args)
            except ValueError:
                pass
        calls.append({'id': call.get('id'), 'type': call.get('type'),
                      'name': function.get('name'), 'arguments': args})
    return json.dumps({'role': message.get('role'), 'content': content, 'tool_calls': calls,
                       'tool_call_id': message.get('tool_call_id')},
                      ensure_ascii=False, sort_keys=True, separators=(',', ':'))


def _history_digest(messages, count):
    digest = hashlib.sha256(b'oxc-anthropic-history-v2\0')
    for message in list(messages or ())[:count]:
        digest.update(_assistant_key(message).encode('utf-8', 'replace'))
        digest.update(b'\0')
    return digest.hexdigest()


def _cache_identity(item, key):
    credential = hashlib.sha256(b'oxc-anthropic-credential-v1\0' + (key or '').encode('utf-8')).hexdigest()
    return (item['id'], item.get('upstream_model'), item.get('base_url'), credential)


def _remember_blocks(item, history, assistant, blocks, key=None):
    """Keep the native assistant content (thinking signatures included) for the next turn."""
    if not isinstance(blocks, list) or not any(
            isinstance(block, dict) and block.get('type') in THINKING_TYPES for block in blocks):
        return
    frozen = json.loads(json.dumps(blocks, ensure_ascii=False))
    cache_key = (*_cache_identity(item, key),
                 _history_digest(list(history or ()) + [assistant], len(list(history or ())) + 1))
    size = len(json.dumps(frozen, ensure_ascii=False).encode('utf-8'))
    with _THINKING_LOCK:
        old = _THINKING.pop(cache_key, None)
        if old is not None:
            _THINKING_SIZE[0] -= len(json.dumps(old, ensure_ascii=False).encode('utf-8'))
        _THINKING[cache_key] = frozen
        _THINKING_SIZE[0] += size
        while _THINKING and (len(_THINKING) > _THINKING_ENTRIES or _THINKING_SIZE[0] > _THINKING_BYTES):
            _, dropped = _THINKING.popitem(last=False)
            _THINKING_SIZE[0] -= len(json.dumps(dropped, ensure_ascii=False).encode('utf-8'))


def _restore_blocks(item, messages, index, key=None):
    """The exact native content stored for the assistant message at ``index``, if known."""
    cache_key = (*_cache_identity(item, key), _history_digest(messages, index + 1))
    with _THINKING_LOCK:
        blocks = _THINKING.get(cache_key)
        if blocks is None:
            return None
        _THINKING.move_to_end(cache_key)
        return json.loads(json.dumps(blocks, ensure_ascii=False))


def _explicit_thinking(message):
    """Thinking blocks a direct client put in ``thinking_blocks``, or []."""
    explicit = message.get('thinking_blocks')
    if isinstance(explicit, list):
        return [block for block in explicit if isinstance(block, dict)
                and block.get('type') in THINKING_TYPES]
    return []


def _native_content(message):
    """A full native content array supplied by a direct client, otherwise None."""
    content = message.get('content')
    if isinstance(content, list) and any(
            isinstance(block, dict) and block.get('type') in THINKING_TYPES for block in content):
        return [block for block in content if isinstance(block, dict)]
    return None


def messages_request(payload, item, key=None):
    """The native Messages API parameters for one OpenAI v1 chat body."""
    if not isinstance(payload, dict):
        raise cloud.CloudError('Invalid cloud request.', 400, 'invalid_request')
    system = []
    converted = []
    raw_messages = payload.get('messages') if isinstance(payload.get('messages'), list) else []
    for position, message in enumerate(raw_messages):
        if not isinstance(message, dict):
            continue
        role = message.get('role')
        if role in ('system', 'developer'):
            text = ''.join(part.get('text', '') for part in _text_parts(message.get('content')))
            if text:
                system.append(text)
            continue
        if role == 'tool':
            block = {'type': 'tool_result', 'tool_use_id': message.get('tool_call_id') or '',
                     'content': [{'type': 'text', 'text': str(message.get('content') or '')}]}
            converted.append(('user', [block]))
            continue
        if role == 'assistant':
            restored = _restore_blocks(item, raw_messages, position, key)
            native = _native_content(message)
            if restored is not None:
                blocks = restored
            elif native is not None:
                blocks = native
            else:
                blocks = _text_parts(message.get('content'))
                for call in message.get('tool_calls') or ():
                    block = _tool_use(call)
                    if block is not None:
                        blocks.append(block)
                blocks = _explicit_thinking(message) + blocks
            if blocks:
                converted.append(('assistant', blocks))
            continue
        blocks = _text_parts(message.get('content'))
        if blocks:
            converted.append(('user', blocks))

    messages = []
    for role, blocks in converted:
        if messages and messages[-1]['role'] == role:
            messages[-1]['content'].extend(blocks)
        else:
            messages.append({'role': role, 'content': list(blocks)})
    if not messages:
        messages = [{'role': 'user', 'content': [{'type': 'text', 'text': ''}]}]
    # The latest history turn ends with a cache breakpoint (the frozen prefix is handled below).
    # Never touch a thinking/redacted block: its signature must stay byte-identical.
    last = messages[-1]['content'][-1]
    if last.get('type') not in THINKING_TYPES:
        last['cache_control'] = _cache_control()

    params = {'model': item['upstream_model'], 'messages': messages,
              'max_tokens': _max_tokens(payload, item)}
    if system:
        params['system'] = [{'type': 'text', 'text': '\n\n'.join(system), 'cache_control': _cache_control()}]
    tools = []
    for tool in payload.get('tools') or ():
        function = tool.get('function') if isinstance(tool, dict) else None
        if not isinstance(function, dict) or not isinstance(function.get('name'), str):
            continue
        tools.append({'name': function['name'],
                      'description': function.get('description') if isinstance(function.get('description'), str) else '',
                      'input_schema': function.get('parameters') if isinstance(function.get('parameters'), dict)
                      else {'type': 'object', 'properties': {}}})
    if tools:
        tools[-1]['cache_control'] = _cache_control()
        params['tools'] = tools
        choice = payload.get('tool_choice')
        forced = choice == 'required' or (isinstance(choice, dict)
                                          and isinstance(choice.get('function'), dict))
        if forced and any(marker in item['upstream_model'] for marker in FORCED_TOOL_UNSUPPORTED):
            raise cloud.CloudError('Forced tool_choice is not supported by this model.',
                                   400, 'invalid_value', 'tool_choice')
        if choice == 'none':
            params.pop('tools')
        elif choice == 'required':
            params['tool_choice'] = {'type': 'any'}
        elif isinstance(choice, dict) and isinstance(choice.get('function'), dict):
            params['tool_choice'] = {'type': 'tool', 'name': choice['function'].get('name') or ''}
        else:
            params['tool_choice'] = {'type': 'auto'}
    # The current 5.5-family models reject temperature/top_p/top_k (400); depth is thinking +
    # effort.
    stop = payload.get('stop')
    if isinstance(stop, str):
        params['stop_sequences'] = [stop]
    elif isinstance(stop, list):
        params['stop_sequences'] = [s for s in stop if isinstance(s, str)][:16]
    thinking, output = _effort(payload)
    if thinking is not None:
        params['thinking'] = thinking
    if output is not None:
        params['output_config'] = output
    if payload.get('stream') is True:
        params['stream'] = True
    return params


def _max_tokens(payload, item):
    return cloud._max_tokens(payload, item)



def _dump(value):
    if isinstance(value, dict):
        return value
    dump = getattr(value, 'model_dump', None)
    if callable(dump):
        try:
            return dump(exclude_none=True)
        except TypeError:
            return dump()
    return json.loads(json.dumps(value, default=str))


def _usage(usage):
    """OpenAI usage from a Messages usage object; cache reads count as prompt tokens."""
    value = _dump(usage) if usage is not None else {}
    input_tokens = value.get('input_tokens') if type(value.get('input_tokens')) is int else 0
    output_tokens = value.get('output_tokens') if type(value.get('output_tokens')) is int else 0
    cache_read = value.get('cache_read_input_tokens') if type(value.get('cache_read_input_tokens')) is int else 0
    cache_write = value.get('cache_creation_input_tokens') if type(value.get('cache_creation_input_tokens')) is int else 0
    prompt = input_tokens + cache_read + cache_write
    return {'prompt_tokens': prompt, 'completion_tokens': output_tokens,
            'total_tokens': prompt + output_tokens,
            'prompt_tokens_details': {'cached_tokens': cache_read, 'cache_write_tokens': cache_write},
            'completion_tokens_details': {'reasoning_tokens': 0}}


def response(message, item, history=None, key=None):
    """An OpenAI chat completion from a native Messages reply."""
    value = _dump(message)
    content, reasoning, calls = [], [], []
    for block in value.get('content') or ():
        if not isinstance(block, dict):
            continue
        if block.get('type') == 'text' and isinstance(block.get('text'), str):
            content.append(block['text'])
        elif block.get('type') == 'thinking' and isinstance(block.get('thinking'), str):
            reasoning.append(block['thinking'])
        elif block.get('type') == 'tool_use':
            calls.append({'id': block.get('id') or 'toolu_unknown', 'type': 'function',
                          'function': {'name': block.get('name') or 'tool',
                                       'arguments': json.dumps(block.get('input') or {}, ensure_ascii=False)}})
    assistant = {'role': 'assistant', 'content': ''.join(content) or None}
    if reasoning:
        assistant['reasoning_content'] = ''.join(reasoning)
    if calls:
        assistant['tool_calls'] = calls
    _remember_blocks(item, [] if history is None else history, assistant, value.get('content'), key)
    finish = STOP_REASONS.get(value.get('stop_reason'), 'stop')
    return {'id': value.get('id') or 'chatcmpl_anthropic', 'object': 'chat.completion',
            'created': int(time.time()), 'model': item['id'],
            'choices': [{'index': 0, 'message': assistant, 'finish_reason': finish}],
            'usage': _usage(value.get('usage'))}


class _Stream:
    """Translate Anthropic's Messages SSE events into OpenAI chat completion chunks."""

    def __init__(self, item, include_usage, history=None, key=None):
        self.item = item
        self.include_usage = include_usage
        self.history = history
        self.key = key
        self.text_redactor = cloud.StreamRedactor(key)
        self.thinking_redactor = cloud.StreamRedactor(key)
        self.call_redactors = {}
        self.model = item['id']
        self.finish = None
        self.usage = None
        self.usage_final = False
        self.raw_usage = {}
        self.tool_slots = {}
        self.blocks = []
        self.open = {}
        self.text = []
        self.thinking = []
        self.calls = {}
        self.stopped = False
        self.error = None

    def _chunk(self, delta, finish=None):
        return {'id': 'chatcmpl_anthropic', 'object': 'chat.completion.chunk', 'created': int(time.time()),
                'model': self.model,
                'choices': [{'index': 0, 'delta': delta, 'finish_reason': finish}]}

    def _remember(self):
        assistant = {'role': 'assistant', 'content': ''.join(self.text) or None}
        if self.thinking:
            assistant['reasoning_content'] = ''.join(self.thinking)
        calls = []
        for index in sorted(self.tool_slots, key=self.tool_slots.get):
            call = self.calls.get(index)
            if call is None:
                continue
            calls.append({'id': call['id'], 'type': 'function',
                          'function': {'name': call['name'], 'arguments': call['args']}})
        if calls:
            assistant['tool_calls'] = calls
        _remember_blocks(self.item, [] if self.history is None else self.history, assistant, self.blocks, self.key)

    def _block(self, index, block):
        native = dict(block)
        kind = native.get('type')
        if kind == 'thinking':
            native.setdefault('thinking', '')
            native.setdefault('signature', '')
            self.open[index] = native
        elif kind == 'text':
            native.setdefault('text', '')
            self.open[index] = native
        elif kind == 'tool_use':
            native.setdefault('id', 'toolu_unknown')
            native.setdefault('name', 'tool')
            native.setdefault('input', {})
            self.open[index] = native
            slot = len(self.tool_slots)
            self.tool_slots[index] = slot
            self.calls[index] = {'id': native['id'], 'name': native['name'], 'args': ''}
            self.call_redactors[index] = cloud.StreamRedactor(self.key)
        self.blocks.append(native)

    def event(self, event):
        value = _dump(event)
        kind = value.get('type')
        out = []
        if kind == 'message_start':
            message = value.get('message') or {}
            usage = message.get('usage')
            if usage is not None:
                self.raw_usage.update(_dump(usage))
                self.usage = _usage(self.raw_usage) if _valid_usage(self.raw_usage) else None
            out.append(self._chunk({'role': 'assistant', 'content': None}))
        elif kind == 'content_block_start':
            index = value.get('index') if type(value.get('index')) is int else 0
            block = value.get('content_block') or {}
            self._block(index, block)
            if block.get('type') == 'tool_use':
                out.append(self._chunk({'tool_calls': [{
                    'index': self.tool_slots[index], 'id': block.get('id') or 'toolu_unknown',
                    'type': 'function',
                    'function': {'name': block.get('name') or 'tool', 'arguments': ''}}]}))
        elif kind == 'content_block_delta':
            index = value.get('index') if type(value.get('index')) is int else 0
            delta = value.get('delta') or {}
            native = self.open.get(index)
            if delta.get('type') == 'text_delta' and isinstance(delta.get('text'), str):
                if native is not None:
                    native['text'] = native.get('text', '') + delta['text']
                self.text.append(delta['text'])
                safe = self.text_redactor.feed(delta['text'].encode('utf-8'))
                if safe:
                    out.append(self._chunk({'content': safe.decode('utf-8', 'replace')}))
            elif delta.get('type') == 'thinking_delta' and isinstance(delta.get('thinking'), str):
                if native is not None:
                    native['thinking'] = native.get('thinking', '') + delta['thinking']
                self.thinking.append(delta['thinking'])
                safe = self.thinking_redactor.feed(delta['thinking'].encode('utf-8'))
                if safe:
                    out.append(self._chunk({'reasoning_content': safe.decode('utf-8', 'replace')}))
            elif delta.get('type') == 'signature_delta' and isinstance(delta.get('signature'), str):
                if native is not None:
                    native['signature'] = delta['signature']
            elif delta.get('type') == 'input_json_delta' and isinstance(delta.get('partial_json'), str):
                call = self.calls.get(index)
                if call is not None:
                    call['args'] += delta['partial_json']
                safe = self.call_redactors[index].feed(delta['partial_json'].encode('utf-8'))
                if safe:
                    out.append(self._chunk({'tool_calls': [{
                        'index': self.tool_slots.get(index, 0),
                        'function': {'arguments': safe.decode('utf-8', 'replace')}}]}))
        elif kind == 'content_block_stop':
            index = value.get('index') if type(value.get('index')) is int else 0
            native = self.open.pop(index, None)
            if native is not None and native.get('type') == 'tool_use':
                args = self.calls.get(index, {}).get('args', '')
                try:
                    native['input'] = json.loads(args) if args else {}
                except ValueError:
                    native['input'] = {'input': args}
        elif kind == 'error':
            self.error = value.get('error') or value
        elif kind == 'message_delta':
            delta = value.get('delta') or {}
            if isinstance(delta.get('stop_reason'), str):
                self.finish = STOP_REASONS.get(delta['stop_reason'], 'stop')
            if value.get('usage') is not None:
                self.raw_usage.update(_dump(value['usage']))
                self.usage = _usage(self.raw_usage) if _valid_usage(self.raw_usage) else None
                self.usage_final = self.usage is not None
        elif kind == 'message_stop':
            self.stopped = True
            self._remember()
            tail = self.text_redactor.finish()
            if tail:
                out.append(self._chunk({'content': tail.decode('utf-8', 'replace')}))
            tail = self.thinking_redactor.finish()
            if tail:
                out.append(self._chunk({'reasoning_content': tail.decode('utf-8', 'replace')}))
            for index in sorted(self.call_redactors):
                tail = self.call_redactors[index].finish()
                if tail:
                    out.append(self._chunk({'tool_calls': [{
                        'index': self.tool_slots.get(index, 0),
                        'function': {'arguments': tail.decode('utf-8', 'replace')}}]}))
            out.append(self._chunk({}, self.finish or 'stop'))
            if self.include_usage and self.usage is not None:
                out.append({'id': 'chatcmpl_anthropic', 'object': 'chat.completion.chunk',
                            'created': int(time.time()), 'model': self.model,
                            'choices': [], 'usage': self.usage})
        return out


def _is_stream(payload):
    return isinstance(payload, dict) and payload.get('stream') is True


def _include_usage(payload):
    options = payload.get('stream_options') if isinstance(payload, dict) else None
    return isinstance(options, dict) and options.get('include_usage') is True


def _client(item, key):
    try:
        import anthropic
    except ImportError:
        raise cloud.CloudError('The Anthropic SDK is not installed on this host.', 503, 'cloud_sdk_unavailable') from None
    return anthropic.Anthropic(api_key=key, base_url=item['base_url'], max_retries=0,
                               timeout=min(cloud.DEFAULT_TIMEOUT, 1800.0))


def _row(item):
    return {'remote': True, 'remote_provider': 'anthropic', 'remote_model': item['upstream_model']}


def _usage_row(row, usage, item):
    row.update({'prompt_tokens': usage['prompt_tokens'],
                'completion_tokens': usage['completion_tokens'],
                'cached_tokens': usage['prompt_tokens_details']['cached_tokens'],
                'cache_write_tokens': usage['prompt_tokens_details'].get('cache_write_tokens', 0),
                'cost_usd': cloud.cost(item, usage['prompt_tokens'], usage['completion_tokens'],
                                       cache_read=usage['prompt_tokens_details']['cached_tokens'],
                                       cache_write=usage['prompt_tokens_details'].get('cache_write_tokens', 0))})
    return row


def _valid_usage(value):
    return (isinstance(value, dict)
            and all(type(value.get(name)) is int and value[name] >= 0
                    for name in ('input_tokens', 'output_tokens'))
            and all(name not in value or (type(value[name]) is int and value[name] >= 0)
                    for name in ('cache_read_input_tokens', 'cache_creation_input_tokens')))


def _has_usage(message):
    value = _dump(message)
    return _valid_usage(_dump(value.get('usage')) if value.get('usage') is not None else None)



def _close(st, status, error=None):
    if st.led is None:
        return
    try:
        st.led.finish(status, error)
    except Exception:
        cloud.mark_ledger_failure()
        return  # Do not retry a potentially completed append.
    if getattr(st.led, 'written', True) is False:
        cloud.mark_ledger_failure()


def _record(st, item, hold, usage, failed, status, error, cancelled=None, usage_final=False):
    if hold is not None and hold.accounted:
        return
    row = _row(item)
    if isinstance(usage, dict):
        _usage_row(row, usage, item)
    if hold is None:
        row.setdefault('cost_usd', 0.0)
        row.setdefault('settled', 'not_forwarded')
    elif cancelled is not None:
        cloud.cancelled_cost(row, hold, item, *cancelled, usage_final=usage_final)
    else:
        cloud.conservative_cost(row, hold, failed=failed)
    if st.led is not None:
        st.led.row.update(row)
    _close(st, status, error)
    if hold is not None:
        hold.accounted = True
        hold.settle()


def _error(self, st, item, status, message, key, code='cloud_upstream_error', hold=None,
           failed=False, usage=None):
    _record(st, item, hold, usage, failed, status, code)
    body = {'error': {'message': cloud.redact(message, key), 'type': 'cloud_error',
                      'param': 'model', 'code': code}}
    return self.json_reply(status, body)


def _write(self, st, data):
    st.cloud_streamed += len(data)
    self.wfile.write(('%x\r\n' % len(data)).encode() + data + b'\r\n')
    self.wfile.flush()


def forward(self, st, item, key):
    """Run one Anthropic Messages request and write the OpenAI v1 reply."""
    payload = st.payload
    include_usage = _include_usage(payload)
    streamed = _is_stream(payload)
    stream = None
    translator = None
    hold = None
    started = False
    try:
        params = messages_request(payload, item, key)
        client = _client(item, key)
        hold = cloud.reserve(st.api_value, item, params)
    except cloud.CloudError as error:
        if hold is not None and not hold.settled:
            hold.settle()
        return _error(self, st, item, error.status, str(error), key, error.code)
    try:
        if not streamed:
            message = client.messages.create(**params)
            present = _has_usage(message)
            reply = response(message, item, history=payload.get('messages'), key=key)
            row = _row(item)
            if present:
                _usage_row(row, reply['usage'], item)
            cloud.conservative_cost(row, hold, failed=not present)
            if st.led is not None:
                st.led.row.update(row)
            data = cloud.redact(json.dumps(reply, ensure_ascii=False).encode(), key)
            if st.led is not None:
                st.led.observe(data)
            _close(st, 200)
            hold.accounted = True
            hold.settle()
            self.reply(200, data, 'application/json')
            return
        stream = client.messages.create(**params)
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Cache-Control', 'no-store')
        self.send_header('Transfer-Encoding', 'chunked')
        for name, value in self.cors_reply_headers():
            self.send_header(name, value)
        self.end_headers()
        started = True
        translator = _Stream(item, include_usage, payload.get('messages'), key)
        redactor = cloud.StreamRedactor(key)

        def relay(data):
            safe = redactor.feed(data)
            if not safe:
                return
            if st.led is not None:
                st.led.observe(safe)
            _write(self, st, safe)

        for event in stream:
            for chunk in translator.event(event):
                relay(('data: ' + json.dumps(chunk, ensure_ascii=False) + '\n\n').encode())
        if translator.error is not None or not translator.stopped:
            _record(st, item, hold, translator.usage, True, 503, 'cloud_stream_error')
            self.close_connection = True
            return
        relay(b'data: [DONE]\n\n')
        tail = redactor.finish()
        if tail:
            if st.led is not None:
                st.led.observe(tail)
            _write(self, st, tail)
        self.wfile.write(b'0\r\n\r\n')
        self.wfile.flush()
        row = _row(item)
        if translator.usage is not None:
            _usage_row(row, translator.usage, item)
        cloud.conservative_cost(row, hold, failed=translator.usage is None)
        if st.led is not None:
            st.led.row.update(row)
        _close(st, 200)
        hold.settle()
    except cloud.CloudError as error:
        if not started:
            _error(self, st, item, error.status, str(error), key, error.code, hold, failed=True)
        else:
            _record(st, item, hold, translator.usage if translator else None, True,
                    error.status, error.code)
            self.close_connection = True
    except (BrokenPipeError, ConnectionResetError):  # the client cancelled the stream
        st.client_write_failed = True
        _record(st, item, hold, translator.usage if translator else None, False, 499, 'cancelled',
                cancelled=(len(json.dumps(params, ensure_ascii=False).encode()),
                           st.cloud_streamed, cloud.elapsed_since(hold)),
                usage_final=bool(translator and translator.usage_final))
        self.close_connection = True
    except Exception as error:  # the SDK's own status/connection errors, redacted
        status = getattr(error, 'status_code', None) or 503
        if not isinstance(status, int) or not 400 <= status <= 599:
            status = 503
        message = cloud.redact(str(error), key)
        if not started:
            _error(self, st, item, status, message or 'The Anthropic provider is unavailable.',
                   key, hold=hold, failed=True)
        else:
            _record(st, item, hold, translator.usage if translator else None, True,
                    status, 'cloud_stream_error')
            self.close_connection = True
    finally:
        try:
            if stream is not None:
                stream.close()
        except Exception:
            pass
        if hold is not None and not hold.settled:
            if st.led is not None and getattr(st.led, 'written', None) is None:
                # An unexpected failure escaped before the row was written; never release a
                # reservation without recording spend.
                _record(st, item, hold, translator.usage if translator else None, True,
                        503, 'cloud_unavailable')
            else:
                hold.settle()
