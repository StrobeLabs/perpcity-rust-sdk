//! W12 expected values in ABI order. Fields absent from a vector use zero.
use alloy::primitives::I256;
use alloy::sol_types::SolCall;
use perpcity_sdk::contracts::v1::Perp;

#[test]
fn decode_w12_preview_words() {
    let cases: Vec<(String, [String; 15])> =
        serde_json::from_str(include_str!("fixtures/w12-previews.json")).unwrap();
    assert_eq!(cases.len(), 77);
    for (id, expected) in cases {
        let words: Vec<u8> = expected.iter().flat_map(|s| {
            s.parse::<I256>().unwrap().into_raw().to_be_bytes::<32>()
        }).collect();
        let preview = Perp::previewPositionCall::abi_decode_returns(&words).unwrap();
        let decoded = [
            u8::from(preview.isMaker).to_string(),
            preview.markPrice.to_string(),
            preview.settledMargin.to_string(),
            preview.pendingLpFees.to_string(),
            preview.pendingFunding.to_string(),
            preview.pendingUtil.to_string(),
            preview.pendingReserveUtil.to_string(),
            preview.value.to_string(),
            preview.unrealizedPnl.to_string(),
            preview.equity.to_string(),
            u8::from(preview.meetsInitialMargin).to_string(),
            u8::from(preview.liquidatable).to_string(),
            u8::from(preview.backstoppable).to_string(),
            preview.nominalLiqFee.to_string(),
            preview.payableLiqFee.to_string(),
        ];
        assert_eq!(decoded, expected, "{id}");
    }
}
