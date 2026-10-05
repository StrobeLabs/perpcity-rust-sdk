//! The tape: custody folded from it, undecodable logs counted, the
//! three-address read in one chain order.

use super::*;

#[tokio::test]
async fn custody_folds_out_of_the_tape_and_answers_at_a_point() {
    let alice = Address::repeat_byte(0x0A);
    let bob = Address::repeat_byte(0x0B);
    let logs = vec![
        mined_event_log(&position_mint(alice, 7), PERP, 10, 0, Some(10)),
        // A trade between the mint and the handoff: the fold ignores it,
        // but it shares a block with the handoff to pin the order key.
        mined_event_log(&taker_opened(7), PERP, 20, 1, Some(20)),
        mined_event_log(&position_transfer(alice, bob, 7), PERP, 20, 2, Some(20)),
        mined_event_log(
            &position_transfer(bob, Address::ZERO, 7),
            PERP,
            30,
            0,
            Some(30),
        ),
        mined_event_log(&position_mint(bob, 9), PERP, 40, 0, Some(40)),
    ];
    let node = FakeNode::new(logs, u64::MAX);
    let tape = market_events(&node.provider(), PERP, 0, 100).await.unwrap();
    let custody = OwnershipLog::fold(&tape);

    let pos = U256::from(7);
    let at = |block, log_index| ChainPoint { block, log_index };
    assert_eq!(custody.owner_at(pos, at(9, 0)), None, "before the mint");
    assert_eq!(custody.owner_at(pos, at(10, 0)), Some(alice));
    assert_eq!(
        custody.owner_at(pos, at(20, 1)),
        Some(alice),
        "the trade in the handoff's block, before it"
    );
    assert_eq!(custody.owner_at(pos, at(20, 2)), Some(bob));
    assert_eq!(custody.owner_at(pos, at(31, 0)), None, "after the burn");
    assert_eq!(custody.latest_owner(pos), Some(bob), "the final holder");

    assert_eq!(custody.len(), 2);
    assert_eq!(
        custody.positions().collect::<Vec<_>>(),
        vec![pos, U256::from(9)]
    );
    assert_eq!(
        custody.transfers(pos).collect::<Vec<_>>(),
        vec![
            (at(10, 0), alice),
            (at(20, 2), bob),
            (at(30, 0), Address::ZERO),
        ]
    );
    // A position the tape never saw, and an empty fold.
    assert_eq!(custody.owner_at(U256::from(11), at(10, 0)), None);
    assert!(OwnershipLog::fold(&[]).is_empty());
}

/// A log of this vocabulary that will not decode is a gap in the tape, and
/// the scan says so rather than either dying or hiding it. Dying would cost
/// a scan of millions of blocks over one log; hiding it is what the old
/// `Option` did, indistinguishably from the admin logs it also skipped.
#[tokio::test]
async fn an_undecodable_known_log_is_counted_and_the_scan_carries_on() {
    // `liquidityDelta` past `i128` is a value no V4 pool produces, so it
    // stands in for a binding that disagrees with the shape on chain.
    let unreadable = IPoolManagerState::ModifyLiquidity {
        id: B256::ZERO,
        sender: Address::ZERO,
        tickLower: alloy::primitives::Signed::<24, 1>::ZERO,
        tickUpper: alloy::primitives::Signed::<24, 1>::ZERO,
        liquidityDelta: alloy::primitives::I256::MAX,
        salt: B256::ZERO,
    };
    let logs = vec![
        mined_event_log(&taker_opened(1), PERP, 10, 0, Some(41)),
        mined_event_log(&unreadable, PERP, 15, 0, Some(42)),
        mined_event_log(&taker_opened(2), PERP, 20, 0, Some(43)),
    ];
    let node = FakeNode::new(logs, u64::MAX);
    let history = History::new(node.provider());

    let tape = history.market_events(PERP, 0, Some(100)).await.unwrap();
    assert_eq!(tape.len(), 2, "the two readable events survive: {tape:?}");
    assert_eq!(
        history.stats().undecodable,
        1,
        "the gap is counted, so a caller can see the tape is short"
    );
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
            if pos_id == U256::from(7) && swap.perp_delta.atoms() == 100_000_000
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

/// The market's tape reads three addresses in one chain order: the perp's
/// own events, the beacon's prints, and the PoolManager's liquidity changes
/// for this pool and no other. Every row carries its block hash.
#[tokio::test]
async fn the_market_tape_reads_three_addresses_in_one_chain_order() {
    let other_pool = B256::repeat_byte(0x78);
    let logs = vec![
        mined_event_log(&taker_opened(1), PERP, 10, 0, Some(41)),
        mined_event_log(
            &IBeacon::IndexUpdated {
                index: Q96 * U256::from(3),
            },
            BEACON,
            10,
            1,
            Some(41),
        ),
        // The PoolManager logs every pool; only this market's pool belongs
        // on its tape.
        mined_event_log(
            &liquidity_change(other_pool, 5, 1_000),
            POOL_MANAGER,
            12,
            0,
            Some(42),
        ),
        mined_event_log(
            &liquidity_change(POOL_ID, 7, 1_000),
            POOL_MANAGER,
            12,
            1,
            Some(42),
        ),
        mined_event_log(&taker_opened(2), PERP, 20, 0, None),
    ];
    let node = FakeNode::new(logs, u64::MAX);
    let history = History::new(node.provider());

    let tape = history
        .market_tape(addresses(), 0, Some(100))
        .await
        .unwrap();

    let points: Vec<(u64, u64)> = tape.iter().map(|t| (t.block_number, t.log_index)).collect();
    assert_eq!(points, vec![(10, 0), (10, 1), (12, 1), (20, 0)], "{tape:?}");
    assert!(matches!(tape[0].event, MarketEvent::TakerOpened { .. }));
    assert!(matches!(
        tape[1].event,
        MarketEvent::IndexUpdated { index } if index == Price::from_x96(Q96 * U256::from(3))
    ));
    assert!(matches!(
        tape[2].event,
        MarketEvent::ModifyLiquidity { pool_id, salt, .. }
            if pool_id == POOL_ID && salt == B256::from(U256::from(7))
    ));
    assert!(
        tape.iter().all(|t| t.block_hash == B256::with_last_byte(1)),
        "every row carries the block hash its log came with"
    );
    assert_eq!(tape[3].timestamp, timestamp_of(20));
    assert_eq!(
        node.header_reads(),
        vec![20],
        "one header read, shared by both scans"
    );
}
