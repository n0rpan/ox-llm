"""OpenAI API-key upstream contracts, independent of the local engine's request knobs.

Exact model ids bind endpoint and parameter support; unknown models fail closed instead
of discovering tool incompatibility in the middle of an agent loop. Transport, redaction,
SSE parsing and spend accounting belong to the shared cloud proxy.
"""
import collections
import hashlib
import json
import threading
import time

if __package__:
    from . import cloud
else:
    import cloud

# Chat Completions tool contracts. Deliberately no prefix matching: a new model or
# snapshot needs a reviewed contract. GPT-6 reasoning/tool workflows require Responses.
CHAT_MODELS = {
    'gpt-4.1': False, 'gpt-4.1-2025-04-14': False,
    'gpt-4.1-mini': False, 'gpt-4.1-mini-2025-04-14': False,
    'gpt-4.1-nano': False, 'gpt-4.1-nano-2025-04-14': False,
    'o3': True, 'o3-2025-04-16': True,
    'o4-mini': True, 'o4-mini-2025-04-16': True,
}
CHAT_FIELDS = ('messages', 'tools', 'tool_choice', 'parallel_tool_calls', 'stream',
               'response_format', 'temperature', 'top_p', 'frequency_penalty',
               'presence_penalty', 'logit_bias', 'logprobs', 'top_logprobs', 'stop', 'seed')
SAMPLING = ('temperature', 'top_p', 'frequency_penalty', 'presence_penalty',
            'logit_bias', 'logprobs', 'top_logprobs', 'stop', 'seed')
EFFORT = {'none': 'low', 'minimal': 'low', 'low': 'low', 'medium': 'medium',
          'high': 'high', 'xhigh': 'high'}


def _output_limit(payload, maximum):
    if type(maximum) is not int or maximum <= 0:
        raise cloud.CloudError('OpenAI models need a catalog max_output.', 400, 'invalid_value', 'model')
    limit = next((payload[field] for field in ('max_completion_tokens', 'max_tokens')
                  if payload.get(field) is not None), maximum)
    if type(limit) is not int or limit <= 0:
        raise cloud.CloudError('The output token limit must be a positive integer.',
                               400, 'invalid_value', 'max_completion_tokens')
    return min(limit, maximum)


def chat_request(payload, item):
    """A single-choice, non-stored Chat Completions call with a bounded output cap."""
    reasoning = CHAT_MODELS.get(item['upstream_model'])
    if reasoning is None:
        raise cloud.CloudError('This OpenAI model has no supported Chat Completions tool contract.',
                               400, 'unsupported_model', 'model')
    if payload.get('n') is not None and (type(payload['n']) is not int or payload['n'] != 1):
        raise cloud.CloudError('Cloud requests support one completion.', 400, 'invalid_value', 'n')
    body = {name: payload[name] for name in CHAT_FIELDS if name in payload}
    body.update(model=item['upstream_model'], store=False)
    body['max_completion_tokens'] = _output_limit(payload, item.get('max_output'))
    effort = payload.get('reasoning_effort')
    kwargs = payload.get('chat_template_kwargs')
    if effort is None and isinstance(kwargs, dict) and isinstance(kwargs.get('enable_thinking'), bool):
        effort = 'high' if kwargs['enable_thinking'] else 'none'
    if effort is not None and (not isinstance(effort, str) or effort not in EFFORT):
        raise cloud.CloudError('Unsupported reasoning effort.', 400, 'invalid_value', 'reasoning_effort')
    if reasoning:
        for name in SAMPLING:
            body.pop(name, None)
        if effort is not None:
            body['reasoning_effort'] = EFFORT[effort]
    if body.get('stream') is True:
        body['stream_options'] = {'include_usage': True}
    return body

# GPT-6 tool use always goes through Responses, including models that also support
# no-reasoning Chat Completions tools. Values state whether `none` is supported.
RESPONSES_MODELS = {'gpt-6-astra': False, 'gpt-6.1-sol': False,
                    'gpt-6.1-terra': True, 'gpt-6-sol': True, 'gpt-6-luna': True}


def endpoint(item):
    return '/responses' if item['upstream_model'] in RESPONSES_MODELS else '/chat/completions'


def _invalid(field):
    raise cloud.CloudError('Invalid OpenAI message or tool shape.', 400, 'invalid_value', field)


def validate(payload):
    """Validate structured inputs before continuation lookup, reservation or transport."""
    messages = payload.get('messages', [])
    if not isinstance(messages, list):
        _invalid('messages')
    for message in messages:
        if not isinstance(message, dict):
            _invalid('messages')
        role = message.get('role')
        if role not in ('system', 'developer', 'user', 'assistant', 'tool'):
            _invalid('messages')
        content = message.get('content')
        if content is None:
            if role != 'assistant':
                _invalid('messages')
        elif isinstance(content, list):
            for part in content:
                if not isinstance(part, dict):
                    _invalid('messages')
                if part.get('type') == 'text':
                    if not isinstance(part.get('text'), str):
                        _invalid('messages')
                elif part.get('type') == 'image_url' and role == 'user':
                    image = part.get('image_url')
                    if (not isinstance(image, dict) or not isinstance(image.get('url'), str)
                            or not image['url'] or image.get('detail', 'auto') not in ('auto', 'low', 'high')):
                        _invalid('messages')
                else:
                    _invalid('messages')
        elif not isinstance(content, str):
            _invalid('messages')
        if role == 'tool' and (not isinstance(message.get('tool_call_id'), str)
                               or not message['tool_call_id']):
            _invalid('messages')
        calls = message.get('tool_calls')
        if calls is not None:
            if role != 'assistant' or not isinstance(calls, list):
                _invalid('messages')
            for call in calls:
                if not isinstance(call, dict) or call.get('type') != 'function':
                    _invalid('messages')
                function = call.get('function')
                if (not isinstance(call.get('id'), str) or not call['id']
                        or not isinstance(function, dict) or not isinstance(function.get('name'), str)
                        or not function['name'] or not isinstance(function.get('arguments'), str)):
                    _invalid('messages')
    tools = payload.get('tools')
    if tools is not None:
        if not isinstance(tools, list):
            _invalid('tools')
        for tool in tools:
            if not isinstance(tool, dict) or tool.get('type') != 'function':
                _invalid('tools')
            function = tool.get('function')
            if (not isinstance(function, dict) or not isinstance(function.get('name'), str)
                    or not function['name'] or ('parameters' in function and not isinstance(function['parameters'], dict))
                    or (function.get('strict') is not None and type(function['strict']) is not bool)):
                _invalid('tools')
    choice = payload.get('tool_choice')
    if choice is not None:
        if isinstance(choice, dict):
            function = choice.get('function')
            if (choice.get('type') != 'function' or not isinstance(function, dict)
                    or not isinstance(function.get('name'), str) or not function['name']):
                _invalid('tool_choice')
        elif not isinstance(choice, str) or choice not in ('auto', 'none', 'required'):
            _invalid('tool_choice')


def request(payload, item, key=None):
    validate(payload)
    return responses_request(payload, item, key) if endpoint(item) == '/responses' else chat_request(payload, item)


# Encrypted reasoning must accompany tool outputs. Keep native output only in bounded,
# expiring process memory, scoped to upstream + credentials + the exact client history.
# It is never logged or persisted; an evicted/restarted continuation fails clearly.

_HISTORY = collections.OrderedDict()
_HISTORY_LOCK = threading.Lock()
_HISTORY_BYTES = 8 * 1024 * 1024
_HISTORY_ENTRIES = 128
_HISTORY_TTL = 1800


def _canonical(message):
    content = message.get('content')
    if (isinstance(content, list)
            and all(isinstance(part, dict) and part.get('type') == 'text' for part in content)):
        # Text arrays and plain text are equivalent; images and other typed content
        # must retain their full identity when scoping native reasoning continuations.
        content = ''.join(part.get('text', '') for part in content
                          if isinstance(part, dict) and part.get('type') == 'text')
    calls = []
    for call in message.get('tool_calls') or []:
        function = call.get('function') or {}
        args = function.get('arguments')
        if isinstance(args, str):
            try:
                args = json.loads(args)
            except ValueError:
                pass
        calls.append([call.get('id'), function.get('name'), args])
    return {'role': message.get('role'), 'content': content or None, 'calls': calls,
            'tool_call_id': message.get('tool_call_id')}


def _history_key(item, history, key):
    material = [item['base_url'], item['upstream_model'], key,
                [_canonical(message) for message in history]]
    return hashlib.sha256(json.dumps(material, sort_keys=True, ensure_ascii=False).encode()).digest()


def _prune():
    now = time.monotonic()
    for key in list(_HISTORY):
        if _HISTORY[key][0] < now:
            del _HISTORY[key]
    while _HISTORY and (len(_HISTORY) > _HISTORY_ENTRIES
                        or sum(len(row[1]) for row in _HISTORY.values()) > _HISTORY_BYTES):
        _HISTORY.popitem(last=False)


def remember(item, history, assistant, output, key):
    # The client sees redacted text, so canonicalize the same text for the next request.
    safe = cloud.redact_value(assistant, key)
    token = _history_key(item, list(history or []) + [safe], key)
    frozen = json.dumps(output, ensure_ascii=False).encode()
    with _HISTORY_LOCK:
        _HISTORY[token] = (time.monotonic() + _HISTORY_TTL, frozen)
        _HISTORY.move_to_end(token)
        _prune()


def _restore(item, history, key):
    token = _history_key(item, history, key)
    with _HISTORY_LOCK:
        _prune()
        row = _HISTORY.get(token)
        if row is not None:
            _HISTORY.move_to_end(token)
            return json.loads(row[1])
    return None


def responses_request(payload, item, key=None):
    limit = _output_limit(payload, item.get('max_output'))
    if payload.get('n') is not None and (type(payload['n']) is not int or payload['n'] != 1):
        raise cloud.CloudError('Cloud requests support one completion.', 400, 'invalid_value', 'n')
    effort = payload.get('reasoning_effort')
    kwargs = payload.get('chat_template_kwargs')
    if effort is None and isinstance(kwargs, dict) and isinstance(kwargs.get('enable_thinking'), bool):
        effort = 'high' if kwargs['enable_thinking'] else 'none'
    if effort is None:
        effort = 'medium'
    if not isinstance(effort, str) or effort not in EFFORT:
        raise cloud.CloudError('Unsupported reasoning effort.', 400, 'invalid_value', 'reasoning_effort')
    if effort == 'minimal' or (effort == 'none' and not RESPONSES_MODELS[item['upstream_model']]):
        effort = 'low'
    messages = payload.get('messages') or []
    inputs = []
    for index, message in enumerate(messages):
        role = message.get('role')
        if role == 'assistant':
            restored = _restore(item, messages[:index + 1], key)
            if restored is not None:
                inputs.extend(restored)
                continue
            if message.get('tool_calls'):
                raise cloud.CloudError('OpenAI tool continuation expired; start a new turn.',
                                       409, 'cloud_continuation_unavailable', 'messages')
        if role == 'tool':
            output = message.get('content') or ''
            if isinstance(output, list):
                output = ''.join(part['text'] for part in output)
            inputs.append({'type': 'function_call_output', 'call_id': message.get('tool_call_id'),
                           'output': output})
            continue
        content = message.get('content')
        if isinstance(content, str):
            inputs.append({'role': role, 'content': content})
        elif isinstance(content, list):
            parts = []
            for part in content:
                if part.get('type') == 'text':
                    parts.append({'type': 'output_text' if role == 'assistant' else 'input_text',
                                  'text': part.get('text', '')})
                elif part.get('type') == 'image_url' and role == 'user':
                    image = part.get('image_url') or {}
                    parts.append({'type': 'input_image', 'image_url': image.get('url'),
                                  'detail': image.get('detail', 'auto')})
                else:
                    raise cloud.CloudError('Unsupported OpenAI message content.', 400,
                                           'invalid_value', 'messages')
            inputs.append({'role': role, 'content': parts})
    body = {'model': item['upstream_model'], 'input': inputs, 'store': False,
            'include': ['reasoning.encrypted_content'], 'reasoning': {'effort': effort},
            'max_output_tokens': limit, 'stream': payload.get('stream') is True}
    tools = []
    for tool in payload.get('tools') or []:
        function = tool.get('function')
        if tool.get('type') != 'function' or not isinstance(function, dict):
            raise cloud.CloudError('Only function tools are supported.', 400, 'invalid_value', 'tools')
        tools.append({'type': 'function', **{name: function[name] for name in
                      ('name', 'description', 'parameters', 'strict') if name in function}})
        tools[-1].setdefault('strict', False)
    if tools:
        body['tools'] = tools
        choice = payload.get('tool_choice', 'auto')
        if isinstance(choice, dict):
            choice = {'type': 'function', 'name': (choice.get('function') or {}).get('name')}
        body['tool_choice'] = choice
        if 'parallel_tool_calls' in payload:
            body['parallel_tool_calls'] = payload['parallel_tool_calls']
    if effort == 'none':
        for name in ('temperature', 'top_p', 'top_logprobs'):
            if name in payload:
                body[name] = payload[name]
    fmt = payload.get('response_format')
    if isinstance(fmt, dict):
        if fmt.get('type') == 'json_schema':
            body['text'] = {'format': {'type': 'json_schema', **fmt.get('json_schema', {})}}
        elif fmt.get('type') in ('text', 'json_object'):
            body['text'] = {'format': fmt}
    return body


def _usage(value):
    usage = value.get('usage')
    if not isinstance(usage, dict):
        return None
    # A partial or malformed usage object must not under-count spend.
    if any(type(usage.get(name)) is not int or usage[name] < 0
           for name in ('input_tokens', 'output_tokens')):
        return None
    return {'prompt_tokens': usage['input_tokens'], 'completion_tokens': usage['output_tokens'],
            'total_tokens': usage['input_tokens'] + usage['output_tokens'],
            'prompt_tokens_details': usage.get('input_tokens_details') or {'cached_tokens': 0},
            'completion_tokens_details': usage.get('output_tokens_details') or {'reasoning_tokens': 0}}


def response(value, item, history, key):
    try:
        return _response(value, item, history, key)
    except (ValueError, KeyError, TypeError, AttributeError):
        raise cloud.CloudError('Invalid OpenAI response.', 502, 'cloud_upstream_error') from None


def _response(value, item, history, key):
    if value.get('status') not in ('completed', 'incomplete'):
        raise cloud.CloudError('The OpenAI response did not complete.', 502, 'cloud_upstream_error')
    output = value.get('output') or []
    texts, calls, refusals, reasoning = [], [], [], []
    for part in output:
        if part.get('type') == 'message':
            for block in part.get('content') or []:
                if block.get('type') == 'output_text':
                    texts.append(block.get('text', ''))
                elif block.get('type') == 'refusal':
                    refusals.append(block.get('refusal', ''))
        elif part.get('type') == 'function_call':
            calls.append({'id': part['call_id'], 'type': 'function', 'function': {
                'name': part['name'], 'arguments': part['arguments']}})
        elif part.get('type') == 'reasoning':
            reasoning.extend(block.get('text', '') for block in part.get('summary') or [])
    assistant = {'role': 'assistant', 'content': ''.join(texts) or None}
    if calls:
        assistant['tool_calls'] = calls
    if refusals:
        assistant['refusal'] = ''.join(refusals)
    if reasoning:
        assistant['reasoning_content'] = ''.join(reasoning)
    finish = 'tool_calls' if calls else 'stop'
    if value.get('status') == 'incomplete':
        finish = 'length' if (value.get('incomplete_details') or {}).get('reason') == 'max_output_tokens' else 'content_filter'
    else:
        remember(item, history, assistant, output, key)
    result = {'id': value.get('id', 'chatcmpl_openai'), 'object': 'chat.completion',
              'created': value.get('created_at', int(time.time())), 'model': item['id'],
              'choices': [{'index': 0, 'message': assistant, 'finish_reason': finish}]}
    usage = _usage(value)
    if usage is not None:
        result['usage'] = usage
    return result


class ResponsesStream:
    """Responses SSE -> Chat Completions SSE; usage is trusted only at a terminal event."""

    def __init__(self, item, history, key):
        self.item, self.history, self.key = item, history, key
        self.parser = cloud.SSEDecoder()
        self.slots = {}
        self.done = False
        self.failed = False

    def _wire(self, delta, finish=None, usage=None):
        value = {'id': 'chatcmpl_openai', 'object': 'chat.completion.chunk',
                 'created': int(time.time()), 'model': self.item['id'],
                 'choices': [{'index': 0, 'delta': delta, 'finish_reason': finish}]}
        if usage is not None:
            value.update(choices=[], usage=usage)
        return b'data: ' + json.dumps(value, ensure_ascii=False).encode() + b'\n\n'

    def _event(self, data):
        try:
            return self._translate(json.loads(data))
        except (ValueError, KeyError, TypeError, AttributeError):
            raise cloud.CloudError('Invalid OpenAI stream event.', 502, 'cloud_stream_error') from None

    def _translate(self, value):
        kind = value.get('type')
        if self.done:
            return b''
        if kind == 'response.created':
            return self._wire({'role': 'assistant', 'content': ''})
        if kind == 'response.output_item.added':
            part = value.get('item') or {}
            if part.get('type') == 'function_call':
                slot = len(self.slots)
                self.slots[value['output_index']] = slot
                return self._wire({'tool_calls': [{'index': slot, 'id': part['call_id'],
                    'type': 'function', 'function': {'name': part['name'], 'arguments': ''}}]})
        if kind == 'response.function_call_arguments.delta':
            return self._wire({'tool_calls': [{'index': self.slots[value['output_index']],
                                              'function': {'arguments': value['delta']}}]})
        if kind in ('response.output_text.delta', 'response.refusal.delta',
                    'response.reasoning_summary_text.delta'):
            field = {'response.output_text.delta': 'content', 'response.refusal.delta': 'refusal',
                     'response.reasoning_summary_text.delta': 'reasoning_content'}[kind]
            return self._wire({field: value['delta']})
        if kind in ('response.completed', 'response.incomplete'):
            reply = response(value['response'], self.item, self.history, self.key)
            self.done = True
            finish = reply['choices'][0]['finish_reason']
            out = self._wire({}, finish)
            if 'usage' in reply:
                out += self._wire({}, usage=reply['usage'])
            return out + b'data: [DONE]\n\n'
        if kind in ('response.failed', 'error'):
            self.failed = True
            raise cloud.CloudError('The OpenAI stream failed.', 502, 'cloud_stream_error')
        return b''

    def feed(self, data):
        return b''.join(self._event(raw) for _, raw in self.parser.feed(data))

    def finish(self):
        out = b''.join(self._event(raw) for _, raw in self.parser.finish())
        if not self.done:
            self.failed = True
            raise cloud.CloudError('The OpenAI stream ended before completion.', 502, 'cloud_stream_error')
        return out
