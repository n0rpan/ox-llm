"""Regenerate the Rust test inputs from the shared synthetic Python cases."""
import json
from pathlib import Path

from http_policy_cases import CASES


if __name__ == '__main__':
    inputs = {name: {key: list(value) if isinstance(value, bytes) else value
                     for key, value in case.items()} for name, case in CASES.items()}
    path = Path(__file__).with_name('goldens') / 'http-policy-inputs.json'
    path.write_text(json.dumps(inputs, indent=2, sort_keys=True) + '\n')
