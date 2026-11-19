"""Parse an oxa `--usage-log` file into the benchmark's per-trial usage details.

The log has one JSON line per model call (see `oxa/docs/pages/cli.md`, `--usage-log`). This
module reads it, refuses a truncated or malformed log, and returns the numbers the benchmark
reports for one trial:

- `input`, `cached`, `output`: sums over every call (null when any call reported no usage).
- `first_call_cached`: the cached tokens of the first call, which the benchmark excludes from
  the cache rate and prices as fresh.
- `cache_hit_rate_normalized`: (cached - first-call cached) / input.
- `cost_first_cold_usd`: cost at the frozen prices with the first call's cached tokens priced
  as fresh.

A missing count stays null. Nothing is filled in with zero.

Usage: python3 usage_details_oxa.py USAGE.jsonl
"""

import argparse
import json
import sys

# Frozen prices, USD per million tokens (scripts/pricing.json of the frontierharness eval).
PRICE_PER_MILLION = {"input": 3.00, "cached": 0.30, "output": 15.00}

REQUIRED_KEYS = (
    "request",
    "turn",
    "kind",
    "model",
    "usage_reported",
    "prompt",
    "cached",
    "output",
    "reasoning_tokens",
    "duration_ms",
)


class UsageLogError(ValueError):
    """The usage log is empty, truncated, malformed or not in the expected shape."""


def _is_count(value):
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def _check_record(record, line_no):
    if not isinstance(record, dict):
        raise UsageLogError(f"line {line_no} is not a JSON object")
    missing = [key for key in REQUIRED_KEYS if key not in record]
    if missing:
        raise UsageLogError(f"line {line_no} lacks {', '.join(missing)}")
    if not _is_count(record["request"]) or record["request"] < 1:
        raise UsageLogError(f"line {line_no}: request must be a number from 1")
    if not _is_count(record["turn"]):
        raise UsageLogError(f"line {line_no}: turn must be a count")
    if not isinstance(record["kind"], str):
        raise UsageLogError(f"line {line_no}: kind must be a string")
    if record["model"] is not None and not isinstance(record["model"], str):
        raise UsageLogError(f"line {line_no}: model must be a string or null")
    if not isinstance(record["usage_reported"], bool):
        raise UsageLogError(f"line {line_no}: usage_reported must be a boolean")
    for key in ("prompt", "cached", "output", "duration_ms"):
        if record[key] is not None and not _is_count(record[key]):
            raise UsageLogError(f"line {line_no}: {key} must be a count or null")
    if record["usage_reported"]:
        if record["prompt"] is None or record["output"] is None:
            raise UsageLogError(f"line {line_no}: reported usage lacks prompt or output")
    elif record["prompt"] is not None or record["cached"] is not None or record["output"] is not None:
        raise UsageLogError(f"line {line_no}: counts without reported usage")
    if record["prompt"] is not None and record["cached"] is not None and record["cached"] > record["prompt"]:
        raise UsageLogError(f"line {line_no}: cached is more than prompt")


def read_records(path):
    """Return the records of a usage log, or raise UsageLogError.

    The writer ends every line with a newline, so a file without a final newline was cut
    short. Request numbers must run 1, 2, 3 ... with no gap or repeat.
    """
    try:
        with open(path, "rb") as handle:
            raw = handle.read()
        text = raw.decode("utf-8")
    except (OSError, UnicodeDecodeError) as exc:
        raise UsageLogError(f"cannot read usage log: {exc}") from exc
    if not text:
        raise UsageLogError("usage log is empty: the run made no model call")
    if not text.endswith("\n"):
        raise UsageLogError("usage log does not end with a newline (truncated)")
    records = []
    for line_no, line in enumerate(text.split("\n")[:-1], start=1):
        if not line.strip():
            raise UsageLogError(f"line {line_no} is blank")
        try:
            record = json.loads(line)
        except ValueError as exc:
            raise UsageLogError(f"line {line_no} is not JSON (truncated or corrupt)") from exc
        _check_record(record, line_no)
        records.append(record)
    for index, record in enumerate(records, start=1):
        if record["request"] != index:
            raise UsageLogError(
                f"request {record['request']} is out of place: expected {index} (lines missing or repeated)"
            )
    return records


def summarize(records, prices=PRICE_PER_MILLION):
    """Return the benchmark's usage details for one trial's records (already validated)."""
    if not records:
        raise UsageLogError("no records to summarize")
    unreported = sum(1 for record in records if not record["usage_reported"])
    all_reported = unreported == 0
    cached_known = all_reported and all(record["cached"] is not None for record in records)

    input_total = sum(record["prompt"] for record in records) if all_reported else None
    output_total = sum(record["output"] for record in records) if all_reported else None
    cached_total = sum(record["cached"] for record in records) if cached_known else None
    first_cached = records[0]["cached"] if cached_known else None
    durations = [record["duration_ms"] for record in records]
    duration_total = sum(durations) if all(d is not None for d in durations) else None

    cache_rate = None
    if cached_total is not None and first_cached is not None and input_total:
        cache_rate = (cached_total - first_cached) / input_total

    cost = None
    if cached_total is not None and first_cached is not None and output_total is not None:
        cached_after_first = cached_total - first_cached
        fresh = input_total - cached_after_first
        cost = (
            fresh * prices["input"]
            + cached_after_first * prices["cached"]
            + output_total * prices["output"]
        ) / 1_000_000

    return {
        "calls": len(records),
        "unreported_calls": unreported,
        "input": input_total,
        "cached": cached_total,
        "output": output_total,
        "first_call_cached": first_cached,
        "cache_hit_rate_normalized": cache_rate,
        "cost_first_cold_usd": cost,
        "duration_ms": duration_total,
    }


def parse_usage_log(path):
    """Read and summarize one usage log file."""
    return summarize(read_records(path))


def main(argv=None):
    parser = argparse.ArgumentParser(description="Summarize an oxa --usage-log file as JSON.")
    parser.add_argument("usage_log", help="path to the JSONL file written by oxa --usage-log")
    args = parser.parse_args(argv)
    try:
        details = parse_usage_log(args.usage_log)
    except UsageLogError as exc:
        print(f"usage_details_oxa: {exc}", file=sys.stderr)
        return 2
    json.dump(details, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
