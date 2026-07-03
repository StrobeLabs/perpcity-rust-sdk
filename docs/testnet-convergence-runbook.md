# Perp City testnet convergence runbook

Status: local working draft for team circulation. Arbitrum Sepolia/testnet only. Not mainnet approval.

## Direct answer: bot or manual helpers?

This workspace is still manual one-shot scaffolding, not a live autonomous bot.

Current helpers:

- `examples/testnet_market_sizing.rs`: read-only market state, price-impact bounds, and approximate minimum maker capital.
- `examples/testnet_liquidity_once.rs`: sends exactly one maker liquidity transaction when `SEND_LIVE=1`.
- `examples/testnet_taker_once.rs`: simulates or sends exactly one taker transaction when `SEND_LIVE=1`.
- `examples/testnet_mint_usdc.rs`: sends exactly one internal TestnetUSDC mint transaction.

There is no daemon, cron, scheduler, infinite loop, production deploy, mainnet wallet, or autonomous spending path.

## Goal

Use the least testnet capital needed to make each feasible market executable and push AMM mark toward index, while preserving safety:

1. Read mark/index/bounds/capacity/OI.
2. Add maker liquidity only in the useful direction.
3. Use `eth_call`/binary search to find a safe taker size.
4. Send bounded one-shot takers.
5. Recompute after every tx.

## Units and scaling

- Prices are X96-scaled on-chain: `price = raw / 2^96`.
- AMM `sqrtPrice` is sqrt-price X96: `price = (raw / 2^96)^2`.
- Margin and USDC amounts use 1e6.
- Taker capacity, open interest, and `perpDelta` amount0 use 1e18.
- Live `open_taker` must scale `perp_delta` by 1e18. Scaling by 1e6 rounds small bot trades to zero and can cause `ZeroDelta`.

## Directional liquidity rule

Maker liquidity must be placed where takers will move the AMM:

- mark < index => LONG takers => maker range above mark toward index.
- mark > index => SHORT takers => maker range below mark toward index.
- Never use blind +/-20% ranges for convergence. They waste capital and can leave no path to the price-impact-limited target.

## Price-impact step limit

`PerpAmmLogic.swap` first performs the Uniswap swap, then checks post-swap sqrt price against:

`IPriceImpact.sqrtPriceBounds(ammPrice, index, emaAmmPrice, emaIndex)`

Common reverts:

- `PriceImpactTooHigh` (`0xfb30d03a`): taker tried to move beyond current module bounds.
- `InsufficientLiquidityToFill` (`0xed126f97`): not enough active liquidity in the path.
- `MarginRatioTooLow` (`0xb2c649db`): taker margin too low for requested delta.
- `TransferFromFailed` (`0x7939f424`): insufficient allowance/balance or token transfer failure.
- `ZeroDelta` (`0x6f0f5899`): delta rounded to zero or was zero.

Convergence is therefore stepwise: price-impact bounds and EMAs must be recomputed after every taker.

## Minimal capital approximation

For taker size `delta_perp_human` across price interval `[P0, P1]`:

```
sqrt_a = sqrt(min(P0, P1))
sqrt_b = sqrt(max(P0, P1))
raw_delta = delta_perp_human * 1e18
L_needed ~= raw_delta * sqrt_a * sqrt_b / (sqrt_b - sqrt_a)
maker_margin_usdc ~= L_needed * (sqrt_b - sqrt_a) / 1e6
```

This approximation scales linearly with taker size. Treat it as a sizing starting point only. Always validate with an `eth_call` binary search before live takers.

## Future target: historical-index-distribution liquidity

Directional point ranges are acceptable for testnet convergence, but production-quality liquidity should use a historical index distribution:

1. Pull recent index history for the market.
2. Estimate probability mass / volatility bands around likely index values.
3. Allocate liquidity density across ranges proportional to expected taker demand and index occupancy.
4. Keep enough liquidity at current mark -> current impact bound for immediate convergence.
5. Rebalance only when index distribution or mark/index gap moves materially.

This avoids overfunding single ranges and makes maker capital reusable across normal index paths.

## Operating loop

1. Run `testnet_market_sizing`.
2. Exclude markets whose first price-impact chunk needs too much maker margin under the run cap.
3. For each feasible market, pick the next useful directional range:
   - LONG: `[mark, min(index, price_impact_upper)]`
   - SHORT: `[max(index, price_impact_lower), mark]`
4. Add near-min maker liquidity only if current max taker is too small.
5. Run binary search with `testnet_taker_once` in simulation mode.
6. Send a conservative taker below max executable size.
7. Recompute state. Stop at run caps or any unexpected revert.
8. Clean Cargo target after the run.

## Feasibility notes as of 2026-07-02

- MCNT-OAK: feasible and close to target; should converge cheaply with current liquidity.
- MCNT-RDP: feasible stepwise, but each 10% price-impact chunk wants large maker margin for 1e-9 style takers.
- MCNT-HOU: thin liquidity; likely needs maker planning first.
- HRMZ-TT / Hormuz tonnage and HRMZ-CT / Hormuz cargo count: do not touch with generic convergence bots. These markets must mimic mainnet exactly: same liquidity amounts, same ticks, and same taker order/sizes. Any generic maker/taker action is blocked until a market-specific replay plan is approved.
- CITI-NYC: not cheap; first 1e-9 chunk is roughly $7M maker margin and currently not feasible under small testnet caps.

## Current risks

- Bounds depend on EMA values; Goldsky lag or missing EMA data can make estimates stale.
- Maker margin formula is approximate; binary search is authoritative.
- Tiny deltas may look economically meaningless but are useful to validate mechanics.
- Testnet mints and large testnet balances must never be confused with mainnet readiness.
- Mainnet requires a separate capped daemon design, kill switch, monitoring, wallet caps, gas/loss limits, allowlist, and explicit approvals. See `docs/mainnet-live-wallet-runtime-plan.md` for the gated mainnet rollout/runbook draft.
