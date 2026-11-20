//! Recorded Sepolia trade logs from perpcity-indexer PR 54,
//! tests/fixtures/sepolia-v1-63da3c.json. Receipt gas fields are synthetic.
use alloy::primitives::{Address, B256, U256};
use alloy::rpc::types::TransactionReceipt;
use alloy::primitives::b256;
use perpcity_sdk::contracts::v1::Perp;
use perpcity_sdk::events::receipt::{MakerExit, V1PositionEvent, minted_position_id, parse_maker_exit, parse_v1_receipt, parse_v1_receipt_for};
use perpcity_sdk::{ContractError, PerpCityError};
use serde_json::{Value, json};

fn receipt(mut value: Value) -> TransactionReceipt {
    value["blockNumber"] = json!("0x1");
    value["blockHash"] = json!(B256::ZERO);
    value["transactionIndex"] = json!("0x0");
    value["contractAddress"] = Value::Null;
    for log in value["logs"].as_array_mut().unwrap() {
        log["blockNumber"] = json!("0x1");
        log["blockHash"] = json!(B256::ZERO);
        log["transactionIndex"] = json!("0x0");
        log["logIndex"] = json!("0x0");
        log["removed"] = json!(false);
    }
    value["type"] = json!("0x2");
    if value["status"].is_null() {
        value["status"] = json!("0x1");
    }
    value["cumulativeGasUsed"] = json!("0x1");
    value["gasUsed"] = json!("0x1");
    value["effectiveGasPrice"] = json!("0x1");
    value["logsBloom"] = json!(format!("0x{}", "00".repeat(256)));
    serde_json::from_value(value).unwrap()
}

fn id<E: V1PositionEvent>(receipt: &TransactionReceipt, perp: Address) -> U256 {
    parse_v1_receipt::<E>(receipt, perp, None).unwrap().pos_id
}

#[test]
fn recorded_position_changes() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/v1-trade-logs.json")).unwrap();
    let rows: Vec<_> = rows.into_iter().map(receipt).collect();
    let perp = rows[0].inner.logs()[0].address();
    for row in &rows[..2] {
        assert_eq!(
            id::<Perp::MakerOpened>(row, perp),
            id::<Perp::PositionOwnerUpdated>(row, perp)
        );
    }
    let open = parse_v1_receipt::<Perp::TakerOpened>(&rows[2], perp, None).unwrap();
    assert_eq!(
        open.pos_id,
        id::<Perp::PositionOwnerUpdated>(&rows[2], perp)
    );
    let posted = open.margin.unwrap().atoms();
    assert_eq!(posted, 199_392_201);
    let adjust = parse_v1_receipt::<Perp::TakerAdjusted>(&rows[3], perp, None).unwrap();
    assert_eq!(adjust.margin.unwrap().atoms(), 249_391_265);
    assert_eq!(
        posted.checked_add_signed(adjust.event.marginDelta).unwrap()
            - u128::try_from(adjust.event.utilFees).unwrap(),
        adjust.event.margin
    );
    assert_eq!(minted_position_id(&rows[2], perp, None).unwrap(), open.pos_id);
    assert!(minted_position_id(&rows[4], perp, None).is_err());
    for row in &rows[4..] {
        assert_eq!(
            id::<Perp::TakerClosed>(row, perp),
            id::<Perp::PositionOwnerUpdated>(row, perp)
        );
    }
    let Err(err) = parse_v1_receipt::<Perp::MakerClosed>(&rows[2], perp, None) else {
        panic!("unexpected maker close");
    };
    assert!(
        matches!(&err, PerpCityError::Contract(ContractError::MinedEventMissing { tx_hash, .. }) if *tx_hash == rows[2].transaction_hash)
    );
    assert_eq!(err.tx_hash(), Some(rows[2].transaction_hash));
    assert!(!err.is_transient());
}

#[test]
fn maker_adjust_and_close_report_margin() {
    let perp = Address::repeat_byte(1);
    let pos_id = U256::from(42);
    let adjusted = b256!("867c59df8ef3ea10a16a572ab85d718d1972f7feabfe847d83d818e60929ca1e");
    let closed = b256!("181a101fe3eb4e5adcaea5a4409bc933dd26341a7c7179252d17b863b99fa671");
    let converted = b256!("5dfddc3ff94e76acf24a54b00d5a855ffd7704f0f7ff0eb2cb54f65df66bc8cc");
    for (topic, expected) in [(adjusted, 19), (closed, 0), (converted, 27)] {
        let mut words = [0_u8; 320];
        words[191] = expected;
        let mut input = json!({
            "transactionHash": B256::ZERO, "from": perp, "to": perp,
            "logs": [{"address": perp, "topics": [topic, B256::from(pos_id), B256::ZERO],
                      "data": alloy::hex::encode_prefixed(words)}]
        });
        let row = receipt(input.clone());
        let margin = if topic == adjusted {
            let mut second = input["logs"][0].clone();
            second["topics"][1] = json!(B256::from(U256::from(43)));
            second["topics"][2] = json!(perp.into_word());
            input["logs"].as_array_mut().unwrap().push(second);
            let trade = parse_v1_receipt_for::<Perp::MakerAdjusted>(
                &receipt(input.clone()), perp, None, Some(perp),
            ).unwrap();
            assert_eq!(trade.pos_id, U256::from(43));
            trade.margin
        } else {
            match parse_maker_exit(&row, perp, pos_id, None).unwrap() {
                MakerExit::Closed(trade) if topic == closed => trade.margin,
                MakerExit::Converted(trade) if topic == converted => trade.margin,
                _ => panic!("wrong maker exit"),
            }
        };
        assert_eq!(margin.unwrap().atoms(), u128::from(expected));
        input["logs"][0]["data"] = json!("0xff");
        let invalid = parse_v1_receipt::<Perp::MakerAdjusted>(&receipt(input.clone()), perp, None);
        if topic == adjusted {
            let error = invalid.err().unwrap();
            assert!(matches!(error, PerpCityError::Contract(ContractError::MinedEventInvalid { .. })));
            assert_eq!(error.tx_hash(), Some(B256::ZERO));
        }
        input["status"] = json!("0x0");
        let error = parse_v1_receipt::<Perp::MakerClosed>(&receipt(input), perp, None).err().unwrap();
        assert!(matches!(error, PerpCityError::Transaction(perpcity_sdk::TransactionError::Reverted { .. })));
        assert_eq!(error.tx_hash(), Some(B256::ZERO));
    }
}
