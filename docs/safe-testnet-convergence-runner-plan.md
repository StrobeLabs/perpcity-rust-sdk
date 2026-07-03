# Safe testnet convergence runner plan

Status: local read-only/implementation plan. This is **not** approval to send transactions, run a daemon, or push code.

## Goal

Get Arbitrum Sepolia testnet convergence across the line while preserving the Hormuz mainnet-mirror exception.

## Hard rule

Generic convergence tooling must never maker/taker these mirror markets:

| Symbol | Testnet perp | Required path |
|---|---|---|
| `HRMZ-TT` | `0xA77De9Df6e08BEB8f523153dD0110465190526E3` | exact mainnet tonnage replay only |
| `HRMZ-CT` | `0xd802ff15C9D828390dc155BA3908fCbe0E868E62` | exact mainnet count replay only |

Use `scripts/hormuz_mirror_manifest.py` to build the read-only replay/drift manifest.

## Generic convergence allowlist

Initial non-Hormuz candidates are in `config/testnet_convergence_allowlist.json`:

1. `MCNT-OAK` — first validation candidate; liquid and close to index.
2. `MCNT-RDP` — liquid but wider gap; stepwise only.
3. `MCNT-HOU` — thin liquidity; likely maker planning first.
4. `CITI-NYC` — thin/expensive; simulation only before live consideration.

`USFA` is blocked pending manual review because the current index appears pathological/tiny.

## Safe runner sequence

For each allowlisted market:

1. Query latest Goldsky + RPC state.
2. Compute direction: `index > mark => LONG`, `index < mark => SHORT`.
3. If maker capacity on the needed side is insufficient, produce maker plan only; do not send.
4. Run `eth_call` simulation / binary search for max executable taker size.
5. Propose one conservative one-shot taker under the simulated max.
6. Ask explicit approval with exact:
   - market
   - perp address
   - direction
   - margin
   - `perp_delta`
   - `amt1_limit`
   - expected price movement
   - stop condition
7. If approved, send one tx, record idempotency state, then recompute before any next tx.

## Current tooling status

- `examples/funding_arb_scanner.rs`: read-only scanner; now hard-excludes Hormuz mirrors.
- `examples/maker_allocation_dry_run.rs`: read-only maker planner; now hard-excludes Hormuz mirrors.
- `examples/testnet_market_sizing.rs`: now excludes Hormuz mirrors by symbol/perp guard and omits them from the hardcoded generic sizing list.
- `examples/testnet_liquidity_once.rs`: one-shot maker helper; now blocks Hormuz mirror market/perp inputs.
- `examples/testnet_taker_once.rs`: one-shot taker helper; now blocks Hormuz mirror market/perp inputs.
- `scripts/hormuz_mirror_manifest.py`: read-only Goldsky manifest/drift builder.

## Not live yet

There is still no daemon/cron/scheduler. That is intentional until:

1. local compile/checks pass in a Rust toolchain environment,
2. dry-run manifests are reviewed,
3. Nirel approves bounded testnet one-shots,
4. we complete a small soak without unexpected reverts/drift.

## Next approval gate

After read-only verification, ask for separate approval before any `SEND_LIVE=1` maker/taker action.
