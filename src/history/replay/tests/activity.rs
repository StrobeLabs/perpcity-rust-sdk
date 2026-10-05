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
