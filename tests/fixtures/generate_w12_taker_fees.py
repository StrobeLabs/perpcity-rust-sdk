"""Extract the W12 taker_fees.json kinds that the v1 taker writes test.

Usage: python3 tests/fixtures/generate_w12_taker_fees.py taker_fees.json
Source: the W12 vector set (424 cases), taker_fees.json (101 cases), the same
file as StrobeLabs/perpcity-bot-api at 5821534b6e1c51e2e5b05dd03972bb85f5daaca1,
test/fixtures/v1/taker_fees.json.
"""
import json
import sys
from pathlib import Path

KINDS = {"max_total_fee_cap": 32, "taker_slippage_limit": 30, "swap_fill": 7, "swap_fees": 7}
data = json.loads(Path(sys.argv[1]).read_text())
assert data["case_count"] == len(data["cases"]) == 101
rows = []
for kind, count in KINDS.items():
    cases = [case for case in data["cases"] if case["kind"] == kind]
    assert len(cases) == count, (kind, len(cases))
    rows.extend(
        {key: case[key] for key in ("id", "kind", "inputs", "expected", "intermediate") if key in case}
        for case in cases
    )
Path(__file__).with_name("w12-taker-fees.json").write_text(json.dumps(rows, indent=2) + "\n")
