# /// script
# requires-python = ">=3.11"
# dependencies = ["jsonschema==4.26.0"]
# ///
"""Data-only contract test: uv run oxa/oxa-resources/tests/test_review_schema.py.

Uses a standards validator, not the future runtime validator from #645.
No model, controller, credentials, or network service is used by these tests.
"""

import json
from pathlib import Path
import unittest

from jsonschema import Draft202012Validator


SCHEMA = Path(__file__).resolve().parents[1] / "builtin-skills/review/findings.schema.json"


class ReviewSchema(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        schema = json.loads(SCHEMA.read_text(encoding="utf-8"))
        Draft202012Validator.check_schema(schema)
        cls.validator = Draft202012Validator(schema)

    def test_two_sample_outputs_validate(self):
        samples = [
            {"findings": [], "overall_verdict": "correct"},
            {
                "findings": [
                    {
                        "title": "[P1] Preserve the last byte when slicing",
                        "body": "The exclusive end drops the final byte for every nonempty input; pass the full length instead.",
                        "priority": 1,
                        "confidence": 0.98,
                        "path": "src/buffer.rs",
                        "line_start": 12,
                        "line_end": 14,
                    }
                ],
                "overall_verdict": "incorrect",
            },
        ]
        for sample in samples:
            with self.subTest(sample=sample):
                self.validator.validate(sample)

    def test_invalid_sample_fails(self):
        sample = {
            "findings": [
                {
                    "title": "Unsubstantiated concern",
                    "priority": 7,
                    "confidence": 1.5,
                    "path": "src/buffer.rs",
                    "line_start": 0,
                    "line_end": "many",
                }
            ],
            "overall_verdict": "maybe",
            "unexpected": True,
        }
        errors = list(self.validator.iter_errors(sample))
        self.assertEqual(
            {error.validator for error in errors},
            {"required", "maximum", "minimum", "type", "enum", "additionalProperties"},
        )


if __name__ == "__main__":
    unittest.main()
