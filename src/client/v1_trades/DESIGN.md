# `v1_trades`: the `v1.0.0` taker writes

Up: [`client`](../DESIGN.md). Each write refuses another build after the era read, carries a deadline `V1_TRADE_DEADLINE` after the signing time and a `maxTotalFee`, and returns the receipt's parsed event from [`events::receipt`](../../events/DESIGN.md), so no `v1.0.0` taker write reports success without its event. A stuck trade is replaced with a new deadline, never with its own bytes, which could mine as `DeadlineExpired`.

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`MaxTotalFee`](../v1_trades.rs#L36) | the `maxTotalFee` a `v1.0.0` taker trade carries: a volume the bound is sized for at the market's rates, or an exact bound. A volume, not a rate, because the caller knows the trade's USDC leg and the SDK knows the rates | the caller, from a buy's `amt1_limit` or a sell's quote | [`PerpClient::open_taker_v1`](../DESIGN.md), [`PerpClient::adjust_taker_v1`](../DESIGN.md) and [`PerpClient::close_taker_v1`](../DESIGN.md), which resolve a volume through `MarketReader::get_taker_fee_rates` and `TakerFeeRates::max_total_fee` at the send. |
| [`V1TakerAdjustment`](../v1_trades.rs#L51) | what a `v1.0.0` taker adjustment mined as, from its receipt's event only: `TakerAdjusted` while the position is open, or `TakerClosed` when no perp amount is left, never both | [`PerpClient::adjust_taker_v1`](../DESIGN.md), over the receipt parsers of [`events::receipt`](../../events/DESIGN.md) | [`V1TakerReplacement`](../v1_trades.rs#L63), which wraps it for a replaced adjustment. The strategy layer records the fill and the margin. |
| [`V1TakerReplacement`](../v1_trades.rs#L63) | the outcome of a replaced `v1.0.0` taker trade: the event of whichever of the two hashes mined, as the stuck call's kind (open, or adjustment) parses it | [`PerpClient::replace_v1_taker`](../DESIGN.md) | nothing in the crate. The strategy layer, which compares the result's hash with the stuck one to know which mined. |

## Accepted structure

The writes take the earlier builds' exact parameters (`ExactOpenTakerParams`, `ExactAdjustTakerParams`) and add the `v1.0.0` fields as arguments, so those structs and their callers are unchanged. The fee bound is a volume, not a rate: the caller knows the trade's USDC leg and the market knows the rates, which are read at the send and not cached.

The replacement is a client method and not a pipeline step: the pipeline's send waits for one hash, and a replacement must poll two. It runs only after that send returned, which `TxPipeline::is_in_flight` checks.
