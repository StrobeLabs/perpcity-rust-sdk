# V1 trade receipts

The parser matches the emitting market, event signature and optional position ID. A mined receipt without the expected event returns a permanent error.

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`V1PositionEvent`](../receipt.rs#L13) | Position ID and optional post-trade margin exposed by a v1 event | Implemented by seven v1 contract event bindings | Type bound for the receipt parser. |
| [`V1TradeResult`](../receipt.rs#L46) | A mined trade, its position ID, reported margin and full v1 event | [`parse_v1_receipt`](../receipt.rs#L60) selects the event by market, signature and optional position ID | Receipt consumers record the executed fill and margin directly. |

## Accepted structure

`V1PositionEvent` supplies the position ID and margin through a generic bound. Callers select the expected event at compile time, which preserves its complete ABI payload.
