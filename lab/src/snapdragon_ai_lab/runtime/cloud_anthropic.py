"""D6: the Anthropic Messages upstream for a remote catalog entry.

This is a native Messages API client (the official ``anthropic`` Python SDK), not an
OpenAI-compatible shim: the controller translates OpenAI v1 chat requests and replies in both
directions, including tools, streamed tool calls, stop reasons (``refusal`` included), adaptive
thinking plus ``output_config.effort`` and cache-control breakpoints on the frozen prefix and
the latest turn.

The key is passed to the SDK per request; it is never logged and every error string is
redacted before it reaches a client or the ledger.
"""
import json
import time
if __package__:
    from . import cloud, openai_api
else:
    import cloud
    import openai_api

MAX_BODY = 64 * 1024 * 1024
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
    """(thinking, output_config) from the OpenAI effort or the local enable_thinking switch."""
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
    if effort in ('none', 'minimal'):
        return {'type': 'disabled'}, {'effort': EFFORT[effort]}
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


def messages_request(payload, item):
    """The native Messages API parameters for one OpenAI v1 chat body."""
    if not isinstance(payload, dict):
        raise cloud.CloudError('Invalid cloud request.', 400, 'invalid_request')
    system = []
    converted = []
    for message in payload.get('messages') or ():
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
            blocks = _text_parts(message.get('content'))
            for call in message.get('tool_calls') or ():
                block = _tool_use(call)
                if block is not None:
                    blocks.append(block)
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
    messages[-1]['content'][-1]['cache_control'] = _cache_control()

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
        if choice == 'none':
            params.pop('tools')
        elif choice == 'required':
            params['tool_choice'] = {'type': 'any'}
        elif isinstance(choice, dict) and isinstance(choice.get('function'), dict):
            params['tool_choice'] = {'type': 'tool', 'name': choice['function'].get('name') or ''}
        else:
            params['tool_choice'] = {'type': 'auto'}
    if isinstance(payload.get('temperature'), (int, float)):
        params['temperature'] = payload['temperature']
    if isinstance(payload.get('top_p'), (int, float)):
        params['top_p'] = payload['top_p']
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
    for field in ('max_tokens', 'max_completion_tokens'):
        value = payload.get(field)
        if type(value) is int and value > 0:
            return min(value, 64_000)
    return item.get('max_output') or 4096


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
            'prompt_tokens_details': {'cached_tokens': cache_read},
            'completion_tokens_details': {'reasoning_tokens': 0}}


def response(message, item):
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
    finish = STOP_REASONS.get(value.get('stop_reason'), 'stop')
    return {'id': value.get('id') or 'chatcmpl_anthropic', 'object': 'chat.completion',
            'created': int(time.time()), 'model': item['id'],
            'choices': [{'index': 0, 'message': assistant, 'finish_reason': finish}],
            'usage': _usage(value.get('usage'))}


class _Stream:
    """Translate Anthropic's Messages SSE events into OpenAI chat completion chunks."""

    def __init__(self, item, include_usage):
        self.item = item
        self.include_usage = include_usage
        self.model = item['id']
        self.finish = None
        self.usage = None
        self.raw_usage = {}
        self.tool_slots = {}

    def _chunk(self, delta, finish=None):
        return {'id': 'chatcmpl_anthropic', 'object': 'chat.completion.chunk', 'created': int(time.time()),
                'model': self.model,
                'choices': [{'index': 0, 'delta': delta, 'finish_reason': finish}]}

    def event(self, event):
        value = _dump(event)
        kind = value.get('type')
        out = []
        if kind == 'message_start':
            message = value.get('message') or {}
            usage = message.get('usage')
            if usage is not None:
                self.raw_usage.update(_dump(usage))
                self.usage = _usage(self.raw_usage)
            out.append(self._chunk({'role': 'assistant', 'content': None}))
        elif kind == 'content_block_start':
            index = value.get('index') if type(value.get('index')) is int else 0
            block = value.get('content_block') or {}
            if block.get('type') == 'tool_use':
                slot = len(self.tool_slots)
                self.tool_slots[index] = slot
                out.append(self._chunk({'tool_calls': [{
                    'index': slot, 'id': block.get('id') or 'toolu_unknown', 'type': 'function',
                    'function': {'name': block.get('name') or 'tool', 'arguments': ''}}]}))
        elif kind == 'content_block_delta':
            index = value.get('index') if type(value.get('index')) is int else 0
            delta = value.get('delta') or {}
            if delta.get('type') == 'text_delta' and isinstance(delta.get('text'), str):
                out.append(self._chunk({'content': delta['text']}))
            elif delta.get('type') == 'thinking_delta' and isinstance(delta.get('thinking'), str):
                out.append(self._chunk({'reasoning_content': delta['thinking']}))
            elif delta.get('type') == 'input_json_delta' and isinstance(delta.get('partial_json'), str):
                out.append(self._chunk({'tool_calls': [{
                    'index': self.tool_slots.get(index, 0),
                    'function': {'arguments': delta['partial_json']}}]}))
        elif kind == 'message_delta':
            delta = value.get('delta') or {}
            if isinstance(delta.get('stop_reason'), str):
                self.finish = STOP_REASONS.get(delta['stop_reason'], 'stop')
            if value.get('usage') is not None:
                self.raw_usage.update(_dump(value['usage']))
                self.usage = _usage(self.raw_usage)
        elif kind == 'message_stop':
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


def _error(self, st, item, status, message, key, code='cloud_upstream_error'):
    if st.led is not None:
        st.led.finish(status, code)
    body = {'error': {'message': cloud.redact(message, key), 'type': 'cloud_error',
                      'param': 'model', 'code': code}}
    return self.json_reply(status, body)


def _write(self, st, data):
    self.wfile.write(('%x\r\n' % len(data)).encode() + data + b'\r\n')
    self.wfile.flush()


def forward(self, st, item, key):
    """Run one Anthropic Messages request and write the OpenAI v1 reply."""
    payload = st.payload
    params = messages_request(payload, item)
    client = _client(item, key)
    include_usage = _include_usage(payload)
    streamed = _is_stream(payload)
    stream = None
    started = False
    try:
        if not streamed:
            message = client.messages.create(**params)
            reply = response(message, item)
            if st.led is not None:
                st.led.row.update({'remote': True, 'remote_provider': 'anthropic',
                                   'remote_model': item['upstream_model']})
                st.led.row.update({'prompt_tokens': reply['usage']['prompt_tokens'],
                                   'completion_tokens': reply['usage']['completion_tokens'],
                                   'cached_tokens': reply['usage']['prompt_tokens_details']['cached_tokens'],
                                   'cost_usd': cloud.cost(item, reply['usage']['prompt_tokens'],
                                                          reply['usage']['completion_tokens'])})
            data = json.dumps(reply, ensure_ascii=False).encode()
            self.reply(200, data, 'application/json')
            if st.led is not None:
                st.led.finish(200)
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
        translator = _Stream(item, include_usage)
        for event in stream:
            for chunk in translator.event(event):
                data = ('data: ' + json.dumps(chunk, ensure_ascii=False) + '\n\n').encode()
                _write(self, st, data)
        _write(self, st, b'data: [DONE]\n\n')
        self.wfile.write(b'0\r\n\r\n')
        self.wfile.flush()
        usage = translator.usage or _usage(None)
        if st.led is not None:
            st.led.row.update({'remote': True, 'remote_provider': 'anthropic',
                               'remote_model': item['upstream_model'],
                               'prompt_tokens': usage['prompt_tokens'],
                               'completion_tokens': usage['completion_tokens'],
                               'cached_tokens': usage['prompt_tokens_details']['cached_tokens'],
                               'cost_usd': cloud.cost(item, usage['prompt_tokens'], usage['completion_tokens'])})
            st.led.finish(200)
    except cloud.CloudError as error:
        if not started:
            _error(self, st, item, error.status, str(error), key, error.code)
        else:
            if st.led is not None:
                st.led.finish(error.status, error.code)
            self.close_connection = True
    except Exception as error:  # the SDK's own status/connection errors, redacted
        status = getattr(error, 'status_code', None) or 503
        if not isinstance(status, int) or not 400 <= status <= 599:
            status = 503
        message = cloud.redact(str(error), key)
        if not started:
            _error(self, st, item, status, message or 'The Anthropic provider is unavailable.', key)
        else:
            if st.led is not None:
                st.led.finish(status, 'cloud_stream_error')
            self.close_connection = True
    finally:
        try:
            if stream is not None:
                stream.close()
        except Exception:
            pass
