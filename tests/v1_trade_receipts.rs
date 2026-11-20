//! Recorded Sepolia trade logs from perpcity-indexer PR 54,
//! tests/fixtures/sepolia-v1-63da3c.json. Receipt gas fields are synthetic.
use alloy::primitives::{Address, B256, U256};
use alloy::rpc::types::TransactionReceipt;
use alloy::sol_types::SolEvent;
use perpcity_sdk::contracts::v1::Perp;
use perpcity_sdk::events::receipt::{V1PositionEvent, parse_v1_receipt};
use perpcity_sdk::{ContractError, PerpCityError};
use serde_json::{Value, json};

fn receipt(mut value: Value) -> TransactionReceipt {
    value["type"] = json!("0x2");
    value["status"] = json!("0x1");
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
    let rows: Vec<Value> = serde_json::from_str(include_str!("fixtures/v1-trade-logs.json")).unwrap();
    let rows: Vec<_> = rows.into_iter().map(receipt).collect();
    let perp = rows[0].inner.logs()[0].address();
    for row in &rows[..2] {
        assert_eq!(id::<Perp::MakerOpened>(row, perp), id::<Perp::PositionOwnerUpdated>(row, perp));
    }
    let open = parse_v1_receipt::<Perp::TakerOpened>(&rows[2], perp, None).unwrap();
    assert_eq!(open.pos_id, id::<Perp::PositionOwnerUpdated>(&rows[2], perp));
    assert_eq!(open.margin.unwrap().atoms(), open.event.margin);
    let adjust = parse_v1_receipt::<Perp::TakerAdjusted>(&rows[3], perp, None).unwrap();
    assert_eq!(adjust.margin.unwrap().atoms(), adjust.event.margin);
    for row in &rows[4..] {
        assert_eq!(id::<Perp::TakerClosed>(row, perp), id::<Perp::PositionOwnerUpdated>(row, perp));
    }
    let Err(err) = parse_v1_receipt::<Perp::MakerClosed>(&rows[2], perp, None) else {
        panic!("unexpected maker close");
    };
    assert!(matches!(&err, ContractError::MinedEventMissing { tx_hash, .. } if *tx_hash == rows[2].transaction_hash));
    assert!(!PerpCityError::from(err).is_transient());
}

#[test]
fn maker_adjust_and_close_report_margin() {
    let perp = Address::repeat_byte(1);
    let pos_id = U256::from(42);
    for topic in [Perp::MakerAdjusted::SIGNATURE_HASH, Perp::MakerClosed::SIGNATURE_HASH] {
        // Ten static words after the indexed ID and owner, with margin in slot five.
        let mut words = [0_u8; 320];
        words[191] = 19;
        let row = receipt(json!({
            "transactionHash": B256::ZERO, "from": perp, "to": perp,
            "logs": [{"address": perp, "topics": [topic, B256::from(pos_id), B256::ZERO],
                      "data": alloy::hex::encode_prefixed(words)}]
        }));
        let margin = if topic == Perp::MakerClosed::SIGNATURE_HASH {
            parse_v1_receipt::<Perp::MakerClosed>(&row, perp, Some(pos_id)).unwrap().margin
        } else {
            parse_v1_receipt::<Perp::MakerAdjusted>(&row, perp, Some(pos_id)).unwrap().margin
        };
        assert_eq!(margin.unwrap().atoms(), 19);
    }
}
