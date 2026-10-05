//! The beacon's prints: timestamps, the newest-first read, and a mainnet
//! log decoded to its value.

use super::*;

#[tokio::test]
async fn prints_take_log_timestamps_and_read_each_missing_header_once() {
    let logs = vec![
        print_log(10, 0, Q96, Some(42)),
        print_log(20, 1, Q96 * U256::from(2), None),
        print_log(20, 4, Q96 * U256::from(3), None),
        print_log(30, 0, Q96 * U256::from(4), None),
    ];
    let node = FakeNode::new(logs, u64::MAX);
    let prints = beacon_prints(&node.provider(), BEACON, 0, 100)
        .await
        .unwrap();

    let rows: Vec<_> = prints
        .iter()
        .map(|p| {
            (
                p.block_number,
                p.log_index,
                p.timestamp,
                p.index_f64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            (10, 0, 42, 1.0),
            (20, 1, timestamp_of(20), 2.0),
            (20, 4, timestamp_of(20), 3.0),
            (30, 0, timestamp_of(30), 4.0),
        ]
    );
    assert_eq!(node.header_reads(), vec![20, 30]);
}

/// A real `IndexUpdated` log from the Arbitrum One beacon
/// 0x0a33ea45fe9011029641ef63ce8e1c94a8a29990, as the node returned it
/// (tx 0x598da8ad…, which carries `blockTimestamp`).
#[tokio::test]
async fn a_mainnet_print_decodes_to_its_block_and_value() {
    let beacon = address!("0a33ea45fe9011029641ef63ce8e1c94a8a29990");
    let mut log = mined_log(
        beacon,
        b256!("acfc085c9be45d2b3f9e5c09a19d4a95749cc16939519c13e090de3a4cb192c6"),
        bytes!("0000000000000000000000000000000000000010edbd439f2076368fba399c29"),
        0x1e2c_c005,
        3,
    );
    log.block_timestamp = Some(0x6aac_7525);
    let node = FakeNode::new(vec![log], u64::MAX);
    let prints = beacon_prints(&node.provider(), beacon, 0x1e2c_0000, 0x1e2d_0000)
        .await
        .unwrap();
    assert_eq!(prints.len(), 1);
    let print = prints[0];
    assert_eq!(
        (print.block_number, print.log_index, print.timestamp),
        (506_249_221, 3, 1_789_687_077)
    );
    assert!((print.index_f64().unwrap() - 16.928669).abs() < 1e-6);
    assert!(node.header_reads().is_empty());
}

#[tokio::test]
async fn latest_prints_read_back_only_as_far_as_the_limit_needs() {
    let logs = (0..=600_000)
        .step_by(500)
        .map(|block| print_log(block, 0, Q96 * U256::from(block + 1), Some(block)))
        .collect();
    let node = FakeNode::new(logs, 7_000);
    let prints = latest_beacon_prints(&node.provider(), BEACON, 0, 600_000, 5)
        .await
        .unwrap();
    let blocks: Vec<u64> = prints.iter().map(|p| p.block_number).collect();
    assert_eq!(blocks, vec![598_000, 598_500, 599_000, 599_500, 600_000]);
    let served: Vec<_> = node
        .requests()
        .into_iter()
        .filter(|(from, to)| to - from < 7_000)
        .collect();
    assert_eq!(
        served.len(),
        1,
        "one served chunk holds five prints: {served:?}"
    );
    assert!(
        served[0].0 > 590_000,
        "read further back than needed: {served:?}"
    );
}

#[tokio::test]
async fn a_zero_limit_or_zero_beacon_reads_nothing() {
    let node = FakeNode::new(vec![print_log(1, 0, Q96, Some(1))], u64::MAX);
    let provider = node.provider();
    assert!(
        latest_beacon_prints(&provider, BEACON, 0, 10, 0)
            .await
            .unwrap()
            .is_empty()
    );
    let error = beacon_prints(&provider, Address::ZERO, 0, 10)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PerpCityError::Validation(ValidationError::InvalidConfig { .. })
    ));
    assert!(node.requests().is_empty());
}

#[tokio::test]
async fn an_undecodable_print_is_an_error_not_a_gap() {
    let mut short = print_log(20, 0, Q96, Some(20));
    short.inner.data = LogData::new_unchecked(short.topics().to_vec(), Bytes::new());
    let node = FakeNode::new(vec![print_log(10, 0, Q96, Some(10)), short], u64::MAX);
    let error = beacon_prints(&node.provider(), BEACON, 0, 100)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PerpCityError::Validation(ValidationError::DecodeFailed { .. })
    ));
}

#[test]
fn a_zero_print_has_no_float_index() {
    let print = IndexPrint {
        block_number: 1,
        log_index: 0,
        timestamp: 1,
        index: Price::from_x96(U256::ZERO),
    };
    assert!(print.index_f64().is_err());
}
