//! The market's activity and its series: one liquidation record however
//! many events say it, and the history the replay keeps under a retention.

use super::*;

/// One arrival however many events say it, with the prices around it;
/// and the same arrival whether the cut falls inside the transaction or
/// before the prices it is recorded against.
#[test]
fn a_liquidation_is_one_arrival_with_the_prices_around_it() {
    let tape = liquidation_tape();
    let market = genesis(&tape);

    let liquidations = market.liquidations();
    assert_eq!(liquidations.len(), 1);
    let arrival = liquidations.last().unwrap();
    assert_eq!(
        arrival.point,
        tape[2].point(),
        "the close, not the event after it"
    );
    assert_eq!(
        arrival.mark,
        Liquidation {
            pos_id: U256::from(1),
            role: PositionRole::Taker,
            fee: UsdcAtoms::new(5_000),
            pool_price_before: Some(price(43)),
            pool_price_after: Some(price(40)),
            index_then: Some(price(42)),
        }
    );
    assert_eq!(market.swaps().len(), 2);
    assert_eq!(market.settlements().len(), 1);
    assert_eq!(market.prints().len(), 1);

    assert_combine_law(Replay::from_genesis(Address::ZERO), &tape);
}

/// The other build's shape: the close says nothing of a liquidation
/// and the dedicated event after it carries no price. The record is
/// the same one, assembled from the transaction's progress, whether
/// the cut falls before the close, between it and the event, or after.
#[test]
fn a_liquidation_said_only_by_the_dedicated_event_is_the_same_record() {
    let mut tape = liquidation_tape();
    let MarketEvent::TakerClosed { is_liquidation, .. } = &mut tape[2].event else {
        unreachable!("the fixture's third row is the close");
    };
    *is_liquidation = false;
    let market = genesis(&tape);

    let liquidations = market.liquidations();
    assert_eq!(liquidations.len(), 1);
    let arrival = liquidations.last().unwrap();
    assert_eq!(arrival.point, tape[3].point(), "the dedicated event");
    assert_eq!(
        arrival.mark,
        Liquidation {
            pos_id: U256::from(1),
            role: PositionRole::Taker,
            fee: UsdcAtoms::new(5_000),
            pool_price_before: Some(price(43)),
            pool_price_after: Some(price(40)),
            index_then: Some(price(42)),
        },
        "the prices around the transaction, not around the event"
    );

    assert_combine_law(Replay::from_genesis(Address::ZERO), &tape);
}

/// The series keep what the contract stated, and a window of it.
#[test]
fn the_replay_keeps_series_and_trims_them_to_a_retention() {
    let tape = liquidation_tape();
    let market = genesis(&tape);
    assert_eq!(market.index().value(), Some(price(42)));
    assert_eq!(
        market.pool_price().samples().len(),
        2,
        "the open's and the close's prices"
    );
    assert_eq!(
        market.pool_price().at(tape[1].point()).map(|r| r.value),
        Some(price(43))
    );
    assert_eq!(
        market
            .pool_price()
            .change_over(Window::seconds(10))
            .map(|r| r.value),
        Some(Change {
            from: price(43),
            to: price(40)
        })
    );

    // The rows are ten seconds apart; a five-second window keeps the
    // last sample and the one before it.
    let mut short =
        Replay::from_genesis(Address::ZERO).retaining(Retention::Last(Window::seconds(5)));
    for row in &tape {
        short.apply(row);
    }
    assert_eq!(short.pool_price().samples().len(), 2);
    assert_eq!(short.pool_price().value(), Some(price(40)));
    assert_eq!(
        short.open_interest().samples().len(),
        1,
        "genesis's zero is the one sample before the window"
    );
    assert_eq!(short.deposited(), UsdcAtoms::ZERO);
}

/// Two wallets, two positions; position 1 changes hands mid-life. A
/// mint lands before the opening trade in the same transaction, as the
/// contract orders them, so the open is the minter's.
fn custody_tape() -> Vec<TapeEvent> {
    let (alice, bob) = (Address::repeat_byte(0x0A), Address::repeat_byte(0x0B));
    let mint = |block, index, to, pos: u64| {
        row(
            block,
            index,
            MarketEvent::PositionTransferred {
                from: Address::ZERO,
                to,
                pos_id: U256::from(pos),
            },
        )
    };
    let open = |block, index, pos: u64, p| {
        row(
            block,
            index,
            MarketEvent::TakerOpened {
                pos_id: U256::from(pos),
                swap: swap(1_000, price(p), 0),
            },
        )
    };
    vec![
        mint(10, 0, alice, 1),
        open(10, 1, 1, 40),
        mint(11, 0, bob, 2),
        open(11, 1, 2, 41),
        row(
            12,
            0,
            MarketEvent::PositionTransferred {
                from: alice,
                to: bob,
                pos_id: U256::from(1),
            },
        ),
        row(
            13,
            0,
            MarketEvent::TakerAdjusted {
                pos_id: U256::from(1),
                swap: swap(500, price(42), 0),
                funding: UsdcDelta::ZERO,
                util_fees: UsdcAtoms::ZERO,
            },
        ),
        // Position 2 closes, then burns: the close before the burn, as the
        // contract orders them.
        row(
            14,
            0,
            MarketEvent::TakerClosed {
                pos_id: U256::from(2),
                swap: swap(-1_000, price(43), 0),
                funding: UsdcDelta::ZERO,
                util_fees: UsdcAtoms::ZERO,
                liquidation_fee: UsdcAtoms::ZERO,
                is_liquidation: false,
            },
        ),
        row(
            14,
            1,
            MarketEvent::PositionTransferred {
                from: bob,
                to: Address::ZERO,
                pos_id: U256::from(2),
            },
        ),
    ]
}

/// A cohort's arrivals are the market's on positions the cohort held when
/// they arrived; its book is the open positions it holds now, and a burn
/// or a close the tape has not yet seen the burn of both end a holding.
#[test]
fn the_custody_filter_scopes_arrivals_to_the_holder_then_and_positions_to_the_holder_now() {
    let tape = custody_tape();
    let market = genesis(&tape);
    let (alice, bob) = (Address::repeat_byte(0x0A), Address::repeat_byte(0x0B));
    let hers = Wallets::one(alice);
    let his = Wallets::one(bob);
    let custody = market.custody();

    let blocks = |swaps: &Arrivals<Swap>| swaps.iter().map(|a| a.point.block).collect::<Vec<_>>();
    assert_eq!(
        blocks(&market.swaps().by(&hers, custody)),
        vec![10],
        "her open; the adjust came after she handed the position over"
    );
    assert_eq!(
        blocks(&market.swaps().by(&his, custody)),
        vec![11, 13, 14],
        "his open, the adjust on the position he received, and his close"
    );
    assert_eq!(
        blocks(
            &market
                .swaps()
                .by(&[alice, bob].into_iter().collect(), custody)
        ),
        vec![10, 11, 13, 14]
    );
    assert!(market.settlements().by(&hers, custody).is_empty());
    assert_eq!(market.settlements().by(&his, custody).len(), 2);

    let held = |market: &Replay, wallets: &Wallets| {
        market
            .positions()
            .held_by(wallets, market.custody())
            .map(|(pos_id, _)| pos_id)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        held(&market, &hers),
        Vec::<U256>::new(),
        "she holds nothing now"
    );
    assert_eq!(
        held(&market, &his),
        vec![U256::from(1)],
        "position 2 is burned; the book is what is held"
    );
    assert_eq!(held(&market, &Wallets::default()), Vec::<U256>::new());

    // The tape ends on the close, before its burn: custody still names him,
    // the fold knows the position closed, and the book agrees with the fold.
    let before_burn = genesis(&tape[..tape.len() - 1]);
    assert_eq!(
        before_burn.custody().holder(U256::from(2)),
        Some(bob),
        "custody has not seen the burn"
    );
    assert_eq!(held(&before_burn, &his), vec![U256::from(1)]);
    assert_eq!(
        held(&genesis(&tape[..tape.len() - 2]), &his),
        vec![U256::from(1), U256::from(2)],
        "and before the close, it is his"
    );
}
