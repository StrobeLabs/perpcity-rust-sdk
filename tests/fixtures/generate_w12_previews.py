"""Extract named expected fields from W12 accrual.json and maker_range.json.

Usage: python3 tests/fixtures/generate_w12_previews.py accrual.json maker_range.json
Sources: StrobeLabs/perpcity-bot-api at 5821534b6e1c51e2e5b05dd03972bb85f5daaca1,
test/fixtures/v1/{accrual,maker_range}.json.
"""
import json
import sys
from pathlib import Path

rows = []
for path, kind, count in zip(sys.argv[1:], ("taker_preview", "maker_preview_value"), (41, 36), strict=True):
    cases = [case for case in json.loads(Path(path).read_text())["cases"] if case["kind"] == kind]
    assert len(cases) == count
    rows.extend({key: case[key] for key in ("id", "kind", "expected")} for case in cases)
Path(__file__).with_name("w12-previews.json").write_text(json.dumps(rows, indent=2) + "\n")
