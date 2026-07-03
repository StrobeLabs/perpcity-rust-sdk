#!/usr/bin/env python3
"""Read-only Hormuz mainnet -> Arbitrum Sepolia mirror manifest builder.

This script does not read keys, load wallets, approve allowances, or send txs.
It queries Goldsky only, then prints a JSON manifest showing:
- raw mainnet maker/taker actions for the two Hormuz markets
- current testnet maker/taker actions for HRMZ-TT / HRMZ-CT
- count/drift summary and next replay gates

The output is a manifest/review artifact, not approval to transact.
"""
from __future__ import annotations

import argparse
import json
import sys
import urllib.request
from collections import Counter
from typing import Any

MAINNET_GOLDSKY_URL = (
    "https://api.goldsky.com/api/public/project_cmbawn40q70fj01ws4jmsfj7f/"
    "subgraphs/perp-city-mainnet/36d9387f-20260620_140021/gn"
)
TESTNET_GOLDSKY_URL = (
    "https://api.goldsky.com/api/public/project_cmbawn40q70fj01ws4jmsfj7f/"
    "subgraphs/perp-city/1746d283-20260702_192245/gn"
)

MAINNET_TO_TESTNET = {
    "HORMUZ-TONNAGE-PERP": "HRMZ-TT",
    "HORMUZ-COUNT-PERP": "HRMZ-CT",
}

ACTION_QUERY = """
query HormuzActions {
  _meta { block { number } }
  perps(first: 1000) { id symbol name liquidity capacityLong capacityShort openInterestLong openInterestShort sqrtPriceX96 beacon { id indexX96 } }
  makerActions(first: 1000, orderBy: timestamp, orderDirection: asc) {
    id timestamp actionType liquidation posId
    tickLower tickUpper liquidityDelta marginDelta perpDelta usdDelta
    avgExecutionPriceX96 longCapacityDelta shortCapacityDelta
    position { id perp { id symbol name } }
  }
  takerActions(first: 1000, orderBy: timestamp, orderDirection: asc) {
    id timestamp actionType liquidation posId
    perpDelta marginDelta usdDelta newAmmPriceX96 avgExecutionPriceX96
    longOiDelta shortOiDelta totalFeeAmt lpFeeAmt protocolFeeAmt creatorFeeAmt insuranceFeeAmt
    position { id perp { id symbol name } }
  }
}
"""

HEADERS = {
    "Content-Type": "application/json",
    "Accept": "application/json",
    "User-Agent": "Mozilla/5.0 Hermes read-only Hormuz mirror manifest",
}


def gql(url: str, query: str) -> dict[str, Any]:
    req = urllib.request.Request(
        url,
        data=json.dumps({"query": query}).encode(),
        headers=HEADERS,
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=45) as resp:
        body = json.loads(resp.read())
    if body.get("errors"):
        raise RuntimeError(f"GraphQL errors from {url}: {body['errors']}")
    return body["data"]


def action_symbol(action: dict[str, Any]) -> str | None:
    return (((action.get("position") or {}).get("perp") or {}).get("symbol"))


def normalize_action(action: dict[str, Any], action_family: str) -> dict[str, Any]:
    # Keep raw subgraph strings. Do not decode or rescale in the manifest builder.
    fields = {
        k: v
        for k, v in action.items()
        if k not in {"position"} and v is not None
    }
    perp = ((action.get("position") or {}).get("perp") or {})
    return {
        "family": action_family,
        "source_action_id": action.get("id"),
        "timestamp": action.get("timestamp"),
        "perp_symbol": perp.get("symbol"),
        "perp_id": perp.get("id"),
        "position_id": (action.get("position") or {}).get("id"),
        "raw": fields,
    }


def collect_actions(data: dict[str, Any], symbols: set[str]) -> dict[str, list[dict[str, Any]]]:
    out: dict[str, list[dict[str, Any]]] = {s: [] for s in symbols}
    for family, key in [("maker", "makerActions"), ("taker", "takerActions")]:
        for action in data.get(key, []):
            sym = action_symbol(action)
            if sym in symbols:
                out[sym].append(normalize_action(action, family))
    for actions in out.values():
        actions.sort(key=lambda a: (str(a.get("timestamp") or ""), str(a.get("source_action_id") or "")))
    return out


def action_counts(actions: list[dict[str, Any]]) -> dict[str, int]:
    return dict(Counter(a["family"] for a in actions))


def build_manifest(main_url: str, test_url: str) -> dict[str, Any]:
    main = gql(main_url, ACTION_QUERY)
    test = gql(test_url, ACTION_QUERY)
    main_symbols = set(MAINNET_TO_TESTNET.keys())
    test_symbols = set(MAINNET_TO_TESTNET.values())
    main_actions = collect_actions(main, main_symbols)
    test_actions = collect_actions(test, test_symbols)

    markets: list[dict[str, Any]] = []
    for main_symbol, test_symbol in MAINNET_TO_TESTNET.items():
        ma = main_actions.get(main_symbol, [])
        ta = test_actions.get(test_symbol, [])
        mc = action_counts(ma)
        tc = action_counts(ta)
        markets.append(
            {
                "mainnet_symbol": main_symbol,
                "testnet_symbol": test_symbol,
                "mainnet_action_count": len(ma),
                "testnet_action_count": len(ta),
                "mainnet_counts_by_family": mc,
                "testnet_counts_by_family": tc,
                "count_gap_by_family": {
                    family: mc.get(family, 0) - tc.get(family, 0)
                    for family in sorted(set(mc) | set(tc))
                },
                "mainnet_actions_raw_order": ma,
                "testnet_actions_current_raw_order": ta,
                "status": "needs_review_before_replay"
                if len(ma) != len(ta) or mc != tc
                else "counts_match_but_raw_state_must_still_be_verified",
            }
        )

    return {
        "dry_run": True,
        "will_send_tx": False,
        "source": "Goldsky GraphQL read-only",
        "mainnet_goldsky_url": main_url,
        "testnet_goldsky_url": test_url,
        "mainnet_block": ((main.get("_meta") or {}).get("block") or {}).get("number"),
        "testnet_block": ((test.get("_meta") or {}).get("block") or {}).get("number"),
        "mapping": MAINNET_TO_TESTNET,
        "markets": markets,
        "limitations": [
            "Goldsky action ordering here uses timestamp then action id because block/tx/log indexes are not exposed in this query.",
            "Raw values are intentionally preserved; replay sizing/decimals must be validated against deployed contracts before txs.",
            "If testnet has prior generic actions, do not infer corrective transactions automatically; review drift first.",
            "This manifest is not an execution script and is not approval to transact.",
        ],
        "next_gates": [
            "Review raw action drift per market.",
            "Confirm exact ordering source; use receipts/log indexes if required.",
            "Build eth_call simulation for each missing/replay action.",
            "Ask explicit approval for each live testnet replay transaction.",
        ],
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mainnet-url", default=MAINNET_GOLDSKY_URL)
    parser.add_argument("--testnet-url", default=TESTNET_GOLDSKY_URL)
    parser.add_argument("--output", help="Optional path to write JSON manifest")
    args = parser.parse_args()

    manifest = build_manifest(args.mainnet_url, args.testnet_url)
    encoded = json.dumps(manifest, indent=2, sort_keys=True)
    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(encoded + "\n")
    print(encoded)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
