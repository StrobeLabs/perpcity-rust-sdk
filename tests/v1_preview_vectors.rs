//! Named W12 fields mapped to the v1 ABI. An absent field has a zero word.
use alloy::primitives::I256;
use alloy::sol_types::SolCall;
use perpcity_sdk::contracts::v1::Perp;
use serde_json::{Value, json};

fn check(row: &Value) {
    let expected = &row["expected"];
    let field = |key: &str| expected[key].as_str().unwrap_or("0");
    let maker = row["kind"] == "maker_preview_value";
    let mark = if maker { "mark_x96" } else { "mark_price" };
    let fields = [
        if maker { "1" } else { "0" },
        field(mark),
        field("settled_margin"),
        field("pending_lp_fees"),
        field("pending_funding"),
        field("pending_util"),
        field("pending_reserve_util"),
        field("value"),
        field("unrealized_pnl"),
        field("equity"),
        field("meets_initial_margin"),
        field("liquidatable"),
        field("backstoppable"),
        field("nominal_liq_fee"),
        field("payable_liq_fee"),
    ];
    let words: Vec<u8> = fields
        .iter()
        .flat_map(|s| s.parse::<I256>().unwrap().into_raw().to_be_bytes::<32>())
        .collect();
    let p = Perp::previewPositionCall::abi_decode_returns(&words).unwrap();
    let decoded = json!({
        "is_maker": p.isMaker, (mark): p.markPrice.to_string(),
        "settled_margin": p.settledMargin.to_string(), "pending_lp_fees": p.pendingLpFees.to_string(),
        "pending_funding": p.pendingFunding.to_string(), "pending_util": p.pendingUtil.to_string(),
        "pending_reserve_util": p.pendingReserveUtil.to_string(), "value": p.value.to_string(),
        "unrealized_pnl": p.unrealizedPnl.to_string(), "equity": p.equity.to_string(),
        "meets_initial_margin": u8::from(p.meetsInitialMargin).to_string(),
        "liquidatable": u8::from(p.liquidatable).to_string(),
        "backstoppable": u8::from(p.backstoppable).to_string(),
        "nominal_liq_fee": p.nominalLiqFee.to_string(), "payable_liq_fee": p.payableLiqFee.to_string(),
    });
    assert_eq!(p.isMaker, maker);
    for (name, value) in expected.as_object().unwrap() {
        assert_eq!(&decoded[name], value, "{} {name}", row["id"]);
    }
}

#[test]
fn decode_w12_preview_words() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/w12-previews.json")).unwrap();
    assert_eq!(
        cases
            .iter()
            .filter(|r| r["kind"] == "taker_preview")
            .count(),
        41
    );
    assert_eq!(
        cases
            .iter()
            .filter(|r| r["kind"] == "maker_preview_value")
            .count(),
        36
    );
    for row in cases {
        check(&row);
    }
    for (initial, liquidatable, backstoppable) in
        [("1", "0", "0"), ("0", "1", "0"), ("0", "0", "1")]
    {
        check(
            &json!({"id":"synthetic", "kind":"maker_preview_value", "expected":{
                "pending_lp_fees":"37","payable_liq_fee":"11","meets_initial_margin":initial,
                "liquidatable":liquidatable,"backstoppable":backstoppable
            }}),
        );
    }
}
