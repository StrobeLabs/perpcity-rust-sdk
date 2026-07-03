#!/usr/bin/env python3
"""Read-only state-correction planner for existing Hormuz mirror testnet markets.

This script does not read keys, load wallets, approve allowances, or send txs.
It compares current active mainnet positions against current active Arbitrum Sepolia
mirror positions and prints the safest correction plan for keeping the existing
HRMZ-TT / HRMZ-CT markets rather than redeploying fresh mirrors.

Important limitation: exact action-history mirroring is already impossible on the
existing testnet markets if they contain non-matching prior actions. This planner
therefore focuses on approximate current-state correction and explicitly flags
positions that cannot be closed unless their holder signs or the position is
otherwise closable/liquidatable.
"""
from __future__ import annotations

import argparse
import json
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

MARKET_PAIRS = {
    "HORMUZ-TONNAGE-PERP": "HRMZ-TT",
    "HORMUZ-COUNT-PERP": "HRMZ-CT",
}

# Known local/testnet bot holder from current testnet positions. This is public
# address data from Goldsky, not a private key. Override with --controlled-holder
# if a different signer will be used for correction simulations.
DEFAULT_CONTROLLED_HOLDERS = {"0x2f9fe7165ed6e1e3034e7e39dd06a12893014917"}

QUERY = """
query HormuzState {
  _meta { block { number } }
  perps(first: 1000) {
    id symbol name liquidity capacityLong capacityShort openInterestLong openInterestShort sqrtPriceX96
    beacon { id indexX96 }
  }
  positions(first: 1000) {
    id
    perp { id symbol name }
    posId holder isMaker isClosed wasMakerLiquidated wasTakerLiquidated
    margin perpDelta usdDelta tickLower tickUpper liquidity capacityLong capacityShort
    avgEntryAmmPriceX96 openedAtTimestamp convertedToTakerTimestamp closedAtTimestamp
  }
}
"""

HEADERS = {
    "Content-Type": "application/json",
    "Accept": "application/json",
    "User-Agent": "Mozilla/5.0 Hermes read-only Hormuz state correction planner",
}


def gql(url: str) -> dict[str, Any]:
    req = urllib.request.Request(
        url,
        data=json.dumps({"query": QUERY}).encode(),
        headers=HEADERS,
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=45) as resp:
        body = json.loads(resp.read())
    if body.get("errors"):
        raise RuntimeError(f"GraphQL errors from {url}: {body['errors']}")
    return body["data"]


def active_positions(data: dict[str, Any], symbol: str) -> list[dict[str, Any]]:
    positions = []
    for pos in data.get("positions", []):
        perp = pos.get("perp") or {}
        if perp.get("symbol") == symbol and not pos.get("isClosed"):
            positions.append(pos)
    positions.sort(key=lambda p: int(p.get("posId") or 0))
    return positions


def market(data: dict[str, Any], symbol: str) -> dict[str, Any]:
    for perp in data.get("perps", []):
        if perp.get("symbol") == symbol:
            return perp
    raise KeyError(f"missing market {symbol}")


def summarize_positions(positions: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "count": len(positions),
        "counts_by_type": dict(Counter("maker" if p.get("isMaker") else "taker" for p in positions)),
        "holders": dict(Counter(p.get("holder") for p in positions)),
        "sum_margin_raw_1e6": str(sum(int(p.get("margin") or 0) for p in positions)),
        "sum_perp_delta_raw": str(sum(int(p.get("perpDelta") or 0) for p in positions)),
        "sum_liquidity_raw": str(sum(int(p.get("liquidity") or 0) for p in positions)),
    }


def position_intent(pos: dict[str, Any], target_symbol: str) -> dict[str, Any]:
    if pos.get("isMaker"):
        return {
            "family": "maker",
            "target_symbol": target_symbol,
            "holder": pos.get("holder"),
            "margin_raw_1e6": pos.get("margin"),
            "tick_lower": pos.get("tickLower"),
            "tick_upper": pos.get("tickUpper"),
            "liquidity_raw": pos.get("liquidity"),
            "note": "Open maker with exact raw ticks/liquidity/margin if contract simulation accepts it.",
        }
    return {
        "family": "taker",
        "target_symbol": target_symbol,
        "holder": pos.get("holder"),
        "margin_raw_1e6": pos.get("margin"),
        "perp_delta_raw_1e18": pos.get("perpDelta"),
        "note": "Open taker with exact raw margin/perpDelta if contract simulation accepts it; price path may require stepwise ordering.",
    }


def close_intent(pos: dict[str, Any], controlled_holders: set[str]) -> dict[str, Any]:
    holder = (pos.get("holder") or "").lower()
    controlled = holder in controlled_holders
    if pos.get("isMaker"):
        action = {
            "family": "maker",
            "testnet_symbol": (pos.get("perp") or {}).get("symbol"),
            "pos_id": pos.get("posId"),
            "holder": pos.get("holder"),
            "owned_by_controlled_holder": controlled,
            "liquidity_delta_raw": str(-int(pos.get("liquidity") or 0)),
            "margin_delta_raw_1e6": str(-int(pos.get("margin") or 0)),
            "tick_lower": pos.get("tickLower"),
            "tick_upper": pos.get("tickUpper"),
            "required_contract_call": "adjustMaker",
        }
    else:
        action = {
            "family": "taker",
            "testnet_symbol": (pos.get("perp") or {}).get("symbol"),
            "pos_id": pos.get("posId"),
            "holder": pos.get("holder"),
            "owned_by_controlled_holder": controlled,
            "perp_delta_raw_1e18": str(-int(pos.get("perpDelta") or 0)),
            "margin_delta_raw_1e6": str(-int(pos.get("margin") or 0)),
            "required_contract_call": "adjustTaker",
        }
    if not controlled:
        action["blocker"] = "Cannot close/adjust unless this holder signs or the position is otherwise liquidatable/backstoppable."
    return action


def build_plan(main_url: str, test_url: str, controlled_holders: set[str]) -> dict[str, Any]:
    main = gql(main_url)
    test = gql(test_url)
    markets = []
    for main_symbol, test_symbol in MARKET_PAIRS.items():
        main_positions = active_positions(main, main_symbol)
        test_positions = active_positions(test, test_symbol)
        close_existing = [close_intent(p, controlled_holders) for p in test_positions]
        blocked_closes = [p for p in close_existing if not p.get("owned_by_controlled_holder")]
        reopen_target = [position_intent(p, test_symbol) for p in main_positions]
        exact_state_possible_with_existing_market = len(blocked_closes) == 0
        markets.append(
            {
                "mainnet_symbol": main_symbol,
                "testnet_symbol": test_symbol,
                "mainnet_perp_state": market(main, main_symbol),
                "testnet_perp_state_current": market(test, test_symbol),
                "mainnet_active_summary": summarize_positions(main_positions),
                "testnet_active_summary_current": summarize_positions(test_positions),
                "state_correction_strategy": [
                    "1. Close all existing active testnet mirror positions that do not belong in the target state.",
                    "2. Re-open target active positions derived from mainnet, using the testnet mirror perp and exact raw margin/ticks/liquidity/perpDelta where possible.",
                    "3. Simulate every close/open with eth_call in order before any approval request for live txs.",
                    "4. Re-query Goldsky/RPC after each tx; stop on any mismatch or unexpected revert.",
                ],
                "exact_state_possible_with_existing_market": exact_state_possible_with_existing_market,
                "blockers": [
                    "Exact action-history mirroring is impossible on this already-used testnet market; this is current-state correction only."
                ]
                + [b["blocker"] + f" posId={b['pos_id']} holder={b['holder']}" for b in blocked_closes],
                "close_existing_testnet_positions_first": close_existing,
                "then_open_mainnet_target_positions_on_testnet": reopen_target,
            }
        )
    return {
        "dry_run": True,
        "will_send_tx": False,
        "source": "Goldsky GraphQL read-only",
        "mainnet_block": ((main.get("_meta") or {}).get("block") or {}).get("number"),
        "testnet_block": ((test.get("_meta") or {}).get("block") or {}).get("number"),
        "controlled_holders_assumed": sorted(controlled_holders),
        "markets": markets,
        "global_next_gates": [
            "Add sim-only correction executor that can encode adjustMaker/adjustTaker/openMaker/openTaker for these exact raw intents.",
            "Run simulation with SEND_LIVE unset; do not read or print private keys beyond deriving wallet address inside the SDK helper.",
            "Ask explicit approval before any testnet transaction, naming market, posId/action, data changed, cost, risk, and rollback path.",
        ],
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mainnet-url", default=MAINNET_GOLDSKY_URL)
    parser.add_argument("--testnet-url", default=TESTNET_GOLDSKY_URL)
    parser.add_argument("--controlled-holder", action="append", default=[])
    parser.add_argument("--output", help="Optional path to write JSON plan")
    args = parser.parse_args()

    controlled = {h.lower() for h in DEFAULT_CONTROLLED_HOLDERS}
    controlled.update(h.lower() for h in args.controlled_holder)
    plan = build_plan(args.mainnet_url, args.testnet_url, controlled)
    encoded = json.dumps(plan, indent=2, sort_keys=True)
    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(encoded + "\n")
    print(encoded)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
