# V1 pinned reads

The [client](../../DESIGN.md) pins each read to the snapshot block. A position preview returns the fifteen contract fields. The single and batch previews read a revert the same way: the missing-position revert is an empty row, and other reverts retain their bytes and selector classification. Batch previews preserve input order, and an empty batch checks the era. Both preview paths require exactly fifteen words. Physical close capacity reports long and short depth in perp atoms. The X64 depth comes from the two `physicalCap` storage words, because the view rounds down and caps at `u128::MAX`; with `fundingState().makerLiquidity` it supplies the inputs of [`MarketCapacity::fits_maker_removal`](../../../math/DESIGN.md).

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`TvlCap`](mod.rs#L19) | V1 collateral cap and accounted TVL in USDC atoms | [`StateAt::tvl_cap`](../../DESIGN.md) reads both at one block | [`TvlCap::allows_deposit`](mod.rs#L28) checks the inclusive limit for the strategy layer. |

## Accepted structure

`TvlCap` is a terminal read for the strategy layer. A caller checks a proposed deposit against the cap at the snapshot block.

`allows_deposit` takes the proposed deposit as `UsdcAtoms`, the unit the cap is kept in, so the comparison needs no conversion. `physical_close_capacity_x64` returns the raw X64 depth as a `PerSide`, one word per side, because the removal check reads the two storage words without the view's rounding.
