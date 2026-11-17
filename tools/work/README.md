# Shadow comparison

Run the comparison locally against a captured shadow JSONL file and a directory
containing `merge-flow.log`, `auto-approve.log` and `night-watch.log`:

```sh
python3 -I tools/work/compare_shadow.py shadow.jsonl --lab-dir ~/lab-logs --date 2026-01-02 --timezone UTC
```

`--date` is the final day in each lab log; midnight rollovers are inferred backwards.
Supply logs ending on that day, in the named timezone. Events are compared within
each snapshot's next five minutes, stopping at the next snapshot if earlier.
Only eligible `would_take` entries are predictions. Unmatched actions print as
`lab_only` or `shadow_only`; these are observations, not correctness verdicts.
Unknown lab messages with a timestamp and PR number remain `unclassified`.
Raw lines, actors and commit identifiers are never printed. Missing logs, malformed
JSON and unsupported schemas fail rather than producing a clean comparison.
This is development tooling; the product runner does not invoke Python.
