# V1 trade receipts

The parser matches the emitting market, event signature and optional position ID and indexed owner. A mined receipt without the expected event returns a permanent error.

| Type | Invariant | Produced by | Consumed by |
|---|---|---|---|
| [`sealed::Sealed`](../receipt.rs#L13) | Only this crate can implement the receipt event trait. | Implemented by the supported event bindings. | Supertrait of the receipt parser bound. |
| [`V1PositionEvent`](../receipt.rs#L17) | Sealed position ID and checked post-fee margin exposed by a v1 event | Implemented by eight v1 contract event bindings | Type bound for the receipt parser. |
| [`V1TradeResult`](../receipt.rs#L61) | A mined trade, its position ID, post-fee margin and full v1 event | [`parse_v1_receipt_for`](../receipt.rs#L84) selects the event by market, signature and optional position ID and indexed owner | Receipt consumers record the executed fill and margin directly. |
| [`MakerExit`](../receipt.rs#L136) | Full maker removal closes or converts the position. | [`parse_maker_exit`](../receipt.rs#L144) selects the emitted outcome. | Strategy layer matches the exit. |

## Accepted structure

The private module holds `Sealed` so only known bindings can implement the trait. `V1PositionEvent` supplies the position ID and margin through a generic bound. Callers select the expected event at compile time, which preserves its complete ABI payload.

`MakerExit` preserves the event and receipt for either terminal maker outcome.
