# V1 pinned reads

The [client](../../DESIGN.md) pins each read to the snapshot block. A position preview returns the fifteen contract fields. Batch previews preserve input order and map the missing-position revert to an empty row. Physical close capacity reports long and short depth in perp atoms.

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`TvlCap`](mod.rs#L15) | V1 collateral cap and accounted TVL in USDC atoms | [`StateAt::tvl_cap`](mod.rs#L86) reads both at one block | [`TvlCap::allows_deposit`](mod.rs#L24) checks the inclusive limit for the strategy layer. |

## Accepted structure

`TvlCap` is a terminal read for the strategy layer. A caller checks a proposed deposit against the cap at the snapshot block.
