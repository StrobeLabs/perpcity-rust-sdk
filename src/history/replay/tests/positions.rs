//! Positions and the pool over two positions' lives: a taker sized by its
//! swaps, a maker's band and the tick map as sums, and what a segment
//! that starts mid-life knows.

use super::*;
use crate::math::liquidity::liquidity_change_delta;
use crate::math::tick::get_sqrt_ratio_at_tick;

fn tick(gross: u128, net: i128) -> TickLiquidity {
    TickLiquidity {
        gross: LUnits::new(gross),
        net: LDelta::new(net),
    }
}

/// A maker's band is the range its first liquidity change named and
/// the sum of the changes since; the pool's tick map is the same changes
/// summed per tick, and its liquidity the net at or below the tick.
#[test]
fn a_makers_band_and_the_pools_book_are_the_sum_of_liquidity_changes() {
    let tape = lifecycle();
    let band = |lower, upper, liquidity| {
        MakerBand::new(
            TickRange::new(lower, upper).unwrap(),
            LUnits::new(liquidity),
        )
    };

    // Deposited: the band stands, at the price of the swap before it.
    let deposited = genesis(&tape[..3]);
    let maker = deposited.position(U256::from(2)).unwrap();
    assert_eq!(maker.maker_band(), Some(band(-600, 600, 1_000)));
    assert_eq!(maker.deposit_pool_price(), Some(price(43)));
    assert_eq!(maker.opened(), Some(tape[2].point()));
    assert_eq!(
        deposited.pool_ticks(),
        Some(BTreeMap::from([
            (-600, tick(1_000, 1_000)),
            (600, tick(1_000, -1_000))
        ]))
    );
    assert_eq!(
        deposited.pool_tick(),
        None,
        "no swap has moved the tick, and the first tick is the factory's"
    );
    assert_eq!(deposited.pool_liquidity(), None);

    // A swap moves the tick into the band; the band's liquidity is active.
    let crossed = genesis(&tape[..5]);
    assert_eq!(crossed.pool_tick(), Some(10));
    assert_eq!(crossed.pool_liquidity(), Some(LUnits::new(1_000)));

    // Trimmed.
    let trimmed = genesis(&tape[..7]);
    assert_eq!(
        trimmed.position(U256::from(2)).unwrap().maker_band(),
        Some(band(-600, 600, 600))
    );
    assert_eq!(trimmed.pool_liquidity(), Some(LUnits::new(600)));

    // Pulled and converted: the band is gone, the tick map is empty, and
    // the position is a taker of a size no perp event carries, and this
    // tape has none of the pool's to price it.
    let converted = genesis(&tape[..11]);
    let taker = converted.position(U256::from(2)).unwrap();
    assert_eq!(taker.maker_band(), None);
    assert_eq!(taker.taker_size(), None);
    assert_eq!(taker.taker_usd(), None, "no event carries the inventory");
    assert!(!taker.level_known());
    assert_eq!(taker.unpriced(), 3, "every change the band made");
    assert!(taker.is_open());
    assert_eq!(converted.pool_ticks(), Some(BTreeMap::new()));
    assert_eq!(converted.pool_liquidity(), Some(LUnits::ZERO));
    assert_eq!(converted.gaps().unknowns.taker_size_unknown, 1);

    // Closed as a taker.
    let closed = genesis(&tape[..12]);
    let taker = closed.position(U256::from(2)).unwrap();
    assert_eq!(taker.closed(), Some(tape[11].point()));
    assert_eq!(closed.gaps().unknowns.taker_size_unknown, 0);
}

/// A taker's size is the sum of its swaps' perp deltas; each dedicated
/// liquidation event, and each tailed close, counts one liquidation.
#[test]
fn a_taker_is_sized_by_its_swaps_and_its_liquidations_are_counted() {
    let tape = lifecycle();
    let one = U256::from(1);

    let opened = genesis(&tape[..1]);
    let taker = opened.position(one).unwrap();
    assert_eq!(taker.taker_size(), Some(PerpDelta::new(1_000_000)));
    assert_eq!(taker.taker_usd(), Some(UsdcDelta::new(-40_000_000)));
    assert_eq!(taker.opened(), Some(tape[0].point()));
    assert_eq!(taker.liquidations(), 0);

    let added = genesis(&tape[..5]);
    let taker = added.position(one).unwrap();
    assert_eq!(taker.taker_size(), Some(PerpDelta::new(1_500_000)));
    assert_eq!(
        taker.taker_usd(),
        Some(UsdcDelta::new(-80_000_000)),
        "the USD leg is the sum of the swaps' usd deltas"
    );

    let partly = genesis(&tape[..9]);
    let taker = partly.position(one).unwrap();
    assert_eq!(taker.taker_size(), Some(PerpDelta::new(1_200_000)));
    assert_eq!(taker.liquidations(), 1);
    assert!(taker.is_open());

    let whole = genesis(&tape);
    let taker = whole.position(one).unwrap();
    assert_eq!(taker.taker_size(), Some(PerpDelta::ZERO));
    assert_eq!(taker.liquidations(), 2);
    assert_eq!(taker.closed(), Some(tape[12].point()));
    assert_eq!(whole.positions().open().count(), 0);
    assert_eq!(whole.gaps().unknowns.margin_unknown, 0);

    // The retired build's shape says it in the close's tail instead.
    let tailed = genesis(&[
        tape[0],
        row(
            21,
            0,
            MarketEvent::TakerClosed {
                pos_id: one,
                swap: swap(-1_000_000, price(44), 0),
                funding: UsdcDelta::ZERO,
                util_fees: UsdcAtoms::ZERO,
                liquidation_fee: UsdcAtoms::new(500),
                is_liquidation: true,
            },
        ),
    ]);
    assert_eq!(tailed.position(one).unwrap().liquidations(), 1);
}

/// A segment that starts mid-life knows what moved and not where a
/// position stands, and says so: no size, no band, no tick map, and the
/// positions counted as partial.
#[test]
fn a_segment_knows_what_moved_and_not_where_positions_stand() {
    let tape = lifecycle();
    let segment = Replay::fold(&tape[3..9]);

    let taker = segment.position(U256::from(1)).unwrap();
    assert_eq!(taker.opened(), None);
    assert_eq!(taker.taker_size(), None);
    assert_eq!(
        taker.kind(),
        PositionKind::Taker {
            moved: PerpDelta::new(200_000),
            usd: UsdcDelta::new(-80_000_000),
            sized: false,
        }
    );
    assert_eq!(taker.liquidations(), 1);

    let maker = segment.position(U256::from(2)).unwrap();
    assert_eq!(maker.opened(), None);
    assert_eq!(maker.maker_band(), None, "the level is unknown");
    assert_eq!(maker.deposit_pool_price(), None);
    assert_eq!(
        maker.kind(),
        PositionKind::Maker {
            range: Some(TickRange::new(-600, 600).unwrap()),
            liquidity: LDelta::new(-400),
            sized: false,
            deposit_pool_price: None,
            moved: PerpDelta::ZERO,
            usd: UsdcDelta::ZERO,
        }
    );
    assert_eq!(maker.unpriced(), 1, "the tape has no pool price");

    assert_eq!(
        segment.pool_ticks(),
        None,
        "a segment's tick map is not whole"
    );
    assert_eq!(segment.pool_liquidity(), None);
    assert_eq!(segment.pool_tick(), Some(10));
    assert_eq!(
        segment.gaps(),
        Gaps {
            unknowns: Unknowns {
                partial_positions: 2,
                taker_size_unknown: 1,
                margin_unknown: 2,
            },
            ..Gaps::default()
        }
    );
}

/// The combine law over positions and the pool's liquidity, at every cut, from
/// genesis and as segments.
#[test]
fn positions_and_the_pool_combine_at_every_cut() {
    let tape = lifecycle();
    assert_combine_law(Replay::from_genesis(Address::ZERO), &tape);
    assert_combine_law(Replay::default(), &tape);
}

/// What the maker's three changes moved, each at the pool's price then.
fn band_moved() -> (PerpDelta, UsdcDelta) {
    let range = TickRange::new(-600, 600).unwrap();
    [(5, 1_000), (10, -400), (8, -600)]
        .into_iter()
        .map(|(tick, delta)| {
            let sqrt_price = get_sqrt_ratio_at_tick(tick).unwrap();
            liquidity_change_delta(sqrt_price, tick, &range, LDelta::new(delta)).unwrap()
        })
        .fold((PerpDelta::ZERO, UsdcDelta::ZERO), |(perp, usd), moved| {
            (perp + moved.0, usd + moved.1)
        })
}

/// A maker converted to a taker holds what its liquidity changes moved,
/// each priced at the pool's exact price and tick then, so the pool's own
/// events size it where no perp event does.
#[test]
fn a_converted_maker_holds_what_its_liquidity_changes_moved() {
    let tape = priced_lifecycle();
    let (perp, usd) = band_moved();
    assert!(!perp.is_zero(), "the price moved between the changes");

    let converted_at = tape
        .iter()
        .position(|row| matches!(row.event, MarketEvent::MakerConverted { .. }))
        .unwrap();
    let converted = genesis(&tape[..=converted_at]);
    let taker = converted.position(U256::from(2)).unwrap();
    assert_eq!(taker.taker_size(), Some(perp));
    assert_eq!(taker.taker_usd(), Some(usd));
    assert!(taker.level_known());
    assert_eq!(taker.unpriced(), 0);
    assert_eq!(converted.gaps().unknowns.taker_size_unknown, 0);

    // Its close is a swap on top of what it held.
    let closed = genesis(&tape);
    assert_eq!(
        closed.position(U256::from(2)).unwrap().taker_size(),
        Some(perp + PerpDelta::new(-700))
    );
}

/// A segment prices the changes it saw after its first pool price, and
/// holds those before it until it is combined after the segment that saw
/// the price they were made at.
#[test]
fn a_segment_prices_its_first_changes_at_the_cut() {
    let tape = priced_lifecycle();
    let cut = tape.iter().position(|row| row.block_number == 21).unwrap();
    let later = Replay::fold(&tape[cut..]);
    assert_eq!(
        later.position(U256::from(2)).unwrap().unpriced(),
        1,
        "the deposit came before the segment's first pool swap"
    );

    let mut whole = genesis(&tape[..cut]);
    whole.combine(later);
    assert_eq!(whole.position(U256::from(2)).unwrap().unpriced(), 0);
    assert_eq!(whole, genesis(&tape));

    assert_combine_law(Replay::from_genesis(Address::ZERO), &tape);
    assert_combine_law(Replay::default(), &tape);
}
