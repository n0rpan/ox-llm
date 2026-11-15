"""Turn labels for chat requests to the Rust engine. Structural only: no client's wording is matched.

Pure functions over a chat payload and the request headers. Output is enum labels and short hashes
only: no prompt text leaves this module.

- `turn_kind`: the client's own label (hint H5, `X-Oxe-Turn-Kind`, see `client_turn_kind`), or
  None when the client sends none. The core never guesses it from message text.
- `tools_hash`: 12 hex of the tool list (unkeyed here; the ledger keys it with its install salt), so
  a request that drops or changes the tools (the whole prefix then misses) is told apart from a
  real miss.
- `fold_suspect`: hook point for the structural fold diagnostic (core-robust lane); None here.
- `session_hint`: the value forwarded to the engine as `X-Oxe-Session`. A hint only: the engine
  finds its echo records by token chain, never by this id.
- `client_turn_kind`: v0.2.0 H5. A client's own `X-Oxe-Turn-Kind` is a free label (`chat`,
  `compaction`, `branch_summary`, `prefill`, `subagent`, ...): it is passed to the engine and the ledger unchanged (when it is 1-32 characters of `[A-Za-z0-9_.-]`;
  anything else is no label). The engine uses it for its ledger only; it places no checkpoint.
- `snapshot_keep`: v0.2.0 H4. A client's `X-Oxe-Snapshot: keep` is the one client `X-Oxe-*` header
  the controller passes on (as exactly `keep`): it only asks the engine to retain a checkpoint at
  the end of the request's last message (at most 2 per session, reported as `timings.snap_pos`).
  It never changes the reply. H6 (`max_tokens: 0`, prefill only) is a body field and passes as is.

A history rewrite (summary, truncation) is recognised from the ledger's message hashes
(`history_change`, cache_miss class `history-rewrite`), not here.
"""
import hashlib
import json
import re

TURN_KINDS = ('chat', 'compaction', 'post-compaction')
SESSION_HEADER = 'X-Oxe-Session'
TURN_KIND_HEADER = 'X-Oxe-Turn-Kind'
SNAPSHOT_HEADER = 'X-Oxe-Snapshot'


def _short(text):
    return hashlib.sha256(text.encode('utf-8', 'replace')).hexdigest()[:12]


def tools_hash(payload):
    tools = payload.get('tools') if isinstance(payload, dict) else None
    if not isinstance(tools, list) or not tools:
        return None
    return _short(json.dumps(tools, sort_keys=True, separators=(',', ':'), ensure_ascii=False))


def label(payload, headers=None):
    """{'turn_kind', 'tools_hash', 'fold_suspect'} for a chat payload."""
    return {'turn_kind': client_turn_kind(headers), 'tools_hash': tools_hash(payload), 'fold_suspect': None}


def valid_label(value):
    """A turn label as it may reach the engine and the ledger (H5): 1-32 chars of [A-Za-z0-9_.-]."""
    return isinstance(value, str) and re.fullmatch(r'[A-Za-z0-9_.-]{1,32}', value) is not None


def client_turn_kind(headers):
    """The client's own `X-Oxe-Turn-Kind` (surrounding blanks ignored) when it is a valid label, else None."""
    try:
        value = headers.get(TURN_KIND_HEADER) if headers is not None else None
    except Exception:
        return None
    value = value.strip() if isinstance(value, str) else None
    return value if valid_label(value) else None


def backend_headers(session, turn):
    """Headers the controller adds to a chat request for the Rust engine."""
    out = {}
    if isinstance(session, str) and 1 <= len(session) <= 128:
        out[SESSION_HEADER] = session
    if isinstance(turn, dict) and valid_label(turn.get('turn_kind')):
        out[TURN_KIND_HEADER] = turn['turn_kind']
    return out


def snapshot_keep(headers):
    """True when the client sent `X-Oxe-Snapshot: keep` (any case, surrounding blanks ignored).

    `headers` is the request's header object (`get_all`) or a plain mapping. Any other value is no hint."""
    try:
        values = headers.get_all(SNAPSHOT_HEADER) if hasattr(headers, 'get_all') else [
            v for k, v in headers.items() if isinstance(k, str) and k.lower() == SNAPSHOT_HEADER.lower()]
    except Exception:
        return False
    return any(isinstance(v, str) and v.strip().lower() == 'keep' for v in (values or ()))
