# Fuzzing the controller (oxc)

Task 61, part A. These are the fast, deterministic checks that run on every pull request; the
same properties can be explored for longer locally:

```sh
cd lab
python3 -m venv /tmp/oxc-fuzz
/tmp/oxc-fuzz/bin/pip install -r tests/fuzz/requirements.txt
PYTHONPATH=src /tmp/oxc-fuzz/bin/python -m unittest discover -s tests/fuzz -v
# longer: 20k examples per property instead of 250
PYTHONPATH=src /tmp/oxc-fuzz/bin/python - <<'PY'
from hypothesis import settings
settings.register_profile('long', max_examples=20000, deadline=None)
settings.load_profile('long')
import unittest
unittest.main(module=None, argv=['fuzz', 'discover', '-s', 'tests/fuzz', '-v'])
PY
```

`corpus/` holds the committed SSE reproductions (CRLF, CR-only, comments, split JSON). The
tests never open a network socket and never touch the real controller state: the stream
translators and request normalisers are pure, and the ledger test uses a temporary directory.

The properties are: no exception escapes a translator on malformed upstream bytes, a frame is
delivered as soon as its blank line arrives for every line ending and every split point, an
unterminated frame is bounded, and ledger rows that are not objects/numbers are skipped.
