#!/usr/bin/env python3
"""Hormuz testnet state-correction simulator/executor.

Default is simulation only. Set --send-live to send one tx per planned action.
Reads PERPCITY_TESTNET_BOT_PRIVATE_KEY from the local key file and never prints it.
This is for the special Hormuz mirror workflow only, not generic convergence.
"""
from __future__ import annotations

import argparse
import json
import os
import time
from pathlib import Path
from typing import Any

from eth_account import Account
from web3 import Web3

DEFAULT_KEY_FILE = "/opt/data/secrets/perpcity/funding-arb-testnet.env"
DEFAULT_RPC_URL = "https://sepolia-rollup.arbitrum.io/rpc"
TESTNET_USDC = Web3.to_checksum_address("0xBEF280BefeE2Cb28c20D1E4Cc1da999B4DA0f1fD")
MARKETS = {
    "HRMZ-TT": Web3.to_checksum_address("0xA77De9Df6e08BEB8f523153dD0110465190526E3"),
    "HRMZ-CT": Web3.to_checksum_address("0xd802ff15C9D828390dc155BA3908fCbe0E868E62"),
}
CONTROLLED_HOLDER = Web3.to_checksum_address("0x2f9fe7165ed6e1e3034e7e39dd06a12893014917")
U256_MAX = (1 << 256) - 1
MAX_APPROVAL = U256_MAX

PERP_ABI = [
    {
        "type": "function",
        "name": "adjustMaker",
        "stateMutability": "nonpayable",
        "inputs": [{"name": "params", "type": "tuple", "components": [
            {"name": "posId", "type": "uint256"},
            {"name": "marginDelta", "type": "int128"},
            {"name": "liquidityDelta", "type": "int128"},
            {"name": "amt0Limit", "type": "uint256"},
            {"name": "amt1Limit", "type": "uint256"},
        ]}],
        "outputs": [],
    },
    {
        "type": "function",
        "name": "adjustTaker",
        "stateMutability": "nonpayable",
        "inputs": [{"name": "params", "type": "tuple", "components": [
            {"name": "posId", "type": "uint256"},
            {"name": "marginDelta", "type": "int128"},
            {"name": "perpDelta", "type": "int256"},
            {"name": "amt1Limit", "type": "uint256"},
        ]}],
        "outputs": [],
    },
    {
        "type": "function",
        "name": "openMaker",
        "stateMutability": "nonpayable",
        "inputs": [{"name": "params", "type": "tuple", "components": [
            {"name": "holder", "type": "address"},
            {"name": "margin", "type": "uint128"},
            {"name": "tickLower", "type": "int24"},
            {"name": "tickUpper", "type": "int24"},
            {"name": "liquidity", "type": "uint128"},
            {"name": "maxAmt0In", "type": "uint256"},
            {"name": "maxAmt1In", "type": "uint256"},
        ]}],
        "outputs": [{"name": "posId", "type": "uint256"}],
    },
]
ERC20_ABI = [
    {"type": "function", "name": "balanceOf", "stateMutability": "view", "inputs": [{"name": "account", "type": "address"}], "outputs": [{"name": "", "type": "uint256"}]},
    {"type": "function", "name": "allowance", "stateMutability": "view", "inputs": [{"name": "owner", "type": "address"}, {"name": "spender", "type": "address"}], "outputs": [{"name": "", "type": "uint256"}]},
    {"type": "function", "name": "approve", "stateMutability": "nonpayable", "inputs": [{"name": "spender", "type": "address"}, {"name": "amount", "type": "uint256"}], "outputs": [{"name": "", "type": "bool"}]},
]


def read_private_key(path: str) -> str:
    raw = Path(path).read_text()
    for line in raw.splitlines():
        if line.startswith("PERPCITY_TESTNET_BOT_PRIVATE_KEY="):
            return line.split("=", 1)[1].strip()
    raise RuntimeError("key file missing PERPCITY_TESTNET_BOT_PRIVATE_KEY")


def tx_base(w3: Web3, account: str, to: str, data: str, value: int = 0) -> dict[str, Any]:
    tx = {"from": account, "to": to, "data": data, "value": value, "chainId": 421614}
    gas_est = w3.eth.estimate_gas(tx)
    gas_price = w3.eth.gas_price
    return {
        "to": to,
        "data": data,
        "value": value,
        "chainId": 421614,
        "nonce": w3.eth.get_transaction_count(account),
        "gas": int(gas_est * 1.35) + 50_000,
        "gasPrice": int(gas_price * 1.20),
    }


def simulate(w3: Web3, account: str, to: str, data: str) -> tuple[bool, str | None]:
    try:
        w3.eth.call({"from": account, "to": to, "data": data})
        return True, None
    except Exception as exc:  # noqa: BLE001
        msg = str(exc)
        return False, msg[:500]


def send_tx(w3: Web3, acct: Any, to: str, data: str) -> str:
    tx = tx_base(w3, acct.address, to, data)
    signed = acct.sign_transaction(tx)
    raw = getattr(signed, "raw_transaction", None) or signed.rawTransaction
    tx_hash = w3.eth.send_raw_transaction(raw)
    receipt = w3.eth.wait_for_transaction_receipt(tx_hash, timeout=180)
    if receipt.status != 1:
        raise RuntimeError(f"tx reverted: {tx_hash.hex()}")
    return tx_hash.hex()


def hrmz_tt_actions(account: str) -> list[dict[str, Any]]:
    return [
        {"label": "HRMZ-TT remove maker liquidity pos 1", "market": "HRMZ-TT", "kind": "adjustMaker", "params": (1, 0, -32_549_361, 0, 0)},
        {"label": "HRMZ-TT remove maker liquidity pos 2", "market": "HRMZ-TT", "kind": "adjustMaker", "params": (2, 0, -7_029_203, 0, 0)},

        {"label": "HRMZ-TT open target maker", "market": "HRMZ-TT", "kind": "openMaker", "requires_allowance": 100_000_000, "params": (account, 100_000_000, 59_910, 81_630, 2_549_082, U256_MAX, U256_MAX)},
    ]


def build_data(perp: Any, action: dict[str, Any]) -> str:
    if action["kind"] == "adjustMaker":
        return perp.functions.adjustMaker(action["params"]).build_transaction({"gas": 1})["data"]
    if action["kind"] == "openMaker":
        return perp.functions.openMaker(action["params"]).build_transaction({"gas": 1})["data"]
    raise ValueError(action["kind"])


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--key-file", default=os.environ.get("PERPCITY_TESTNET_BOT_KEY_FILE", DEFAULT_KEY_FILE))
    parser.add_argument("--rpc-url", default=os.environ.get("PERPCITY_RPC_URL", DEFAULT_RPC_URL))
    parser.add_argument("--market", choices=["HRMZ-TT"], default="HRMZ-TT")
    parser.add_argument("--only", choices=["all", "close", "open"], default="all")
    parser.add_argument("--send-live", action="store_true")
    args = parser.parse_args()

    key = read_private_key(args.key_file)
    acct = Account.from_key(key)
    w3 = Web3(Web3.HTTPProvider(args.rpc_url, request_kwargs={"timeout": 45}))
    if w3.eth.chain_id != 421614:
        raise RuntimeError(f"wrong chain id {w3.eth.chain_id}")
    if Web3.to_checksum_address(acct.address) != CONTROLLED_HOLDER:
        raise RuntimeError(f"unexpected wallet {acct.address}; expected controlled holder {CONTROLLED_HOLDER}")

    usdc = w3.eth.contract(address=TESTNET_USDC, abi=ERC20_ABI)
    balance = usdc.functions.balanceOf(acct.address).call()
    perp_addr = MARKETS[args.market]
    allowance = usdc.functions.allowance(acct.address, perp_addr).call()
    perp = w3.eth.contract(address=perp_addr, abi=PERP_ABI)

    actions = hrmz_tt_actions(acct.address)
    if args.only == "close":
        actions = [a for a in actions if a["kind"] == "adjustMaker"]
    elif args.only == "open":
        actions = [a for a in actions if a["kind"] == "openMaker"]

    results = []
    for action in actions:
        if action.get("requires_allowance", 0) > allowance:
            approve_data = usdc.functions.approve(perp_addr, MAX_APPROVAL).build_transaction({"gas": 1})["data"]
            ok, err = simulate(w3, acct.address, TESTNET_USDC, approve_data)
            row = {"label": f"approve {args.market}", "simulation_ok": ok, "simulation_error": err, "tx_hash": None}
            if args.send_live:
                if not ok:
                    raise RuntimeError(f"approval simulation failed: {err}")
                row["tx_hash"] = send_tx(w3, acct, TESTNET_USDC, approve_data)
                allowance = MAX_APPROVAL
                time.sleep(2)
            results.append(row)
        data = build_data(perp, action)
        ok, err = simulate(w3, acct.address, perp_addr, data)
        row = {"label": action["label"], "kind": action["kind"], "params": list(action["params"]), "simulation_ok": ok, "simulation_error": err, "tx_hash": None}
        if args.send_live:
            if not ok:
                raise RuntimeError(f"simulation failed for {action['label']}: {err}")
            row["tx_hash"] = send_tx(w3, acct, perp_addr, data)
            time.sleep(2)
        results.append(row)

    print(json.dumps({
        "dry_run": not args.send_live,
        "send_live": args.send_live,
        "chain_id": w3.eth.chain_id,
        "wallet": acct.address,
        "market": args.market,
        "perp": perp_addr,
        "usdc_balance_before_raw": str(balance),
        "usdc_allowance_before_raw": str(allowance),
        "results": results,
    }, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
