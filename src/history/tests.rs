//! Tests for the history readers, over the in-memory
//! [`FakeNode`](super::test_support::FakeNode).

use std::time::Duration;

use alloy::primitives::{
    Address, B256, Bytes, Log as PrimitiveLog, LogData, U256, address, b256, bytes,
};
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;
use futures_util::TryStreamExt;

use super::scan::{SharedWidths, scan_newest};
use super::test_support::{FakeNode, Mode, mined_log, timestamp_of};
use super::*;
use crate::constants::Q96;
use crate::contracts::{IBeacon, IERC20, Perp, SwapResult};
use crate::errors::{ContractError, PerpCityError, ValidationError};
use crate::events::MarketEvent;

const EMITTER: Address = Address::repeat_byte(0xAA);
const TOPIC: B256 = B256::repeat_byte(0x11);

/// One log every `step` blocks in `0..=last`.
fn logs_every(step: u64, last: u64) -> Vec<Log> {
    (0..=last)
        .step_by(step as usize)
        .map(|block| mined_log(EMITTER, TOPIC, Bytes::new(), block, 0))
        .collect()
}

fn filter() -> Filter {
    Filter::new().address(EMITTER).event_signature(TOPIC)
}

fn blocks(logs: &[Log]) -> Vec<u64> {
    logs.iter().map(|log| log.block_number.unwrap()).collect()
}

/// The accepted requests tile `from..=to` with no gap and no overlap.
fn assert_tiles(accepted: &[(u64, u64)], from: u64, to: u64) {
    let mut sorted = accepted.to_vec();
    sorted.sort_unstable();
    let mut next = from;
    for &(start, end) in &sorted {
        assert_eq!(start, next, "gap or overlap before {start} in {sorted:?}");
        next = end + 1;
    }
    assert_eq!(next, to + 1, "range not covered to its end: {sorted:?}");
}

#[tokio::test]
async fn an_unlimited_provider_is_read_in_growing_chunks() {
    let node = FakeNode::new(logs_every(1_000, 350_000), u64::MAX);
    let logs = get_logs_chunked(&node.provider(), &filter(), 0, 350_000)
        .await
        .unwrap();
    assert_eq!(
        blocks(&logs),
        (0..=350_000).step_by(1_000).collect::<Vec<_>>()
    );
    assert_eq!(
        node.requests(),
        vec![(0, 99_999), (100_000, 299_999), (300_000, 350_000)]
    );
}

#[tokio::test]
async fn a_capped_provider_is_learned_once_and_the_range_read_exactly() {
    let cap = 10_000;
    let node = FakeNode::new(logs_every(777, 120_000), cap);
    let logs = get_logs_chunked(&node.provider(), &filter(), 0, 120_000)
        .await
        .unwrap();
    assert_eq!(
        blocks(&logs),
        (0..=120_000).step_by(777).collect::<Vec<_>>()
    );

    let requests = node.requests();
    let (accepted, rejected): (Vec<_>, Vec<_>) =
        requests.iter().partition(|(from, to)| to - from < cap);
    assert_tiles(&accepted, 0, 120_000);
    // Four halvings from 100k to an accepted 6,250, then two probes
    // between the accepted and rejected widths; none after that.
    assert_eq!(rejected.len(), 6, "rejected requests: {requests:?}");
}

#[tokio::test]
async fn a_dense_stretch_under_a_result_cap_is_narrowed_by_halving() {
    let sparse = logs_every(1_000, 400_000)
        .into_iter()
        .filter(|log| !(300_000..=310_000).contains(&log.block_number.unwrap()));
    let dense = (300_000..=310_000).map(|block| mined_log(EMITTER, TOPIC, Bytes::new(), block, 0));
    let mut logs: Vec<Log> = sparse.chain(dense).collect();
    logs.sort_by_key(|log| log.block_number);
    let expected = blocks(&logs);
    let node = FakeNode::new(logs, u64::MAX).with_max_results(1_000);

    let read = get_logs_chunked(&node.provider(), &filter(), 0, 400_000)
        .await
        .unwrap();
    assert_eq!(blocks(&read), expected);
    let requests = node.requests();
    // Eleven chunks are the least that fit the dense stretch under the
    // cap; the rest is the halving into it and the regrowth after it.
    assert!(
        requests.len() < 40,
        "{} requests to read one dense stretch: {requests:?}",
        requests.len()
    );
}

#[tokio::test]
async fn a_long_capped_scan_retests_the_limit_ever_less_often() {
    let cap = 10_000;
    let node = FakeNode::new(logs_every(5_000, 3_000_000), cap);
    let logs = get_logs_chunked(&node.provider(), &filter(), 0, 3_000_000)
        .await
        .unwrap();
    assert_eq!(logs.len(), 601);
    let requests = node.requests();
    let (accepted, rejected): (Vec<_>, Vec<_>) =
        requests.iter().partition(|(from, to)| to - from < cap);
    assert_tiles(&accepted, 0, 3_000_000);
    // 300 chunks at the cap is the floor. Six rejections learn the
    // limit; each retest (after 16, 32, 64, 128 and 256 accepted
    // chunks) costs about three more.
    assert!(accepted.len() <= 330, "{} accepted", accepted.len());
    assert!(rejected.len() <= 24, "rejected requests: {rejected:?}");
}

#[tokio::test]
async fn newest_first_reads_the_same_range_from_the_top() {
    let node = FakeNode::new(logs_every(500, 60_000), 7_000);
    let provider = node.provider();
    let widths = SharedWidths::new();
    let filter = filter();
    let mut chunks = std::pin::pin!(scan_newest(&provider, &filter, 1_000, 60_000, &widths));
    let mut seen = Vec::new();
    while let Some(chunk) = chunks.try_next().await.unwrap() {
        let chunk = blocks(&chunk);
        if let (Some(last), Some(first)) = (seen.last(), chunk.first()) {
            assert!(first < last, "chunks must arrive newest first");
        }
        seen.extend(chunk.into_iter().rev());
    }
    seen.reverse();
    assert_eq!(seen, (1_000..=60_000).step_by(500).collect::<Vec<_>>());
    let accepted: Vec<_> = node
        .requests()
        .into_iter()
        .filter(|(from, to)| to - from < 7_000)
        .collect();
    assert_tiles(&accepted, 1_000, 60_000);
}

const BEACON: Address = Address::repeat_byte(0xBE);

fn print_log(block: u64, index: u64, value: U256, timestamp: Option<u64>) -> Log {
    let mut log = mined_log(
        BEACON,
        IBeacon::IndexUpdated::SIGNATURE_HASH,
        value.to_be_bytes_vec().into(),
        block,
        index,
    );
    log.block_timestamp = timestamp;
    log
}

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
        .map(|p| (p.block_number, p.log_index, p.timestamp, p.index().unwrap()))
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
    assert!((print.index().unwrap() - 16.928669).abs() < 1e-6);
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
        index_x96: U256::ZERO,
    };
    assert!(print.index().is_err());
}

const USDC: Address = address!("af88d065e77c8cc2239327c5edb3a432268e5831");
const TREASURY: Address = Address::repeat_byte(0x01);
const WALLET_A: Address = Address::repeat_byte(0x0A);
const WALLET_B: Address = Address::repeat_byte(0x0B);
const OUTSIDER: Address = Address::repeat_byte(0x0C);

fn transfer_log(from: Address, to: Address, value: u64, block: u64) -> Log {
    let mut log = mined_log(USDC, B256::ZERO, Bytes::new(), block, 0);
    log.inner.data = LogData::new_unchecked(
        vec![
            IERC20::Transfer::SIGNATURE_HASH,
            from.into_word(),
            to.into_word(),
        ],
        U256::from(value).to_be_bytes_vec().into(),
    );
    log
}

fn flows(transfers: &[TokenTransfer]) -> Vec<(Address, Address, U256, u64)> {
    transfers
        .iter()
        .map(|t| (t.from, t.to, t.value, t.block_number))
        .collect()
}

/// A real Arbitrum One USDC transfer of 89.999998 USDC, as the node
/// returned it in the receipt of tx 0xa55d3cc1…e26e.
#[tokio::test]
async fn a_mainnet_usdc_transfer_decodes_to_its_parties_and_value() {
    let sender = address!("c3da549ee508386a12f3908d5bf3060fd04b89f5");
    let recipient = address!("e4fb292b59e3d2cdcc16a332035058f9796b5786");
    let tx_hash = b256!("a55d3cc1c657e72ac6d34f47fc20ad1ac7dce3de2c497b3bcf1d559057e6e26e");
    let log = Log {
        inner: PrimitiveLog {
            address: USDC,
            data: LogData::new_unchecked(
                vec![
                    b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"),
                    b256!("000000000000000000000000c3da549ee508386a12f3908d5bf3060fd04b89f5"),
                    b256!("000000000000000000000000e4fb292b59e3d2cdcc16a332035058f9796b5786"),
                ],
                bytes!("00000000000000000000000000000000000000000000000000000000055d4a7e"),
            ),
        },
        block_hash: Some(b256!(
            "5414fbeaa040956c8fce9279e1253bbf0772f9a472030ebd72f9c04fb5d90071"
        )),
        block_number: Some(0x1e40_09af),
        block_timestamp: Some(0x6ab1_68d8),
        transaction_hash: Some(tx_hash),
        transaction_index: Some(1),
        log_index: Some(0),
        removed: false,
    };
    let node = FakeNode::new(vec![log], u64::MAX);
    let transfers = token_transfers(
        &node.provider(),
        USDC,
        Some(&[sender]),
        Some(&[recipient]),
        0x1e40_0000,
        0x1e41_0000,
    )
    .await
    .unwrap();
    assert_eq!(
        transfers,
        vec![TokenTransfer {
            block_number: 507_513_263,
            log_index: 0,
            tx_hash,
            from: sender,
            to: recipient,
            value: U256::from(89_999_998u64),
        }]
    );
}

#[tokio::test]
async fn transfers_are_filtered_by_both_sets_and_none_is_any() {
    let logs = vec![
        transfer_log(TREASURY, WALLET_A, 100, 10),
        transfer_log(TREASURY, OUTSIDER, 7, 11),
        transfer_log(WALLET_B, TREASURY, 40, 12),
        transfer_log(OUTSIDER, TREASURY, 5, 13),
        transfer_log(WALLET_A, WALLET_B, 1, 14),
    ];
    let node = FakeNode::new(logs, u64::MAX);
    let provider = node.provider();
    let wallets = [WALLET_A, WALLET_B];

    let out = token_transfers(&provider, USDC, Some(&[TREASURY]), Some(&wallets), 0, 100)
        .await
        .unwrap();
    assert_eq!(flows(&out), vec![(TREASURY, WALLET_A, U256::from(100), 10)]);

    let back = token_transfers(&provider, USDC, Some(&wallets), Some(&[TREASURY]), 0, 100)
        .await
        .unwrap();
    assert_eq!(flows(&back), vec![(WALLET_B, TREASURY, U256::from(40), 12)]);

    let any_recipient = token_transfers(&provider, USDC, Some(&[TREASURY]), None, 0, 100)
        .await
        .unwrap();
    assert_eq!(
        flows(&any_recipient),
        vec![
            (TREASURY, WALLET_A, U256::from(100), 10),
            (TREASURY, OUTSIDER, U256::from(7), 11),
        ]
    );
}

#[tokio::test]
async fn an_unfiltered_or_zero_token_query_is_refused_before_any_request() {
    let node = FakeNode::new(Vec::new(), u64::MAX);
    let provider = node.provider();
    for (token, senders) in [(USDC, None), (Address::ZERO, Some(&[TREASURY][..]))] {
        let error = token_transfers(&provider, token, senders, None, 0, 100)
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                PerpCityError::Validation(ValidationError::InvalidConfig { .. })
            ),
            "{error:?}"
        );
    }
    assert!(node.requests().is_empty());
}

#[tokio::test]
async fn an_empty_set_matches_nothing_without_a_request() {
    let node = FakeNode::new(vec![transfer_log(TREASURY, WALLET_A, 100, 10)], u64::MAX);
    let provider = node.provider();
    for (senders, recipients) in [
        (Some(&[][..]), None),
        (None, Some(&[][..])),
        (Some(&[TREASURY][..]), Some(&[][..])),
    ] {
        let out = token_transfers(&provider, USDC, senders, recipients, 0, 100)
            .await
            .unwrap();
        assert!(out.is_empty(), "{out:?}");
    }
    assert!(node.requests().is_empty());

    let error = token_transfers(&provider, USDC, Some(&[]), None, 100, 0)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            PerpCityError::Validation(ValidationError::InvalidBlockRange { .. })
        ),
        "{error:?}"
    );
}

/// A set over the 1,000-value topic limit is refused before any
/// request; a set at the limit is sent.
#[tokio::test]
async fn a_set_over_the_topic_limit_is_refused() {
    let senders: Vec<Address> = (1..=1_001u64)
        .map(|i| Address::from_word(U256::from(i).into()))
        .collect();
    let node = FakeNode::new(vec![], u64::MAX);
    let provider = node.provider();
    let error = token_transfers(&provider, USDC, Some(&senders), Some(&[WALLET_A]), 0, 100)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            PerpCityError::Validation(ValidationError::InvalidConfig { .. })
        ),
        "{error:?}"
    );
    assert!(node.requests().is_empty());
    token_transfers(&provider, USDC, Some(&senders[..1_000]), None, 0, 100)
        .await
        .unwrap();
    assert_eq!(node.requests(), vec![(0, 100)]);
}

/// An ERC-721 `Transfer` shares the topic but indexes the token id, so
/// it has no data to decode as a value.
#[tokio::test]
async fn a_transfer_that_does_not_decode_is_an_error_not_a_gap() {
    let mut nft = transfer_log(TREASURY, WALLET_A, 0, 10);
    nft.inner.data = LogData::new_unchecked(
        vec![
            IERC20::Transfer::SIGNATURE_HASH,
            TREASURY.into_word(),
            WALLET_A.into_word(),
            B256::with_last_byte(9),
        ],
        Bytes::new(),
    );
    let node = FakeNode::new(vec![nft], u64::MAX);
    let error = token_transfers(&node.provider(), USDC, Some(&[TREASURY]), None, 0, 100)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            PerpCityError::Validation(ValidationError::DecodeFailed { .. })
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_single_rejected_block_is_the_error() {
    let node = FakeNode::new(Vec::new(), 0);
    let error = get_logs_chunked(&node.provider(), &filter(), 0, 3)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PerpCityError::Contract(ContractError::LogsRejected {
            from_block: 0,
            to_block: 0,
            ..
        })
    ));
    assert!(!error.is_transient());
    assert_eq!(node.requests().last(), Some(&(0, 0)));
}

#[tokio::test]
async fn failures_no_narrower_range_fixes_are_returned_at_once() {
    let unanswered = [
        Mode::Http(429),
        Mode::Http(503),
        Mode::RpcError(
            429,
            "Your app has exceeded its compute units per second capacity",
        ),
        Mode::RpcError(
            -32_005,
            "daily request count exceeded, request rate limited",
        ),
    ];
    let refused = [
        Mode::Http(401),
        Mode::Http(403),
        Mode::RpcError(-32_601, "the method eth_getLogs does not exist"),
        Mode::RpcError(-32_700, "parse error"),
    ];
    let cases = unanswered
        .into_iter()
        .map(|mode| (mode, true))
        .chain(refused.into_iter().map(|mode| (mode, false)));
    for (mode, transient) in cases {
        let node = FakeNode::new(Vec::new(), u64::MAX).with_mode(mode);
        let error = get_logs_chunked(&node.provider(), &filter(), 0, 500_000)
            .await
            .unwrap_err();
        assert_eq!(error.is_transient(), transient, "{mode:?}: {error}");
        assert_eq!(node.requests(), vec![(0, 99_999)], "{mode:?}");
    }
}

#[tokio::test]
async fn range_rejections_narrow_to_a_single_block() {
    let modes = [
        Mode::Http(504),
        Mode::RpcError(-32_005, "query returned more than 10000 results"),
        Mode::RpcError(-32_602, "Log response size exceeded"),
    ];
    for mode in modes {
        let node = FakeNode::new(Vec::new(), u64::MAX).with_mode(mode);
        get_logs_chunked(&node.provider(), &filter(), 0, 3)
            .await
            .unwrap_err();
        assert_eq!(node.requests().last(), Some(&(0, 0)), "{mode:?}");
    }
}

const PERP: Address = Address::repeat_byte(0xF0);

/// A mined log carrying a typed event, as a node would return it.
fn mined_event_log<E: SolEvent>(
    event: &E,
    address: Address,
    block: u64,
    index: u64,
    timestamp: Option<u64>,
) -> Log {
    Log {
        inner: PrimitiveLog {
            address,
            data: event.encode_log_data(),
        },
        block_hash: Some(B256::with_last_byte(1)),
        block_number: Some(block),
        block_timestamp: timestamp,
        transaction_hash: Some(B256::with_last_byte(3)),
        transaction_index: Some(0),
        log_index: Some(index),
        removed: false,
    }
}

/// Pack two int128 amounts into a Uniswap V4 `BalanceDelta` (`int256`).
fn pack_balance_delta(amount0: i128, amount1: i128) -> alloy::primitives::I256 {
    let mut bytes = [0u8; 32];
    bytes[0..16].copy_from_slice(&amount0.to_be_bytes());
    bytes[16..32].copy_from_slice(&amount1.to_be_bytes());
    alloy::primitives::I256::from_be_bytes(bytes)
}

fn taker_opened(pos_id: u64) -> Perp::TakerOpened {
    Perp::TakerOpened {
        posId: U256::from(pos_id),
        sr: SwapResult {
            delta: pack_balance_delta(100_000_000, -100_000_000),
            ammPrice: Q96,
            totalFeeAmt: alloy::primitives::I256::try_from(1_000_000i64).unwrap(),
            lpFeeAmt: U256::from(700_000u64),
            protocolFeeAmt: U256::from(100_000u64),
            creatorFeeAmt: U256::from(100_000u64),
            insuranceFeeAmt: U256::from(100_000u64),
        },
    }
}

fn position_mint(owner: Address, pos_id: u64) -> Perp::Transfer {
    Perp::Transfer {
        from: Address::ZERO,
        to: owner,
        tokenId: U256::from(pos_id),
    }
}

#[tokio::test]
async fn the_tape_replays_a_perps_events_and_skips_what_the_feed_skips() {
    let owner = Address::repeat_byte(0x0D);
    let logs = vec![
        mined_event_log(&position_mint(owner, 7), PERP, 10, 2, Some(41)),
        // A log the decoder does not recognize (an approval, say): the
        // live feed skips it, so the tape does too.
        mined_log(PERP, TOPIC, Bytes::new(), 15, 0),
        mined_event_log(&taker_opened(7), PERP, 20, 1, None),
    ];
    let node = FakeNode::new(logs, u64::MAX);
    let tape = market_events(&node.provider(), PERP, 0, 100).await.unwrap();

    assert_eq!(tape.len(), 2, "the stranger log must be skipped: {tape:?}");
    let mint = &tape[0];
    assert_eq!(
        (mint.block_number, mint.log_index, mint.timestamp),
        (10, 2, 41)
    );
    assert!(matches!(
        mint.event,
        MarketEvent::PositionTransferred { from, to, pos_id }
            if from == Address::ZERO && to == owner && pos_id == U256::from(7)
    ));
    let open = &tape[1];
    assert_eq!(
        (open.block_number, open.log_index, open.timestamp),
        (20, 1, timestamp_of(20))
    );
    assert!(matches!(
        open.event,
        MarketEvent::TakerOpened { pos_id, swap }
            if pos_id == U256::from(7) && (swap.perp_delta - 100.0).abs() < 1e-9
    ));
    // Only the recognized event without a log timestamp cost a header read.
    assert_eq!(node.header_reads(), vec![20]);
}

#[tokio::test]
async fn latest_market_events_counts_events_not_skipped_logs() {
    // Recognized events on even blocks, strangers on odd ones.
    let mut logs = Vec::new();
    for block in 1..=10u64 {
        if block % 2 == 0 {
            logs.push(mined_event_log(
                &position_mint(Address::repeat_byte(0x0D), block),
                PERP,
                block,
                0,
                Some(block),
            ));
        } else {
            logs.push(mined_log(PERP, TOPIC, Bytes::new(), block, 0));
        }
    }
    let node = FakeNode::new(logs, u64::MAX);
    let tape = latest_market_events(&node.provider(), PERP, 0, 10, 2)
        .await
        .unwrap();
    let blocks: Vec<u64> = tape.iter().map(|t| t.block_number).collect();
    assert_eq!(blocks, vec![8, 10], "newest two events, oldest first");
}

#[tokio::test]
async fn a_zero_perp_reads_no_tape() {
    let node = FakeNode::new(Vec::new(), u64::MAX);
    let error = market_events(&node.provider(), Address::ZERO, 0, 10)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PerpCityError::Validation(ValidationError::InvalidConfig { .. })
    ));
    assert!(node.requests().is_empty());
}

#[tokio::test]
async fn a_history_handle_keeps_the_learned_width_across_scans() {
    let cap = 10_000;
    let node = FakeNode::new(logs_every(777, 120_000), cap);
    let history = History::new(node.provider());
    history.logs(&filter(), 0, Some(120_000)).await.unwrap();
    let after_first = node.requests().len();

    // A second scan starts at the learned width instead of re-paying the
    // halvings from the initial span.
    history
        .logs(&filter(), 120_001, Some(240_000))
        .await
        .unwrap();
    let second: Vec<_> = node.requests().into_iter().skip(after_first).collect();
    let (first_from, first_to) = second[0];
    assert!(
        first_to - first_from < cap,
        "second scan re-learned the width from scratch: {second:?}"
    );
    // The only rejections left are one periodic retest re-narrowing
    // (a few probes each, as the long-scan budget test prices them),
    // not the initial halvings from the 100k span.
    let rejected = second.iter().filter(|(from, to)| to - from >= cap).count();
    assert!(
        rejected <= 4,
        "more than a periodic retest was rejected: {second:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_handle_reads_windows_concurrently() {
    let latency = Duration::from_millis(250);
    let node = FakeNode::new(logs_every(1_000, 350_000), u64::MAX).with_latency(latency);
    let history = History::new(node.provider());
    let started = tokio::time::Instant::now();
    let logs = history.logs(&filter(), 0, Some(350_000)).await.unwrap();
    assert_eq!(
        blocks(&logs),
        (0..=350_000).step_by(1_000).collect::<Vec<_>>()
    );
    // Four windows carve at the initial span and fly together, so the
    // scan takes about one round trip, not four.
    assert_eq!(node.peak_in_flight(), 4);
    assert!(
        started.elapsed() < latency * 2,
        "windows were read sequentially: {:?}",
        started.elapsed()
    );
    assert_tiles(&node.requests(), 0, 350_000);
}

#[tokio::test]
async fn a_concurrent_scan_learns_a_cap_and_still_reads_exactly() {
    let cap = 10_000;
    let node = FakeNode::new(logs_every(777, 120_000), cap);
    let history = History::new(node.provider());
    let logs = history.logs(&filter(), 0, Some(120_000)).await.unwrap();
    assert_eq!(
        blocks(&logs),
        (0..=120_000).step_by(777).collect::<Vec<_>>()
    );
    let requests = node.requests();
    let (accepted, rejected): (Vec<_>, Vec<_>) =
        requests.iter().partition(|(from, to)| to - from < cap);
    assert_tiles(&accepted, 0, 120_000);
    // The windows in flight when the scan starts each pay their own
    // rejections at the unlearned widths; the budget must stay the same
    // order as the sequential search's six.
    assert!(rejected.len() <= 12, "rejected requests: {requests:?}");
}

#[tokio::test]
async fn a_handles_newest_read_stays_sequential_and_tight() {
    let logs = (0..=600_000)
        .step_by(500)
        .map(|block| print_log(block, 0, Q96 * U256::from(block + 1), Some(block)))
        .collect();
    let node = FakeNode::new(logs, 7_000);
    let history = History::new(node.provider()).with_in_flight(8);
    let prints = history
        .latest_beacon_prints(BEACON, 0, Some(600_000), 5)
        .await
        .unwrap();
    assert_eq!(prints.len(), 5);
    assert_eq!(prints.last().unwrap().block_number, 600_000);
    let served: Vec<_> = node
        .requests()
        .into_iter()
        .filter(|(from, to)| to - from < 7_000)
        .collect();
    assert_eq!(
        served.len(),
        1,
        "a newest-first read must not fan out below its limit: {served:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_handle_accounts_for_every_request_it_sent() {
    let latency = Duration::from_millis(100);
    let cap = 10_000;
    let node = FakeNode::new(logs_every(777, 120_000), cap).with_latency(latency);
    let history = History::new(node.provider()).with_in_flight(1);
    // A fresh handle has counted nothing; its width is the initial span.
    assert_eq!(history.stats().requests, 0);
    assert_eq!(history.stats().learned_width, 100_000);

    let logs = history.logs(&filter(), 0, Some(120_000)).await.unwrap();
    let stats = history.stats();
    let requests = node.requests();
    assert_eq!(stats.requests, requests.len() as u64);
    assert_eq!(
        stats.rejections,
        requests
            .iter()
            .filter(|(from, to)| to - from >= cap)
            .count() as u64
    );
    assert_eq!(stats.logs, logs.len() as u64);
    assert!(
        stats.learned_width > 0 && stats.learned_width < cap,
        "width not learned: {stats:?}"
    );
    // Sequential scan under a paused clock: exactly one latency quantum
    // per request.
    assert_eq!(stats.request_time, latency * stats.requests as u32);

    // Counters are cumulative across the handle's scans; the second
    // range holds no logs, so only requests and time move.
    history
        .logs(&filter(), 120_001, Some(240_000))
        .await
        .unwrap();
    let later = history.stats();
    assert!(later.requests > stats.requests);
    assert_eq!(later.logs, stats.logs);
}

#[tokio::test]
async fn the_handle_reads_to_the_lagged_head_by_default() {
    let node = FakeNode::new(logs_every(1_000, 100_000), u64::MAX).with_head(100_000);
    let history = History::new(node.provider()).with_lag(8);
    assert_eq!(history.tip().await.unwrap(), 99_992);

    let logs = history.logs(&filter(), 0, None).await.unwrap();
    assert_eq!(node.requests(), vec![(0, 99_992)]);
    assert_eq!(logs.last().unwrap().block_number.unwrap(), 99_000);
}

#[tokio::test]
async fn a_reversed_range_is_rejected_before_any_request() {
    let node = FakeNode::new(Vec::new(), u64::MAX);
    let error = get_logs_chunked(&node.provider(), &filter(), 5, 4)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PerpCityError::Validation(ValidationError::InvalidBlockRange {
            from_block: 5,
            to_block: 4
        })
    ));
    assert!(node.requests().is_empty());
}
