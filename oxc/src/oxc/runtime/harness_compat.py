"""Request compatibility for any OpenAI-compatible client.

Pure functions over the OpenAI chat body; no I/O. The controller applies them to requests
for the Rust engine (oxe) before `oxe_runtime.request_defaults`.

Every rewrite here is deterministic and depends only on the message it rewrites, so a
conversation that is re-sent with more turns renders to the same leading tokens as before
(the engine's prefix cache and the persistent prefix store need exactly that).

What is rewritten, and why (each case is a 400 from oxe-server or a silent slow path):
- `temperature: null` / `cache_prompt: null`: the engine reads a null as "absent" and falls
  back to its own default (temperature 0.8, which samples and switches speculative decoding
  off). Nulls are dropped so the controller defaults (0, true) apply.
- a system/developer message after the conversation started: the Qwen3.x/Ornith chat
  template raises "System message must be at the beginning". It becomes a user message
  wrapped in <system-reminder> tags.
- `tool_choice` as an object naming one function: the engine accepts only
  auto/none/required. It becomes `required` with the tool list cut down to that function,
  which keeps the meaning exactly (such requests miss the stored tools prefix; they are
  rare: titles, structured output).
- content parts the text-only engine rejects (image_url, input_audio, file): replaced by a
  short text note so the model knows something was left out.
"""
import copy

CHAT_PATHS = ('/chat/completions', '/v1/chat/completions')
# a null here means "absent" to the engine, which then uses its own default instead of the controller's
NULLABLE_KEYS = ('temperature', 'cache_prompt')
TEXT_PART_TYPES = ('text', 'media_marker', 'tool_reference')
REMINDER_OPEN, REMINDER_CLOSE = '<system-reminder>\n', '\n</system-reminder>'

def _demote_late_system(messages, notes):
    started = False
    out = []
    for message in messages:
        if not isinstance(message, dict):
            out.append(message)
            continue
        role = message.get('role')
        if role not in ('system', 'developer'):
            started = True
            out.append(message)
            continue
        if not started:
            out.append(message)
            continue
        content = message.get('content')
        if isinstance(content, list):
            content = ''.join(p.get('text', '') for p in content
                              if isinstance(p, dict) and isinstance(p.get('text'), str))
        if not isinstance(content, str):
            content = ''
        out.append({'role': 'user', 'content': REMINDER_OPEN + content + REMINDER_CLOSE})
        notes.append('late_system_message_to_user')
    return out


def _text_only_parts(messages, notes):
    out = []
    for message in messages:
        content = message.get('content') if isinstance(message, dict) else None
        if not isinstance(content, list):
            out.append(message)
            continue
        parts, changed = [], False
        for part in content:
            kind = part.get('type') if isinstance(part, dict) else None
            if kind in TEXT_PART_TYPES:
                parts.append(part)
            else:
                parts.append({'type': 'text', 'text': '[%s omitted: this local model reads text only]'
                              % (kind if isinstance(kind, str) and kind.isidentifier() else 'attachment')})
                changed = True
        if changed:
            notes.append('non_text_part_replaced')
            out.append({**message, 'content': parts})
        else:
            out.append(message)
    return out


def _named_tool_choice(payload, notes):
    choice = payload.get('tool_choice')
    if not isinstance(choice, dict):
        return
    name = choice.get('function', {}).get('name') if isinstance(choice.get('function'), dict) else choice.get('name')
    tools = payload.get('tools') if isinstance(payload.get('tools'), list) else []
    selected = [t for t in tools if isinstance(t, dict) and isinstance(t.get('function'), dict)
                and t['function'].get('name') == name]
    if isinstance(name, str) and selected:
        payload['tools'] = selected
        payload['tool_choice'] = 'required'
        notes.append('named_tool_choice_to_required')
    else:
        # nothing sensible to force: let the model choose rather than fail the request
        payload['tool_choice'] = 'auto'
        notes.append('unknown_tool_choice_to_auto')


def normalize(payload, path):
    """Return (payload, notes). The input is never modified; an unchanged request returns
    the same object and an empty list."""
    if not isinstance(payload, dict):
        return payload, []
    notes = []
    result = copy.copy(payload)
    for key in NULLABLE_KEYS:
        if key in result and result[key] is None:
            del result[key]
            notes.append('null_%s_dropped' % key)
    if path in CHAT_PATHS and isinstance(result.get('messages'), list):
        messages = _demote_late_system(result['messages'], notes)
        messages = _text_only_parts(messages, notes)
        result['messages'] = messages
        _named_tool_choice(result, notes)
    return (result, notes) if notes else (payload, [])
