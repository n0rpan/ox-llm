"""v0.2.0: a minimal, stateless `POST /v1/responses` over the chat engine (OpenAI Responses API).

Pure functions and a stream translator; no I/O and nothing is stored. The controller translates
the request into a chat completion for the Rust engine (oxe), and the engine's reply (JSON or SSE)
back into a Response object or Response stream events. One behaviour for every client (O-133).

Request (supported subset):
- `model`, `instructions` (a system message first), `input`: a string (one user message) or a list
  of items: messages (`{"role", "content"}`, with or without `"type": "message"`; roles user,
  system, developer, assistant; content a string or parts `input_text` / `output_text` / `text` /
  `refusal`, and `input_image` / `input_file` / `input_audio`, which the text-only model gets as a
  short note), `function_call` + `function_call_output` (tool round trips), `custom_tool_call` +
  `custom_tool_call_output`, and `reasoning` items (skipped: the engine keeps its own reasoning).
- `tools`: `function` tools; `custom` tools (free-form input) become a function with one string
  parameter `input` and come back as `custom_tool_call` items. Any other tool type: 400.
- `tool_choice` (auto / none / required / a named function), `parallel_tool_calls`, `temperature`,
  `top_p`, `max_output_tokens` (else the model's default output limit), `stream`,
  `reasoning.effort` (the chat `reasoning_effort` mapping), `text.format` (text / json_object /
  json_schema: the chat `response_format` rules), `prompt_cache_key`, `seed`, `stop`.
- Accepted and ignored: `store` (nothing is ever stored; the reply says `"store": false`),
  `metadata` (echoed), `user`, `safety_identifier`, `include`, `truncation`, `service_tier`,
  `stream_options`, `text.verbosity`, `reasoning.summary`.
- Refused with a 400: `previous_response_id`, `conversation`, `background: true`, `prompt`
  (stored prompts), `item_reference` items: they need server-side state.

Reply: `output` items in order: `reasoning` (when the model reasoned), the assistant `message`
(`output_text`), then the `function_call` / `custom_tool_call` items. `status` is `completed`, or
`incomplete` with `incomplete_details.reason = "max_output_tokens"` when the output limit cut it.
Usage: `input_tokens`, `input_tokens_details.cached_tokens`, `output_tokens`,
`output_tokens_details.reasoning_tokens`, `total_tokens`.

Stream: `response.created`, `response.in_progress`, then per item `response.output_item.added`,
its deltas (`response.reasoning_text.delta`, `response.content_part.added` +
`response.output_text.delta`, `response.function_call_arguments.delta`,
`response.custom_tool_call_input.delta`) and their `.done` events, `response.output_item.done`,
and last `response.completed` (or `response.incomplete`) with the whole Response. Every event has
`type` and `sequence_number` and is sent as `event: <type>` + `data: <json>`. An engine error in
the stream becomes an `error` event followed by `response.failed`. SSE comment lines (`: queued`,
`: prefill`) pass through unchanged.
"""
import json
import secrets
import time

if __package__:
    from .openai_api import BadParam
    from . import openai_api, openai_conformance
else:
    from openai_api import BadParam
    import openai_api
    import openai_conformance

STATEFUL = {'previous_response_id': 'previous_response_id', 'conversation': 'conversation', 'prompt': 'prompt'}
TEXT_PARTS = ('input_text', 'output_text', 'text', 'summary_text', 'reasoning_text')
MEDIA_PARTS = ('input_image', 'input_file', 'input_audio')
CUSTOM_PARAMETERS = {'type': 'object', 'properties': {'input': {'type': 'string', 'description': 'The raw input for the tool.'}},
                     'required': ['input']}
PASS_FIELDS = ('temperature', 'top_p', 'parallel_tool_calls', 'prompt_cache_key', 'seed', 'stop', 'top_logprobs')


def _new_id(prefix):
    return prefix + '_' + secrets.token_hex(12)


def _bad(message, param=None, code='unsupported_value'):
    return BadParam(message, param, code)


def _text_of(content, param):
    """Chat content for Responses message content (a string or a list of parts)."""
    if content is None:
        return ''
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        raise _bad('Message "content" must be a string or a list of parts.', param, 'invalid_value')
    parts = []
    for part in content:
        kind = part.get('type') if isinstance(part, dict) else None
        if kind in TEXT_PARTS and isinstance(part.get('text'), str):
            parts.append(part['text'])
        elif kind == 'refusal' and isinstance(part.get('refusal'), str):
            parts.append(part['refusal'])
        elif kind in MEDIA_PARTS:
            parts.append('[%s omitted: this local model reads text only]' % kind[len('input_'):])
        else:
            raise _bad('Unsupported content part type %r.' % str(kind)[:40], param, 'invalid_value')
    return '\n'.join(parts)


def _output_text(output):
    if isinstance(output, str):
        return output
    if isinstance(output, list):
        return _text_of(output, 'input')
    return json.dumps(output, ensure_ascii=False)


def _messages(payload):
    messages = []
    instructions = payload.get('instructions')
    if instructions is not None:
        if not isinstance(instructions, str):
            raise _bad('"instructions" must be a string.', 'instructions', 'invalid_value')
        if instructions:
            messages.append({'role': 'system', 'content': instructions})
    items = payload.get('input')
    if items is None:
        items = []
    if isinstance(items, str):
        items = [{'role': 'user', 'content': items}]
    if not isinstance(items, list):
        raise _bad('"input" must be a string or a list of items.', 'input', 'invalid_value')
    pending = None  # the assistant message tool calls are added to
    for index, item in enumerate(items):
        param = 'input[%d]' % index
        if not isinstance(item, dict):
            raise _bad('Each input item must be an object.', param, 'invalid_value')
        kind = item.get('type', 'message' if 'role' in item else None)
        if kind == 'message':
            role = item.get('role')
            if role not in ('user', 'system', 'developer', 'assistant'):
                raise _bad('Unsupported message role %r.' % str(role)[:20], param + '.role', 'invalid_value')
            text = _text_of(item.get('content'), param + '.content')
            if role == 'assistant':
                pending = {'role': 'assistant', 'content': text}
                messages.append(pending)
            else:
                pending = None
                messages.append({'role': role, 'content': text})
        elif kind in ('function_call', 'custom_tool_call'):
            name, call_id = item.get('name'), item.get('call_id')
            if not isinstance(name, str) or not isinstance(call_id, str):
                raise _bad('A %s item needs "name" and "call_id".' % kind, param, 'invalid_value')
            if kind == 'function_call':
                arguments = item.get('arguments', '')
                arguments = arguments if isinstance(arguments, str) else json.dumps(arguments, ensure_ascii=False)
            else:
                arguments = json.dumps({'input': item.get('input', '') if isinstance(item.get('input'), str) else ''},
                                       ensure_ascii=False)
            call = {'id': call_id, 'type': 'function', 'function': {'name': name, 'arguments': arguments}}
            if pending is None:
                pending = {'role': 'assistant', 'content': ''}
                messages.append(pending)
            pending.setdefault('tool_calls', []).append(call)
        elif kind in ('function_call_output', 'custom_tool_call_output'):
            call_id = item.get('call_id')
            if not isinstance(call_id, str):
                raise _bad('A %s item needs "call_id".' % kind, param, 'invalid_value')
            messages.append({'role': 'tool', 'tool_call_id': call_id, 'content': _output_text(item.get('output', ''))})
            pending = None
        elif kind == 'reasoning':
            continue  # the engine keeps its own reasoning (preserve_thinking / ECHO)
        elif kind == 'item_reference':
            raise _bad('"item_reference" input items need stored responses; this local server is stateless. '
                       'Send the items themselves.', param)
        else:
            raise _bad('Unsupported input item type %r.' % str(kind)[:40], param)
    if not any(m.get('role') != 'system' for m in messages):
        raise _bad('"input" must contain at least one item.', 'input', 'invalid_value')
    return messages


def _tools(payload):
    tools = payload.get('tools')
    if tools is None:
        return None, set()
    if not isinstance(tools, list):
        raise _bad('"tools" must be a list.', 'tools', 'invalid_value')
    out, custom = [], set()
    for index, tool in enumerate(tools):
        kind = tool.get('type') if isinstance(tool, dict) else None
        name = tool.get('name') if isinstance(tool, dict) else None
        if kind == 'function' and isinstance(name, str):
            function = {'name': name, 'parameters': tool.get('parameters') or {'type': 'object', 'properties': {}}}
            if isinstance(tool.get('description'), str):
                function['description'] = tool['description']
            out.append({'type': 'function', 'function': function})
        elif kind == 'custom' and isinstance(name, str):
            function = {'name': name, 'parameters': CUSTOM_PARAMETERS}
            if isinstance(tool.get('description'), str):
                function['description'] = tool['description']
            out.append({'type': 'function', 'function': function})
            custom.add(name)
        else:
            raise _bad('Tool type %r is not supported by this local server (function and custom tools only).'
                       % str(kind)[:40], 'tools[%d].type' % index)
    return out, custom


def _tool_choice(choice):
    if choice is None or choice in ('auto', 'none', 'required'):
        return choice
    if isinstance(choice, dict) and choice.get('type') in ('function', 'custom') and isinstance(choice.get('name'), str):
        return {'type': 'function', 'function': {'name': choice['name']}}
    return 'auto'  # allowed_tools / hosted tools: let the model choose


def _text_format(payload):
    text = payload.get('text')
    fmt = text.get('format') if isinstance(text, dict) else None
    if not isinstance(fmt, dict) or fmt.get('type') in (None, 'text'):
        return None
    if fmt.get('type') == 'json_object':
        return {'type': 'json_object'}
    if fmt.get('type') == 'json_schema':
        return {'type': 'json_schema', 'json_schema': {k: fmt[k] for k in ('name', 'schema', 'strict', 'description') if k in fmt}}
    raise _bad('Unknown "text.format" type %r.' % str(fmt.get('type'))[:40], 'text.format.type', 'invalid_value')


def chat_request(payload):
    """(chat body, meta) for a Responses request. Raises BadParam (400)."""
    if not isinstance(payload, dict):
        raise _bad('The request body must be a JSON object.', None, 'invalid_value')
    for key, param in STATEFUL.items():
        if payload.get(key) is not None:
            raise _bad('"%s" is not supported: this local server is stateless and stores no responses or '
                       'conversations. Send the whole conversation in "input".' % key, param)
    if payload.get('background') is True:
        raise _bad('"background" responses are not supported by this local server.', 'background')
    tools, custom = _tools(payload)
    chat = {'messages': _messages(payload)}
    if 'model' in payload:
        chat['model'] = payload['model']
    if tools:
        chat['tools'] = tools
        if payload.get('tool_choice') is not None:
            chat['tool_choice'] = _tool_choice(payload['tool_choice'])
    for key in PASS_FIELDS:
        if payload.get(key) is not None:
            chat[key] = payload[key]
    limit = payload.get('max_output_tokens')
    if limit is not None:
        if type(limit) is not int or limit < 1:
            raise _bad('"max_output_tokens" must be a positive integer.', 'max_output_tokens', 'invalid_value')
        chat['max_completion_tokens'] = limit
    reasoning = payload.get('reasoning')
    if isinstance(reasoning, dict) and reasoning.get('effort') is not None:
        chat['reasoning_effort'] = reasoning['effort']
    fmt = _text_format(payload)
    if fmt is not None:
        chat['response_format'] = fmt
    stream = payload.get('stream') is True
    if stream:
        chat['stream'] = True
        chat['stream_options'] = {'include_usage': True}
    meta = {'id': _new_id('resp'), 'created_at': int(time.time()), 'model': payload.get('model'),
            'stream': stream, 'custom': custom, 'request': payload}
    return chat, meta


# ------------------------------------------------------------------ replies
def _response(meta, status='in_progress', output=None, usage=None, incomplete=None, error=None):
    request = meta['request']
    reasoning = request.get('reasoning') if isinstance(request.get('reasoning'), dict) else {}
    text = request.get('text') if isinstance(request.get('text'), dict) else {}
    fmt = text.get('format') if isinstance(text.get('format'), dict) else {'type': 'text'}
    value = {
        'id': meta['id'], 'object': 'response', 'created_at': meta['created_at'], 'status': status,
        'background': False, 'error': error, 'incomplete_details': incomplete,
        'instructions': request.get('instructions') if isinstance(request.get('instructions'), str) else None,
        'max_output_tokens': request.get('max_output_tokens') if type(request.get('max_output_tokens')) is int else None,
        'model': meta.get('model') or '', 'output': output or [],
        'parallel_tool_calls': request.get('parallel_tool_calls') is not False,
        'previous_response_id': None,
        'reasoning': {'effort': reasoning.get('effort'), 'summary': None},
        'store': False, 'temperature': request.get('temperature', 0),
        'text': {'format': fmt},
        'tool_choice': request.get('tool_choice') if request.get('tool_choice') is not None else 'auto',
        'tools': request.get('tools') if isinstance(request.get('tools'), list) else [],
        'top_p': request.get('top_p', 1), 'truncation': 'disabled',
        'usage': usage, 'metadata': request.get('metadata') if isinstance(request.get('metadata'), dict) else {},
    }
    if status == 'completed':
        value['completed_at'] = int(time.time())
    return value


def _usage(chat_usage, timings=None, reasoning_tokens=0):
    usage = openai_conformance.complete_usage(chat_usage if isinstance(chat_usage, dict) else {}, reasoning_tokens, timings)
    prompt = usage.get('prompt_tokens') if type(usage.get('prompt_tokens')) is int else 0
    completion = usage.get('completion_tokens') if type(usage.get('completion_tokens')) is int else 0
    return {'input_tokens': prompt,
            'input_tokens_details': {'cached_tokens': usage['prompt_tokens_details']['cached_tokens']},
            'output_tokens': completion,
            'output_tokens_details': {'reasoning_tokens': usage['completion_tokens_details']['reasoning_tokens']},
            'total_tokens': prompt + completion}


def _reasoning_item(item_id, text, status='completed'):
    return {'id': item_id, 'type': 'reasoning', 'summary': [], 'status': status,
            'content': [{'type': 'reasoning_text', 'text': text}]}


def _message_item(item_id, text, status='completed'):
    content = [{'type': 'output_text', 'text': text, 'annotations': [], 'logprobs': []}] if text is not None else []
    return {'id': item_id, 'type': 'message', 'status': status, 'role': 'assistant', 'content': content}


def _call_item(item_id, call_id, name, arguments, custom, status='completed'):
    if name in custom:
        try:
            value = json.loads(arguments) if arguments else {}
            text = value.get('input') if isinstance(value, dict) and isinstance(value.get('input'), str) else arguments
        except ValueError:
            text = arguments
        return {'id': item_id, 'type': 'custom_tool_call', 'status': status, 'call_id': call_id, 'name': name, 'input': text}
    return {'id': item_id, 'type': 'function_call', 'status': status, 'call_id': call_id, 'name': name, 'arguments': arguments}


def _status(finish):
    if finish == 'length':
        return 'incomplete', {'reason': 'max_output_tokens'}
    return 'completed', None


def response_body(chat, meta):
    """A non-streamed chat completion (parsed) -> a Response object."""
    if not isinstance(chat, dict) or not isinstance(chat.get('choices'), list):
        return chat  # an error object: already in the OpenAI shape
    choice = chat['choices'][0] if chat['choices'] and isinstance(chat['choices'][0], dict) else {}
    message = choice.get('message') if isinstance(choice.get('message'), dict) else {}
    output = []
    reasoning = message.get('reasoning_content')
    if isinstance(reasoning, str) and reasoning:
        output.append(_reasoning_item(_new_id('rs'), reasoning))
    content = message.get('content')
    calls = [c for c in message.get('tool_calls') or () if isinstance(c, dict) and isinstance(c.get('function'), dict)]
    if (isinstance(content, str) and content) or not calls:
        output.append(_message_item(_new_id('msg'), content if isinstance(content, str) else ''))
    for call in calls:
        function = call['function']
        output.append(_call_item(_new_id('fc'), call.get('id') or _new_id('call'), function.get('name') or '',
                                 function.get('arguments') or '', meta['custom']))
    status, incomplete = _status(choice.get('finish_reason'))
    usage = _usage(chat.get('usage'), chat.get('timings'),
                   openai_conformance.reasoning_estimate(message, (chat.get('usage') or {}).get('completion_tokens')))
    if chat.get('model'):
        meta = {**meta, 'model': chat['model']}
    return _response(meta, status, output, usage, incomplete)


class ResponseStream:
    """Chat completion SSE -> Responses SSE events."""

    def __init__(self, meta):
        self.meta = meta
        self.buffer = b''
        self.seq = 0
        self.started = False
        self.closed = False
        self.done = {}          # output_index -> finished item (the index announced at output_item.added)
        self.next_index = 0     # each output item gets one id and one index, at its first announcement
        self.current = None     # {'kind', 'id', 'index', 'text', ...} of the open item
        self.calls = {}         # chat tool-call index -> open call state
        self.call_order = []
        self.finish_reason = None
        self.usage = None
        self.timings = None
        self.reasoning_deltas = 0

    # -- framing
    def _event(self, kind, **fields):
        value = {'type': kind, 'sequence_number': self.seq, **fields}
        self.seq += 1
        return b'event: ' + kind.encode() + b'\ndata: ' + json.dumps(value, ensure_ascii=False).encode() + b'\n\n'

    def _start(self):
        if self.started:
            return b''
        self.started = True
        response = _response(self.meta)
        return self._event('response.created', response=response) + self._event('response.in_progress', response=response)

    # -- items
    def _allocate(self):
        index, self.next_index = self.next_index, self.next_index + 1
        return index

    def _output(self):
        """The finished items in the order of their announced output_index."""
        return [self.done[index] for index in sorted(self.done)]

    def _close_current(self, status='completed'):
        item, out = self.current, b''
        if item is None:
            return out
        self.current = None
        index = item['index']
        if item['kind'] == 'reasoning':
            out += self._event('response.reasoning_text.done', item_id=item['id'], output_index=index, content_index=0, text=item['text'])
            done = _reasoning_item(item['id'], item['text'], status)
        else:
            part = {'type': 'output_text', 'text': item['text'], 'annotations': [], 'logprobs': []}
            out += self._event('response.output_text.done', item_id=item['id'], output_index=index, content_index=0,
                               text=item['text'], logprobs=[])
            out += self._event('response.content_part.done', item_id=item['id'], output_index=index, content_index=0, part=part)
            done = _message_item(item['id'], item['text'], status)
        self.done[index] = done
        return out + self._event('response.output_item.done', output_index=index, item=done)

    def _open(self, kind):
        out = self._close_current()  # calls stay open: their fragments may still arrive (interleaved streams)
        index = self._allocate()
        item = {'kind': kind, 'id': _new_id('rs' if kind == 'reasoning' else 'msg'), 'index': index, 'text': ''}
        self.current = item
        if kind == 'reasoning':
            out += self._event('response.output_item.added', output_index=index, item=_reasoning_item(item['id'], '', 'in_progress'))
            return out
        added = _message_item(item['id'], None, 'in_progress')
        out += self._event('response.output_item.added', output_index=index, item=added)
        out += self._event('response.content_part.added', item_id=item['id'], output_index=index, content_index=0,
                           part={'type': 'output_text', 'text': '', 'annotations': [], 'logprobs': []})
        return out

    def _text(self, kind, text):
        out = b''
        if self.current is None or self.current['kind'] != kind:
            out += self._open(kind)
        item = self.current
        item['text'] += text
        if kind == 'reasoning':
            self.reasoning_deltas += 1
            return out + self._event('response.reasoning_text.delta', item_id=item['id'], output_index=item['index'],
                                     content_index=0, delta=text)
        return out + self._event('response.output_text.delta', item_id=item['id'], output_index=item['index'],
                                 content_index=0, delta=text, logprobs=[])

    def _tool_delta(self, delta):
        out = b''
        index = delta.get('index', 0)  # a missing index continues slot 0, as before
        if type(index) is not int or index < 0:  # JSON can put anything here
            index = 0
        state = self.calls.get(index)
        function = delta.get('function') if isinstance(delta.get('function'), dict) else {}
        if state is None:
            out += self._close_current()
            state = {'id': _new_id('fc'), 'call_id': delta.get('id') or _new_id('call'), 'name': function.get('name') or '',
                     'arguments': '', 'index': self._allocate()}
            self.calls[index] = state
            self.call_order.append(index)
            out += self._event('response.output_item.added', output_index=state['index'],
                               item=_call_item(state['id'], state['call_id'], state['name'], '', self.meta['custom'], 'in_progress'))
        elif function.get('name') and not state['name']:
            state['name'] = function['name']
        piece = function.get('arguments')
        if isinstance(piece, str) and piece:
            state['arguments'] += piece
            if state['name'] not in self.meta['custom']:
                out += self._event('response.function_call_arguments.delta', item_id=state['id'],
                                   output_index=state['index'], delta=piece)
        return out

    def _close_calls(self, status='completed'):
        out = b''
        for index in self.call_order:
            state = self.calls[index]
            item = _call_item(state['id'], state['call_id'], state['name'], state['arguments'], self.meta['custom'], status)
            if item['type'] == 'custom_tool_call':
                if item['input']:
                    out += self._event('response.custom_tool_call_input.delta', item_id=state['id'],
                                       output_index=state['index'], delta=item['input'])
                out += self._event('response.custom_tool_call_input.done', item_id=state['id'],
                                   output_index=state['index'], input=item['input'])
            else:
                out += self._event('response.function_call_arguments.done', item_id=state['id'], output_index=state['index'],
                                   name=state['name'], arguments=state['arguments'])
            out += self._event('response.output_item.done', output_index=state['index'], item=item)
            self.done[state['index']] = item
        self.call_order = []
        return out

    def _complete(self):
        if self.closed:
            return b''
        self.closed = True
        out = self._start()
        if not self.done and self.current is None and not self.call_order:
            out += self._open('message')  # an empty reply: one empty message item, as the non-streamed body has
        out += self._close_current() + self._close_calls()
        status, incomplete = _status(self.finish_reason)
        usage = _usage(self.usage, self.timings, self.reasoning_deltas) if self.usage is not None else None
        response = _response(self.meta, status, self._output(), usage, incomplete)
        return out + self._event('response.completed' if status == 'completed' else 'response.incomplete', response=response)

    def _fail(self, error):
        if self.closed:
            return b''
        self.closed = True
        error = error if isinstance(error, dict) else {'message': str(error)}
        code = error.get('code') if isinstance(error.get('code'), str) else 'server_error'
        message = str(error.get('message', 'Local backend error.'))
        out = self._start()
        # the items announced so far end as `incomplete`; the failed response carries them
        out += self._close_current('incomplete') + self._close_calls('incomplete')
        out += self._event('error', code=code, message=message, param=error.get('param'))
        response = _response(self.meta, 'failed', self._output(), None, None, {'code': code, 'message': message})
        return out + self._event('response.failed', response=response)

    # -- input
    def _chunk(self, value):
        if isinstance(value.get('error'), (dict, str)):
            return self._fail(value['error'])
        out = self._start()
        if value.get('model') and not self.meta.get('model'):
            self.meta['model'] = value['model']
        if isinstance(value.get('usage'), dict):
            self.usage = value['usage']
        if isinstance(value.get('timings'), dict):
            self.timings = value['timings']
        choices = value.get('choices')
        for choice in choices if isinstance(choices, list) else ():
            if not isinstance(choice, dict):
                continue
            delta = choice.get('delta') if isinstance(choice.get('delta'), dict) else {}
            if isinstance(delta.get('reasoning_content'), str) and delta['reasoning_content']:
                out += self._text('reasoning', delta['reasoning_content'])
            if isinstance(delta.get('content'), str) and delta['content']:
                out += self._text('message', delta['content'])
            for call in delta.get('tool_calls') or ():
                if isinstance(call, dict):
                    out += self._tool_delta(call)
            if choice.get('finish_reason'):
                self.finish_reason = choice['finish_reason']
        return out

    def _translate(self, raw):
        if not raw.strip():
            return b''
        if raw.startswith(b':'):
            return raw.rstrip(b'\r\n') + b'\n\n'  # comments (`: queued`, `: prefill`), one blank line
        lines = raw.replace(b'\r\n', b'\n').replace(b'\r', b'\n').split(b'\n')
        data = b''.join(line[5:].strip() for line in lines if line.startswith(b'data:'))
        is_error = any(line.strip() == b'event: error' for line in lines)
        if not data:
            return b''
        if data == b'[DONE]':
            return self._complete()
        try:
            value = json.loads(data)
        except ValueError:
            return b''
        if not isinstance(value, dict):
            return b''
        if is_error and 'error' not in value:
            value = {'error': value}
        return self._chunk(value)

    def feed(self, chunk):
        self.buffer += chunk
        out = b''
        while True:
            raw, self.buffer = openai_api.next_sse_frame(self.buffer)
            if raw is None:
                if len(self.buffer) > openai_api.MAX_SSE_FRAME:
                    self.buffer = b''
                    return out + self._fail({'message': 'The backend sent an SSE frame above the size limit.',
                                             'code': 'stream_too_large'})
                break
            out += self._translate(raw)
        return out

    def finish(self):
        out = self._translate(self.buffer) if self.buffer.strip() else b''
        self.buffer = b''
        if not self.closed and self.finish_reason is not None:
            out += self._complete()  # an engine that ended without [DONE]
        return out


class ResponsesShim:
    """The reply side for /v1/responses (ctl_proxy_stream): JSON body or stream translator."""

    def __init__(self, meta, chat_meta):
        self.meta = meta
        self.chat_meta = chat_meta
        self.stream = meta['stream']

    def body(self, data):
        try:
            value = json.loads(data)
        except ValueError:
            return data
        if isinstance(value, dict) and isinstance(value.get('choices'), list):
            value = openai_conformance.chat_response(value, self.chat_meta)
        return json.dumps(response_body(value, self.meta), ensure_ascii=False).encode()

    def streamer(self):
        return ResponseStream(self.meta)

    def headers(self):
        return self.chat_meta.headers()
